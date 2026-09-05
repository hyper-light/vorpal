//! MCP protocol handling + the warm-index tool implementations.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{Value, json};

use vorpal_index::{CacheMode, ExtractionEnv, ParseHealthPolicy, build_index_env};

use crate::supervised::{BuildOutcome, Supervisor};

/// Serializes IN-PROCESS builds within this daemon: staging dirs are per-PID, so two
/// same-process builds (the D1 worker's fallback racing a query-path rebuild) would share —
/// and clobber — one staging directory. Child-process builds need no lock (their PIDs differ).
static IN_PROCESS_BUILD: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn in_process_build_guard() -> std::sync::MutexGuard<'static, ()> {
  IN_PROCESS_BUILD
    .lock()
    .unwrap_or_else(std::sync::PoisonError::into_inner)
}
use vorpal_kg::Kg;

use crate::watch::SourceWatch;

use crate::protocol::{Handler, RpcError, decorate_tools};

/// The warm-index MCP server: one persisted index directory, its graph held in memory across
/// calls (lazily cold-opened via mmap on first query, reloaded after each `index` tool call).
///
/// When the index lives at the default `<src>/.vorpal/index` location, the daemon watches
/// `<src>` (§7.5): queries revalidate lazily whenever the watch reports possible changes, so
/// the steady-state freshness check is one atomic load — no walk, no stats — while answers
/// stay as fresh as an explicit re-index. Custom index locations (no derivable source root)
/// keep the explicit-`index`-tool behavior unchanged.
/// Which tool subset this daemon serves. Slimmer surfaces mean fewer tokens of tool schema
/// per agent turn and a smaller blast radius for read-only deployments.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Profile {
  #[default]
  Full,
  /// Read-only navigation + traversal, evidence, and health surfaces.
  Analysis,
  /// Read-only navigation only: find, search, read.
  Scout,
}

impl Profile {
  pub fn parse(text: &str) -> Option<Self> {
    match text {
      "full" => Some(Profile::Full),
      "analysis" => Some(Profile::Analysis),
      "scout" => Some(Profile::Scout),
      _ => None,
    }
  }

  fn label(self) -> &'static str {
    match self {
      Profile::Full => "full",
      Profile::Analysis => "analysis",
      Profile::Scout => "scout",
    }
  }

  /// The single authority on membership: tools_list filters by it and run_tool gates on it,
  /// so the advertised surface and the callable surface can never drift apart.
  fn allows(self, tool: &str) -> bool {
    const SCOUT: &[&str] = &["node", "search", "snippet", "schema", "fetch_span"];
    const ANALYSIS_EXTRA: &[&str] = &[
      "graph", "reachable", "why",
      "health", "dead_code", "coverage", "impact", "compare_generations", "architecture",
      "code_search", "data_flow", "query",
    ];
    match self {
      Profile::Full => true,
      Profile::Analysis => SCOUT.contains(&tool) || ANALYSIS_EXTRA.contains(&tool),
      Profile::Scout => SCOUT.contains(&tool),
    }
  }
}

pub struct Server {
  index_dir: PathBuf,
  profile: Profile,
  /// The extraction environment every rebuild runs under (F-M6): custom/dynamic language
  /// rules, ref specs, canaries, and injection config. Fixed at construction — REGISTRATION
  /// (any dlopen of a grammar .so) is the launching process's one-shot startup act; nothing
  /// reachable through the MCP surface can ever trigger a dlopen.
  env: ExtractionEnv,
  /// Crash isolation for builds (D3): rebuilds run in a child indexer process when one can be
  /// discovered, so a pathological input costs one build attempt, never the daemon.
  supervisor: Supervisor,
  /// `Arc` because the live-rebuild path serves the sealed graph from RAM while the deferred
  /// persistence tail still holds a reference on its background thread.
  kg: Option<Arc<Kg>>,
  /// The resolved generation directory the cached graph was loaded from — the artifacts a
  /// `why` snippet is digest-verified against, so a concurrent `CURRENT` swap can never split
  /// an answer from the pinned graph that produced its ids.
  kg_dir: Option<PathBuf>,
  watch: Option<SourceWatch>,
  /// Hinted-rebuild counter — every 64th watched revalidation full-scans as reconciliation
  /// insurance, even when capture certainty held.
  hinted_rebuilds: u64,
  /// The in-flight restamp-class background canonicalization, if any (serve-immediately
  /// probe). The synchronous rebuild path drains this first so an older stamp-only commit
  /// can never land after — and regress — a newer semantic one.
  canonicalizing: Option<std::thread::JoinHandle<bool>>,
  /// The in-flight PROACTIVE full rebuild (`tick`'s heavy tier): a supervised child indexer
  /// — or an in-process background thread when none is discoverable — committing a
  /// generation while the daemon keeps serving. One more drain-ordered committer: every
  /// other commit path reaps or drains it first, so commits can never invert.
  rebuilding: Option<std::thread::JoinHandle<bool>>,
  /// Watch-quiet debounce for `tick`: set when the watch first reports dirt, cleared when
  /// the proactive build starts — an editor's save burst builds once, after it settles.
  dirty_since: Option<std::time::Instant>,
  /// Whether `tick` may START builds (the D1 toggle, re-homed): `--no-watch-rebuild` /
  /// `VORPAL_WATCH_REBUILD=0` turn proactive building off; query-path freshness is lazy
  /// and unaffected.
  proactive: bool,
  /// The in-flight background ANN warm, if any. Warms are **single-flight and coalescing**:
  /// a rebuild that lands while one is running sets `warm_pending` instead of stacking a
  /// second core-saturating build, and the trailing warm re-resolves `CURRENT` when it
  /// finally spawns — so an edit burst costs at most one wasted warm, and the newest
  /// generation always ends up warm.
  warm: Option<std::thread::JoinHandle<()>>,
  warm_pending: bool,
  /// The in-flight deferred persistence of a live-adopted build (SUBSECOND.md live rebuild
  /// v1): the daemon is already serving the sealed graph; this handle is writing its
  /// generation. `kg_dir` stays `None` until it lands — generation-bound tools drain it
  /// first (see `run_tool`), and the synchronous rebuild path drains it so two committers
  /// never race.
  persisting: Option<std::thread::JoinHandle<Result<PathBuf, String>>>,
  /// The retained live overlay (SUBSECOND.md Phase 3): in-memory pipeline state that turns
  /// a small semantic edit into a sealed scratch-identical graph without replaying the
  /// corpus. Built in the background after a successful rebuild; any change it cannot
  /// absorb exactly drops it (rebuilt later) — stale overlays never serve.
  overlay: Option<vorpal_index::live::LiveOverlay>,
  overlay_building: Option<std::thread::JoinHandle<Result<vorpal_index::live::LiveOverlay, String>>>,
  /// The live vector tier (ANN_FRONTIER.md Tier 3): per-edit tombstone+insert instead of
  /// the full per-generation warm. `live_ann_task` covers both adoption (from a fresh
  /// committed tier) and per-edit updates — the tier travels INTO the task and back, so
  /// the serve path never blocks on O(n) id refreshes.
  live_ann: Option<vorpal_index::live_ann::LiveAnnTier>,
  live_ann_task: Option<
    std::thread::JoinHandle<
      Result<vorpal_index::live_ann::LiveAnnTier, vorpal_index::live_ann::AdoptDecline>,
    >,
  >,
  /// Edit churn that arrived while a tier task was in flight — applied at reap.
  pending_churn: Vec<(Vec<u64>, Vec<u64>)>,
  /// Adoption latch: the generation adoption last declined against, how many attempts
  /// it has consumed there, and whether a completed warm re-armed one retry. Bounds
  /// EVERY decline class — known or future — to at most
  /// [`LIVE_ANN_ATTEMPTS_PER_GENERATION`] attempts per generation: a curable decline
  /// gets exactly one warm-mediated retry; an incurable one (flat tier for this corpus
  /// size) consumes the whole budget at once. The next commit changes the generation
  /// and naturally re-opens the question.
  live_ann_latch: Option<LiveAnnLatch>,
  /// Liveness backstop clocks (see `refresh`): when the last manifest stat sweep ran and
  /// what it cost. `None` cost = never measured; the first eligible quiet query measures.
  last_sweep_at: Option<std::time::Instant>,
  last_sweep_cost: Option<std::time::Duration>,
  /// Set when the served graph advances past an in-flight live-ANN task: the reap
  /// drops that task's result instead of installing a tier keyed to a retired graph.
  live_ann_discard_task: bool,
}

/// The live overlay is on by default; `VORPAL_NO_LIVE_OVERLAY=1` (or `true`/`yes`) keeps the
/// daemon on the replay pipeline for every semantic edit — the escape hatch while the
/// overlay earns trust, and the knob for memory-constrained hosts (the overlay retains the
/// pre-link pipeline state in RAM).
/// Retained persistence (a served build commits its own generation — no replay pipeline in
/// the background) is on by default; `VORPAL_NO_RETAINED_PERSIST=1` restores the full
/// hinted canonicalizer build behind every overlay serve.
fn retained_persist_enabled() -> bool {
  !matches!(
    std::env::var("VORPAL_NO_RETAINED_PERSIST").ok().as_deref(),
    Some("1" | "true" | "yes")
  )
}

fn overlay_enabled() -> bool {
  !matches!(
    std::env::var("VORPAL_NO_LIVE_OVERLAY").ok().as_deref(),
    Some("1" | "true" | "yes")
  )
}

/// Eager ANN warming is on by default; `VORPAL_NO_AUTOWARM=1` (or `true`/`yes`) switches the
/// daemon to fully lazy vector-tier builds — the first semantic search pays instead. For
/// benchmarking, constrained machines, and operators who never use semantic search.
/// Adoption attempts a single generation may consume: the artifacts have exactly two
/// observable states per generation — as committed, and as rewritten by the one classic
/// warm a failed adopt requests — so two judgments exhaust the information available.
/// A structural count, not a tuning value; anything still declining past it is
/// generation-inherent and waits for the next commit.
const LIVE_ANN_ATTEMPTS_PER_GENERATION: u8 = 2;

/// See `Server::live_ann_latch`.
struct LiveAnnLatch {
  generation: PathBuf,
  attempts: u8,
  /// A warm landed since the last attempt — the one signal that may justify a retry
  /// (the warm rewrote the artifacts the last judgment saw). Consumed by the retry.
  rearmed: bool,
}

/// Wall-time share the liveness-backstop sweep may consume on the quiet query path
/// (`refresh`): the sweep re-runs only after 100× its own measured duration has elapsed,
/// i.e. a stated <=1% overhead budget. The PERIOD is therefore data-derived per corpus —
/// microseconds-scale trees re-check within milliseconds, kernel-scale trees every few
/// seconds — and the constant is the policy share, not a tuned interval.
const BACKSTOP_OVERHEAD_INVERSE: u32 = 100;

fn autowarm_enabled() -> bool {
  !matches!(
    std::env::var("VORPAL_NO_AUTOWARM").ok().as_deref(),
    Some("1" | "true" | "yes")
  )
}

/// A clean shutdown lets in-flight committers finish their (short) tails rather than
/// abandoning staged generations to GC. The ANN warm is deliberately NOT joined: it can run
/// for seconds and is stamp-validated + lazily rebuilt, so losing it costs nothing.
impl Handler for Server {
  fn tools(&self) -> Vec<Value> {
    tool_declarations(self.profile)
  }

  fn call_tool(&mut self, name: &str, params: &Value) -> Result<Value, RpcError> {
    if !self.serves(name) {
      return Err(RpcError::invalid_params(format!("Unknown tool: {name}")));
    }
    Ok(self.tool_result(name, params))
  }

  fn instructions(&self) -> Option<String> {
    Some(format!("{INSTRUCTIONS} {}", self.fast_path_note()))
  }
}

impl Server {
  /// The CLI one-liner for THIS index, carried in the server instructions: a client that
  /// loads tool schemas lazily (Claude Code) spends a model turn per tool it loads, while
  /// its shell tool is always resident — so a single lookup through `vorpal graph` is one
  /// turn cheaper than the same lookup through the MCP tool. The path is this daemon's own
  /// index, absolute, and the binary is the one serving (or `vorpal` on PATH).
  fn fast_path_note(&self) -> String {
    let index = std::fs::canonicalize(&self.index_dir).unwrap_or_else(|_| self.index_dir.clone());
    let bin = std::env::current_exe()
      .ok()
      .filter(|exe| exe.file_name().is_some_and(|n| n == "vorpal"))
      .map(|exe| exe.to_string_lossy().into_owned())
      .unwrap_or_else(|| "vorpal".to_string());
    let index = index.display();
    format!(
      "Fast path when this client loads tool schemas lazily: the same graph answers from \
       the shell without a schema load. Exact commands: `{bin} graph callers <name> --index \
       {index} --format lean` (other verbs in that position: refs, importers, implementors, \
       typeusers, similar, node, snippet); what a symbol calls: `{bin} graph reachable <name> \
       --direction out --depth 1 --index {index} --format lean` (--direction in for what \
       reaches it; omit --depth for the full closure). Always the `graph` word, always \
       --index. Prefer it for a single lookup; use the MCP tools for several calls."
    )
  }
}

/// Guidance a client may show its model once; the tool descriptions carry the details.
pub(crate) const INSTRUCTIONS: &str = "Knowledge graph of one indexed repository. Call `graph` \
(relation: callers | references | importers | implementors | type_users | similar | \
observed), `reachable`, `snippet`, or `why` DIRECTLY with the exact symbol name; use `node` \
or `search` only when the name is unknown or ambiguous. Every graph, reachable, and why \
result is the complete resolved set at the grade each row states — never confirm it with \
search, code_search, or grep; callers rows already carry the call-site line. Pass \
`format: \"lean\"` unless you need signatures. If your client defers these tools, load all \
you will need in ONE ToolSearch call. Results page with cursor/limit and name the index \
generation they were read from.";

impl Drop for Server {
  fn drop(&mut self) {
    if let Some(handle) = self.canonicalizing.take() {
      let _ = handle.join();
    }
    if let Some(handle) = self.persisting.take() {
      let _ = handle.join();
    }
  }
}

impl Server {
  pub fn new(index_dir: PathBuf) -> Self {
    Self::with_profile(index_dir, Profile::Full)
  }

  pub fn with_profile(index_dir: PathBuf, profile: Profile) -> Self {
    Self::with_profile_env(index_dir, profile, ExtractionEnv::default())
  }

  pub fn with_profile_env(index_dir: PathBuf, profile: Profile, env: ExtractionEnv) -> Self {
    Self::with_profile_env_rebuild(index_dir, profile, env, true)
  }

  /// Full-control constructor: `watch_rebuild` gates the proactive background rebuild (D1).
  pub fn with_profile_env_rebuild(
    index_dir: PathBuf,
    profile: Profile,
    env: ExtractionEnv,
    watch_rebuild: bool,
  ) -> Self {
    let watch = watch_root(&index_dir).and_then(|src| SourceWatch::start(&src));
    // Boot-time warm: if the persisted index exists with a stale (or absent) vector tier,
    // start building it now instead of on the first semantic search. The generation must be
    // resolved first — artifacts live in `gen/<id>/`, never at the index root. When the
    // persisted tier looks RECONCILABLE (identity sidecar present, built by the active
    // model), the rebuild yields to live-tier adoption on the first refresh — ~200 ms of
    // reconciliation instead of a full build; if adoption declines after all (torn pair,
    // churn past the overlay ceiling), its reap requests this very warm.
    let generation = vorpal_kg::resolve_index_dir(&index_dir);
    let tier_reconcilable = generation.join("ann.files").exists()
      && vorpal_index::persisted_model_provenance(&generation).as_ref()
        == Some(&vorpal_index::model_provenance());
    let mut warm = None;
    if autowarm_enabled() && generation.join("nodes.vseg").exists() && !tier_reconcilable {
      let warm_dir = index_dir.clone();
      warm = Some(std::thread::spawn(move || {
        let _ = vorpal_index::warm_ann(&warm_dir);
      }));
    }
    let supervisor = Supervisor::discover();
    // Proactive freshness (D1) is a serve-loop concern now: the protocol loop calls
    // [`Server::tick`] between requests, which drives the SAME retained freshness path
    // queries use. The original stateless worker thread was a second committer — its
    // child-process builds could land between this daemon's own drain-ordered commits
    // and regress `CURRENT` — so the worker's debounce and supervised build live inside
    // `tick`/`refresh` instead, where commit ordering is provable.
    let proactive =
      watch_rebuild && !std::env::var("VORPAL_WATCH_REBUILD").is_ok_and(|v| v == "0");
    let mut server = Self {
      index_dir,
      profile,
      env,
      supervisor,
      kg: None,
      kg_dir: None,
      hinted_rebuilds: 0,
      canonicalizing: None,
      rebuilding: None,
      dirty_since: None,
      proactive,
      warm,
      warm_pending: false,
      persisting: None,
      overlay: None,
      overlay_building: None,
      live_ann: None,
      live_ann_task: None,
      pending_churn: Vec::new(),
      live_ann_latch: None,
      last_sweep_at: None,
      last_sweep_cost: None,
      live_ann_discard_task: false,
      watch,
    };
    // The overlay is the serving architecture, not an optimization to warm lazily: start
    // building it the moment the daemon exists (its own gates decline when there is no
    // generation yet, a committer is mid-write, or the environment is custom).
    server.spawn_overlay_build();
    server
  }

  /// Retire the live tier because the served graph advanced WITHOUT an eid-churn ledger
  /// (replay-pipeline commits): its id translations and edited-symbol vectors are stale
  /// against the new graph, and no later signal would ever resync them. An in-flight
  /// live-ANN task computed against the old graph, so its result is discarded on reap;
  /// queued churn is superseded by the fresh adoption's reconciliation.
  fn retire_live_ann_for_resync(&mut self) {
    if self.live_ann.is_none() && self.live_ann_task.is_none() {
      return;
    }
    vorpal_kg::phase_stamp("live-ann: retiring tier (graph advanced without churn ledger)");
    self.live_ann = None;
    self.pending_churn.clear();
    if self.live_ann_task.is_some() {
      self.live_ann_discard_task = true;
    }
  }

  /// Reap a finished live-ANN task (adopt or update); queued churn drains through a fresh
  /// update task so the serve path stays free of O(n) work.
  fn reap_live_ann(&mut self) {
    if !self.live_ann_task.as_ref().is_some_and(|h| h.is_finished()) {
      return;
    }
    if let Some(handle) = self.live_ann_task.take() {
      // A panicked task is a curable decline: nothing about the generation was judged.
      let joined = handle
        .join()
        .unwrap_or(Err(vorpal_index::live_ann::AdoptDecline { curable: true }));
      if std::mem::take(&mut self.live_ann_discard_task) {
        // The graph this task computed against is no longer the served one — drop the
        // result; the commit that retired the tier already queued a fresh adoption.
        vorpal_kg::phase_stamp("live-ann: discarded stale task result");
        return;
      }
      match joined {
        Ok(tier) => {
          vorpal_kg::phase_stamp("live-ann: tier ready");
          self.live_ann = Some(tier);
          self.live_ann_latch = None;
        }
        Err(decline) => {
          let generation = vorpal_kg::resolve_index_dir(&self.index_dir);
          let attempts = match &self.live_ann_latch {
            Some(latch) if latch.generation == generation => latch.attempts,
            _ => 0,
          };
          let attempts = if decline.curable {
            attempts.saturating_add(1)
          } else {
            // Generation-inherent: no warm can change the verdict — spend the budget.
            LIVE_ANN_ATTEMPTS_PER_GENERATION
          };
          vorpal_kg::phase_stamp(&format!(
            "live-ann: declined for this generation ({}, attempt {attempts}/{LIVE_ANN_ATTEMPTS_PER_GENERATION})",
            if decline.curable { "curable" } else { "incurable" },
          ));
          self.live_ann_latch = Some(LiveAnnLatch { generation, attempts, rearmed: false });
          // The tier is absent and its presence was suppressing warms: hand the
          // generation to the classic warm (stamp-guarded — a fresh sidecar no-ops).
          // The attempts budget above keeps the warm→retry cycle bounded.
          self.request_warm();
        }
      }
    }
    if self.live_ann.is_some() && !self.pending_churn.is_empty() {
      let churn = std::mem::take(&mut self.pending_churn);
      self.spawn_live_ann_update(churn);
    }
  }

  /// Try to adopt the committed generation's tier (background). Preconditions mirror the
  /// overlay builder's: never while a committer is writing.
  fn spawn_live_ann_adopt(&mut self) {
    if self.live_ann.is_some()
      || self.live_ann_task.is_some()
      || self.canonicalizing.is_some()
      || self.persisting.is_some()
      // A running warm is about to REPLACE these artifacts (compaction, failed-adopt
      // recovery): adopting mid-warm re-loads the tier the warm is retiring. `reap_warm`
      // clears the latch when it lands, so adoption follows the fresh tier instead.
      || self.warm.is_some()
    {
      return;
    }
    let Some(kg) = self.kg.clone() else { return };
    // Structural gate: below the quantized-graph floor the committed tier is provably
    // flat and adoption can never succeed — decide with one integer compare instead of
    // a thread spawn + a tier load whose verdict is predetermined (the sub-floor case
    // is every small repo). While ANY latch stands the hot path is compare + Option
    // check, no CURRENT resolve; the stamp fires on the transition, not per query.
    if !vorpal_index::live_ann::quantized_tier_possible(kg.node_count()) {
      if self.live_ann_latch.is_none() {
        vorpal_kg::phase_stamp(&format!(
          "live-ann: inapplicable (n={} at or below the quantized-graph floor)",
          kg.node_count(),
        ));
        self.live_ann_latch = Some(LiveAnnLatch {
          generation: vorpal_kg::resolve_index_dir(&self.index_dir),
          attempts: LIVE_ANN_ATTEMPTS_PER_GENERATION,
          rearmed: false,
        });
      }
      return;
    }
    // Budgeted latch: a declined generation is retried only while budget remains AND a
    // warm re-armed the question by rewriting the artifacts the last judgment saw.
    if let Some(latch) = &mut self.live_ann_latch {
      if latch.generation == vorpal_kg::resolve_index_dir(&self.index_dir) {
        if latch.attempts >= LIVE_ANN_ATTEMPTS_PER_GENERATION || !latch.rearmed {
          return;
        }
        latch.rearmed = false; // consume the retry
      } else {
        // New generation: the question re-opens from scratch.
        self.live_ann_latch = None;
      }
    }
    let index_dir = self.index_dir.clone();
    vorpal_kg::phase_stamp("live-ann: adopt spawned");
    self.live_ann_task = Some(std::thread::spawn(move || {
      let generation = vorpal_kg::resolve_index_dir(&index_dir);
      vorpal_index::live_ann::LiveAnnTier::adopt(&generation, &kg)
    }));
  }

  /// Apply edit churn to the tier on a background task: refresh the eid→id translation
  /// against the newly served graph, tombstone removed eids, insert added ones (~ms each).
  fn spawn_live_ann_update(&mut self, churn: Vec<(Vec<u64>, Vec<u64>)>) {
    let Some(mut tier) = self.live_ann.take() else {
      // No tier to maintain. Queue only if one can ever exist for this corpus size:
      // sub-floor corpora previously accumulated churn here UNBOUNDED (the tier never
      // adopts, and only an adopted tier drains the queue).
      if self
        .kg
        .as_ref()
        .is_some_and(|kg| vorpal_index::live_ann::quantized_tier_possible(kg.node_count()))
      {
        self.pending_churn.extend(churn);
      }
      return;
    };
    let Some(kg) = self.kg.clone() else {
      self.live_ann = Some(tier);
      return;
    };
    vorpal_kg::phase_stamp(&format!("live-ann: update spawned ({} batches)", churn.len()));
    self.live_ann_task = Some(std::thread::spawn(move || {
      let start = std::time::Instant::now();
      tier.refresh_ids(&kg);
      let (mut removed_total, mut added_total) = (0usize, 0usize);
      for (removed, added) in &churn {
        removed_total += removed.len();
        added_total += added.len();
        tier.apply_edit(&kg, removed, added);
      }
      vorpal_kg::phase_stamp(&format!(
        "live-ann: update done (-{removed_total} +{added_total} rows, {} ms)",
        start.elapsed().as_millis(),
      ));
      // Quality is measured, not assumed: anchor the baseline on the first update, then
      // re-probe per churn step; a degraded probe latches the compaction trigger.
      tier.probe_if_due();
      Ok(tier)
    }));
  }

  /// Start building the live overlay from the committed generation, unless one is already
  /// live, building, or disabled. Heavy (one product replay) — always a background thread.
  fn spawn_overlay_build(&mut self) {
    if !overlay_enabled() || self.overlay.is_some() || self.overlay_building.is_some() {
      return;
    }
    // Retained fast paths re-extract with the BUNDLED extractor: under a custom extraction
    // environment they would absorb edits with the wrong rules — decline, and every change
    // takes the full env-aware pipeline instead (see ExtractionEnv::is_default).
    if !self.env.is_default() {
      return;
    }
    // NEVER build from a generation a committer is still writing: reading stale CURRENT
    // resurrects rows the daemon already retired (a deleted file's symbols would reappear).
    // The committer reaps retrigger this the moment the commit lands.
    if self.canonicalizing.is_some() || self.persisting.is_some() || self.rebuilding.is_some()
    {
      return;
    }
    // The overlay serves WATCHED trees (its serve path consumes captured hints), and its
    // per-link co-change pass needs the source root — no watch, no overlay.
    let Some(src) = self.watch.as_ref().map(|watch| watch.src().to_path_buf()) else {
      return;
    };
    let index_dir = self.index_dir.clone();
    vorpal_kg::phase_stamp("overlay: builder spawned");
    self.overlay_building = Some(std::thread::spawn(move || {
      vorpal_index::live::LiveOverlay::build(&index_dir, &src)
    }));
  }

  /// Reap a finished overlay build (non-blocking). A failed build simply leaves the daemon
  /// on the replay pipeline; the next successful rebuild retriggers construction.
  fn reap_overlay_build(&mut self) {
    if !self.overlay_building.as_ref().is_some_and(|h| h.is_finished()) {
      return;
    }
    if let Some(handle) = self.overlay_building.take()
      && let Ok(Ok(overlay)) = handle.join()
    {
      vorpal_kg::phase_stamp("overlay: adopted");
      self.overlay = Some(overlay);
    }
  }

  /// Keep the overlay truthful across a NON-overlay mutation: absorb the change set when it
  /// is completely known, drop the overlay when it is not (or when absorption fails, or the
  /// tombstone debt says a fresh build is cheaper). Every path that changes the committed
  /// index must pass through here.
  fn overlay_absorb_or_drop(&mut self, hints: Option<&std::collections::HashSet<PathBuf>>) {
    let Some(overlay) = &mut self.overlay else {
      return;
    };
    let absorbed = match hints {
      Some(paths) => overlay.absorb(paths).is_ok(),
      None => false,
    };
    if !absorbed || overlay.dead_row_fraction() > 0.5 {
      // Retire WITHOUT respawning: this runs before the caller's commit is durable, and a
      // builder started now would read the pre-commit generation and resurrect retired
      // rows. The post-commit sites (committer reaps, committed adopt branches) respawn.
      vorpal_kg::phase_stamp("overlay: retired (sync-path change)");
      self.overlay = None;
    }
  }

  /// Reap (or, when `block`, drain) the background persistence of a live-adopted build. On
  /// success the committed generation becomes the artifact pin and the ANN warm fires; on
  /// failure the watch re-arms so the next query rebuilds — the served graph stays correct
  /// (it reflects the source tree), only durability lagged.
  fn reap_persist(&mut self, block: bool) {
    if !block && !self.persisting.as_ref().is_some_and(|h| h.is_finished()) {
      return;
    }
    let Some(handle) = self.persisting.take() else {
      return;
    };
    match handle.join() {
      Ok(Ok(dir)) => {
        self.kg_dir = Some(dir);
        // Adoption first, warm second: with an adopt task in flight the warm request
        // defers (its gate treats the task as tier-present), and a failed adopt requests
        // the warm itself — so the ~full-build rebuild only runs when reconciliation
        // genuinely cannot bridge the committed tier to the served graph.
        self.spawn_live_ann_adopt();
        self.request_warm();
        self.spawn_overlay_build();
      }
      _ => {
        if let Some(watch) = &self.watch {
          watch.mark_dirty();
        }
      }
    }
  }

  /// Reap a finished warm (non-blocking). A completed warm rewrote this generation's ANN
  /// artifacts IN PLACE, so a CURABLE decline's judgment is stale — re-arm one retry.
  /// The latch itself survives (its attempts budget is the spin fence: adopt → fail →
  /// warm → retry forever was exactly the loop that buried this daemon's watcher in its
  /// own artifact churn on every sub-floor corpus).
  fn reap_warm(&mut self) {
    if self.warm.as_ref().is_some_and(|h| h.is_finished()) {
      let _ = self.warm.take().map(std::thread::JoinHandle::join);
      if let Some(latch) = &mut self.live_ann_latch {
        latch.rearmed = true;
      }
    }
  }

  /// Request an eager background ANN warm of the current generation. Single-flight: while a
  /// warm is running this only marks the request, and the reap below (called from the same
  /// places) spawns the trailing warm once the runner finishes. `warm_ann` re-resolves
  /// `CURRENT` at spawn time, so the trailing warm covers everything the burst committed.
  fn request_warm(&mut self) {
    if !autowarm_enabled() {
      return;
    }
    // A healthy live tier IS the warm tier: per-edit maintenance replaces the ~full-build
    // background rebuild. The compaction trigger clears `live_ann` first when a real
    // rebuild is wanted, so this gate never blocks compaction. A live-ANN task in flight
    // counts as present: an update task briefly OWNS the tier (it travels into the
    // thread), and treating that window as tier-less fired a full rebuild on every edit —
    // the reap reinstalls the tier, or requests this warm itself when the task fails.
    if self.live_ann.as_ref().is_some_and(|t| !t.needs_compaction())
      || self.live_ann_task.is_some()
    {
      return;
    }
    self.reap_warm();
    if self.warm.is_some() {
      self.warm_pending = true;
      return;
    }
    self.warm_pending = false;
    let index_dir = self.index_dir.clone();
    self.warm = Some(std::thread::spawn(move || {
      let _ = vorpal_index::warm_ann(&index_dir);
    }));
  }

  /// Bring the in-memory graph up to date with the watched source tree. A dirty watch runs
  /// the incremental build and **adopts the sealed in-memory graph the build hands back**
  /// (no reload of bytes we just wrote); fast paths keep the already-mapped graph, whose
  /// artifacts the new generation hardlinks. Any failure re-arms the dirty flag so the next
  /// query retries rather than serving stale data as fresh.
  /// Adopt whatever generation `CURRENT` names as the served graph. Shared by every
  /// commit that arrives WITHOUT a change-set capture or eid-churn ledger (the proactive
  /// child rebuild, first-contact builds): the overlay retires (rebuilt in the background
  /// from the new generation) and the live ANN tier retires for resync (lifecycle law 5 —
  /// the stale-tolerant adopt reconciles a fresh tier from the committed artifacts).
  fn adopt_committed_generation(&mut self) -> Result<(), String> {
    let dir = vorpal_kg::resolve_index_dir(&self.index_dir);
    match Kg::load(&dir) {
      Ok(kg) => {
        self.overlay = None;
        self.retire_live_ann_for_resync();
        self.kg = Some(Arc::new(kg));
        self.kg_dir = Some(dir);
        self.spawn_live_ann_adopt();
        self.request_warm();
        self.spawn_overlay_build();
        Ok(())
      }
      Err(err) => {
        if let Some(watch) = &self.watch {
          watch.mark_dirty();
        }
        Err(format!("loading committed generation failed: {err}"))
      }
    }
  }

  /// Reap (or drain) the proactive rebuild. Success adopts the committed generation;
  /// failure re-arms the watch — queries surface the error — and holds the debounce for
  /// the worker's original 5-second retry cadence so a persistently failing build never
  /// thrashes the CPU.
  fn reap_rebuilding(&mut self, block: bool) {
    if !block && !self.rebuilding.as_ref().is_some_and(|h| h.is_finished()) {
      return;
    }
    let Some(handle) = self.rebuilding.take() else {
      return;
    };
    let ok = handle.join().unwrap_or(false);
    if !ok {
      if let Some(watch) = &self.watch {
        watch.mark_dirty();
      }
      self.dirty_since =
        Some(std::time::Instant::now() + std::time::Duration::from_millis(4500));
      return;
    }
    if let Err(err) = self.adopt_committed_generation() {
      eprintln!("vorpal-mcp: proactive rebuild committed but adoption failed: {err}");
    }
  }

  /// Advance every background lifecycle without doing freshness work: reap finished
  /// committers, warms, overlay builds, and live-ANN tasks; run compaction policy; and
  /// green-light follow-on stages. Shared by the query path ([`Self::ensure_fresh`]) and
  /// the between-requests pulse ([`Self::tick`]).
  fn advance_background(&mut self) {
    // Trailing coalesced warm: if a warm finished while a newer request was pending, spawn
    // the follow-up now (it warms whatever CURRENT is today).
    if self.warm_pending && self.warm.as_ref().is_none_or(|h| h.is_finished()) {
      self.request_warm();
    }
    self.reap_warm();
    // Reap a finished background persist (non-blocking) so `kg_dir` pins the committed
    // generation as soon as it exists.
    self.reap_persist(false);
    self.reap_overlay_build();
    self.reap_live_ann();
    // Compaction trigger: tombstone debt past the ceiling OR measured recall through the
    // degradation bar — either way the incremental tier retires and the classic full warm
    // rebuilds a dense one (re-adopted on the next pass).
    if self.live_ann.as_ref().is_some_and(vorpal_index::live_ann::LiveAnnTier::needs_compaction) {
      vorpal_kg::phase_stamp("live-ann: retiring tier to compactor");
      self.live_ann = None;
      self.request_warm();
    }
    if self.live_ann.is_none() && self.live_ann_task.is_none() {
      self.spawn_live_ann_adopt();
    }
    // Reap a finished background canonicalization (non-blocking): a failure re-arms the
    // dirty flag; a success is the overlay builder's green light (spawn_overlay_build
    // itself re-checks every committer, so this can never read a mid-write generation).
    if self.canonicalizing.as_ref().is_some_and(|h| h.is_finished()) {
      let ok = self
        .canonicalizing
        .take()
        .expect("checked above")
        .join()
        .unwrap_or(false);
      if ok {
        self.spawn_overlay_build();
      } else if let Some(watch) = &self.watch {
        watch.mark_dirty();
      }
    }
    self.reap_rebuilding(false);
  }

  /// Between-requests freshness pulse (D1, re-homed): reap background work, debounce the
  /// watch, and START — never run — anything heavy, so the protocol loop stays responsive.
  /// Small change sets absorb inline through the retained tiers (milliseconds); one that
  /// needs the full pipeline builds through [`Self::refresh`]'s background tier as the
  /// single in-flight committer, and the daemon keeps serving the current graph meanwhile.
  pub fn tick(&mut self) {
    self.advance_background();
    if !self.proactive {
      return;
    }
    let Some(watch) = &self.watch else {
      return;
    };
    if watch.take_dirty() {
      self.dirty_since = Some(std::time::Instant::now());
    } else if self.kg.is_none() && self.dirty_since.is_none() && self.rebuilding.is_none() {
      // Boot on a possibly-stale tree: changes since the last index produced no watch
      // events, so treat startup itself as dirt — the first pass brings the index current
      // before the first query needs it (the worker's old "starts dirty" behavior).
      self.dirty_since = Some(std::time::Instant::now());
    }
    let Some(since) = self.dirty_since else {
      return;
    };
    // Editor-burst debounce (the worker's half-second quiet rule): build once per burst.
    if since.elapsed() < std::time::Duration::from_millis(500) {
      return;
    }
    // Single committer: never stack a proactive build on an in-flight commit path. The
    // debounce timestamp survives, so the next quiet tick retries.
    if self.rebuilding.is_some() || self.canonicalizing.is_some() || self.persisting.is_some()
    {
      return;
    }
    self.dirty_since = None;
    // Hand the consumed dirt back to the shared flag — `refresh` keys off it, and a fresh
    // edit landing meanwhile simply re-arms the debounce on the next tick.
    watch.mark_dirty();
    if let Err(err) = self.refresh(true) {
      // stderr is free under stdio MCP (the protocol owns stdout). The failure path
      // re-armed the dirty flag, so queries retry and surface the error themselves.
      eprintln!("vorpal-mcp: proactive refresh failed: {err}");
    }
  }

  fn ensure_fresh(&mut self) -> Result<(), String> {
    self.advance_background();
    self.refresh(false)
  }

  /// The freshness path proper. `background: false` is the query path — any full pipeline
  /// run happens synchronously because the caller needs its answer. `background: true` is
  /// the proactive pulse — the full pipeline commits through a supervised child indexer
  /// (crash isolation, D3; in-process thread fallback) parked in `self.rebuilding`, and
  /// serving continues from the current graph until [`Self::reap_rebuilding`] adopts.
  fn refresh(&mut self, background: bool) -> Result<(), String> {
    // A proactive rebuild in flight means the tree moved and its commit is pending: the
    // clean-fast-path below must not serve pre-edit state, so drain and adopt FIRST (a
    // no-op when nothing is in flight). Draining costs at most what building here
    // ourselves would have — the child is building the very freshness a query wants.
    self.reap_rebuilding(true);
    let Some(watch) = &self.watch else {
      return Ok(());
    };
    let mut backstop = false;
    if self.kg.is_some() && !watch.take_dirty() {
      // Liveness backstop: a clean flag is necessary-condition evidence ONLY while the
      // OS channel is actually delivering — and FSEvents can defer delivery beyond any
      // deadline with no error and no overflow flag (event-trace-proven on a loaded
      // APFS box: the full event history arrived in one coalesced burst, tens of
      // seconds late). So when the stat-sweep lane is available and the amortized
      // budget allows (see BACKSTOP_OVERHEAD_INVERSE — period = 100× the sweep's own
      // measured cost), re-verify the tree against the retained manifest instead of
      // trusting silence. Worst-case staleness under a wedged watcher drops from
      // unbounded to one period, self-scaled per corpus; the common quiet query stays
      // one atomic load + one clock read.
      // The sweep needs no overlay: without one it diffs against the COMMITTED
      // manifest (the boot window under load was exactly where a disarmed backstop
      // let a silent watcher starve convergence).
      let lane_ready = overlay_enabled() && self.env.is_default();
      let due = match (self.last_sweep_at, self.last_sweep_cost) {
        (Some(at), Some(cost)) => at.elapsed() >= cost * BACKSTOP_OVERHEAD_INVERSE,
        // Never swept or never timed: the first eligible quiet query bootstraps the
        // measurement the period derives from.
        _ => true,
      };
      if !(lane_ready && due) {
        return Ok(());
      }
      backstop = true;
    }
    // Hinted revalidation: a COMPLETE captured change set patches the prior manifest in
    // place of the stat sweep (SUBSECOND.md 1c). Certainty gaps (`None`) and every 64th
    // hinted rebuild (belt-and-braces reconciliation) take the full sweep; the committed
    // generation is identical either way (pinned by crates/index/tests/hinted_scan.rs).
    // Backstop entries deliberately IGNORE the capture set: the flag was clean, so a
    // complete capture is empty — and an empty hint set would route to the probe
    // short-circuit, not the sweep this entry exists to run.
    let hints = if backstop { None } else { watch.take_changes() };
    // Decision telemetry (VORPAL_PHASE_TRACE): which freshness tier a dirty pass takes is
    // the first question every daemon-latency investigation asks — stamp the input.
    match &hints {
      Some(paths) => vorpal_kg::phase_stamp(&format!("refresh: captured {} path(s)", paths.len())),
      None if backstop => vorpal_kg::phase_stamp("refresh: backstop sweep (watcher quiet)"),
      None => vorpal_kg::phase_stamp("refresh: capture lost"),
    }
    let src = watch.src().to_path_buf();
    // The absorbable change set: capture-certain hints, or — when the watcher lost
    // certainty — the overlay's own stat sweep against its retained manifest. The retained
    // tier is THE path for absorbable edits; the streaming pipeline is reserved for change
    // sets past the absorb budget, a missing/retired overlay, a custom extraction
    // environment, or boot — each stated in the trace, never a silent fall-through.
    let change_set: Option<std::collections::HashSet<PathBuf>> = match &hints {
      Some(paths) => Some(paths.clone()),
      None => {
        if overlay_enabled() && self.kg.is_some() && self.env.is_default() {
          let sweep_started = std::time::Instant::now();
          let swept = match self.overlay.as_ref() {
            Some(overlay) => Some(overlay.stat_changes(&src)),
            // No adopted overlay (boot, retirement): the committed generation's
            // manifest answers the same question from disk.
            None => Some(vorpal_index::live::stat_changes_against_generation(
              &self.index_dir,
              &src,
            )),
          };
          if swept.is_some() {
            // Both outcomes measure: the period the backstop derives must reflect what
            // a sweep costs HERE, on this corpus, on this filesystem — success or not.
            self.last_sweep_cost = Some(sweep_started.elapsed());
            self.last_sweep_at = Some(std::time::Instant::now());
          }
          match swept {
            Some(Ok(paths)) => {
              vorpal_kg::phase_stamp(&format!(
                "refresh: stat sweep recovered {} path(s) in {:?}",
                paths.len(),
                self.last_sweep_cost.unwrap_or_default(),
              ));
              if paths.is_empty() {
                // Spurious wake (or a quiet backstop pass): the tree stat-matches the
                // retained manifest exactly — nothing to rebuild, nothing to serve
                // differently.
                return Ok(());
              }
              Some(paths)
            }
            Some(Err(err)) => {
              vorpal_kg::phase_stamp(&format!("refresh: stat sweep failed ({err})"));
              if backstop {
                // The flag never fired — nothing asserted the tree changed. Serving on
                // is the stock behavior; the next period retries the sweep. Falling
                // through would run the FULL pipeline on a quiet daemon.
                return Ok(());
              }
              None
            }
            None => None,
          }
        } else {
          None
        }
      }
    };
    // Serve-immediately probe (SUBSECOND.md Phase 3): when the capture is complete, small,
    // and every changed file re-extracts byte-identical to its cached product, NO answer can
    // differ from the loaded graph's — so answer now (single-digit ms: one re-extraction per
    // changed file) and canonicalize the stamps in a background build. Any doubt falls
    // through to the synchronous rebuild below. A failed or superseded background build
    // re-arms the dirty flag, so the next query retries.
    // One extraction per changed file serves BOTH fast paths: the serve-immediately
    // decision (byte-identical to cached products?) and, failing that, the overlay's
    // absorb — which previously re-extracted the same files.
    let mut probe = if let Some(paths) = &change_set
      && !paths.is_empty()
      && self.kg.is_some()
      // The probe re-extracts with the bundled extractor — custom environments fall
      // through to the full env-aware pipeline (ExtractionEnv::is_default). No size cap:
      // the extraction is the same per-file work the absorb pays, done once and shared;
      // the absorb budget below is the routing bound.
      && self.env.is_default()
      && let Some(watch_src) = self.watch.as_ref().map(|watch| watch.src().to_path_buf())
    {
      vorpal_index::live::probe_extraction(&self.index_dir, &watch_src, paths).ok()
    } else {
      None
    };
    if probe.as_ref().is_some_and(vorpal_index::live::ExtractionProbe::all_unchanged) {
      if self.canonicalizing.is_some() || self.persisting.is_some() || self.rebuilding.is_some()
      {
        // A background committer is still in flight (stamp canonicalization or a live
        // build's persistence): answers are STILL provably unchanged — the probe verified
        // the touched files against the cached products, and the pending generation was
        // extracted from the same tree state — so keep serving; the armed flag makes the
        // next quiet query re-probe once the committer lands.
        watch.mark_dirty();
        return Ok(());
      }
      let src = watch.src().to_path_buf();
      let index_dir = self.index_dir.clone();
      let env = self.env.clone();
      let paths = change_set.as_ref().expect("a probe implies a change set").clone();
      // The overlay saw no graph change here, but its retained manifest must track the
      // moved stamps or a LATER served persistence would commit stale ones and fork the
      // generation id from what a scratch build produces.
      if let (Some(overlay), Some(probe)) = (self.overlay.as_mut(), probe.as_ref()) {
        overlay.note_stamps(probe);
      }
      self.canonicalizing = Some(std::thread::spawn(move || {
        vorpal_index::build_index_watched(&src, &index_dir, &paths, &env).is_ok()
      }));
      return Ok(());
    }
    // Live-overlay semantic serve (SUBSECOND.md Phase 3): a COMPLETE, small change set with
    // a ready overlay skips the replay pipeline — extract the changed files, re-link the
    // retained state, seal in canonical order, and serve. The sealed bytes are pinned
    // byte-identical to a from-scratch build of this tree, so the background canonicalizer
    // spawned here commits the very generation these answers came from; ordering holds
    // because both committers are drained first, exactly like the synchronous path.
    if overlay_enabled()
      && self.kg.is_some()
      && self
        .overlay
        .as_ref()
        .is_some_and(|overlay| {
          let within = change_set
            .as_ref()
            .is_some_and(|paths| overlay.within_absorb_budget(paths.len()));
          if !within && change_set.is_some() {
            vorpal_kg::phase_stamp("refresh: change set past absorb budget — pipeline");
          }
          within
        })
      && let Some(probe) = probe.take()
      && let Some(paths) = &change_set
    {
      if let Some(handle) = self.canonicalizing.take() {
        let _ = handle.join();
      }
      self.reap_persist(true);
      let paths = paths.clone();
      let overlay = self.overlay.as_mut().expect("checked above");
      vorpal_kg::phase_stamp("overlay: serving");
      if retained_persist_enabled() {
        // Retained persistence: the served build commits its OWN generation on the
        // background thread — same bytes a from-scratch build of this tree produces, at a
        // fraction of the replay pipeline's CPU. All the `persisting` ordering machinery
        // (drain-before-sync, generation-bound drains, reap→pin→warm→overlay-greenlight)
        // applies unchanged.
        let prior = vorpal_kg::resolve_index_dir(&self.index_dir);
        match overlay.apply_and_link_probed_persisting(probe, prior, self.index_dir.clone()) {
          Ok((kg, pending)) => {
            let stale = overlay.dead_row_fraction() > 0.5;
            let churn = overlay.take_eid_churn();
            self.kg = Some(kg);
            self.kg_dir = None;
            self.persisting = Some(std::thread::spawn(move || pending.persist()));
            if !(churn.0.is_empty() && churn.1.is_empty()) {
              self.spawn_live_ann_update(vec![churn]);
            }
            if stale {
              vorpal_kg::phase_stamp("overlay: retired (tombstone debt)");
              self.overlay = None;
              self.spawn_overlay_build();
            }
            return Ok(());
          }
          Err(err) => {
            vorpal_kg::phase_stamp(&format!("overlay: dropped ({err})"));
            self.overlay = None;
          }
        }
      } else {
        match overlay.apply_and_link_probed(&probe) {
          Ok(kg) => {
            let stale = overlay.dead_row_fraction() > 0.5;
            self.kg = Some(Arc::new(kg));
            self.kg_dir = None;
            let index_dir = self.index_dir.clone();
            let canon_src = src.clone();
            let env = self.env.clone();
            self.canonicalizing = Some(std::thread::spawn(move || {
              vorpal_index::build_index_watched(&canon_src, &index_dir, &paths, &env).is_ok()
            }));
            if stale {
              // Tombstone debt crossed the line: retire this overlay and rebuild it from
              // the canonical generation in the background (fresh writer, fresh interner).
              vorpal_kg::phase_stamp("overlay: retired (tombstone debt)");
              self.overlay = None;
              self.spawn_overlay_build();
            }
            return Ok(());
          }
          Err(err) => {
            // Something the overlay cannot absorb exactly (unreadable file, extraction
            // change, unknown spelling): drop it and take the replay pipeline below.
            vorpal_kg::phase_stamp(&format!("overlay: dropped ({err})"));
            self.overlay = None;
          }
        }
      }
    }
    // Synchronous rebuild: drain any in-flight background committer FIRST — commits must
    // land in order (an older generation must never supersede a newer one), and the build
    // about to run must see the freshest CURRENT.
    if let Some(handle) = self.canonicalizing.take() {
      let _ = handle.join();
    }
    self.reap_persist(true);
    self.hinted_rebuilds = self.hinted_rebuilds.wrapping_add(1);
    // Both sources are COMPLETE change sets (watcher certainty, or the stat sweep that IS
    // a full-scan diff), so either satisfies the hinted-manifest-patch contract.
    let use_hints = change_set.as_ref().is_some_and(|set| !set.is_empty())
      && self.hinted_rebuilds % 64 != 0
      && self.kg.is_some();
    // Live adoption (SUBSECOND.md Phase 3, live rebuild v1): a full pipeline run returns
    // with the sealed in-memory graph — serve it NOW; its artifact writes + content-
    // addressed commit continue on a background thread. Fast paths (whole-tree reuse, the
    // stamp-only cutoff) commit synchronously and hardlink the very artifacts the loaded
    // graph has mapped, so the graph is kept and only `kg_dir` repoints.
    if background {
      // Proactive heavy tier: commit through a supervised child indexer (crash isolation,
      // D3) — or an in-process background thread when no indexer binary is discoverable —
      // while the daemon keeps serving the current graph. Hints are deliberately NOT
      // forwarded: the child runs the plain incremental pipeline (stat sweep + product
      // replay), and the serving thread's capture state stays intact for the query path.
      let supervisor = self.supervisor.clone();
      let index_dir = self.index_dir.clone();
      let env = self.env.clone();
      self.rebuilding = Some(std::thread::spawn(move || {
        match supervisor.build(&src, &index_dir) {
          Ok(BuildOutcome::Supervised(_)) => true,
          Ok(BuildOutcome::Unavailable) => {
            let _guard = in_process_build_guard();
            // `from_env` (not `default`) for parity with both the child indexer and the
            // synchronous path — all three honor the same cache-mode override.
            build_index_env(
              &src,
              &index_dir,
              CacheMode::from_env(),
              ParseHealthPolicy::default(),
              &env,
            )
            .is_ok()
          }
          Err(err) => {
            eprintln!("vorpal-mcp: supervised rebuild failed: {err}");
            false
          }
        }
      }));
      return Ok(());
    }
    if self.kg.is_none()
      && !vorpal_kg::resolve_index_dir(&self.index_dir)
        .join("nodes.vseg")
        .exists()
    {
      // First contact with a never-indexed tree: the likeliest place for a pathological
      // input, and no retained state exists to protect — crash-isolate the build (D3).
      // Steady-state watched rebuilds stay in-process below: retained serving re-extracts
      // in-process by design (probe/overlay), so the isolation boundary is first contact
      // and the explicit `index` tool, stated.
      let built = match self.supervisor.build(&src, &self.index_dir) {
        Ok(BuildOutcome::Supervised(_)) => Ok(()),
        Ok(BuildOutcome::Unavailable) => {
          let _guard = in_process_build_guard();
          build_index_env(
            &src,
            &self.index_dir,
            CacheMode::from_env(),
            ParseHealthPolicy::default(),
            &self.env,
          )
          .map(|_| ())
          .map_err(|err| err.to_string())
        }
        Err(err) => Err(err),
      };
      if let Err(err) = built {
        if let Some(watch) = &self.watch {
          watch.mark_dirty();
        }
        return Err(format!("revalidating watched index failed: {err}"));
      }
      return self.adopt_committed_generation();
    }
    let hint_set = use_hints.then(|| change_set.as_ref().expect("checked above"));
    match vorpal_index::build_index_live(&src, &self.index_dir, hint_set, &self.env) {
      Ok(build) => {
        if let Some(kg) = build.kg {
          // The committed tree moved without the overlay: absorb the exact change set or
          // retire the overlay (rebuilt in the background from the new generation). A
          // COMPLETE capture absorbs even on the every-64th reconciliation sweep — the
          // sweep insures manifest patching, not capture exactness.
          self.overlay_absorb_or_drop(change_set.as_ref());
          // The replay pipeline produced a NEW sealed graph with no eid-churn ledger:
          // the live tier's translations and edited-symbol vectors are stale against it,
          // and nothing downstream would ever resync them. Retire the tier; the
          // stale-tolerant adopt reconciles a fresh one from the committed generation
          // (the same primitive that bridges any persisted-tier drift).
          self.retire_live_ann_for_resync();
          self.kg = Some(kg);
          if let Some(pending) = build.pending {
            // No committed generation for this graph yet: leave `kg_dir` unpinned;
            // generation-bound tools drain the handle, and `reap_persist` pins + warms
            // the moment it lands.
            self.kg_dir = None;
            self.persisting = Some(std::thread::spawn(move || pending.persist()));
          } else {
            self.kg_dir = Some(vorpal_kg::resolve_index_dir(&self.index_dir));
            // Adoption before warm at every kg-servable site — see `reap_persist`.
            self.spawn_live_ann_adopt();
            self.request_warm();
          }
          self.spawn_overlay_build();
          return Ok(());
        }
        let dir = vorpal_kg::resolve_index_dir(&self.index_dir);
        if build.report.graph_reused && self.kg.is_some() {
          // Byte-identical graph carry (whole-tree reuse or the stamp-only cutoff): the
          // SAME graph keeps serving — the live tier's translations are still exact.
          self.kg_dir = Some(dir);
          self.spawn_live_ann_adopt();
          self.request_warm();
          self.spawn_overlay_build();
          return Ok(());
        }
        match Kg::load(&dir) {
          Ok(kg) => {
            // A synchronously committed generation replaces the served graph — same
            // staleness argument as the live-build branch above.
            self.retire_live_ann_for_resync();
            self.kg = Some(Arc::new(kg));
            self.kg_dir = Some(dir);
            self.spawn_live_ann_adopt();
            self.request_warm();
            self.spawn_overlay_build();
            Ok(())
          }
          Err(err) => {
            if let Some(watch) = &self.watch {
              watch.mark_dirty();
            }
            Err(format!("revalidating watched index failed: {err}"))
          }
        }
      }
      Err(err) => {
        if let Some(watch) = &self.watch {
          watch.mark_dirty();
        }
        Err(format!("revalidating watched index failed: {err}"))
      }
    }
  }

  /// Handle one JSON-RPC message line (see [`crate::protocol`] for every framing and era
  /// rule): requests return a response line; notifications and blank lines return `None`.
  pub fn handle_line(&mut self, line: &str) -> Option<String> {
    crate::protocol::handle_line(self, line)
  }

  /// The `CallToolResult` body for one tool run. Tool-level failures ride in-band (`isError`
  /// with a stable `code`); the caller has already established that the tool exists.
  ///
  /// Every result carries `structuredContent` (IMPROVEMENTS #7): successes state the pinned
  /// **generation** content id the answer came from (`null` before any graph is loaded, e.g.
  /// pure-parse tools like `ast_dump`), so ids and spans are attributable to exactly one
  /// index state; failures state a **stable machine-readable code** alongside the message.
  pub(crate) fn tool_result(&mut self, tool: &str, params: &Value) -> Value {
    let args = params
      .get("arguments")
      .cloned()
      .unwrap_or_else(|| json!({}));
    match self.run_tool(tool, &args) {
      Ok((text, mut data)) => {
        // Token-oriented text: `format: "toon" | "lean" | "ids"` rewrites the rendered half
        // from this page's records — one renderer for every record-bearing tool; tools
        // without records keep their prose.
        let text = match (
          args.get("format").and_then(Value::as_str),
          data.get("records").and_then(Value::as_array),
        ) {
          (Some("toon"), Some(rows)) => vorpal_index::records::toon_from_values(rows),
          (Some("lean"), Some(rows)) => vorpal_index::records::lean_from_values(rows),
          (Some("ids"), Some(rows)) => vorpal_index::records::ids_from_values(rows),
          _ => text,
        };
        // The structured half follows the same format: `base` + relative paths on every
        // page, fat columns dropped under `lean`, identity only under `ids` — because the
        // client may feed the model structuredContent rather than the text.
        vorpal_index::records::shape_structured(&mut data, args.get("format").and_then(Value::as_str));
        // Typed tools return their records/pagination here; text-only tools return `{}`.
        // Generation identity rides every success either way.
        data["generation"] = self.generation_id();
        json!({
          "content": [{"type": "text", "text": text}],
          "structuredContent": data,
          "isError": false
        })
      }
      Err(err) => json!({
        "content": [{"type": "text", "text": err.message}],
        "structuredContent": {"code": err.code},
        "isError": true
      }),
    }
  }

  /// Is `tool` one this daemon serves? Unknown names and names outside the profile are the
  /// same answer to a client: the tool is not on the list it was given.
  pub(crate) fn serves(&self, tool: &str) -> bool {
    self.profile.allows(tool) && ALL_TOOL_NAMES.contains(&tool)
  }

  /// The content id of the generation the pinned graph was loaded from.
  fn generation_id(&self) -> Value {
    match self
      .kg_dir
      .as_ref()
      .and_then(|dir| dir.file_name())
      .and_then(|name| name.to_str())
    {
      Some(name) => json!(name),
      None => Value::Null,
    }
  }

  /// Run one tool: rendered text for humans plus, for the typed tools, a structured object
  /// (records + pagination) that `tools_call` merges into `structuredContent`.
  fn run_tool(&mut self, tool: &str, args: &Value) -> Result<(String, Value), ToolError> {
    let str_arg = |key: &str| {
      args
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| ToolError::coded("bad-argument", format!("missing required argument '{key}'")))
    };
    if !self.profile.allows(tool) {
      return Err(ToolError::coded(
        "internal",
        format!("tool '{tool}' is not in this daemon's '{}' profile", self.profile.label()),
      ));
    }
    // `graph` is one tool over seven relations (one schema for a client to load, one
    // description to read); the relation names the arm below exactly as the former
    // per-relation tools did.
    let tool: &str = if tool == "graph" {
      match args.get("relation").and_then(Value::as_str) {
        Some(relation) if GRAPH_RELATIONS.contains(&relation) => relation,
        Some(other) => {
          return Err(ToolError::coded(
            "bad-argument",
            format!("unknown relation '{other}' (one of: {})", GRAPH_RELATIONS.join(", ")),
          ));
        }
        None => {
          return Err(ToolError::coded(
            "bad-argument",
            format!("graph needs `relation` (one of: {})", GRAPH_RELATIONS.join(", ")),
          ));
        }
      }
    } else {
      tool
    };
    // Query tools serve from a graph the watch keeps fresh; the explicit `index` tool builds
    // from its own `src` argument and needs no pre-validation.
    if tool != "index" {
      self
        .ensure_fresh()
        .map_err(|message| ToolError::coded("index-unavailable", message))?;
    }
    // Generation-bound tools answer from committed artifacts (digest-verified spans, the
    // product pack, ANN sidecars) rather than the served in-memory graph — they wait for an
    // in-flight background persist so their generation pin exists. The navigation and
    // pattern tools keep serving from the sealed graph at full speed during the window.
    const GENERATION_BOUND: &[&str] = &[
      "index",
      "search",
      "code_search",
      "architecture",
      "coverage",
      "health",
      "schema",
      "fetch_span",
      "dead_code",
      "snippet",
      "why",
      "compare_generations",
      "impact",
    ];
    if GENERATION_BOUND.contains(&tool) {
      self.reap_persist(true);
      // An overlay-served answer's generation is written by the canonicalizer: wait for it,
      // then pin the fresh generation — its bytes (and therefore its ids) are the very ones
      // the served graph was sealed from. A failed canonicalization leaves `kg_dir` unpinned
      // (the tool reports unavailable) and re-arms the watch.
      if let Some(handle) = self.canonicalizing.take() {
        let ok = handle.join().unwrap_or(false);
        if !ok && let Some(watch) = &self.watch {
          watch.mark_dirty();
        }
      }
      if self.kg_dir.is_none() && self.kg.is_some() {
        let dir = vorpal_kg::resolve_index_dir(&self.index_dir);
        if dir.join("nodes.vseg").exists() {
          self.kg_dir = Some(dir);
        }
      }
    }
    if tool == "index"
      && let Some(handle) = self.canonicalizing.take()
    {
      // The explicit `index` tool commits synchronously — it must not race the
      // serve-immediately probe's stamp canonicalization either.
      let _ = handle.join();
    }
    match tool {
      "index" => {
        let src = str_arg("src")?;
        // Explicit cache-validity mode: `verify: true` selects content-authoritative
        // validation (immune to preserved-mtime edits); default is fast-stat.
        let mode = if args.get("verify").and_then(Value::as_bool).unwrap_or(false) {
          vorpal_index::CacheMode::Verified
        } else {
          vorpal_index::CacheMode::default()
        };
        // Parse-health policy (IMPROVEMENTS #11): warn (default) | exclude | fail, with an
        // error-byte-ratio threshold.
        let policy = vorpal_index::ParseHealthPolicy {
          mode: match args.get("parse_health").and_then(Value::as_str) {
            None | Some("warn") => vorpal_index::ParseHealthMode::Warn,
            Some("exclude") => vorpal_index::ParseHealthMode::Exclude,
            Some("fail") => vorpal_index::ParseHealthMode::Fail,
            Some(other) => {
              return Err(ToolError::coded(
                "bad-argument",
                format!("parse_health wants warn|exclude|fail, got '{other}'"),
              ));
            }
          },
          max_error_ratio: args
            .get("max_error_ratio")
            .and_then(Value::as_f64)
            .unwrap_or(0.0),
        };
        // An explicit rebuild is a commit: drain the proactive rebuild first so commits
        // stay single-file (its generation lands, then this one supersedes it).
        self.reap_rebuilding(true);
        // Optional embedding-tier selection: written to the index ROOT before the build,
        // because the selection file is the single cross-process truth every warm reads
        // (in-daemon or child indexer alike). Absent = keep the existing selection.
        if let Some(tier) = args.get("semantic_tier").and_then(Value::as_str) {
          let tier = match tier {
            "lexical" => vorpal_index::SemanticTier::Lexical,
            "learned" => vorpal_index::SemanticTier::Learned,
            other => {
              return Err(ToolError::coded(
                "bad-argument",
                format!("semantic_tier wants lexical|learned, got '{other}'"),
              ));
            }
          };
          vorpal_index::write_tier_selection(&self.index_dir, tier)
            .map_err(|err| ToolError::coded("index-unavailable", err.to_string()))?;
        }
        // Supervised when a child indexer exists (D3): a crashing input costs this call,
        // never the daemon. NOTE: the child runs the default cache/health policy; explicit
        // policy args force the in-process path so they are honored exactly.
        let wants_default_policy =
          mode == CacheMode::default() && policy == ParseHealthPolicy::default();
        let mut supervised_note: Option<String> = None;
        let report = if wants_default_policy {
          match self.supervisor.build(Path::new(&src), &self.index_dir) {
            Err(err) => return Err(err.into()),
            Ok(BuildOutcome::Supervised(child_text)) => {
              supervised_note = Some(child_text);
              None
            }
            Ok(BuildOutcome::Unavailable) => {
              let _guard = in_process_build_guard();
              Some(
                build_index_env(Path::new(&src), &self.index_dir, mode, policy, &self.env)
                  .map_err(|err| err.to_string())?,
              )
            }
          }
        } else {
          let _guard = in_process_build_guard();
          Some(
            build_index_env(Path::new(&src), &self.index_dir, mode, policy, &self.env)
              .map_err(|err| err.to_string())?,
          )
        };
        // Reload so queries serve the fresh graph (a cheap mmap cold-open), pinning the
        // new generation directory alongside it.
        let dir = vorpal_kg::resolve_index_dir(&self.index_dir);
        self.kg = Some(Arc::new(Kg::load(&dir).map_err(|err| err.to_string())?));
        self.kg_dir = Some(dir);
        // An explicit rebuild moved the committed tree with no change-set capture: the
        // overlay cannot be trusted to match — retire it and rebuild from the new generation.
        // The live ANN tier goes with it (lifecycle law 5): this commit carried no eid-churn
        // ledger, so the tier's id translations are stale against the reloaded graph; the
        // stale-tolerant adopt reconciles a fresh tier from the committed generation.
        self.overlay = None;
        self.retire_live_ann_for_resync();
        self.spawn_overlay_build();
        let mut text = match (&report, supervised_note) {
          // Child ran: its stdout tail IS the report (counts, damage note, unverified note).
          (None, Some(child_text)) => format!("(supervised) {child_text}"),
          (Some(report), _) if report.reused => {
            format!("unchanged — reused existing index ({} nodes)", report.nodes)
          }
          (Some(report), _) => format!(
            "indexed {} files ({} skipped) → {} nodes; refs: {} resolved, {} ambiguous, {} external, {} masked",
            report.indexed,
            report.skipped,
            report.nodes,
            report.resolved,
            report.ambiguous,
            report.external,
            report.masked
          ),
          (None, None) => unreachable_report()?,
        };
        if let Some(report) = &report {
          if !report.unverified_langs.is_empty() {
            text.push_str(&format!(
              "\nnote: {} dynamic language(s) extracted without a canary (best-effort, \
               unverified): {}",
              report.unverified_langs.len(),
              report.unverified_langs.join(", ")
            ));
          }
        }
        Ok((text, json!({})))
      }
      "code_search" => {
        let pattern = str_arg("pattern")?;
        let k = args.get("k").and_then(Value::as_u64).unwrap_or(20) as usize;
        let lang = args.get("lang").and_then(Value::as_str).map(str::to_string);
        let prefix = args.get("prefix").and_then(Value::as_str).map(str::to_string);
        self.kg()?;
        let dir = self.kg_dir.clone();
        let Some(kg) = self.kg.as_deref() else {
          return Err(ToolError::coded("index-unavailable", "no graph is loaded — run the 'index' tool first"));
        };
        let report = vorpal_index::records::code_search(
          kg,
          dir.as_deref(),
          &pattern,
          lang.as_deref(),
          prefix.as_deref(),
          k,
        )
        .map_err(ToolError::from)?;
        let text = vorpal_index::records::render_code_search(&report);
        let mut data = paged(report.records, args, "hits")?;
        data["staleFiles"] = report.stale_files.into();
        data["unreadableFiles"] = report.unreadable_files.into();
        data["scannedFiles"] = report.scanned_files.into();
        data["totalMatches"] = report.total_matches.into();
        Ok((text, data))
      }
      "architecture" => {
        let top = args.get("top").and_then(Value::as_u64).unwrap_or(20).clamp(1, 500) as usize;
        self.kg()?;
        let dir = self.kg_dir.clone();
        let Some(kg) = self.kg.as_deref() else {
          return Err(ToolError::coded("index-unavailable", "no graph is loaded — run the 'index' tool first"));
        };
        let report = vorpal_index::records::architecture_report(kg, dir.as_deref(), top);
        let text = vorpal_index::records::render_architecture(&report);
        let data = serde_json::to_value(&report).unwrap_or(Value::Null);
        Ok((text, data))
      }
      "compare_generations" => {
        let root = self.index_dir.clone();
        let from_spec = args.get("from").and_then(Value::as_str).unwrap_or("prev");
        let to_spec = args.get("to").and_then(Value::as_str).unwrap_or("CURRENT");
        let from_dir =
          vorpal_index::gendiff::resolve_generation(&root, from_spec).map_err(ToolError::from)?;
        let to_dir =
          vorpal_index::gendiff::resolve_generation(&root, to_spec).map_err(ToolError::from)?;
        let from_kg = vorpal_index::Kg::load(&from_dir).map_err(|err| err.to_string())?;
        let to_kg = vorpal_index::Kg::load(&to_dir).map_err(|err| err.to_string())?;
        let label = |dir: &Path| {
          dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
        };
        let diff =
          vorpal_index::gendiff::diff(&from_kg, &to_kg, &label(&from_dir), &label(&to_dir));
        let report = vorpal_index::records::diff_page(
          &from_kg,
          &to_kg,
          diff,
          vorpal_index::records::PageRequest {
            cursor: args.get("cursor").and_then(Value::as_str),
            limit: args.get("limit").and_then(Value::as_u64),
          },
        )
        .map_err(|message| ToolError::coded("bad-argument", message))?;
        let text = vorpal_index::records::render_diff(&report);
        let data = serde_json::to_value(&report).unwrap_or(Value::Null);
        Ok((text, data))
      }
      "impact" => {
        // Blast radius vs a git ref (or the uncommitted worktree): needs the watched source
        // root — the same precondition structural_search states.
        let root = self
          .watch
          .as_ref()
          .map(|w| w.src().to_path_buf())
          .ok_or_else(|| {
            "impact needs a watched source tree (daemon started on a default \
             <src>/.vorpal/index location)"
              .to_string()
          })?;
        let since = args.get("since").and_then(Value::as_str).map(str::to_string);
        let relations: Vec<vorpal_kg::EdgeType> = match args.get("relations") {
          Some(Value::Array(items)) => {
            let mut out = Vec::new();
            for it in items {
              let name = it.as_str().ok_or_else(|| "relations must be strings".to_string())?;
              out.push(
                vorpal_kg::EdgeType::from_name(name)
                  .ok_or_else(|| format!("unknown relation '{name}'"))?,
              );
            }
            if out.is_empty() { vec![vorpal_kg::EdgeType::CALLS] } else { out }
          }
          Some(_) => {
            return Err(ToolError::coded("bad-argument", "relations must be an array of relation names"));
          }
          None => vec![vorpal_kg::EdgeType::CALLS],
        };
        let max_depth = match args.get("max_depth").and_then(Value::as_u64) {
          Some(0) | None => None,
          Some(d) => Some(d as u32),
        };
        let min_confidence = vorpal_index::min_confidence_for_grade(
          args.get("min_grade").and_then(Value::as_str),
        )
        .map_err(|err| err.to_string())?;
        let changed = vorpal_index::impact::changed_paths(&root, since.as_deref())
          .map_err(ToolError::from)?;
        self.kg()?;
        let Some(kg) = self.kg.as_deref() else {
          return Err(ToolError::coded("index-unavailable", "no graph is loaded — run the 'index' tool first"));
        };
        let (seeds, missing) = vorpal_index::impact::seeds_for_paths(kg, &root, &changed);
        let report = vorpal_index::records::impact_page(
          kg,
          &seeds,
          &relations,
          max_depth,
          min_confidence,
          (changed.len(), missing),
          vorpal_index::records::PageRequest {
            cursor: args.get("cursor").and_then(Value::as_str),
            limit: args.get("limit").and_then(Value::as_u64),
          },
        )
        .map_err(ToolError::from)?;
        let text = vorpal_index::records::render_impact(&report);
        let mut data = json!({
          "outcome": "hits",
          "records": serde_json::to_value(&report.records).unwrap_or(Value::Null),
          "total": report.total,
          "truncated": report.end < report.total,
          "changedFiles": report.changed_files,
          "missingFiles": report.missing_files,
          "seeds": report.seeds,
        });
        if report.end < report.total {
          data["nextCursor"] = json!(format!("o:{}", report.end));
        }
        Ok((text, data))
      }
      "coverage" => {
        // The cheap parse-coverage overview (header peeks over the product bank); span and
        // entity detail lives in `health`.
        self.kg()?;
        let dir = self.kg_dir.clone();
        let report = vorpal_index::records::coverage_records(dir.as_deref());
        let text = vorpal_index::records::render_coverage(&report);
        let mut data = paged(report.records, args, "hits")?;
        data["totalFiles"] = report.total_files.into();
        data["damagedFiles"] = report.damaged_files.into();
        data["totalErrorBytes"] = report.total_error_bytes.into();
        Ok((text, data))
      }
      "node" | "callers" | "references" | "importers" | "implementors" | "type_users"
      | "similar" => {
        // Pattern listing (node only): regex over names, matches ARE the answer.
        if tool == "node" {
          if let Some(pattern) = args.get("pattern").and_then(Value::as_str) {
            self.kg()?;
            let Some(kg) = self.kg.as_deref() else {
          return Err(ToolError::coded("index-unavailable", "no graph is loaded — run the 'index' tool first"));
        };
            let text =
              vorpal_index::pattern_query_on(kg, pattern, 200).map_err(|err| err.to_string())?;
            let records =
              vorpal_index::records::pattern_records(kg, pattern).map_err(ToolError::from)?;
            let data = paged(records, args, "hits")?;
            return Ok((text, data));
          }
        }
        // Symbol identity contract (IMPROVEMENTS §1): ambiguous names return the candidate
        // list (with node ids) instead of silently merging namesake neighborhoods; refine
        // with `path`/`kind`/`id`, or pass `all: true` to merge explicitly.
        let target = graph_target(args, str_arg("name")?);
        let verb = match tool {
          "type_users" => "typeusers",
          "references" => "refs",
          other => other,
        };
        // `self.kg()` keeps the daemon contract: freshness revalidation, the warm cached
        // graph, and the "run the 'index' tool first" error when nothing is indexed yet.
        let dir = self.kg_dir.clone();
        let kg = self.kg()?;
        let data = if tool == "node" {
          let records =
            vorpal_index::records::listing_records(kg, &target).map_err(ToolError::from)?;
          paged(records, args, "hits")?
        } else {
          // Rows carry the call site (line + text) so "who calls X" needs no snippet.
          let selected =
            vorpal_index::records::related_records_with_sites(kg, dir.as_deref(), verb, &target)
              .map_err(ToolError::from)?;
          selected_data(selected, args)?
        };
        let text = vorpal_index::graph_query_on(kg, verb, &target)
          .map_err(|err| err.to_string())
          .map_err(ToolError::from)?;
        Ok((text, data))
      }
      "search" => {
        let query = str_arg("query")?;
        let k = args.get("k").and_then(Value::as_u64).unwrap_or(10) as usize;
        // Structured pre-ranking filters (IMPROVEMENTS #9): k results means k MATCHING
        // results — filters apply to every channel before fusion, never as a post-cut.
        let filter = vorpal_index::SearchFilter {
          path_prefix: args.get("prefix").and_then(Value::as_str).map(str::to_string),
          path_suffix: args.get("path").and_then(Value::as_str).map(str::to_string),
          kind: args.get("kind").and_then(Value::as_str).map(str::to_string),
          lang: args.get("lang").and_then(Value::as_str).map(str::to_string),
          exported_only: args.get("exported").and_then(Value::as_bool).unwrap_or(false),
          exclude_tests: args.get("exclude_tests").and_then(Value::as_bool).unwrap_or(false),
        };
        // One ranking serves both surfaces: records for machines, and the explained text
        // rendered from the same records (byte-compatible with `search_index_explained`) —
        // agents get ranking provenance by default (§11's "expose which rankers
        // contributed"). Multi-phrase conjunctions (`"…" AND "…"`) ride the same report:
        // phrase-tagged provenance, and an eliminator line instead of silent emptiness.
        // With a live ANN tier, the semantic pool is served live and everything
        // downstream (rerank, fusion, report) is the shared path.
        let report = if let Some(tier) = &self.live_ann {
          vorpal_kg::phase_stamp("live-ann: semantic pool served by live tier");
          vorpal_index::search_report_filtered_live(&self.index_dir, &query, k, &filter, tier)
        } else {
          vorpal_index::search_report_filtered(&self.index_dir, &query, k, &filter)
        }
        .map_err(|err| err.to_string())?;
        let vorpal_index::records::SearchReport { hits, multi_phrase } = report;
        let mut text = String::new();
        for hit in &hits {
          let mut provenance = format!("id {}", hit.node.id);
          for channel in &hit.channels {
            match channel.phrase {
              Some(phrase) => provenance.push_str(&format!(
                "; p{}:{}#{}",
                phrase + 1,
                channel.channel,
                channel.rank
              )),
              None => provenance.push_str(&format!("; {}#{}", channel.channel, channel.rank)),
            }
          }
          text.push_str(&format!(
            "{:.4}  {} [{}] {}  ({provenance})\n",
            hit.score, hit.node.name, hit.node.kind, hit.node.path
          ));
        }
        if text.is_empty() {
          text = match multi_phrase.as_ref().and_then(|mp| Some((mp, mp.eliminated_by?))) {
            Some((mp, index)) => format!(
              "(no results: phrase {}/{} {:?} eliminated all candidates; per-phrase pools: {} \
               at depth {})",
              index + 1,
              mp.phrases.len(),
              mp.phrases.get(index).map(String::as_str).unwrap_or("?"),
              mp.per_phrase_pool
                .iter()
                .map(usize::to_string)
                .collect::<Vec<_>>()
                .join(", "),
              mp.intersection_depth,
            ),
            None => format!("(no results for '{query}')"),
          };
        }
        let mut data = paged(hits, args, "hits")?;
        if let Some(mp) = &multi_phrase {
          data["multiPhrase"] = serde_json::to_value(mp).map_err(|err| err.to_string())?;
        }
        Ok((text, data))
      }
      "structural_search" => {
        let pattern = str_arg("pattern")?;
        let lang = str_arg("lang")?;
        let path = args.get("path").and_then(Value::as_str);
        let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(100) as usize;
        let root = self
          .watch
          .as_ref()
          .map(|w| w.src().to_path_buf())
          .ok_or_else(|| {
            ToolError::coded(
              "no-watch",
              "structural_search needs a watched source tree (daemon started on a default \
               <src>/.vorpal/index location)",
            )
          })?;
        crate::tools::structural_search(&root, &pattern, &lang, path, limit.clamp(1, 1000))
          .map_err(ToolError::from)
          .map(|text| (text, json!({})))
      }
      "health" => {
        // Serve from the pinned generation so spans/entities match the ids other tools hand out.
        self.kg()?;
        let dir = self.kg_dir.clone().unwrap_or_else(|| self.index_dir.clone());
        vorpal_index::parse_health_report(&dir)
          .map(|text| (text, json!({})))
          .map_err(|err| ToolError::from(err.to_string()))
      }
      "schema" => {
        // Vocabulary introspection: what kinds/relations/grades exist here, with counts —
        // the call an agent makes before forming its first real query.
        self.kg()?;
        let dir = self.kg_dir.clone();
        let Some(kg) = self.kg.as_deref() else {
          return Err(ToolError::coded("index-unavailable", "no graph is loaded — run the 'index' tool first"));
        };
        let report = vorpal_index::records::schema_report(kg, dir.as_deref());
        let text = vorpal_index::records::render_schema(&report);
        let data = serde_json::to_value(&report).unwrap_or(Value::Null);
        Ok((text, data))
      }
      "rule_search" => {
        let rule = str_arg("rule")?;
        let path = args.get("path").and_then(Value::as_str);
        let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(100) as usize;
        let root = self
          .watch
          .as_ref()
          .map(|w| w.src().to_path_buf())
          .ok_or_else(|| {
            ToolError::coded(
              "no-watch",
              "rule_search needs a watched source tree (daemon started on a default \
               <src>/.vorpal/index location)",
            )
          })?;
        crate::tools::rule_search(&root, &rule, path, limit.clamp(1, 1000))
          .map_err(ToolError::from)
          .map(|text| (text, json!({})))
      }
      "ast_dump" => {
        let (source, lang) = match (args.get("source").and_then(Value::as_str), args.get("path").and_then(Value::as_str)) {
          (Some(inline), _) => (inline.to_string(), str_arg("lang")?),
          (None, Some(path)) => {
            let source =
              std::fs::read_to_string(path).map_err(|err| format!("read {path}: {err}"))?;
            let lang = match args.get("lang").and_then(Value::as_str) {
              Some(lang) => lang.to_string(),
              None => <vorpal_ingest::SgLang as vorpal_core::Language>::from_path(
                std::path::Path::new(path),
              )
              .map(|l: vorpal_ingest::SgLang| l.to_string())
              .ok_or_else(|| format!("cannot infer language from {path}; pass lang"))?,
            };
            (source, lang)
          }
          (None, None) => return Err(ToolError::coded("bad-argument", "pass source+lang, or path")),
        };
        let max_nodes = args
          .get("max_nodes")
          .and_then(Value::as_u64)
          .unwrap_or(500) as usize;
        crate::tools::ast_dump(&source, &lang, max_nodes.clamp(10, 5000))
          .map_err(ToolError::from)
          .map(|text| (text, json!({})))
      }
      "fetch_span" => {
        let id = args
          .get("id")
          .and_then(Value::as_u64)
          .ok_or_else(|| "missing required argument: id".to_string())?;
        let max_bytes = args
          .get("max_bytes")
          .and_then(Value::as_u64)
          .unwrap_or(16_384) as usize;
        // Slice against the pinned generation's digests: stale offsets refuse, never guess.
        self.kg()?;
        let dir = self.kg_dir.clone();
        let Some(kg) = self.kg.as_deref() else {
          return Err(ToolError::coded("index-unavailable", "no graph is loaded — run the 'index' tool first"));
        };
        crate::tools::fetch_span(kg, dir.as_deref(), id, max_bytes.clamp(64, 262_144))
          .map(|text| (text, json!({})))
          .map_err(|err| match err {
            crate::tools::FetchSpanError::Stale(message) => {
              ToolError::coded("stale-source", message)
            }
            crate::tools::FetchSpanError::Other(message) => ToolError::from(message),
          })
      }
      "dead_code" => {
        let filter = vorpal_index::records::DeadFilter {
          kind: args.get("kind").and_then(Value::as_str).map(str::to_string),
          path_prefix: args.get("prefix").and_then(Value::as_str).map(str::to_string),
          path_suffix: args.get("path").and_then(Value::as_str).map(str::to_string),
          exported_only: args.get("exported").and_then(Value::as_bool).unwrap_or(false),
          exclude_tests: args.get("exclude_tests").and_then(Value::as_bool).unwrap_or(false),
        };
        self.kg()?;
        let dir = self.kg_dir.clone();
        let Some(kg) = self.kg.as_deref() else {
          return Err(ToolError::coded("index-unavailable", "no graph is loaded — run the 'index' tool first"));
        };
        let report = vorpal_index::records::dead_records_page(
          kg,
          dir.as_deref(),
          &filter,
          vorpal_index::records::PageRequest {
            cursor: args.get("cursor").and_then(Value::as_str),
            limit: args.get("limit").and_then(Value::as_u64),
          },
        )
        .map_err(ToolError::from)?;
        let text = vorpal_index::records::render_dead(&report);
        let mut data = json!({
          "outcome": "hits",
          "records": serde_json::to_value(&report.records).unwrap_or(Value::Null),
          "total": report.total,
          "truncated": report.end < report.total,
          "suppressedReferenced": report.suppressed_referenced,
          "suppressedDamaged": report.suppressed_damaged,
          "nameSuppression": report.name_suppression,
        });
        if report.end < report.total {
          data["nextCursor"] = json!(format!("o:{}", report.end));
        }
        Ok((text, data))
      }
      "data_flow" => {
        // Outgoing data-flow rows (G-M3): which arguments flow from this definition into
        // which callees, from the dataflow.bin sidecar. Absence of rows is NOT proof of no
        // flows — the sidecar covers the typefacts launch languages, and older generations
        // have no sidecar at all (said explicitly in the response).
        let target = graph_target(args, str_arg("name")?);
        self.kg()?;
        let dir = self
          .kg_dir
          .clone()
          .ok_or_else(|| ToolError::coded("index-unavailable", "no generation dir pinned"))?;
        let Some(kg) = self.kg.as_ref() else {
          return Err(ToolError::coded("index-unavailable", "no graph is loaded — run the 'index' tool first"));
        };
        let (records, sidecar_present) =
          vorpal_index::records::flow_records(kg, &dir, &target).map_err(ToolError::from)?;
        let mut lines = Vec::new();
        if !sidecar_present {
          lines.push(
            "no data-flow sidecar in this generation (built before flows existed) — rebuild \
             the index to record flows"
              .to_string(),
          );
        } else if records.is_empty() {
          lines.push("no outgoing data flows recorded for this selection".to_string());
        }
        for r in &records {
          lines.push(format!(
            "{} --arg#{}({}{})--> {} param#{} [{}]",
            r.from_name,
            r.arg_index,
            r.class,
            r.expr.as_deref().map(|e| format!(" {e}")).unwrap_or_default(),
            r.to_name,
            if r.param_index == u16::MAX { "?".to_string() } else { r.param_index.to_string() },
            r.to_path
          ));
        }
        Ok((
          lines.join("\n") + "\n",
          json!({"records": records, "sidecarPresent": sidecar_present}),
        ))
      }
      "observed" => {
        // Runtime-observed calls (ADOPTION #26): rows from ingested traces, each flagged
        // with whether the static graph already carries the edge — `false` is dynamic
        // dispatch or a function pointer no static resolver can prove. Absence of the
        // sidecar (never ingested, or invalidated by a rebuild) is stated.
        let target = graph_target(args, str_arg("name")?);
        self.kg()?;
        let dir = self
          .kg_dir
          .clone()
          .ok_or_else(|| ToolError::coded("index-unavailable", "no generation dir pinned"))?;
        let Some(kg) = self.kg.as_ref() else {
          return Err(ToolError::coded("index-unavailable", "no graph is loaded — run the 'index' tool first"));
        };
        let (records, sidecar_present) =
          vorpal_index::records::observed_records(kg, &dir, &target).map_err(ToolError::from)?;
        let mut lines = Vec::new();
        if !sidecar_present {
          lines.push(
            "no observed-calls sidecar for this generation — ingest runtime traces with \
             `vorpal-index ingest-traces <index> <folded-stacks>` (a rebuild invalidates \
             it until traces are re-ingested)"
              .to_string(),
          );
        } else if records.is_empty() {
          lines.push("no observed calls recorded for this selection".to_string());
        }
        for r in &records {
          lines.push(format!(
            "{} {} x{} {}{}",
            if r.direction == "in" { "<-observed-" } else { "-observed->" },
            r.counterpart_name,
            r.count,
            r.counterpart_path,
            if r.in_static_graph { "" } else { "  (not in the static graph)" }
          ));
        }
        Ok((
          lines.join("\n") + "\n",
          json!({"records": records, "sidecarPresent": sidecar_present}),
        ))
      }
      "query" => {
        // Cypher-shaped read-only queries (G-M4): `text` in the query language, or `ir`
        // carrying the typed IR document. Ceilings (text bytes, depth, edge visits, rows)
        // are typed refusals naming the ceiling — never a silently truncated answer.
        let parsed = if let Some(text) = args.get("text").and_then(Value::as_str) {
          vorpal_query::parse(text)
        } else if let Some(ir) = args.get("ir") {
          vorpal_query::parse_ir_json(&ir.to_string())
        } else {
          return Err(ToolError::coded(
            "bad-argument",
            "pass `text` (the query language) or `ir` (a typed IR document)",
          ));
        }
        .map_err(|err| ToolError::coded("bad-query", err.to_string()))?;
        self.kg()?;
        let Some(kg) = self.kg.as_ref() else {
          return Err(ToolError::coded(
            "index-unavailable",
            "no graph is loaded — run the 'index' tool first",
          ));
        };
        let result = vorpal_query::execute(kg, &parsed)
          .map_err(|err| ToolError::coded("bad-query", err.to_string()))?;
        const TEXT_ROWS: usize = 200;
        let mut lines = vec![result.columns.join(" | ")];
        for row in result.rows.iter().take(TEXT_ROWS) {
          lines.push(row.iter().map(ToString::to_string).collect::<Vec<_>>().join(" | "));
        }
        if result.rows.len() > TEXT_ROWS {
          lines.push(format!(
            "… {} more rows in structuredContent",
            result.rows.len() - TEXT_ROWS
          ));
        }
        if result.rows.is_empty() {
          lines.push("(no rows)".to_string());
        }
        lines.push(format!(
          "{} row{} (of {} before SKIP/LIMIT)",
          result.rows.len(),
          if result.rows.len() == 1 { "" } else { "s" },
          result.total_rows
        ));
        let structured = serde_json::to_value(&result)
          .map_err(|err| ToolError::coded("internal", err.to_string()))?;
        Ok((lines.join("\n") + "\n", structured))
      }
      "snippet" => {
        // The selector-driven sibling of `fetch_span`: name/path/kind/id/eid resolution with
        // the shared ambiguity contract, whole-line context, and the same digest refusal.
        let target = graph_target(args, str_arg("name")?);
        let context_lines =
          args.get("context_lines").and_then(Value::as_u64).unwrap_or(0) as usize;
        let max_bytes = args
          .get("max_bytes")
          .and_then(Value::as_u64)
          .unwrap_or(16_384)
          .clamp(64, 262_144) as usize;
        self.kg()?;
        let dir = self.kg_dir.clone();
        let Some(kg) = self.kg.as_deref() else {
          return Err(ToolError::coded("index-unavailable", "no graph is loaded — run the 'index' tool first"));
        };
        let selected = vorpal_index::records::snippet_records(
          kg,
          dir.as_deref(),
          &target,
          context_lines,
          max_bytes,
        )
        .map_err(|err| match err {
          vorpal_index::records::SnippetError::Stale(message) => {
            ToolError::coded("stale-source", message)
          }
          vorpal_index::records::SnippetError::Other(message) => ToolError::from(message),
        })?;
        let text = match &selected {
          vorpal_index::records::Selected::NoMatch => {
            format!("(no results for '{}' — no symbol matches that selector)\n", target.name)
          }
          vorpal_index::records::Selected::Ambiguous(candidates) => format!(
            "ambiguous: '{}' matches {} definitions — refine with path/kind/id, or all: true\n",
            target.name,
            candidates.len()
          ),
          vorpal_index::records::Selected::Hits(hits) => {
            vorpal_index::records::render_snippets(hits)
          }
        };
        let data = selected_data(selected, args)?;
        Ok((text, data))
      }
      "why" => {
        let from_id = args
          .get("from_id")
          .and_then(Value::as_u64)
          .ok_or_else(|| "missing required argument: from_id".to_string())?;
        let to_id = args.get("to_id").and_then(Value::as_u64);
        let name = args.get("name").and_then(Value::as_str).map(str::to_string);
        // Answer from the pinned graph + its generation dir: id-consistent with the queries
        // that produced these ids, and the snippet digest-checks against the same generation.
        self.kg()?;
        let dir = self.kg_dir.clone();
        let Some(kg) = self.kg.as_deref() else {
          return Err(ToolError::coded("index-unavailable", "no graph is loaded — run the 'index' tool first"));
        };
        let text = match (to_id, name.as_deref()) {
          (Some(to_id), _) => vorpal_index::explain_edge_on(kg, dir.as_deref(), from_id, to_id)
            .map_err(|err| err.to_string())?,
          (None, Some(name)) => vorpal_index::explain_absence_on(kg, from_id, name)
            .map_err(|err| err.to_string())?,
          (None, None) => {
            return Err(ToolError::coded(
              "bad-argument",
              "pass to_id (edge evidence) or name (absence evidence)",
            ));
          }
        };
        let records = vorpal_index::records::evidence_records(kg, from_id, to_id, name.as_deref());
        let data = paged(records, args, "hits")?;
        Ok((text, data))
      }
      "reachable" => {
        let name = str_arg("name")?;
        let direction = str_arg("direction")?;
        let dir = match direction.as_str() {
          "in" => vorpal_kg::Direction::In,
          "out" => vorpal_kg::Direction::Out,
          "both" => vorpal_kg::Direction::Both,
          other => {
            return Err(ToolError::coded(
              "bad-argument",
              format!("direction must be \"in\", \"out\", or \"both\", got '{other}'"),
            ));
          }
        };
        // Selector-consistent (07-29 §6): same refinement contract as the direct graph tools —
        // ambiguous names return candidates; id/eid/path/kind refine; `all` merges explicitly.
        let target = graph_target(args, name);
        let relations: Vec<vorpal_kg::EdgeType> = match args.get("relations") {
          Some(Value::Array(items)) => {
            let mut out = Vec::new();
            for it in items {
              let s = it
                .as_str()
                .ok_or_else(|| "relations must be strings".to_string())?;
              out.push(
                vorpal_kg::EdgeType::from_name(s).ok_or_else(|| format!("unknown relation '{s}'"))?,
              );
            }
            if out.is_empty() {
              vec![vorpal_kg::EdgeType::CALLS]
            } else {
              out
            }
          }
          Some(_) => return Err(ToolError::coded("bad-argument", "relations must be an array of relation names")),
          None => vec![vorpal_kg::EdgeType::CALLS],
        };
        let max_depth = match args.get("max_depth").and_then(Value::as_u64) {
          Some(0) | None => None,
          Some(d) => Some(d as u32),
        };
        let min_confidence = vorpal_index::min_confidence_for_grade(
          args.get("min_grade").and_then(Value::as_str),
        )
        .map_err(|err| err.to_string())?;
        self.kg()?;
        // Freshness first: kg() pins the generation and its dir together, so the sidecar
        // read can never come from a different generation than the ids it annotates.
        let flows_dir = self.kg_dir.clone();
        let Some(kg) = self.kg.as_ref() else {
          return Err(ToolError::coded(
            "index-unavailable",
            "no graph is loaded — run the 'index' tool first",
          ));
        };
        // Page-materialized: the BFS runs whole (that IS the deterministic vector), but
        // record construction is paid per page — an undirected kernel walk reaches 200K+
        // nodes and building all their records to serve one page dominated this tool.
        let selected = vorpal_index::records::reach_records_page(
          kg,
          flows_dir.as_deref(),
          &target,
          dir,
          &relations,
          max_depth,
          min_confidence,
          vorpal_index::records::PageRequest {
            cursor: args.get("cursor").and_then(Value::as_str),
            limit: args.get("limit").and_then(Value::as_u64),
          },
        )
        .map_err(ToolError::from)?;
        let data = vorpal_index::records::selected_page_value(
          selected,
          args.get("cursor").and_then(Value::as_str),
          args.get("limit").and_then(Value::as_u64),
        )
        .map_err(|message| ToolError::coded("bad-argument", message))?;
        // Text stays human-shaped but capped: a full undirected closure renders tens of MB.
        let text = vorpal_index::reachable_query_on(kg, flows_dir.as_deref(), &target, dir, &relations, max_depth, min_confidence)
          .map_err(|err| err.to_string())
          .map_err(ToolError::from)?;
        const TEXT_CAP: usize = 200;
        let text = match text.match_indices('\n').nth(TEXT_CAP - 1) {
          Some((at, _)) if at + 1 < text.len() => {
            let lines = text.lines().count();
            format!("{}… {} more lines — page structuredContent\n", &text[..at + 1], lines - TEXT_CAP)
          }
          _ => text,
        };
        Ok((text, data))
      }
      // `serves` gates every entry, so an unknown name here is a drift between the
      // declarations and this match — report it as such rather than as the caller's fault.
      other => Err(ToolError::coded("internal", format!("tool '{other}' is declared but has no implementation"))),
    }
  }

  /// The warm graph: lazily cold-open the persisted index on first query, then reuse.
  fn kg(&mut self) -> Result<&Kg, String> {
    if self.kg.is_none() {
      let dir = vorpal_kg::resolve_index_dir(&self.index_dir);
      let loaded = Kg::load(&dir).map_err(|err| {
        format!(
          "no index loaded from {} — call the 'index' tool first ({err})",
          self.index_dir.display()
        )
      })?;
      self.kg = Some(Arc::new(loaded));
      self.kg_dir = Some(dir);
    }
    self
      .kg
      .as_deref()
      .ok_or_else(|| "index load raced away — retry the query".to_string())
  }
}

/// Every tool name this server can ever serve (the full profile), in listing order — the
/// membership authority behind [`Server::serves`].
const ALL_TOOL_NAMES: &[&str] = &[
  "index", "health", "schema", "coverage", "code_search", "architecture", "compare_generations",
  "impact", "dead_code", "node", "graph", "reachable", "structural_search", "rule_search",
  "ast_dump", "fetch_span", "data_flow", "query", "snippet", "why", "search",
];

/// The relations `graph` serves — each was a tool of its own before 2026-09-04, and the
/// arm names below are still theirs.
const GRAPH_RELATIONS: &[&str] = &[
  "callers", "references", "importers", "implementors", "type_users", "similar", "observed",
];

/// The tool declarations `tools/list` returns for `profile`: filtered by the one membership
/// authority, then decorated (titles, annotation hints, `format`, output schemas).
pub(crate) fn tool_declarations(profile: Profile) -> Vec<Value> {
  // Every description is terse ON PURPOSE: a client such as Claude Code either loads a
  // tool's schema in a model turn of its own or carries the whole listing in every turn's
  // input, so bytes here are tokens per call. The prose lives in docs/mcp.md; the
  // listing is size-gated by a test (tests/protocol.rs) so it cannot silently re-bloat.
  let sel = json!({
    "name": {"type": "string"},
    "path": {"type": "string", "description": "suffix"},
    "kind": {"type": "string"},
    "id": {"type": "integer"},
    "eid": {"type": "string"},
    "all": {"type": "boolean", "description": "merge same-named"},
    "cursor": {"type": "string"},
    "limit": {"type": "integer", "description": "max 1000"}
  });
  let page = json!({
    "cursor": {"type": "string"},
    "limit": {"type": "integer", "description": "max 1000"}
  });
  let with = |base: &Value, extra: Value| -> Value {
    let mut props = base.clone();
    if let (Some(p), Some(e)) = (props.as_object_mut(), extra.as_object()) {
      for (k, v) in e {
        p.insert(k.clone(), v.clone());
      }
    }
    props
  };
  let tools: Vec<Value> = vec![
    tool(
      "index",
      "Build or refresh the index from a source directory; near-instant when unchanged.",
      json!({
        "src": {"type": "string"},
        "verify": {"type": "boolean", "description": "by content"},
        "parse_health": {"type": "string", "enum": ["warn", "exclude", "fail"]},
        "max_error_ratio": {"type": "number", "description": "error-byte ratio"},
        "semantic_tier": {"type": "string", "enum": ["lexical", "learned"]}
      }),
      &["src"],
    ),
    tool("health", "Per-file parse damage: ERROR nodes, affected bytes, definitions in damaged regions.", json!({}), &[]),
    tool("schema", "Kinds, relations, grades, and tier state in this index, with counts.", json!({}), &[]),
    tool("coverage", "Per-file parse coverage (error bytes and ratio), worst first.", page.clone(), &[]),
    tool(
      "code_search",
      "ast-grep pattern search ranked by graph importance.",
      with(&page, json!({
        "pattern": {"type": "string", "description": "ast-grep pattern"},
        "lang": {"type": "string"},
        "prefix": {"type": "string"},
        "k": {"type": "integer", "description": "top-k"}
      })),
      &["pattern"],
    ),
    tool("architecture", "Orientation summary: module mass, hubs by in-degree, entry-point candidates.", json!({"top": {"type": "integer", "description": "rows per section"}}), &[]),
    tool(
      "compare_generations",
      "What changed between two index generations: files, nodes by durable eid, edge counts.",
      with(&page, json!({
        "from": {"type": "string", "description": "generation id"},
        "to": {"type": "string", "description": "generation id"}
      })),
      &[],
    ),
    tool(
      "impact",
      "Blast radius of changed files: git-diff-seeded transitive inbound closure.",
      with(&page, json!({
        "since": {"type": "string", "description": "git ref"},
        "relations": {"type": "array", "items": {"type": "string"}},
        "max_depth": {"type": "integer", "description": "0 = unbounded"},
        "min_grade": {"type": "string"}
      })),
      &[],
    ),
    tool(
      "dead_code",
      "Definitions with no semantic in-edges anywhere (suppression-honest dead-code leads).",
      with(&page, json!({
        "prefix": {"type": "string"},
        "path": {"type": "string", "description": "suffix"},
        "kind": {"type": "string"},
        "exported": {"type": "boolean"},
        "exclude_tests": {"type": "boolean"}
      })),
      &[],
    ),
    tool(
      "node",
      "Definitions by exact name or regex `pattern`. Only needed when a name is unknown or ambiguous; graph/reachable/snippet take names directly.",
      with(&sel, json!({"pattern": {"type": "string", "description": "ast-grep pattern"}})),
      &[],
    ),
    tool(
      "graph",
      "Direct neighbours of a symbol over one relation: the COMPLETE resolved set at the stated grade, no confirmation needed. callers/references rows carry the call-site line.",
      with(&sel, json!({"relation": {"type": "string", "enum": GRAPH_RELATIONS}})),
      &["relation", "name"],
    ),
    tool(
      "reachable",
      "Transitive closure from a symbol over `relations` (default calls), each row with its path to the seed. Complete; no confirmation needed.",
      with(&sel, json!({
        "direction": {"type": "string", "enum": ["in", "out", "both"]},
        "relations": {"type": "array", "items": {"type": "string"}},
        "max_depth": {"type": "integer", "description": "0 = unbounded"},
        "min_grade": {"type": "string", "enum": ["exact", "constrained", "heuristic"]}
      })),
      &["name", "direction"],
    ),
    tool(
      "structural_search",
      "ast-grep pattern over the watched source tree: path:line + matched text.",
      json!({
        "pattern": {"type": "string", "description": "ast-grep pattern"},
        "lang": {"type": "string"},
        "path": {"type": "string", "description": "suffix"},
        "limit": {"type": "integer", "description": "max 1000"}
      }),
      &["pattern", "lang"],
    ),
    tool(
      "rule_search",
      "Run YAML rule(s), constraints and fix dry-run included, over the watched tree.",
      json!({
        "rule": {"type": "string", "description": "YAML"},
        "path": {"type": "string", "description": "suffix"},
        "limit": {"type": "integer", "description": "max 1000"}
      }),
      &["rule"],
    ),
    tool(
      "ast_dump",
      "Named-node parse tree (kind, span, leaf text) for a file or inline source.",
      json!({
        "path": {"type": "string", "description": "suffix"},
        "source": {"type": "string", "description": "inline source"},
        "lang": {"type": "string"},
        "max_nodes": {"type": "integer"}
      }),
      &[],
    ),
    tool(
      "fetch_span",
      "Verbatim, digest-verified source of a node by id.",
      json!({
        "id": {"type": "integer"},
        "max_bytes": {"type": "integer"}
      }),
      &["id"],
    ),
    tool(
      "data_flow",
      "Where a symbol's arguments flow: arg→param rows (Rust, Python, TypeScript).",
      json!({
        "name": {"type": "string"},
        "path": {"type": "string", "description": "suffix"},
        "kind": {"type": "string"},
        "id": {"type": "integer"},
        "eid": {"type": "string"}
      }),
      &["name"],
    ),
    tool(
      "query",
      "Read-only Cypher-shaped graph query: MATCH (a:Kind {name: \"x\"})-[:calls*1..3]->(b) WHERE … WITH/UNWIND … RETURN [DISTINCT] properties or count/sum/avg/min/max/collect ORDER BY/SKIP/LIMIT, UNION. Refuses unsupported clauses and work ceilings by name.",
      json!({
        "text": {"type": "string"},
        "ir": {"type": "object", "description": "typed IR"}
      }),
      &[],
    ),
    tool(
      "snippet",
      "Verbatim, digest-verified source of a symbol by name, whole lines, with context.",
      with(&sel, json!({
        "context_lines": {"type": "integer"},
        "max_bytes": {"type": "integer"}
      })),
      &["name"],
    ),
    tool(
      "why",
      "Evidence for the edge from_id→to_id, or with `name` why no edge to that name exists: type, grade, reason, candidates, span.",
      with(&page, json!({
        "from_id": {"type": "integer"},
        "to_id": {"type": "integer"},
        "name": {"type": "string"}
      })),
      &["from_id"],
    ),
    tool(
      "search",
      "Hybrid search over definitions: name match + embedding similarity + graph in-degree.",
      with(&page, json!({
        "query": {"type": "string", "description": "text, or phrase AND phrase"},
        "k": {"type": "integer", "description": "top-k"},
        "kind": {"type": "string"},
        "lang": {"type": "string"},
        "path": {"type": "string", "description": "suffix"},
        "prefix": {"type": "string"},
        "exported": {"type": "boolean"},
        "exclude_tests": {"type": "boolean"}
      })),
      &["query"],
    ),
  ];
  // Advertise exactly what call_tool will accept: one membership authority.
  let mut tools: Vec<Value> = tools
    .into_iter()
    .filter(|t| profile.allows(t["name"].as_str().unwrap_or("")))
    .collect();
  decorate_tools(&mut tools);
  tools
}

/// The cursor/pagination contract shared by every record-returning tool (IMPROVEMENTS #7):
/// results are deterministic vectors, `cursor` is an opaque `o:<offset>` into that order,
/// `limit` caps the page (default 100, max 1000), and the returned object always **declares**
/// truncation — `total`, `truncated`, and `nextCursor` when more remain. Recomputing the
/// vector per page keeps the server stateless; determinism makes the pages coherent.
fn paged<T: serde::Serialize>(
  records: Vec<T>,
  args: &Value,
  outcome: &str,
) -> Result<Value, ToolError> {
  vorpal_index::records::paged_value(
    &records,
    args.get("cursor").and_then(Value::as_str),
    args.get("limit").and_then(Value::as_u64),
    outcome,
  )
  .map_err(|message| ToolError::coded("bad-argument", message))
}

/// Map a selector outcome to the structured object: `no-match` and `ambiguous` are answers
/// (the ambiguous candidates page like any records), never errors.
fn selected_data<T: serde::Serialize>(
  selected: vorpal_index::records::Selected<T>,
  args: &Value,
) -> Result<Value, ToolError> {
  vorpal_index::records::selected_value(
    selected,
    args.get("cursor").and_then(Value::as_str),
    args.get("limit").and_then(Value::as_u64),
  )
  .map_err(|message| ToolError::coded("bad-argument", message))
}

/// A tool failure with a stable machine-readable code (IMPROVEMENTS #7). Codes are part of
/// the MCP contract (and enumerated in every tool's `outputSchema`): `bad-argument` (caller
/// passed something unusable), `bad-query` (a `query` the engine refuses by name — a work
/// ceiling or unsupported clause), `index-unavailable` (no graph to answer from —
/// build/revalidate failed), `no-watch` (a source-tree tool on a custom index location),
/// `stale-source` (file changed since the pinned generation indexed it), `internal` (a
/// server-side invariant broke), and `tool-error` (everything else, message-only).
struct ToolError {
  code: &'static str,
  message: String,
}

impl ToolError {
  fn coded(code: &'static str, message: impl Into<String>) -> Self {
    Self {
      code,
      message: message.into(),
    }
  }
}

/// Plain-string errors keep their message and take the generic code, so tool arms only name
/// a code where the class is structurally distinguishable.
impl From<String> for ToolError {
  fn from(message: String) -> Self {
    Self {
      code: "tool-error",
      message,
    }
  }
}

/// The shared selector construction every symbol-addressed tool uses: `name` (or
/// `eid:<hex>` in name position), plus optional `id`/`eid`/`path`/`kind`/`all` facets.
fn graph_target(args: &Value, name: String) -> vorpal_index::GraphTarget {
  vorpal_index::GraphTarget {
    name,
    id: args.get("id").and_then(Value::as_u64),
    // The durable-bookmark facet; `name: "eid:<hex>"` works too (shared wire form).
    external_id: args
      .get("eid")
      .and_then(Value::as_str)
      .and_then(|hex| u128::from_str_radix(hex, 16).ok()),
    path_suffix: args.get("path").and_then(Value::as_str).map(str::to_string),
    kind: args.get("kind").and_then(Value::as_str).map(str::to_string),
    merge_all: args.get("all").and_then(Value::as_bool).unwrap_or(false),
    show_ids: true,
  }
}

fn tool(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
  json!({
    "name": name,
    "description": description,
    "inputSchema": {"type": "object", "properties": properties, "required": required}
  })
}

/// Coded error for a state the match above makes impossible (report and note both absent);
/// returning an error keeps the no-panic contract even if a refactor breaks the invariant.
fn unreachable_report() -> Result<String, String> {
  Err("internal: index build produced neither a report nor a supervised note".to_string())
}


/// The source root a default-layout index dir implies (`<src>/.vorpal/index` → `<src>`), if
/// that root exists — the precondition for watching.
fn watch_root(index_dir: &Path) -> Option<PathBuf> {
  let vorpal = index_dir.parent()?;
  if index_dir.file_name()? != "index" || vorpal.file_name()? != ".vorpal" {
    return None;
  }
  let src = vorpal.parent()?;
  // An empty parent means the index dir was given as a bare relative `.vorpal/index`: the
  // source root is the current directory.
  let src = if src.as_os_str().is_empty() {
    Path::new(".")
  } else {
    src
  };
  src.is_dir().then(|| src.to_path_buf())
}


