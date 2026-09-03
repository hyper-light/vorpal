//! `vorpal-index` — a thin CLI over the ingest → resolve → persist → query pipeline (§3.6).
//!
//! `build_index` is incremental (§3.4): a stat manifest decides per file whether its cached
//! extraction product can be replayed or the file must be re-parsed; the graph is always
//! re-linked from the complete product set (so removals/renames cannot leave stale nodes), and
//! an entirely unchanged tree short-circuits to reusing the persisted index outright.
//!
//! Per-file work (read → parse → extract → cache write, or cache replay) fans out on rayon
//! (§7.5 work-stealing parse/extract): workers borrow the shared extractor immutably, and the
//! order-preserving collect keeps the product list — and therefore node-id assignment — exactly
//! as deterministic as the serial loop was.

pub mod annfiles;
pub mod artifact;
mod dense;
pub mod duration;
pub mod cochange;
pub mod traces;
pub mod live;
pub mod live_ann;
pub mod gendiff;
pub mod impact;
pub mod autowarm;
pub mod graph_predicates;
pub mod postings;
pub mod records;

use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::UNIX_EPOCH;

use vorpal_ann::learned::{LearnedModel, LearnedStaticEmbedder, TrainResources, save_model};
use vorpal_ann::{AnnIndex, Embedder, LexicalEmbedder, ModelProvenance, tokenize};
use vorpal_ingest::{
  ExtractScratch, Manifest, OutlineExtractor, PackMsg, PackReader, PackWriter, Resolver,
  StreamWork, cache_file_name, decode_product,
  load_product, peek_product_stamps, save_product, stream_apply_spilled,
  validate_product,
};
// `Kg` is imported once and re-exported for downstream surfaces (CLI) that route all graph
// access through this crate.
pub use vorpal_ingest::{DynamicCanary, ExtractionEnv, RuleSource};
pub use vorpal_kg::{Direction, EdgeType, Kg};
use vorpal_kg::NodeId;

/// Summary of an indexing run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexReport {
  /// The tree was unchanged since the last index — reused without re-parsing (§3.4).
  pub reused: bool,
  /// The committed generation's GRAPH artifacts are byte-identical carries of the prior
  /// generation's (whole-tree reuse, and the stamp-only cutoff — which re-extracts changed
  /// files but proves their extraction unchanged). A serving daemon may keep its loaded
  /// graph, ANN tier, and overlay: they remain exact for the new generation. Distinct from
  /// `reused`, which additionally claims no file was re-parsed at all.
  pub graph_reused: bool,
  /// Files re-parsed this run (changed, new, or cache-missing).
  pub indexed: u64,
  /// Files whose cached extraction product was replayed without a parse.
  pub skipped: u64,
  /// Files whose tree-sitter parse produced ERROR nodes — some of their definitions may be
  /// missing from the graph. A language-agnostic parse-health signal (graceful degradation
  /// made visible), 0 when every file parsed cleanly.
  pub error_files: u64,
  /// Total tree-sitter ERROR nodes across all files — the magnitude behind `error_files`, so a
  /// corpus with one badly-broken file reads differently from one with many lightly-broken files.
  pub error_nodes: u64,
  /// Total bytes covered by (merged) ERROR ranges across all files — with per-file sizes, the
  /// covered-byte ratio parse-health policies threshold on (IMPROVEMENTS #11).
  pub error_bytes: u64,
  /// Files the parse-health `exclude` policy dropped from the graph this build.
  pub excluded_files: u64,
  pub nodes: usize,
  /// Confidently resolved references (single visible definition).
  pub resolved: u64,
  /// Approximately resolved references (multiple candidates; labeled edges).
  pub ambiguous: u64,
  /// References whose name is defined nowhere in the tree (std/dependencies) — honest.
  pub external: u64,
  /// References with in-tree candidates none of which is safely attributable.
  pub masked: u64,
  /// The cache-validity mode this run used (`fast-stat` / `verified`).
  pub cache_mode: &'static str,
  /// Dynamic languages extracted this build WITHOUT a canary (F-M4): best-effort tier, named
  /// on every report so an unverified grammar can never pass as a verified one. Empty for the
  /// default environment and whenever every dynamic language has a canary.
  pub unverified_langs: Vec<String>,
  /// Symmetric `changes_with` pairs sealed from git history (each pair counted once).
  pub cochange_edges: u64,
  /// Why the co-change pass produced nothing (not a repository, disabled, no history) —
  /// stated on the report, never a silent zero. `None` when edges were computed.
  pub cochange_note: Option<String>,
  /// Symmetric `similar_to` near-clone pairs sealed from extraction-time sketches (each pair
  /// counted once).
  pub similar_edges: u64,
  /// Why the similarity pass produced nothing, or what it truncated — stated, never a
  /// silent zero. `None` when pairs were sealed.
  pub similar_note: Option<String>,
  /// HTTP client call sites with a literal URL seen this build.
  pub request_sites: u64,
  /// Directional `requests` edges sealed (unique client URL → route template matches).
  pub request_edges: u64,
  /// Stated when request sites existed but nothing linked — external services are normal,
  /// silence is not.
  pub request_note: Option<String>,
}

impl IndexReport {
  /// References that produced no edge (external + masked).
  pub fn unresolved(&self) -> u64 {
    self.external + self.masked
  }
}

/// In-flight byte ceiling for the streaming ingest (§7.5): sources+products in transit stay
/// under this regardless of corpus size. Sized to feed every worker generously; the essential
/// output (the graph under construction) is not part of transit and scales with the corpus.
fn stream_budget_bytes() -> u64 {
  // Tunable for memory-vs-throughput experiments; the default keeps every worker fed with
  // lookahead without letting decoded products pile up ahead of the committers.
  if let Some(mb) = std::env::var("VORPAL_STREAM_BUDGET_MB")
    .ok()
    .and_then(|v| v.parse::<u64>().ok())
    .filter(|&mb| mb > 0)
  {
    return mb * 1024 * 1024;
  }
  let threads = std::thread::available_parallelism()
    .map(|n| n.get() as u64)
    .unwrap_or(1);
  // 24 MiB per worker: at 8 MiB the kernel's 10–20 MiB generated headers stalled admission
  // (measured ~0.5 s of wall at 18 cores); peak RSS is unaffected because the build's
  // high-water lives at seal (~1.08 GB), well above the stream phase either way.
  (threads * 24 * 1024 * 1024).clamp(64 * 1024 * 1024, 768 * 1024 * 1024)
}

/// How cache validity is decided (IMPROVEMENTS 07-29 §3) — an explicit, testable product
/// contract instead of an environment convention.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CacheMode {
  /// Stat (path, size, mtime) is trusted outside the racy-mtime window; content digests are
  /// verified only inside it. Fast (a no-change re-index is a metadata sweep) with one
  /// documented blind spot: a same-size edit whose mtime is deliberately preserved, made
  /// outside the racy window, can replay stale extraction.
  #[default]
  FastStat,
  /// Content-authoritative: every replay decision verifies the stored source digest against
  /// the file's current bytes. Reads every candidate file — slower, and immune to
  /// preserved-mtime edits. `VORPAL_VERIFY_CACHE=1` selects this mode for any entry point
  /// that does not pass one explicitly (CI convention).
  Verified,
}

impl CacheMode {
  /// The mode the environment requests when a caller passes none.
  pub fn from_env() -> CacheMode {
    if std::env::var_os("VORPAL_VERIFY_CACHE").is_some_and(|v| v == "1") {
      CacheMode::Verified
    } else {
      CacheMode::FastStat
    }
  }

  pub fn label(self) -> &'static str {
    match self {
      CacheMode::FastStat => "fast-stat",
      CacheMode::Verified => "verified",
    }
  }
}

/// What a build does about files whose parse produced ERROR nodes (IMPROVEMENTS #11).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ParseHealthMode {
  /// Ingest everything, report the damage (the default — graceful degradation, visible).
  #[default]
  Warn,
  /// Files past the threshold contribute NOTHING to the graph (their products still bank,
  /// so a later Warn build reuses them). Missing relations from excluded files are then
  /// meaningful absence, not silent decay.
  Exclude,
  /// Fail the build, listing the offenders — for pipelines that treat parse damage as a
  /// stop-the-line defect.
  Fail,
}

/// The threshold a [`ParseHealthMode`] acts on: a file is unhealthy when its merged
/// ERROR-covered bytes exceed `max_error_ratio` of its size (0.0 = any error byte).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ParseHealthPolicy {
  pub mode: ParseHealthMode,
  pub max_error_ratio: f64,
}

impl Default for ParseHealthPolicy {
  fn default() -> Self {
    Self {
      mode: ParseHealthMode::Warn,
      max_error_ratio: 0.0,
    }
  }
}

impl ParseHealthPolicy {
  /// Whether a file with `error_bytes` of `size` crosses this policy's threshold.
  pub fn is_unhealthy(&self, error_bytes: u64, size: u64) -> bool {
    error_bytes > 0
      && (size == 0 || error_bytes as f64 / size as f64 > self.max_error_ratio)
  }
}

/// Ingest `src`, resolve cross-file references, and persist the knowledge graph to `out`,
/// with cache validity decided by [`CacheMode::from_env`] (fast-stat unless
/// `VORPAL_VERIFY_CACHE=1`).
pub fn build_index(src: &Path, out: &Path) -> Result<IndexReport, Box<dyn Error>> {
  build_index_with(src, out, CacheMode::from_env())
}

/// [`build_index`] with an explicit [`CacheMode`] — the first-class form CLI/MCP/bindings
/// select rather than routing through an environment variable.
pub fn build_index_with(
  src: &Path,
  out: &Path,
  cache_mode: CacheMode,
) -> Result<IndexReport, Box<dyn Error>> {
  build_index_full(src, out, cache_mode, ParseHealthPolicy::default(), None)
}

/// Watched-daemon build: `hints` is a COMPLETE set of every file changed since the prior
/// manifest (the watcher's certainty contract — see `SourceWatch::take_changes`). The stat
/// sweep is replaced by patching the prior manifest for exactly those paths; any hint the
/// patch cannot prove equivalent to a full scan (a path the prior manifest never held — a
/// nested .gitignore could make the walker disagree with the watcher about it) falls back to
/// the full sweep. The committed generation is identical either way (pinned by test).
pub fn build_index_watched(
  src: &Path,
  out: &Path,
  hints: &std::collections::HashSet<PathBuf>,
  env: &vorpal_ingest::ExtractionEnv,
) -> Result<IndexReport, Box<dyn Error>> {
  build_index_inner(
    src,
    out,
    CacheMode::from_env(),
    ParseHealthPolicy::default(),
    Some(hints),
    None,
    env,
  )
}

/// [`build_index_with`] plus an explicit [`ParseHealthPolicy`] (IMPROVEMENTS #11): warn is
/// today's behavior; exclude drops unhealthy files from the graph; fail aborts before the
/// generation commits, listing offenders. Non-warn policies bypass the unchanged-tree fast
/// path (its prior generation was built under some other policy and proves nothing).
/// Patch a prior manifest with a COMPLETE set of changed paths in place of a stat sweep.
/// `None` = the patch cannot be proven equivalent to a full scan (a hinted path the prior
/// manifest never carried — the walker's ignore rules could disagree with the watcher about
/// it) → the caller sweeps. Modified files re-stat; vanished files drop; hints the extractor
/// does not handle are irrelevant by construction (the sweep would skip them too).
fn patch_manifest(
  prior: &Manifest,
  hints: &std::collections::HashSet<PathBuf>,
  handled: impl Fn(&str) -> bool,
) -> Option<Manifest> {
  let mut entries = prior.entries().to_vec();
  for hint in hints {
    let path_str = hint.to_string_lossy();
    if !handled(&path_str) {
      continue;
    }
    let at = entries.binary_search_by(|entry| entry.path.as_str().cmp(&path_str));
    match (at, fs::metadata(hint)) {
      (Ok(found), Ok(meta)) if meta.is_file() => {
        let mtime_ns = meta
          .modified()
          .ok()
          .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
          .map(|d| d.as_nanos() as u64)
          .unwrap_or(0);
        entries[found].size = meta.len();
        entries[found].mtime_ns = mtime_ns;
      }
      (Ok(found), _) => {
        // Vanished (deleted, or replaced by a non-file): drop it, as the sweep would.
        entries.remove(found);
      }
      (Err(_), Err(_)) => {
        // Never indexed and no longer present — transient noise, nothing to patch.
      }
      (Err(_), Ok(_)) => {
        // A path the prior manifest never carried: only the ignore-aware walk can decide
        // whether it belongs. Fall back to the sweep.
        return None;
      }
    }
  }
  Some(Manifest::from_entries(entries))
}

pub fn build_index_full(
  src: &Path,
  out: &Path,
  cache_mode: CacheMode,
  policy: ParseHealthPolicy,
  hints: Option<&std::collections::HashSet<PathBuf>>,
) -> Result<IndexReport, Box<dyn Error>> {
  build_index_inner(
    src,
    out,
    cache_mode,
    policy,
    hints,
    None,
    &vorpal_ingest::ExtractionEnv::default(),
  )
}

/// [`build_index_full`] under an explicit [`ExtractionEnv`] (F-M3): extra outline-rule sources
/// extend extraction to registered custom/dynamic languages. The default environment is
/// byte-identical to the bundled behavior. Registration (any dlopen) is the caller's one-shot
/// startup act — this function never loads code, so serving surfaces (MCP tools) can never
/// trigger a dlopen through it.
pub fn build_index_env(
  src: &Path,
  out: &Path,
  cache_mode: CacheMode,
  policy: ParseHealthPolicy,
  env: &vorpal_ingest::ExtractionEnv,
) -> Result<IndexReport, Box<dyn Error>> {
  build_index_inner(src, out, cache_mode, policy, None, None, env)
}

/// Persist the sigs family (P4.5c): every signed definition's near-clone sketch, keyed by
/// sealed node id, identity-coded to `(file_key, ordinal)` and bucketed for hard-link carry.
/// Bucketed generations only — the flat lane's artifact set is frozen (its byte-identity to
/// the pre-P4 binary is a proven law), so under `SegmentLayout::Flat` this writes nothing.
/// The u64→u32 narrowing is checked: a sealed id past u32 is a corrupt universe, not data.
pub(crate) fn save_sig_family(
  staging: &Path,
  rows: &[vorpal_ingest::SigRow],
  bases: &Option<vorpal_kg::NodeIdMap>,
  prior: &Path,
) -> io::Result<()> {
  let Some(map) = bases else {
    return Ok(());
  };
  let mut family = Vec::with_capacity(rows.len());
  for row in rows {
    let node = u32::try_from(row.node)
      .map_err(|_| io::Error::other("sig row node id exceeds the sealed id space"))?;
    family.push(vorpal_kg::SigFamilyRow { node, shingles: row.shingles, sketch: row.sketch });
  }
  vorpal_kg::save_sigs(staging, &family, map, Some(prior))
}

/// A full-pipeline build whose persistence tail was deferred (SUBSECOND.md Phase 3, live
/// rebuild v1). Everything answer-affecting is already computed — the graph is sealed and
/// [`PendingPersist::persist`] performs only writes: evidence + segments + manifest into the
/// staged directory, then the content-addressed generation commit. It is the exact
/// synchronous tail, byte for byte and in the same order — only the thread differs — so the
/// committed generation is identical to what a synchronous build of the same tree commits.
pub struct PendingPersist {
  out: PathBuf,
  prior: PathBuf,
  staging: PathBuf,
  evidence: Vec<vorpal_kg::EvidenceRow>,
  flows: Vec<vorpal_kg::DataflowRow>,
  /// Sigs-family rows (P4.5c) in sealed-id space — persisted beside evidence, bucketed
  /// generations only.
  sigs: Vec<vorpal_ingest::SigRow>,
  manifest: Manifest,
  /// Encoded `reach.bin` bytes from the link — the deferred tail writes them exactly
  /// where the synchronous tail does (beside dataflow), so both commit identical
  /// generations.
  reach_graph: Vec<u8>,
  kg: Arc<Kg>,
  /// The node-store layout this generation persists under (P4.2) — decided by the build
  /// that deferred here, carried so the background tail writes the same format.
  layout: vorpal_kg::SegmentLayout,
  /// The still-running pack consolidation from the build that deferred here — joined
  /// before the manifest/commit, so the deferred tail overlaps it exactly like the
  /// synchronous tail does.
  pack: Option<std::thread::JoinHandle<io::Result<()>>>,
}

impl PendingPersist {
  /// Run the deferred tail. Returns the committed generation directory — the pin for
  /// artifact-grade tools (`fetch_span`, `why`) once the caller adopts it. `String` error
  /// (not `Box<dyn Error>`) because this crosses a thread boundary.
  pub fn persist(self) -> Result<PathBuf, String> {
    let PendingPersist {
      out,
      prior,
      staging,
      evidence,
      flows,
      sigs,
      manifest,
      reach_graph,
      kg,
      layout,
      pack,
    } = self;
    let evidence_bases = kg
      .node_id_map(&layout)
      .map_err(|err| format!("evidence bases: {err}"))?;
    let (evidence_result, dataflow_result, sigs_result, reach_result, kg_result) = std::thread::scope(|scope| {
      let evidence_task = scope.spawn(|| {
        let evidence_layout = match &evidence_bases {
          None => vorpal_kg::EvidenceLayout::Flat,
          Some(map) => vorpal_kg::EvidenceLayout::Bucketed {
            nodes: map,
            prior: Some(&prior),
          },
        };
        vorpal_kg::save_evidence_with(&staging, evidence, &evidence_layout)
      });
      let dataflow_task = scope.spawn(|| vorpal_kg::save_dataflow(&staging, flows));
      let sigs_task = scope.spawn(|| save_sig_family(&staging, &sigs, &evidence_bases, &prior));
      let reach_result = fs::write(staging.join("reach.bin"), &reach_graph);
      let kg_result = kg.save_with(&staging, &layout);
      (
        evidence_task.join().expect("evidence saver panicked"),
        dataflow_task.join().expect("dataflow saver panicked"),
        sigs_task.join().expect("sigs saver panicked"),
        reach_result,
        kg_result,
      )
    });
    evidence_result.map_err(|err| format!("evidence save failed: {err}"))?;
    dataflow_result.map_err(|err| format!("dataflow save failed: {err}"))?;
    sigs_result.map_err(|err| format!("sigs save failed: {err}"))?;
    reach_result.map_err(|err| format!("reach save failed: {err}"))?;
    kg_result.map_err(|err| format!("graph save failed: {err}"))?;
    if let Some(pack) = pack {
      pack
        .join()
        .map_err(|_| "pack writer panicked".to_string())?
        .map_err(|err| format!("pack write failed: {err}"))?;
    }
    manifest
      .save(&staging.join("manifest.bin"))
      .map_err(|err| format!("manifest save failed: {err}"))?;
    let id = commit_generation(&out, &prior, staging)
      .map_err(|err| format!("generation commit failed: {err}"))?;
    Ok(out.join("gen").join(id))
  }
}

/// What [`build_index_live`] hands the daemon.
pub struct LiveBuild {
  pub report: IndexReport,
  /// The sealed in-memory graph when the full pipeline ran — the daemon serves it directly
  /// instead of re-loading bytes it just wrote. Fast paths (whole-tree reuse, the stamp-only
  /// cutoff) leave it `None`: the caller's loaded graph is already answer-identical, and the
  /// generation those paths committed hardlinks the very artifacts it has mapped.
  pub kg: Option<Arc<Kg>>,
  /// The deferred persistence tail when the full pipeline ran; the caller runs it on a
  /// background thread and serves `kg` meanwhile. `None` whenever `kg` is `None` (fast
  /// paths commit synchronously — they are already cheap).
  pub pending: Option<PendingPersist>,
}

#[derive(Default)]
struct LiveSlots {
  kg: Option<Arc<Kg>>,
  pending: Option<PendingPersist>,
}

/// Daemon variant of [`build_index_full`]: identical computation, but a full pipeline run
/// returns with the sealed graph in hand and persistence deferred to the caller — the
/// edit→queryable latency stops paying artifact writes, content hashing, and the commit.
pub fn build_index_live(
  src: &Path,
  out: &Path,
  hints: Option<&std::collections::HashSet<PathBuf>>,
  env: &vorpal_ingest::ExtractionEnv,
) -> Result<LiveBuild, Box<dyn Error>> {
  let mut slots = LiveSlots::default();
  let report = build_index_inner(
    src,
    out,
    CacheMode::from_env(),
    ParseHealthPolicy::default(),
    hints,
    Some(&mut slots),
    env,
  )?;
  Ok(LiveBuild {
    report,
    kg: slots.kg,
    pending: slots.pending,
  })
}

fn build_index_inner(
  src: &Path,
  out: &Path,
  cache_mode: CacheMode,
  policy: ParseHealthPolicy,
  hints: Option<&std::collections::HashSet<PathBuf>>,
  live: Option<&mut LiveSlots>,
  env: &vorpal_ingest::ExtractionEnv,
) -> Result<IndexReport, Box<dyn Error>> {
  // One tree, ONE spelling: canonicalize the root so every producer — CLI argv, daemon
  // watch root, bindings — keys manifests, pack entries, and node identities (eids hash
  // the path) identically. Without this a daemon-committed generation (canonical watch
  // root, e.g. /private/tmp on macOS) and a CLI run of the same tree (verbatim argv,
  // /tmp) miss every cache lookup and silently re-parse the world on each alternation.
  // Hints rebase onto the canonical root by prefix swap (pure string op — works for
  // deleted files, which canonicalize() cannot touch); an unreadable root keeps its
  // verbatim spelling and the scan below reports the real error.
  let canonical_src = src.canonicalize().unwrap_or_else(|_| src.to_path_buf());
  let rebased_hints: Option<std::collections::HashSet<PathBuf>> =
    hints.filter(|_| canonical_src != src).map(|set| {
      set
        .iter()
        .map(|hint| match hint.strip_prefix(src) {
          Ok(rel) => canonical_src.join(rel),
          Err(_) => hint.clone(),
        })
        .collect()
    });
  let hints = rebased_hints.as_ref().or(hints);
  let src = canonical_src.as_path();
  // The build session's string interner (scoped-interner contract, docs/EMBEDDING.md):
  // created here, dropped when this function returns — reclaim is `Drop`, and the `NameId`
  // lifetime brand makes anything holding a session id un-returnable at compile time.
  // Embedded hosts get bounded memory with no reclaim call at all.
  let interner = vorpal_ingest::Interner::default();
  vorpal_kg::phase_stamp("build: enter");
  let extractor = env.extractor()?;
  // Computed once, up front: reported on every exit path (fast-path reuse included) so an
  // unverified dynamic language is never silently trusted.
  let unverified_langs = env.unverified_langs(&extractor);
  vorpal_kg::phase_stamp("build: rules compiled");
  // Extraction identity for this run: the whole grammar set folded with the outline-rule digest.
  // Both the whole-tree fast path (via the manifest stamp) and the per-file replay gates key on
  // it, so editing a grammar OR an outline rule invalidates reuse just as a file edit would.
  let rules_digest = extractor.rules_digest();
  // The prior generation resolves before the scan so a hinted build can patch its manifest.
  let hinted_prior = vorpal_kg::resolve_index_dir(out);
  let mut manifest = 'scan: {
    if let Some(hints) = hints
      && let Ok(prior_manifest) = Manifest::load(&hinted_prior.join("manifest.bin"))
      && let Some(patched) = patch_manifest(&prior_manifest, hints, |p| extractor.handles(p))
    {
      vorpal_kg::phase_stamp("scan: hinted patch");
      break 'scan patched;
    }
    vorpal_kg::phase_stamp("scan: manifest start");
    let swept = Manifest::scan(src, |p| extractor.handles(p))?;
    vorpal_kg::phase_stamp("scan: manifest done");
    swept
  };
  manifest.set_grammar_stamp(vorpal_ingest::extraction_identity(
    vorpal_ingest::global_grammar_stamp(),
    rules_digest,
  ));
  // Phase-4 identity gate (P4.0): every file's tree-relative path must map to a unique
  // 64-bit file key — the bucketed format keys storage by it. O(files) on every build,
  // same posture as the u32 ceilings: loud and actionable, never a silent degradation.
  // `tree_root` is also the pack's absolute→tree-relative conversion root (P4.1).
  let tree_root: String = src.to_string_lossy().into_owned();
  vorpal_kg::identity::verify_file_keys(
    manifest
      .entries()
      .iter()
      .map(|entry| vorpal_kg::identity::tree_relative(&entry.path, &tree_root)),
  )
  .map_err(io::Error::other)?;
  vorpal_kg::phase_stamp("build: grammar stamp done");
  // Generation layout (IMPROVEMENTS §4): `out` is the index *root*. The live artifacts sit in
  // an immutable, content-addressed generation dir named by `out/CURRENT`; this run reads the
  // prior generation, stages a new one, and commits it with one atomic pointer swap — a
  // concurrent reader sees the complete old index or the complete new one, never a mixture.
  // A legacy flat root (no CURRENT) resolves to itself, so its artifacts still serve as the
  // prior; the first rebuild migrates it into a generation.
  let prior = vorpal_kg::resolve_index_dir(out);
  let manifest_path = prior.join("manifest.bin");

  // Staged cache validation (IMPROVEMENTS §6): stat (size+mtime) is the cheap hint; the v6
  // content digest is the identity. Digests are verified (a) always under
  // `VORPAL_VERIFY_CACHE=1` and (b) automatically for files in the **racy window** — mtime
  // within 2s of the previous manifest's write, where an edit can restore size+mtime within
  // timestamp granularity (the git racily-clean hazard). Both gates also suppress the
  // whole-tree reuse fast path below, which is otherwise stat-only.
  let verify_all = cache_mode == CacheMode::Verified;
  let prior_manifest_ns: u64 = fs::metadata(&manifest_path)
    .and_then(|m| m.modified())
    .ok()
    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
    .map(|d| d.as_nanos() as u64)
    .unwrap_or(0);
  // Racy files get their digests verified *inside* the fast path (reading only those few
  // files against the pack's stored digests) — an immediate no-change re-index keeps its
  // 0.1s reuse, while a stat-invisible racy edit falls through to a rebuild.
  let racy_files_verify = || -> bool {
    let racy: Vec<&vorpal_ingest::FileStat> = manifest
      .entries()
      .iter()
      .filter(|e| e.mtime_ns.abs_diff(prior_manifest_ns) < 2_000_000_000)
      .collect();
    if racy.is_empty() {
      return true;
    }
    let Some(pack) = PackReader::open_rooted(&prior, Some(&tree_root)) else {
      return false;
    };
    racy.iter().all(|entry| {
      let stored = pack
        .get(&entry.path)
        .and_then(vorpal_ingest::peek_product_digest);
      match (stored, fs::read(&entry.path)) {
        (Some(digest), Ok(bytes)) => xxhash_rust::xxh3::xxh3_64(&bytes) == digest,
        _ => false,
      }
    })
  };

  // Whole-tree fast path: nothing changed → reuse the persisted index without touching a file.
  // The report only needs the node count, read from the segment header — no heap read, no edge
  // read, no CSR rebuild. An unreadable/corrupt index falls through to a rebuild instead of
  // wedging every subsequent run on the same error.
  if let Ok(prior_manifest) = Manifest::load(&manifest_path) {
    // Family-law compatibility: a bucketed prior whose sigs family predates the current
    // survivor law (family VERSION mismatch) is not reused and not composed from — every
    // carry lane (whole-tree reuse, cutoff, respan, the scoped composes) would
    // perpetuate the old family's bytes while a scratch build of the same tree now
    // writes the new law's, breaking scratch ≡ incremental. One loud decline; the full
    // pipeline rebuilds the family once and every fast lane reopens.
    let prior_families_current = !prior.join(vorpal_kg::SIGS_TOC).is_file()
      || vorpal_kg::SigStore::open(&prior).is_some();
    if !prior_families_current {
      vorpal_kg::phase_stamp(
        "compose: prior sigs family is a previous version — full pipeline rebuilds it",
      );
    }
    if manifest.unchanged_since(&prior_manifest)
      && manifest.grammar_stamp() == prior_manifest.grammar_stamp()
      && !verify_all
      && prior_families_current
      && policy.mode == ParseHealthMode::Warn
      // Readiness is FORMAT-AWARE: a bucketed generation's node store answers through
      // its TOC (graph.bin is a lazy cache there, legitimately absent); the flat names
      // were a silent v2 gap that made unchanged trees pay a rebuild pass — exposed the
      // day the default flipped.
      && (prior.join(vorpal_kg::NODES_TOC).is_file()
        || (prior.join("strings.heap").exists() && prior.join("graph.bin").exists()))
      && racy_files_verify()
    {
      // Backfill: index dirs written before the name-index sidecar existed gain it here,
      // once — sublinear name lookup without forcing a rebuild. (An additive, self-validating
      // sidecar — like the lazy ANN tier, it is the one kind of write an existing generation
      // admits.)
      if !prior.join("names.idx").exists() {
        if let Ok(kg) = Kg::load(&prior) {
          let _ = kg.write_names_index(&prior);
        }
      }
      if let Ok(nodes) = Kg::peek_node_count(&prior) {
        return Ok(IndexReport {
          reused: true,
          graph_reused: true,
          cache_mode: cache_mode.label(),
          error_files: 0,
          error_nodes: 0,
          error_bytes: 0,
          excluded_files: 0,
          indexed: 0,
          skipped: manifest.len() as u64,
          nodes,
          resolved: 0,
          ambiguous: 0,
          external: 0,
          masked: 0,
          unverified_langs: unverified_langs.clone(),
          cochange_edges: 0,
          cochange_note: None,
          similar_edges: 0,
          similar_note: None,
          request_sites: 0,
          request_edges: 0,
          request_note: None,
        });
      }
    }
    // Product-equality early cutoff (docs/wip/SUBSECOND.md Phase 1a — Bazel-style "change
    // pruning"): stats changed, but if every changed file re-extracts to a product whose BODY
    // is byte-equal to the cached one (mtime-only touches, checkout restamps, comment or
    // whitespace edits that extraction cannot see), then a from-scratch build's graph
    // artifacts are provably identical to the prior generation's — determinism makes
    // "inputs byte-equal ⇒ outputs byte-equal" a theorem, not a hope. Stage the new
    // generation as hardlinks of the five graph artifacts + a stamp-patched pack clone + the
    // fresh manifest, and commit through the ordinary atomic path. Any doubt bails to the
    // full pipeline. Gated exactly like the whole-tree fast path (verified mode and
    // non-Warn health policies re-derive everything; racy files digest-verify).
    // Trust gates BEFORE the stamp-only cutoff: the cutoff re-extracts changed files and
    // rewrites stamp windows — extraction it trusts must pass the same selfcheck and
    // environment canaries the pipeline demands. (The whole-tree reuse path above stays
    // ungated: a reused build writes nothing, so there is nothing to trust anew.)
    vorpal_ingest::verify_default_extraction(&extractor).map_err(io::Error::other)?;
    vorpal_ingest::verify_env_extraction(&extractor, &env.canaries).map_err(io::Error::other)?;
    if !verify_all
      && policy.mode == ParseHealthMode::Warn
      && prior_families_current
      && manifest.grammar_stamp() == prior_manifest.grammar_stamp()
    {
      let ctx = CutoffContext {
        manifest: &manifest,
        prior_manifest: &prior_manifest,
        prior_manifest_ns,
        tree_root: &tree_root,
      };
      if let Some(mut report) =
        try_stamp_only_cutoff(out, &prior, &ctx, &extractor, cache_mode.label())?
      {
        // The cutoff re-extracts changed files with this environment's extractor — the
        // best-effort tier disclosure travels with every report, fast paths included.
        report.unverified_langs = unverified_langs.clone();
        return Ok(report);
      }
      // The scoped composes share ONE prior-graph load (each lane loaded its own kg +
      // map before — ~50 ms per load at kernel scale, two wasted loads on every
      // defs-changed edit). `load_prior_graph` answers None on flat/legacy priors —
      // the chain is bucketed-only, exactly as the per-lane TOC guards had it.
      if env.is_default()
        && let Some(prior_graph) = compose::load_prior_graph(&prior)
      {
        // The RESPAN compose (P4.5b): span-only edits — the class between
        // "byte-identical products" and "semantic change" — compose the next
        // generation mechanically from the prior one. Same trust gates, same
        // fallback posture.
        if let Some(mut report) = compose::try_respan_compose(
          out,
          &prior,
          &ctx,
          &extractor,
          cache_mode.label(),
          &prior_graph,
        )? {
          report.unverified_langs = unverified_langs.clone();
          return Ok(report);
        }
        // Past the respan, the DEFS-STABLE compose (P4.5c-2): body-edited files —
        // definitions unchanged, references free — re-resolve against the prior
        // universe and the families splice. Same trust gates, same loud fallback.
        if let Some(mut report) = compose::try_defs_stable_compose(
          out,
          &prior,
          &ctx,
          &extractor,
          cache_mode.label(),
          &prior_graph,
        )? {
          report.unverified_langs = unverified_langs.clone();
          return Ok(report);
        }
        // Past defs-stable, the DEFS-CHANGED compose (P4.5c-3): the definition set
        // moved — the usage-dirty closure re-resolves against the successor universe
        // and the families splice under the shift law. Same trust gates, same loud
        // fallback.
        if let Some(mut report) = compose::try_defs_changed_compose(
          out,
          &prior,
          &ctx,
          &extractor,
          cache_mode.label(),
          &prior_graph,
        )? {
          report.unverified_langs = unverified_langs.clone();
          return Ok(report);
        }
      }
    }
  }

  // Past the fast path, this run will stage a new generation and write bank products —
  // prove the binary can extract before letting it (crates/ingest selfcheck: a stale or
  // internally inconsistent build otherwise seals a silently gutted graph with exit 0).
  // Manifest-scoped: only languages this tree contains are checked (each memoized
  // process-wide), so a small repo neither compiles nor canaries the other ~45 languages.
  vorpal_ingest::verify_extraction_for_manifest(&extractor, manifest.entries())
    .map_err(io::Error::other)?;
  // Dynamic-language canaries (F-M4): environment-scoped, so not memoized — the same refusal
  // gate builtin languages get, extended to grammars that arrived via dlopen.
  vorpal_ingest::verify_env_extraction(&extractor, &env.canaries).map_err(io::Error::other)?;

  // Incremental path, streamed (§7.5): replay cached products for stat-unchanged files,
  // re-parse the rest — admission is byte-budget-gated, extraction fans out over scoped
  // workers with per-worker scratch, and products flow straight into the sharded single-writer
  // commit, so a product exists in RAM only between extraction and application. Products are
  // **self-validating** (§3.4): each carries the stat of the source it was extracted from, so
  // any cached product whose stamp matches replays — whoever wrote it. Read/extract errors
  // skip the file (as before); a cache-write error is fatal (as before). Sequence-ordered
  // per-shard application keeps the output bit-identical to the batch path.
  // The loose bank lives at the *root* (outside any generation): concurrent searches feed it
  // and every build consumes it, whichever generation is live.
  // Co-change pass in flight (ADOPTION #27): a HEAD-keyed cache hit costs two `git
  // rev-parse` calls; otherwise `git log` runs as a child beside the extraction stream and
  // is joined before link — its serial cost (1.1 s at kernel scale) hides under parsing.
  let cochange_pending = cochange::start(src, &out.join("cochange.cache"));
  let products_dir = out.join("products");
  fs::create_dir_all(&products_dir)?;
  // Stage the new generation in a scratch dir under `gen/`; it becomes `gen/<content-id>` at
  // commit. Fresh per run (a crashed run's staging is swept by the next commit's GC).
  let staging = out
    .join("gen")
    .join(format!(
      ".staging-{}-{}",
      std::process::id(),
      staging_nonce()
    ));
  let _ = fs::remove_dir_all(&staging);
  fs::create_dir_all(&staging)?;

  let digest_must_match = move |entry_mtime_ns: u64, stored_digest: Option<u64>, path: &Path| {
    let racy = entry_mtime_ns.abs_diff(prior_manifest_ns) < 2_000_000_000;
    if !verify_all && !racy {
      return true;
    }
    match (stored_digest, fs::read(path)) {
      (Some(stored), Ok(bytes)) => xxhash_rust::xxh3::xxh3_64(&bytes) == stored,
      _ => false,
    }
  };

  // Product cache, two tiers (§3.4): the **pack** (one mapped file — replay pays zero opens;
  // the loose-file cache cost one `open` per product, 72k of them at kernel scale, and macOS
  // serializes the open path) plus **loose files**, still written by search banking (separate
  // processes must not contend on the pack) and consolidated — then deleted — here. Loose
  // wins over pack on lookup: it is only ever fresher.
  let pack_reader = PackReader::open_rooted(&prior, Some(&tree_root)).map(Arc::new);
  let loose: HashSet<OsString> = fs::read_dir(&products_dir)
    .map(|dir| dir.flatten().map(|f| f.file_name()).collect())
    .unwrap_or_default();
  // The generation's storage format, decided once: bucketed by default since the flip,
  // with VORPAL_FORMAT=flat as the deprecated legacy escape. It governs the pack layout,
  // the node store layout, AND the canonical ingest order below — one decision, never
  // re-read.
  let format = vorpal_ingest::PackFormat::from_env();
  // The writer builds the new generation's pack in staging, copying reused bodies out of the
  // prior generation's mapping — the prior pack is never touched.
  let pack_writer = PackWriter::new(
    &staging,
    pack_reader.clone(),
    Some(tree_root.clone()),
    format,
  );
  let pack_sink = pack_writer.sink();
  let pack_pool = pack_writer.buf_pool();
  let live_paths: Vec<String> = manifest.entries().iter().map(|e| e.path.clone()).collect();
  let pack_thread = std::thread::spawn(move || pack_writer.finish(live_paths));
  let send_fatal =
    |_| std::io::Error::other("product pack writer failed; see the join error for the cause");

  // References spill to disk between commit and resolve — they are written once and read
  // once, sequentially; buffering them in RAM only raised the peak footprint (~220 MB at
  // kernel scale).
  let error_files = std::sync::atomic::AtomicU64::new(0);
  let error_nodes = std::sync::atomic::AtomicU64::new(0);
  let error_bytes = std::sync::atomic::AtomicU64::new(0);
  let excluded_files = std::sync::atomic::AtomicU64::new(0);
  // Fail-policy offenders: collected during the stream, judged before the commit.
  let unhealthy_files = std::sync::Mutex::new(Vec::<(String, f64)>::new());
  let note_unhealthy = |path: &str, bytes: u64, size: u64| {
    let ratio = if size == 0 { 1.0 } else { bytes as f64 / size as f64 };
    unhealthy_files.lock().unwrap().push((path.to_string(), ratio));
  };
  let spill_path = staging.join(".refs.spill");
  let heap_stream = staging.join("strings.heap.tmp");
  // P4.2: under the bucketed format the canonical order is BUCKET-MAJOR — and the batch
  // pipeline's canonical order IS its ingest order, so the stream walks a permuted view of
  // the entries. The manifest FILE stays path-sorted: every diff (two-pointer, racy-window,
  // whole-tree fast path) keeps its sort invariant.
  let bucket_major: Option<Vec<vorpal_ingest::FileStat>> = match format {
    vorpal_ingest::PackFormat::Bucketed => {
      let buckets = vorpal_kg::identity::bucket_count_for(manifest.entries().len());
      let mut entries = manifest.entries().to_vec();
      entries.sort_by(|a, b| {
        let ka = vorpal_kg::identity::bucket_of(
          vorpal_kg::identity::tree_relative(&a.path, &tree_root),
          buckets,
        );
        let kb = vorpal_kg::identity::bucket_of(
          vorpal_kg::identity::tree_relative(&b.path, &tree_root),
          buckets,
        );
        ka.cmp(&kb).then_with(|| a.path.cmp(&b.path))
      });
      Some(entries)
    }
    vorpal_ingest::PackFormat::Flat => None,
  };
  let stream_entries: &[vorpal_ingest::FileStat] =
    bucket_major.as_deref().unwrap_or(manifest.entries());
  vorpal_kg::phase_stamp("stream: start");
  // Fresh-product sink: the worker hands over the encoded bytes it just applied from;
  // they travel to the pack writer by move (the canonical consolidating sort makes
  // arrival order irrelevant), and the spool thread hands the emptied buffers back
  // through `pack_pool` — fresh parses start from warm, right-sized buffers.
  let fresh_sink = |path: String, body: Vec<u8>| -> io::Result<()> {
    pack_sink
      .send(PackMsg { path, body })
      .map_err(|_| io::Error::other("pack sink closed"))
  };
  let stream_result = stream_apply_spilled(
    &interner,
    stream_entries,
    stream_budget_bytes(),
    &spill_path,
    Some(&heap_stream),
    pack_reader.as_deref(),
    Some(&fresh_sink),
    |entry, scratch: &mut ExtractScratch| {
      let cache_name = cache_file_name(&entry.path);
      if loose.contains(&OsString::from(&cache_name)) {
        if let Ok(bytes) = fs::read(products_dir.join(&cache_name)) {
          if let Ok(product) = decode_product(&bytes) {
            if product.source_size == entry.size
              && product.source_mtime_ns == entry.mtime_ns
              && Some(product.grammar_digest)
                == vorpal_ingest::extraction_identity_for_path(&entry.path, rules_digest)
              && digest_must_match(
                entry.mtime_ns,
                Some(product.source_xxh3),
                Path::new(&entry.path),
              )
            {
              if product.error_nodes > 0 {
                error_files.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                error_nodes.fetch_add(product.error_nodes as u64, std::sync::atomic::Ordering::Relaxed);
                error_bytes.fetch_add(product.error_bytes, std::sync::atomic::Ordering::Relaxed);
              }
              let unhealthy = policy.is_unhealthy(product.error_bytes, entry.size);
              if unhealthy && policy.mode == ParseHealthMode::Fail {
                note_unhealthy(&entry.path, product.error_bytes, entry.size);
              }
              // Bank the product either way; exclusion is a graph decision, not a cache one.
              pack_sink
                .send(PackMsg {
                  path: entry.path.clone(),
                  body: bytes,
                })
                .map_err(send_fatal)?;
              if unhealthy && policy.mode == ParseHealthMode::Exclude {
                excluded_files.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return Ok(StreamWork::Skipped);
              }
              return Ok(StreamWork::Replayed(entry.path.clone(), product));
            }
          }
        }
      }
      if let Some(reader) = &pack_reader {
        if let Some(bytes) = reader.get(&entry.path) {
          // Stamp peek + full view-decode validation here (cheap: no owned strings), so a
          // corrupt entry still falls through to a re-parse; the committer then decodes the
          // validated views again straight from the map and applies them — the product
          // itself never crosses the channel and never materializes.
          if peek_product_stamps(bytes) == Some((entry.size, entry.mtime_ns))
            && vorpal_ingest::peek_product_grammar_digest(bytes)
              == vorpal_ingest::extraction_identity_for_path(&entry.path, rules_digest)
            && digest_must_match(
              entry.mtime_ns,
              vorpal_ingest::peek_product_digest(bytes),
              Path::new(&entry.path),
            )
            && validate_product(bytes)
          {
            let ec = vorpal_ingest::peek_product_error_nodes(bytes).unwrap_or(0);
            let eb = vorpal_ingest::peek_product_error_bytes(bytes).unwrap_or(0);
            if ec > 0 {
              error_files.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
              error_nodes.fetch_add(ec as u64, std::sync::atomic::Ordering::Relaxed);
              error_bytes.fetch_add(eb, std::sync::atomic::Ordering::Relaxed);
            }
            let unhealthy = policy.is_unhealthy(eb, entry.size);
            if unhealthy && policy.mode == ParseHealthMode::Fail {
              note_unhealthy(&entry.path, eb, entry.size);
            }
            if unhealthy && policy.mode == ParseHealthMode::Exclude {
              // The reused pack body stays banked (live_paths keeps it); only the graph
              // contribution is dropped.
              excluded_files.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
              return Ok(StreamWork::Skipped);
            }
            return Ok(StreamWork::ReplayedPacked(entry.path.clone()));
          }
        }
      }
      // Changed, new, or cache-missing: re-parse (unreadable files are skipped, not fatal).
      // Encode-only extraction (hyperopt pass 10, re-landed post-merge): borrowed parts
      // stream straight to `.vpb` bytes — no owned product is ever materialized for a
      // fresh parse. The worker applies from a view over these bytes, then the SAME
      // buffer reaches the pack through the fresh sink above. The buffer is taken out
      // of the scratch first (read_source's borrow spans the whole extraction) and
      // restored on the skip paths so its capacity keeps recycling.
      let mut encode = pack_pool.grab();
      let Ok(source) = scratch.read_source(Path::new(&entry.path)) else {
        pack_pool.give(encode);
        return Ok(StreamWork::Skipped);
      };
      let Some(stats) = extractor.extract_product_encoded(
        &entry.path,
        source,
        entry.size,
        entry.mtime_ns,
        &mut encode,
      ) else {
        pack_pool.give(encode);
        return Ok(StreamWork::Skipped);
      };
      if stats.error_nodes > 0 {
        error_files.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        error_nodes.fetch_add(stats.error_nodes as u64, std::sync::atomic::Ordering::Relaxed);
        error_bytes.fetch_add(stats.error_bytes, std::sync::atomic::Ordering::Relaxed);
      }
      let unhealthy = policy.is_unhealthy(stats.error_bytes, entry.size);
      if unhealthy && policy.mode == ParseHealthMode::Fail {
        note_unhealthy(&entry.path, stats.error_bytes, entry.size);
      }
      if unhealthy && policy.mode == ParseHealthMode::Exclude {
        // Banked in the pack (live_paths keeps it) but absent from this build's graph —
        // ship the bytes here and skip the apply.
        excluded_files.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        pack_sink
          .send(PackMsg {
            path: entry.path.clone(),
            body: encode,
          })
          .map_err(send_fatal)?;
        return Ok(StreamWork::Skipped);
      }
      Ok(StreamWork::ParsedEncoded(entry.path.clone(), encode))
    },
  );
  drop(pack_sink);
  // The pack writer's tail (consolidation + flush of a ~0.5 GB pack at kernel scale) is
  // pure I/O nothing before the commit reads — it keeps running while cochange, the table,
  // resolution, and the seal proceed; every exit below joins it (the staged directory must
  // never outlive an abandoned build with a writer still in it).
  let (mut writer, spilled_refs, stream, mut arg_spill) = match stream_result {
    Ok(parts) => parts,
    Err(err) => {
      let _ = pack_thread.join();
      return Err(err.into());
    }
  };
  // Near-clone pairing starts HERE — its input (the sig spill) closed with the stream, and
  // it needs nothing else, so it overlaps the pack tail, cochange, the table build, AND
  // resolution instead of only the link (it was the link's critical path by ~110ms at
  // kernel scale).
  let pairing = match vorpal_ingest::spawn_sig_pairing(&mut arg_spill) {
    Ok(handle) => handle,
    Err(err) => {
      let _ = pack_thread.join();
      return Err(err.into());
    }
  };
  // Fail policy judges here — before sealing, before the generation commits: nothing is
  // published, and the message lists the worst offenders with their damage ratios.
  let offenders = unhealthy_files.into_inner().unwrap();
  if !offenders.is_empty() {
    let _ = pack_thread.join();
    let mut sorted = offenders;
    sorted.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let shown: Vec<String> = sorted
      .iter()
      .take(5)
      .map(|(path, ratio)| format!("{path} ({:.1}% error bytes)", ratio * 100.0))
      .collect();
    return Err(
      format!(
        "parse-health policy 'fail' (max error ratio {:.3}): {} file(s) unhealthy — {}{}",
        policy.max_error_ratio,
        sorted.len(),
        shown.join(", "),
        if sorted.len() > shown.len() { ", …" } else { "" }
      )
      .into(),
    );
  }
  // Hard-limit gate before sealing (IMPROVEMENTS §12): node ids, graph endpoints, and string-heap
  // offsets are all 32-bit on disk. Rather than let a corpus past those ceilings silently wrap a
  // cast and persist a corrupt index, fail loudly and actionably. The heap grows monotonically,
  // so a single check on its final size proves no intermediate offset wrapped.
  const U32_CEIL: usize = u32::MAX as usize;
  if writer.node_count() > U32_CEIL {
    let _ = pack_thread.join();
    return Err(format!(
      "index exceeds the supported node limit: {} definitions (max {U32_CEIL}); node ids are \
       32-bit — split the corpus into multiple indexes",
      writer.node_count()
    )
    .into());
  }
  if writer.heap_len() > U32_CEIL as u64 {
    let _ = pack_thread.join();
    return Err(format!(
      "index string heap exceeds the 4 GiB addressable limit: {} bytes (max {U32_CEIL}); \
       names/paths/signatures use 32-bit offsets — split the corpus into multiple indexes",
      writer.heap_len()
    )
    .into());
  }
  // Extraction/commit scratch (products in transit, per-worker buffers, shard canonicals)
  // is dead now — hand its pages back before the link pass allocates the table and edges.
  vorpal_ingest::release_freed_pages();
  let (reparsed, replayed) = (stream.parsed, stream.replayed);

  // Temporal coupling (ADOPTION #27): files that changed together in recent git history
  // gain symmetric `changes_with` edges between their File nodes — derived from `git log`,
  // bounded, and stated on the report when nothing could be computed.
  vorpal_kg::phase_stamp("cochange: start");
  let cochange = cochange::finish(
    cochange_pending,
    src,
    manifest.entries().iter().map(|e| e.path.as_str()),
  );
  let mut cochange_edges = 0u64;
  if !cochange.edges.is_empty() {
    // File-node lookup by an on-demand scan of the writer's rows — per-file identity keys
    // are shed as files land (`forget_identity_scope`), and a resident path map would sit
    // in memory through the whole stream phase to answer this once. Only the paths the
    // pass actually relates are collected.
    let wanted: HashSet<&str> = cochange
      .edges
      .iter()
      .flat_map(|e| [e.a.as_str(), e.b.as_str()])
      .collect();
    let mut file_ids: HashMap<&str, NodeId> = HashMap::with_capacity(wanted.len());
    writer.for_each_file(|id, path| {
      if let Some(&key) = wanted.get(path) {
        file_ids.insert(key, id);
      }
    });
    for edge in &cochange.edges {
      if let (Some(&a), Some(&b)) = (file_ids.get(edge.a.as_str()), file_ids.get(edge.b.as_str())) {
        let label = vorpal_kg::EdgeType::CHANGES_WITH.with_confidence(edge.confidence);
        writer.add_edge(a, b, label);
        writer.add_edge(b, a, label);
        cochange_edges += 1;
      }
    }
  }
  let cochange_note = cochange.note;
  vorpal_kg::phase_stamp("cochange: done");

  // Loose-file hygiene: everything snapshotted above is now consolidated in the pack (or
  // superseded by a re-parse, or stale) — delete it. Files banked by searches *during* this
  // run are not in the snapshot and survive untouched.
  for name in &loose {
    let _ = fs::remove_file(products_dir.join(name));
  }

  // Full re-link from the complete product set: identity, resolution, and edges are recomputed
  // from scratch, so stale state is structurally impossible; resolution links the merged
  // graph over the sharded table/resolve passes.
  let linked = vorpal_ingest::link_writer_spilled_with_flows(
    &interner,
    writer,
    spilled_refs,
    &Resolver::new(),
    Some(arg_spill),
    pairing,
  );
  let (kg, resolve, evidence, flows, similar, request_report, sig_rows, reach_graph) =
    match linked {
    Ok(parts) => parts,
    Err(err) => {
      let _ = pack_thread.join();
      return Err(err.into());
    }
  };
  let report = IndexReport {
    reused: false,
    graph_reused: false,
    cache_mode: cache_mode.label(),
    indexed: reparsed,
    skipped: replayed,
    error_files: error_files.into_inner(),
    error_nodes: error_nodes.into_inner(),
    error_bytes: error_bytes.into_inner(),
    excluded_files: excluded_files.into_inner(),
    nodes: kg.node_count(),
    resolved: resolve.resolved,
    ambiguous: resolve.ambiguous,
    external: resolve.external,
    masked: resolve.masked,
    unverified_langs,
    cochange_edges,
    cochange_note,
    similar_edges: similar.edges,
    similar_note: similar.note,
    request_sites: request_report.sites,
    request_edges: request_report.edges,
    request_note: request_report.note,
  };
  // The node-store layout this generation persists under — same format decision as the
  // pack, with the bucket law's one input (live file count) pinned from the manifest.
  let layout = match format {
    vorpal_ingest::PackFormat::Flat => vorpal_kg::SegmentLayout::Flat,
    vorpal_ingest::PackFormat::Bucketed => vorpal_kg::SegmentLayout::Bucketed {
      tree_root: tree_root.clone(),
      prior: Some(prior.clone()),
      live_files: manifest.entries().len(),
    },
  };
  if let Some(slots) = live {
    // Deferred persistence: the sealed graph is answer-complete NOW — hand it to the daemon
    // and move the artifact writes + content-addressed commit onto its background thread.
    // `PendingPersist::persist` is the exact tail below, so the committed generation is
    // byte-identical either way.
    let mut kg = kg;
    // A served-from-RAM graph needs the in-memory name index: without it, every named query
    // pays the full parallel scan the persisted names.idx exists to avoid.
    kg.build_names_index();
    let kg = Arc::new(kg);
    slots.kg = Some(kg.clone());
    slots.pending = Some(PendingPersist {
      out: out.to_path_buf(),
      prior,
      staging,
      evidence,
      flows,
      sigs: sig_rows,
      manifest,
      reach_graph,
      kg,
      layout,
      pack: Some(pack_thread),
    });
    return Ok(report);
  }
  // Synchronous tail: the pack must be complete before the manifest (the commit point) and
  // the content hash — join it here, after everything it overlapped.
  pack_thread
    .join()
    .expect("pack writer panicked")?;
  // Persist the evidence sidecar (§5) and the graph segments CONCURRENTLY: they are
  // independent artifacts in the same staged generation, and running them serially left
  // 17 cores idle for the longer of the two. Evidence is canonically sorted (total order)
  // inside its saver, so it still joins the content identity deterministically. The
  // manifest stays strictly last — it is the commit point.
  let evidence_bases = kg.node_id_map(&layout)?;
  let (evidence_result, dataflow_result, sigs_result, reach_result, kg_result) = std::thread::scope(|scope| {
    let evidence_task = scope.spawn(|| {
      let evidence_layout = match &evidence_bases {
        None => vorpal_kg::EvidenceLayout::Flat,
        Some(map) => vorpal_kg::EvidenceLayout::Bucketed {
          nodes: map,
          prior: Some(&prior),
        },
      };
      vorpal_kg::save_evidence_with(&staging, evidence, &evidence_layout)
    });
    let dataflow_task = scope.spawn(|| vorpal_kg::save_dataflow(&staging, flows));
    let sigs_task = scope.spawn(|| save_sig_family(&staging, &sig_rows, &evidence_bases, &prior));
    // The include-edge graph scoped composes replay the reach oracle from — a
    // single canonical artifact beside dataflow.bin (see resolve::reach).
    let reach_result = fs::write(staging.join("reach.bin"), &reach_graph);
    drop(reach_graph);
    let kg_result = kg.save_with(&staging, &layout);
    (
      evidence_task.join().map_err(|_| io::Error::other("evidence saver panicked")),
      dataflow_task.join().map_err(|_| io::Error::other("dataflow saver panicked")),
      sigs_task.join().map_err(|_| io::Error::other("sigs saver panicked")),
      reach_result,
      kg_result,
    )
  });
  evidence_result??;
  dataflow_result??;
  sigs_result??;
  reach_result?;
  kg_result?;
  manifest.save(&staging.join("manifest.bin"))?;
  // Commit: name the staged generation by its content, atomically repoint CURRENT, GC.
  commit_generation(out, &prior, staging)?;
  Ok(report)
}

/// The core artifact set a generation is named by — the complete persisted index, in fixed
/// (sorted) order. Lazy sidecars added after commit (the ANN tier) are deliberately excluded:
/// they are stamp-validated against the node segment, deterministic given the generation, and
/// must not change its identity.
pub(crate) const GENERATION_ARTIFACTS: [&str; 10] = [
  "dataflow.bin",
  "reach.bin",
  "evidence.bin",
  "graph.bin",
  "manifest.bin",
  "names.idx",
  "nodes.vseg",
  "products.idx",
  "products.pack",
  "strings.heap",
];

/// Whether `name` is a legal generation-artifact name: the fixed flat set, a bucketed
/// pack member (P4.1), or a bucketed node-store member (P4.2). The import allowlist and
/// every sweep share this single predicate.
pub(crate) fn is_generation_artifact_name(name: &str) -> bool {
  GENERATION_ARTIFACTS.contains(&name)
    || vorpal_ingest::is_pack_member(name)
    || vorpal_kg::is_nodes_member(name)
    || vorpal_kg::is_evidence_member(name)
    || vorpal_kg::is_edges_member(name)
    || vorpal_kg::is_usage_member(name)
    || vorpal_kg::is_sigs_member(name)
}

/// Every artifact name this generation actually carries, in fixed order: the flat list
/// (missing members skipped by callers that open them), then each bucketed family's
/// members sorted by name (`products/…`, then `nodes/…`). The bucketed families have
/// corpus-dependent file counts, so identity and staging walk THIS, never the const alone.
pub(crate) fn generation_artifact_names(dir: &Path) -> Vec<String> {
  // Bucketed generations (P4.2/P4.3) carry two DERIVED caches the identity must not fold:
  // graph.bin (rebuilt from the edge slabs on any stamp mismatch) and names.idx (queries
  // fall back to the scan). Flat generations keep both as truth.
  let bucketed = dir.join(vorpal_kg::NODES_TOC).is_file();
  let mut names: Vec<String> = GENERATION_ARTIFACTS
    .iter()
    .filter(|name| !(bucketed && (**name == "graph.bin" || **name == "names.idx")))
    .map(|s| s.to_string())
    .collect();
  for family in [
    vorpal_ingest::PACK_DIR,
    vorpal_kg::NODES_DIR,
    vorpal_kg::EVIDENCE_DIR,
    vorpal_kg::EDGES_DIR,
    vorpal_kg::USAGE_DIR,
    vorpal_kg::SIGS_DIR,
  ] {
    if let Ok(dirents) = fs::read_dir(dir.join(family)) {
      let mut members: Vec<String> = dirents
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .map(|file| format!("{family}/{file}"))
        .filter(|name| is_generation_artifact_name(name))
        .collect();
      members.sort_unstable();
      names.extend(members);
    }
  }
  names
}

/// Commit a staged generation (IMPROVEMENTS §4): name it by its **content**, atomically swap
/// the `CURRENT` pointer, and garbage-collect superseded generations.
///
/// Content-addressing is what preserves the determinism contract under the generation layout:
/// the id is a pure function of the artifact bytes, so two from-scratch builds of the same tree
/// commit the same `gen/<id>`, and an incremental rebuild converges to the byte-identical
/// generation a from-scratch build of the final tree produces — directory names included. A
/// staged generation whose id already exists is byte-identical to the committed one, so staging
/// is simply dropped and `CURRENT` repointed.
///
/// GC keeps two generations — the new one and the one `CURRENT` pointed at when this build
/// began — so a reader that resolved just before the swap keeps a complete index on disk.
/// Readers that hold an even older generation keep their mmaps (POSIX unlink semantics); only
/// *new* opens in a collected generation fail, as a clean retryable error. A legacy flat root
/// that served as the prior is swept the same way: its artifacts are superseded by the
/// generation just committed from them.
/// The content id of a generation directory: the deterministic fold over every artifact (in
/// fixed order) that names `gen/<id>` dirs. Pure function of the artifact bytes; the folding
/// shape is an internal detail of this binary version (ids are content addresses, not a
/// cross-version interchange format — see docs on the shareable-artifact import for how that
/// is handled honestly).
pub(crate) fn generation_content_id(dir: &Path) -> io::Result<String> {
  generation_content_id_folded(dir, HASH_CHUNK)
}

/// The content-id fold's chunk size. A **fold-protocol constant**, not a tuning knob: chunk
/// boundaries shape the per-chunk digests the id folds, so the value must be identical on
/// every machine (a runtime-derived size would make generation ids machine-dependent) —
/// changing it re-keys every generation id, which the dedup guard absorbs as one re-commit
/// per tree. 1 MiB sits mid-plateau in the recorded two-scale sweep (docs/wip/SUBSECOND.md
/// §P4.1, `content_id_sweep` example — linux-kernel and vorpal-repo generations, flat and
/// bucketed): 256 KiB–1 MiB tie at the optimum on every shape, and the 8 MiB this replaced
/// measured ~47% slower (kernel flat 24.0 ms → 16.3 ms).
const HASH_CHUNK: u64 = 1 << 20;

/// [`generation_content_id`] with an explicit chunk size — the sweep tool's entry point
/// (`examples/content_id_sweep.rs`). Not an API: ids from non-default chunk sizes name
/// nothing.
#[doc(hidden)]
pub fn generation_content_id_folded(dir: &Path, chunk: u64) -> io::Result<String> {
  // P4.4 — the MERKLE commit: for bucketed generations the fold's input is the small
  // fixed set {manifest.bin, dataflow.bin, each family's toc.bin} — the TOCs already pin
  // every member's digest, so the id covers the same bytes transitively while the commit
  // reads ~13 MB instead of ~1.5 GB at kernel scale. Dedup-guard soundness is preserved
  // by construction: writers compute TOCs FROM member bytes, so equal TOCs from
  // pipeline-produced directories mean equal members. (Adversarial member tampering
  // without its TOC is caught where trust is needed — import verifies every exported
  // artifact's raw digest, and loaders verify lengths and self-checks.)
  if dir.join(vorpal_kg::NODES_TOC).is_file() {
    return content_id_fold(dir, merkle_artifact_names(), chunk);
  }
  content_id_fold(dir, generation_artifact_names(dir), chunk)
}

/// The Merkle fold's fixed input set for bucketed generations, in sorted order.
fn merkle_artifact_names() -> Vec<String> {
  [
    "dataflow.bin",
    "edges/toc.bin",
    "evidence/toc.bin",
    "manifest.bin",
    "nodes/toc.bin",
    "products/toc.bin",
    "reach.bin",
    "sigs/toc.bin",
    "usage/toc.bin",
  ]
  .into_iter()
  .map(str::to_string)
  .collect()
}

/// The pre-Merkle full-rehash fold over every artifact byte — the agreement oracle's
/// reference (`tests/pack_v2.rs`): two generations agree under the Merkle id exactly when
/// they agree under this.
#[doc(hidden)]
pub fn generation_content_id_full(dir: &Path) -> io::Result<String> {
  content_id_fold(dir, generation_artifact_names(dir), HASH_CHUNK)
}

fn content_id_fold(dir: &Path, names: Vec<String>, chunk: u64) -> io::Result<String> {
  use rayon::prelude::*;
  let hash_chunk = chunk.max(1);
  // Two axes of parallelism, one deterministic fold. The flat layout is one ~gigabyte pack
  // — the win is chunk-parallel hashing WITHIN the file; the bucketed layout (P4.1) is ~10³
  // megabyte-scale files — the win is hashing ACROSS them. Both run under one nested
  // par_iter (rayon work-steals whichever axis has work), and the serial fold below
  // consumes per-artifact digests in fixed name order, so the id is byte-for-byte the same
  // fold as the sequential form — parallelism is a schedule, never a semantics.
  // (artifact length, ordered per-chunk digests) — `None` for an absent artifact.
  type ArtifactDigest = Option<(u64, Vec<u128>)>;
  let digests: io::Result<Vec<ArtifactDigest>> = names
    .par_iter()
    .map(|artifact| {
      let path = dir.join(artifact);
      let Ok(file) = fs::File::open(&path) else {
        // An artifact a smaller index legitimately lacks still yields a stable id.
        return Ok(None);
      };
      let len = file.metadata()?.len();
      let chunk_count = len.div_ceil(hash_chunk);
      // Positional reads per chunk: threads never share a cursor, and in-flight memory
      // stays at (pool width × 8 MiB) — the whole-artifact read would have re-materialized
      // a ~gigabyte pack this build just spent effort never holding at once. Unix pread's
      // Windows sibling moves the handle's file pointer, so non-unix opens a fresh handle
      // per chunk instead (identical bytes, no shared-cursor races).
      #[cfg(unix)]
      let read_chunk = |buf: &mut [u8], offset: u64| -> io::Result<()> {
        use std::os::unix::fs::FileExt;
        file.read_exact_at(buf, offset)
      };
      #[cfg(not(unix))]
      let read_chunk = |buf: &mut [u8], offset: u64| -> io::Result<()> {
        use std::io::{Read, Seek, SeekFrom};
        let mut chunk_file = fs::File::open(&path)?;
        chunk_file.seek(SeekFrom::Start(offset))?;
        chunk_file.read_exact(buf)
      };
      let chunks: io::Result<Vec<u128>> = (0..chunk_count as usize)
        .into_par_iter()
        // Without a split floor, rayon's adaptive splitting reaches single-chunk
        // jobs under work-stealing and `map_init` runs its init per job — the
        // reused buffer degenerated back to one allocation per chunk (measured:
        // the 3.9 GB stood). Floor the split length the same way shards derive.
        // (usize indices: an artifact past usize chunks is unindexable long
        // before this point on any supported target.)
        .with_min_len(
          (chunk_count as usize)
            .div_ceil(rayon::current_num_threads().max(1) * 2)
            .max(1),
        )
        .map_init(
          // One reusable buffer per worker split: the per-chunk `vec![0; chunk]`
          // this replaces churned ~4 GB of allocations through the commit
          // hash at kernel scale (ledger-attributed) for identical digests.
          Vec::<u8>::new,
          |buf, index| {
            let offset = index as u64 * hash_chunk;
            let want = hash_chunk.min(len - offset) as usize;
            buf.clear();
            buf.resize(want, 0);
            read_chunk(buf, offset)?;
            Ok(xxhash_rust::xxh3::xxh3_128(buf))
          },
        )
        .collect();
      Ok(Some((len, chunks?)))
    })
    .collect();
  let mut hasher = xxhash_rust::xxh3::Xxh3::new();
  for (artifact, entry) in names.iter().zip(digests?) {
    let Some((len, chunks)) = entry else {
      continue;
    };
    hasher.update(artifact.as_bytes());
    hasher.update(&len.to_le_bytes());
    for digest in chunks {
      hasher.update(&digest.to_le_bytes());
    }
  }
  Ok(format!("{:032x}", hasher.digest128()))
}

/// The stamp-only commit cutoff (Phase 1a). Returns `Ok(Some(report))` after committing a
/// new generation whose graph artifacts are carried forward byte-identically, `Ok(None)` to
/// fall through to the full pipeline. Never guesses: every changed file's fresh extraction
/// must match its cached product byte-for-byte outside the stamp window `[8..32)`
/// (source size, mtime, content digest — magic/version before it and the grammar digest and
/// entire extraction body after it must be equal), and adds/removes/loose-bank products all
/// disqualify.
/// The manifest-side context [`try_stamp_only_cutoff`] judges a build against: the fresh
/// and prior manifests, the prior's write clock (racy-mtime window), and the canonical
/// tree root (the pack's absolute→relative conversion point).
#[derive(Clone, Copy)]
pub(crate) struct CutoffContext<'a> {
  pub(crate) manifest: &'a Manifest,
  pub(crate) prior_manifest: &'a Manifest,
  pub(crate) prior_manifest_ns: u64,
  pub(crate) tree_root: &'a str,
}

fn try_stamp_only_cutoff(
  out: &Path,
  prior: &Path,
  ctx: &CutoffContext<'_>,
  extractor: &OutlineExtractor,
  cache_mode_label: &'static str,
) -> io::Result<Option<IndexReport>> {
  let CutoffContext {
    manifest,
    prior_manifest,
    prior_manifest_ns,
    tree_root,
  } = *ctx;
  /// Above this many changed files the full pipeline is competitive and the cutoff's
  /// serial re-extraction is not — a policy bound, not a correctness one.
  const MAX_RESTAMPED: usize = 64;
  // Path-sorted two-pointer diff: any add or remove disqualifies (the pipeline would change
  // node ids and the pack's record set).
  let current = manifest.entries();
  let previous = prior_manifest.entries();
  let (mut i, mut j) = (0usize, 0usize);
  let mut changed: Vec<&vorpal_ingest::FileStat> = Vec::new();
  while i < current.len() && j < previous.len() {
    match current[i].path.cmp(&previous[j].path) {
      std::cmp::Ordering::Less | std::cmp::Ordering::Greater => return Ok(None),
      std::cmp::Ordering::Equal => {
        if current[i].size != previous[j].size || current[i].mtime_ns != previous[j].mtime_ns {
          changed.push(&current[i]);
          if changed.len() > MAX_RESTAMPED {
            return Ok(None);
          }
        }
        i += 1;
        j += 1;
      }
    }
  }
  if i < current.len() || j < previous.len() || changed.is_empty() {
    return Ok(None);
  }
  // The racy-mtime hazard applies to files this run will TRUST WITHOUT READING — the
  // stat-unchanged ones. (Changed files are re-extracted and byte-compared below, a
  // strictly stronger check than the digest probe.) A stat-invisible edit inside the racy
  // window among the unchanged set disqualifies the cutoff, exactly as it disqualifies the
  // whole-tree fast path.
  {
    let changed_paths: std::collections::HashSet<&str> =
      changed.iter().map(|e| e.path.as_str()).collect();
    let racy_unchanged: Vec<&vorpal_ingest::FileStat> = manifest
      .entries()
      .iter()
      .filter(|e| {
        e.mtime_ns.abs_diff(prior_manifest_ns) < 2_000_000_000
          && !changed_paths.contains(e.path.as_str())
      })
      .collect();
    if !racy_unchanged.is_empty() {
      let Some(pack) = PackReader::open_rooted(prior, Some(tree_root)) else {
        return Ok(None);
      };
      let all_match = racy_unchanged.iter().all(|entry| {
        let stored = pack
          .get(&entry.path)
          .and_then(vorpal_ingest::peek_product_digest);
        match (stored, fs::read(&entry.path)) {
          (Some(digest), Ok(bytes)) => xxhash_rust::xxh3::xxh3_64(&bytes) == digest,
          _ => false,
        }
      });
      if !all_match {
        return Ok(None);
      }
    }
  }
  // The full pipeline would consolidate loose bank products into the pack — a non-empty bank
  // means the carried-forward pack would diverge.
  if let Ok(mut bank) = fs::read_dir(out.join("products"))
    && bank.next().is_some()
  {
    return Ok(None);
  }
  for artifact in ["dataflow.bin", "reach.bin", "manifest.bin"] {
    if !prior.join(artifact).exists() {
      return Ok(None);
    }
  }
  // The node store, evidence, and edges are format-dependent families: flat files, or
  // bucketed members (a family's toc present ⇒ every member it names is verified by its
  // loader; the cutoff links whole families verbatim — stamps never live in any of them).
  // graph.bin/names.idx are TRUTH for flat generations (required) and derived CACHES for
  // bucketed ones (linked when present, never required).
  let nodes_bucketed = prior.join(vorpal_kg::NODES_TOC).is_file();
  if nodes_bucketed {
    if !prior.join(vorpal_kg::EVIDENCE_TOC).is_file()
      || !prior.join(vorpal_kg::EDGES_TOC).is_file()
      || !prior.join(vorpal_kg::USAGE_TOC).is_file()
      || !prior.join(vorpal_kg::SIGS_TOC).is_file()
    {
      return Ok(None);
    }
  } else if !prior.join("nodes.vseg").exists()
    || !prior.join("strings.heap").exists()
    || !prior.join("graph.bin").exists()
    || !prior.join("evidence.bin").exists()
    || !prior.join("names.idx").exists()
  {
    return Ok(None);
  }
  let Some(pack) = PackReader::open_rooted(prior, Some(tree_root)) else {
    return Ok(None);
  };
  // Pack members are layout-specific. Flat: the staging below clones the one pack file and
  // hard-links its sidecar, so both must exist. Bucketed: members come from the TOC — and a
  // recovery-scanned pack (no trusted TOC) cannot prove per-bucket byte identity, so it
  // falls to the full pipeline, which republishes a consistent TOC.
  if pack.is_bucketed() {
    if pack.bucket_meta().is_none() {
      return Ok(None);
    }
  } else if !prior.join("products.pack").exists() || !prior.join("products.idx").exists() {
    return Ok(None);
  }
  // Re-extract each changed file and compare against its cached product. The stamp window
  // [8..32) (size u64, mtime u64, source-xxh3 u64 at fixed offsets after magic+version) is
  // the only region allowed to differ; it is patched into the pack clone below so the pack
  // equals what the pipeline would have written (fresh stamps, identical body).
  let mut patches: Vec<(u32, u64, [u8; 24])> = Vec::new();
  let mut encode_buf: Vec<u8> = Vec::new();
  for entry in &changed {
    let Ok(source) = fs::read_to_string(&entry.path) else {
      return Ok(None);
    };
    let Some(mut product) = extractor.extract_product(&entry.path, &source) else {
      return Ok(None);
    };
    product.source_size = entry.size;
    product.source_mtime_ns = entry.mtime_ns;
    encode_buf.clear();
    vorpal_ingest::encode_product_into(&product, &mut encode_buf);
    let Some(cached) = pack.get(&entry.path) else {
      return Ok(None);
    };
    if encode_buf.len() != cached.len()
      || encode_buf.len() < 32
      || encode_buf[0..8] != cached[0..8]
      || encode_buf[32..] != cached[32..]
    {
      return Ok(None);
    }
    // (The daemon's `extraction_unchanged` probe shares this exact window contract via
    // `extraction_matches_cache`; the cutoff additionally needs the fresh stamps below.)
    let Some((bucket, body_off, _)) = pack.body_locus(&entry.path) else {
      return Ok(None);
    };
    let stamp: [u8; 24] = encode_buf[8..32].try_into().expect("stamp window");
    patches.push((bucket, body_off + 8, stamp));
  }
  let bucketed = pack.is_bucketed();
  let bucket_total = pack.loaded_buckets();
  let prior_meta: Option<Vec<vorpal_ingest::BucketMeta>> = pack.bucket_meta().map(<[_]>::to_vec);
  drop(pack);

  // Stage the generation: hardlink (copy fallback) the byte-identical artifacts, clone the
  // pack and patch the stamp windows in place (fs::copy clones on reflink-capable
  // filesystems — APFS/btrfs/XFS — and degrades to a plain copy elsewhere; the patch then
  // touches only the affected blocks), and write the fresh manifest. commit_generation
  // provides the same atomic CURRENT swap, dedup guard, GC, and ANN carry-forward as the
  // full pipeline — and because nodes.vseg is unchanged, the carried ANN tier's stamp still
  // matches: the vector tier stays warm through the cutoff.
  let staging = out.join(format!(
    ".staging-{}-{}",
    std::process::id(),
    staging_nonce()
  ));
  let _ = fs::remove_dir_all(&staging);
  fs::create_dir_all(&staging)?;
  // Required flat carries; under the bucketed format graph.bin/names.idx are caches —
  // linked when present via the optional list below, never required.
  let link_required: &[&str] = if nodes_bucketed {
    &["dataflow.bin", "reach.bin"]
  } else {
    &[
      "graph.bin",
      "evidence.bin",
      "dataflow.bin",
      "reach.bin",
      "names.idx",
      "nodes.vseg",
      "strings.heap",
    ]
  };
  for artifact in link_required {
    let (from, to) = (prior.join(artifact), staging.join(artifact));
    if fs::hard_link(&from, &to).is_err() {
      fs::copy(&from, &to)?;
    }
  }
  if nodes_bucketed {
    for artifact in ["graph.bin", "graph.stamp", "names.idx"] {
      let (from, to) = (prior.join(artifact), staging.join(artifact));
      if from.exists() && fs::hard_link(&from, &to).is_err() {
        fs::copy(&from, &to)?;
      }
    }
  }
  // Carry the bucketed families byte-identically (hard-links — the cutoff never
  // patches their bytes; stamps live in the manifest and the pack). The carries run
  // CONCURRENTLY: each destination directory's inode lock serializes its own links
  // (~0.27 ms/entry on APFS), but the directories lock independently, so the fan-out
  // costs roughly one family's worth of wall. The bucketed pack rides in the same
  // batch; its patched buckets are replaced with private copies below.
  let mut carry_list: Vec<(&str, vorpal_kg::FamilyMemberFn)> = Vec::with_capacity(6);
  if nodes_bucketed {
    carry_list.extend([
      (vorpal_kg::NODES_DIR, vorpal_kg::is_nodes_member as fn(&str) -> bool),
      (vorpal_kg::EVIDENCE_DIR, vorpal_kg::is_evidence_member),
      (vorpal_kg::EDGES_DIR, vorpal_kg::is_edges_member),
      (vorpal_kg::USAGE_DIR, vorpal_kg::is_usage_member),
      (vorpal_kg::SIGS_DIR, vorpal_kg::is_sigs_member),
    ]);
  }
  if bucketed {
    carry_list.push((vorpal_ingest::PACK_DIR, vorpal_ingest::is_pack_member));
  }
  if !carry_list.is_empty() {
    vorpal_kg::carry_families(prior, &staging, &carry_list)?;
  }
  if bucketed {
    // Bucketed layout: untouched buckets hard-link (zero bytes moved); buckets holding a
    // restamped product COPY-then-patch — never patch through a link, the prior
    // generation's bytes are sealed. The TOC digest column is recomputed for exactly the
    // patched buckets and spliced into the carried TOC.
    use std::io::{Seek, SeekFrom, Write};
    let Some(mut meta) = prior_meta else {
      return Ok(None); // unreachable: gated above, kept as a fact not an assumption
    };
    let mut by_bucket: std::collections::HashMap<u32, Vec<(u64, [u8; 24])>> =
      std::collections::HashMap::new();
    for (bucket, offset, stamp) in patches {
      by_bucket.entry(bucket).or_default().push((offset, stamp));
    }
    // The whole pack was link-carried in the parallel family batch above (the bucketed
    // branch); a patched bucket must never be written through its link — the prior
    // generation's bytes are sealed — so it is replaced with a private copy first.
    for k in 0..bucket_total {
      let name = vorpal_ingest::bucket_file_name(k);
      let (from, to) = (prior.join(&name), staging.join(&name));
      let Some(bucket_patches) = by_bucket.get(&k) else {
        continue; // link-carried by the parallel family batch
      };
      let _ = fs::remove_file(&to);
      fs::copy(&from, &to)?;
      let mut pack_file = fs::OpenOptions::new().write(true).open(&to)?;
      for (offset, stamp) in bucket_patches {
        pack_file.seek(SeekFrom::Start(*offset))?;
        pack_file.write_all(stamp)?;
      }
      pack_file.sync_all()?;
      let Some(row) = meta.get_mut(k as usize) else {
        return Ok(None);
      };
      row.digest = xxhash_rust::xxh3::xxh3_64(&fs::read(&to)?);
    }
    let mut toc = fs::read(prior.join(vorpal_ingest::PACK_TOC))?;
    for &k in by_bucket.keys() {
      let Some(row) = meta.get(k as usize) else {
        return Ok(None);
      };
      if !vorpal_ingest::splice_toc_digest(&mut toc, k, row.digest) {
        return Ok(None);
      }
    }
    // The carried TOC entry is REPLACED, never written through (a truncate through the
    // link would destroy the prior generation's TOC).
    let toc_path = staging.join(vorpal_ingest::PACK_TOC);
    let _ = fs::remove_file(&toc_path);
    fs::write(&toc_path, &toc)?;
  } else {
    let (from, to) = (prior.join("products.idx"), staging.join("products.idx"));
    if fs::hard_link(&from, &to).is_err() {
      fs::copy(&from, &to)?;
    }
    fs::copy(prior.join("products.pack"), staging.join("products.pack"))?;
    use std::io::{Seek, SeekFrom, Write};
    let mut pack_file = fs::OpenOptions::new()
      .write(true)
      .open(staging.join("products.pack"))?;
    for (_, offset, stamp) in &patches {
      pack_file.seek(SeekFrom::Start(*offset))?;
      pack_file.write_all(stamp)?;
    }
    pack_file.sync_all()?;
  }
  manifest.save(&staging.join("manifest.bin"))?;
  commit_generation(out, prior, staging)?;
  let nodes = Kg::peek_node_count(&vorpal_kg::resolve_index_dir(out)).unwrap_or(0);
  Ok(Some(IndexReport {
    // A stamp-only commit re-extracted the changed files and committed a NEW generation —
    // `reused` ("unchanged, nothing re-parsed") would misreport both. The GRAPH, however,
    // is a proven byte-identical carry.
    reused: false,
    graph_reused: true,
    cache_mode: cache_mode_label,
    error_files: 0,
    error_nodes: 0,
    error_bytes: 0,
    excluded_files: 0,
    indexed: changed.len() as u64,
    skipped: manifest.len() as u64 - changed.len() as u64,
    nodes,
    resolved: 0,
    ambiguous: 0,
    external: 0,
    masked: 0,
    // The stamp-only commit carries every graph artifact forward byte-identically —
    // cochange/similarity/request edges from the prior generation persist unchanged; the
    // passes themselves did not run this build. Stated, never a silent zero.
    unverified_langs: Vec::new(),
    cochange_edges: 0,
    cochange_note: Some(STAMP_ONLY_CARRY_NOTE.to_string()),
    similar_edges: 0,
    similar_note: Some(STAMP_ONLY_CARRY_NOTE.to_string()),
    request_sites: 0,
    request_edges: 0,
    request_note: Some(STAMP_ONLY_CARRY_NOTE.to_string()),
  }))
}

/// Report note for edge passes on the stamp-only path: the edges live in the carried
/// artifacts; this build did not recompute them.
const STAMP_ONLY_CARRY_NOTE: &str =
  "carried forward from prior generation (stamp-only commit)";

/// Process-unique staging suffix: concurrent builds in ONE process (the daemon's background
/// canonicalizer racing a synchronous rebuild) must never share a staging directory.
pub(crate) fn staging_nonce() -> u64 {
  static NONCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
  NONCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

pub(crate) fn commit_generation(root: &Path, prior: &Path, staging: PathBuf) -> io::Result<String> {
  // Sweep staging scratch that is not part of the named artifact set (spill files, tmp names)
  // so the generation holds exactly its artifacts.
  for entry in fs::read_dir(&staging)?.flatten() {
    let name = entry.file_name();
    // The bucketed families (pack, node store) are the artifacts that are directories;
    // their members are swept by the predicate below.
    let keep = name.as_os_str() == vorpal_ingest::PACK_DIR
      || name.as_os_str() == vorpal_kg::NODES_DIR
      || name.as_os_str() == vorpal_kg::EVIDENCE_DIR
      || name.as_os_str() == vorpal_kg::EDGES_DIR
      || name.as_os_str() == vorpal_kg::USAGE_DIR
      || name.as_os_str() == vorpal_kg::SIGS_DIR
      || name.as_os_str() == "graph.stamp"
      || GENERATION_ARTIFACTS
        .iter()
        .any(|artifact| name.as_os_str() == *artifact);
    if !keep {
      let path = entry.path();
      let _ = if path.is_dir() {
        fs::remove_dir_all(&path)
      } else {
        fs::remove_file(&path)
      };
    }
  }
  for family in [
    vorpal_ingest::PACK_DIR,
    vorpal_kg::NODES_DIR,
    vorpal_kg::EVIDENCE_DIR,
    vorpal_kg::EDGES_DIR,
    vorpal_kg::USAGE_DIR,
    vorpal_kg::SIGS_DIR,
  ] {
    if let Ok(dirents) = fs::read_dir(staging.join(family)) {
      for entry in dirents.flatten() {
        let member = entry
          .file_name()
          .into_string()
          .map(|file| format!("{family}/{file}"));
        if !member.as_deref().is_ok_and(is_generation_artifact_name) {
          let _ = fs::remove_file(entry.path());
        }
      }
    }
  }
  // Content id over every artifact in fixed order. Chunked-parallel: each artifact is read
  // and hashed in 8 MiB chunks fanned across the pool, and the id folds the per-chunk
  // digests (with the artifact name and length) in deterministic order. The previous
  // single-stream hash serialized ~2 GB of read+hash at kernel scale into one thread at the
  // very end of the build. The id remains a pure function of the artifact bytes — the
  // folding shape is an internal detail of this binary version (ids are content addresses,
  // not a cross-version interchange format; see docs/INDEX_FORMAT.md).
  vorpal_kg::phase_stamp("commit: content-id hash start");
  let id = generation_content_id(&staging)?;
  vorpal_kg::phase_stamp("commit: content-id hash done");
  let final_dir = root.join("gen").join(&id);
  // Dedup guard: an existing same-id generation is byte-identical *by construction*, but only
  // if it is still complete — a tampered or partially-deleted dir must not be trusted over the
  // freshly staged one. Same-id ⇒ same artifact set, so presence-checking staging's artifacts
  // against it is a sufficient completeness test.
  let existing_is_complete = final_dir.exists()
    && generation_artifact_names(&staging)
      .iter()
      .all(|artifact| !staging.join(artifact).exists() || final_dir.join(artifact).exists());
  if existing_is_complete {
    fs::remove_dir_all(&staging)?;
  } else {
    if final_dir.exists() {
      fs::remove_dir_all(&final_dir)?;
    }
    fs::rename(&staging, &final_dir)?;
  }

  // Carry the prior generation's lazy ANN tier forward (hardlink; copy as fallback) so an
  // incremental rebuild keeps its overlay fast path: the carried tier's base stamp no longer
  // matches the new node segment, which is exactly the condition the search-side overlay
  // reconciles through the `ann.files` digest map — without the carry, every post-edit search
  // would pay the exhaustive fallback until a full re-warm. Hardlinks are safe because ANN
  // artifacts are only ever *replaced* (tmp + rename), never mutated in place: a re-warm in
  // this generation unlinks its name without touching the prior generation's inode. Never
  // overwrites — a generation that already warmed its own tier keeps it. The model
  // provenance travels with the tier it describes: without it the carried bin reads as
  // "unverifiable model" and every provenance-gated consumer (freshness, live-tier
  // adoption) rejects a tier that is in fact reconcilable.
  if *prior != final_dir {
    for ann_file in [
      "ann.bin",
      "ann.files",
      "ann.model.bin",
      "ann.model.json",
      "ann.calibration.json",
      "ann.stamp",
    ] {
      let from = prior.join(ann_file);
      let to = final_dir.join(ann_file);
      if from.exists() && !to.exists() && fs::hard_link(&from, &to).is_err() {
        let _ = fs::copy(&from, &to);
      }
    }
  }

  // Atomic pointer swap: readers resolve CURRENT in one read; tmp + rename means they see the
  // old pointer or the new one, never a torn write. The tmp is synced so a crash straddling
  // the rename cannot publish a pointer whose bytes never reached disk.
  let pointer = format!("gen/{id}\n");
  let current_tmp = root.join("CURRENT.tmp");
  {
    let mut file = fs::File::create(&current_tmp)?;
    io::Write::write_all(&mut file, pointer.as_bytes())?;
    file.sync_all()?;
  }
  fs::rename(&current_tmp, root.join("CURRENT"))?;

  // GC: keep the new generation and the prior one; sweep everything else under gen/ —
  // superseded generations and dead staging dirs — skipping anything modified in the last two
  // minutes (a concurrent build's staging must not be swept mid-flight). Errors are ignored:
  // GC is hygiene, never correctness.
  let keep_prior = prior.starts_with(root.join("gen")).then(|| prior.to_path_buf());
  if let Ok(entries) = fs::read_dir(root.join("gen")) {
    for entry in entries.flatten() {
      let path = entry.path();
      if path == final_dir || Some(&path) == keep_prior.as_ref() {
        continue;
      }
      let recent = entry
        .metadata()
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|age| age.as_secs() < 120);
      if !recent {
        let _ = fs::remove_dir_all(&path);
      }
    }
  }
  // Legacy migration: when the prior was a flat root, its artifacts are now superseded by the
  // generation committed from them — sweep the known filenames (never products/, gen/, or
  // CURRENT) so the root holds only the generation layout.
  if prior == root {
    for artifact in GENERATION_ARTIFACTS {
      let _ = fs::remove_file(root.join(artifact));
    }
    for scratch in [
      "strings.heap.tmp",
      ".refs.spill",
      "ann.bin",
      "ann.files",
      "ann.model.bin",
      "ann.model.json",
      "ann.calib",
      "ann.stamp",
      "ann.build.lock",
      "products.pack.tmp",
      "products.pack.spool",
      "products.idx.tmp",
    ] {
      let _ = fs::remove_file(root.join(scratch));
    }
    for scratch_dir in ["train.scratch", "retrofit.scratch"] {
      let _ = fs::remove_dir_all(root.join(scratch_dir));
    }
  }
  Ok(id)
}

/// Build the semantic tier over every KG node: each definition embeds its name (double-weighted),
/// signature, and file *basename* — never the full path, whose directory tokens are shared junk
/// that drowns the signal — through the pluggable embedder (default: the deterministic lexical
/// hasher) into the adaptive ANN index persisted beside the graph. Rows embed in parallel with
/// no per-node intermediate text; the order-preserving collect keeps the index bit-identical to
/// the serial build.
/// The ANN freshness stamp: xxh3 of the node segment bytes of the **loaded** graph. Any node
/// change (name, signature, count, order) changes the segment, which invalidates the stamp —
/// necessary-condition semantics, same as every other cache in the pipeline. Hashing the
/// loaded mapping (never re-reading `nodes.vseg`) pins every freshness decision to the
/// generation actually in hand.
pub(crate) fn stamp_of(kg: &Kg) -> u64 {
  kg.node_segment_stamp()
}

/// The vector tier's row set: every non-Import node id, ascending. Import nodes are wiring,
/// not semantic targets (they stay reachable through the exact-name channel); build and
/// fallback must agree on this filter or cold results would resurface import noise.
pub(crate) fn semantic_row_ids(kg: &Kg) -> Vec<u64> {
  (0..kg.node_count() as u64)
    .filter(|&i| {
      kg.node(NodeId::new(i))
        .is_some_and(|view| view.kind != vorpal_kg::SymbolKind::Import)
    })
    .collect()
}

/// Embed node `id`'s parts into `row` — the one embedding recipe (name double-weighted,
/// signature, file basename) shared by the index build, the cold fallback, and the rerank.
/// The canonical tree prefix every stored path shares (up to and including its trailing
/// `/`), from the File nodes. Embeddings strip it so vectors are a function of the TREE,
/// never its mount point: the same corpus indexed from two directories, machines, or OS
/// temp layouts must rank identically — the cross-platform pinned retrieval baselines
/// exist because of this invariance. Empty when the graph holds no File nodes.
pub(crate) fn embedding_root(kg: &Kg) -> String {
  let mut root: Option<String> = None;
  for raw in 0..kg.node_count() as u64 {
    let id = NodeId::new(raw);
    if kg.node_kind(id) != Some(vorpal_kg::SymbolKind::File) {
      continue;
    }
    let Some(path) = kg.node_path(id) else { continue };
    match &mut root {
      None => root = Some(path.to_string()),
      Some(prefix) => {
        let common = prefix
          .bytes()
          .zip(path.bytes())
          .take_while(|(a, b)| a == b)
          .count();
        prefix.truncate(common);
      }
    }
  }
  let mut root = root.unwrap_or_default();
  match root.rfind('/') {
    Some(at) => root.truncate(at + 1),
    None => root.clear(),
  }
  root
}

pub(crate) fn embed_node_into(
  kg: &Kg,
  embedder: &ActiveEmbedder,
  id: u64,
  row: &mut [f32],
  root: &str,
) {
  if let Some(view) = kg.node(NodeId::new(id)) {
    // File nodes are NAMED by their stored (canonical, absolute) path — strip the tree
    // root so only tree-relative tokens reach the vector (see `embedding_root`).
    let name = view.name.strip_prefix(root).unwrap_or(view.name);
    let basename = view.path.rsplit('/').next().unwrap_or(view.path);
    let parts = [name, name, view.signature, basename];
    embedder.embed_node_parts(&parts, row);
  } else {
    row.fill(0.0);
  }
}

/// Which of `phrase_token_sets` share at least one real token with node `id`'s embedded
/// surface — EXACTLY the parts [`embed_node_into`] hashes (name, signature, file
/// basename), tokenized by the same [`tokenize`]. Bit `p` set ⇔ phrase `p` lexically
/// matches the row. This is the multi-phrase support criterion at the lexical tier:
/// exact token overlap, immune to the hashed-bucket collisions that make vector-space
/// sign tests meaningless as match criteria (measured: universal tokens like `fn`/`u32`
/// collide nonsense phrases into near-global "positive" dot products).
fn node_lexical_support_bits(kg: &Kg, id: u64, phrase_token_sets: &[Vec<String>]) -> u64 {
  let Some(view) = kg.node(NodeId::new(id)) else {
    return 0;
  };
  let basename = view.path.rsplit('/').next().unwrap_or(view.path);
  let mut row_tokens = tokenize(view.name);
  row_tokens.extend(tokenize(view.signature));
  row_tokens.extend(tokenize(basename));
  let mut bits = 0u64;
  for (phrase, tokens) in phrase_token_sets.iter().enumerate().take(u64::BITS as usize) {
    if tokens.iter().any(|token| row_tokens.contains(token)) {
      bits |= 1u64 << phrase;
    }
  }
  bits
}

/// The `(dim, base_stamp)` of the persisted ann.bin header, if present and current-format —
/// exposed for coherence tests and diagnostics.
pub fn peek_ann_header(index_dir: &Path) -> Option<(usize, u64)> {
  AnnIndex::peek_header(&vorpal_kg::resolve_index_dir(index_dir).join("ann.bin"))
}

/// The one production embedder construction point (IMPROVEMENTS #9): build, warm, overlay,
/// and query must all agree on the model, so selection is explicit and singular. The
/// deterministic lexical hasher is the offline default; a learned adapter would be selected
/// here and carry `learned: true` in its persisted provenance — labeled honestly, never
/// silently swapped (the provenance gate below invalidates the tier on ANY model change).
pub(crate) fn active_embedder() -> LexicalEmbedder {
  LexicalEmbedder::default()
}

/// The build/query embedder, selected PER INDEX (docs/wip/SEMANTIC_TIER.md Stage 1):
/// the deterministic lexical hasher is the default everywhere; the learned tier
/// activates only through the `semantic.tier` selection file (build side) or a fully
/// coherent persisted model (query side) — never silently, never mixed.
pub enum ActiveEmbedder {
  Lexical(LexicalEmbedder),
  Learned(Box<LearnedStaticEmbedder>),
}

impl ActiveEmbedder {
  pub fn dim(&self) -> usize {
    match self {
      ActiveEmbedder::Lexical(lexical) => lexical.dim(),
      ActiveEmbedder::Learned(learned) => learned.dim(),
    }
  }

  pub fn embed(&self, text: &str) -> Vec<f32> {
    match self {
      ActiveEmbedder::Lexical(lexical) => lexical.embed(text),
      ActiveEmbedder::Learned(learned) => learned.embed(text),
    }
  }

  pub fn provenance(&self) -> ModelProvenance {
    match self {
      ActiveEmbedder::Lexical(lexical) => lexical.provenance(),
      ActiveEmbedder::Learned(learned) => learned.provenance(),
    }
  }

  fn tier_label(&self) -> &'static str {
    match self {
      ActiveEmbedder::Lexical(_) => "lexical",
      ActiveEmbedder::Learned(_) => "learned",
    }
  }

  /// Embed one node's parts (the [`embed_node_into`] recipe). The lexical hasher takes
  /// the zero-alloc parts path; the learned model embeds the space-joined surface
  /// (part boundaries are token boundaries under the shared tokenizer either way).
  fn embed_node_parts(&self, parts: &[&str], out: &mut [f32]) {
    match self {
      ActiveEmbedder::Lexical(lexical) => lexical.embed_parts_into(parts, out),
      ActiveEmbedder::Learned(learned) => {
        let joined = parts.join(" ");
        learned.embed_into(&joined, out);
      }
    }
  }
}

/// The per-index semantic-tier selection, persisted at `<root>/semantic.tier`. Written
/// only by index-shaped commands (CLI/MCP `index`, `vorpal-index index`); the warm
/// child, the daemon, and every query path are pure READERS — no env var, no
/// process-global (an env would be the hijack surface autowarm's argv sentinel exists
/// to avoid; a global cannot serve one process holding many indexes). A missing file
/// means lexical; an unreadable or unknown file is a typed error, never a silent
/// default.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SemanticTier {
  Lexical,
  Learned,
}

impl SemanticTier {
  pub fn label(self) -> &'static str {
    match self {
      SemanticTier::Lexical => "lexical",
      SemanticTier::Learned => "learned",
    }
  }
}

/// Read `<root>/semantic.tier` (see [`SemanticTier`]).
pub fn tier_selection(index_root: &Path) -> Result<SemanticTier, Box<dyn Error>> {
  let path = index_root.join("semantic.tier");
  let text = match fs::read_to_string(&path) {
    Ok(text) => text,
    Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(SemanticTier::Lexical),
    Err(error) => return Err(format!("reading {}: {error}", path.display()).into()),
  };
  let value: serde_json::Value = serde_json::from_str(&text)
    .map_err(|error| format!("unparseable {}: {error}", path.display()))?;
  match value.get("tier").and_then(|tier| tier.as_str()) {
    Some("lexical") => Ok(SemanticTier::Lexical),
    Some("learned") => Ok(SemanticTier::Learned),
    other => Err(format!("unknown semantic tier {other:?} in {}", path.display()).into()),
  }
}

/// Persist the tier selection at `<root>/semantic.tier` (tmp + rename).
pub fn write_tier_selection(index_root: &Path, tier: SemanticTier) -> Result<(), Box<dyn Error>> {
  // Selection legitimately precedes the first build (every writer is an index-shaped
  // command that writes BEFORE building) — the root may not exist yet.
  fs::create_dir_all(index_root)?;
  let tmp = index_root.join("semantic.tier.tmp");
  fs::write(&tmp, format!("{{\"tier\":\"{}\"}}\n", tier.label()))?;
  fs::rename(&tmp, index_root.join("semantic.tier"))?;
  Ok(())
}

/// The active embedding model's provenance — the public configuration contract: model id,
/// dimensionality, normalization, semantics version, and whether weights are learned.
pub fn model_provenance() -> ModelProvenance {
  active_embedder().provenance()
}

/// The full persisted tier record beside the vector artifacts: the embedding-model
/// provenance, which tier built it, the model-file checksum for learned tiers, and an
/// optional stated fallback note.
struct PersistedTierRecord {
  provenance: ModelProvenance,
  tier: String,
  weights_hash: Option<u128>,
  /// Present exactly when the build STATED a fallback (learned selection, lexical
  /// outcome) — the bit that distinguishes it from a deliberate lexical selection.
  note: Option<String>,
  /// The retrofit penalty form the tier's rows were built under ("none" when
  /// unretrofitted, and for every pre-retrofit file) — part of the tier's identity:
  /// a form change in code must retrain, never serve mixed row semantics.
  retrofit: String,
  /// The training kind policy the learned model was fitted under ("all" for every
  /// pre-policy file) — part of the tier's identity for the same reason: a policy
  /// change in code retrains old tiers instead of serving a model the code would
  /// no longer produce.
  train_kinds: String,
  /// Per-corpus BM25 fourth-list verdict from the warm-time self-probe gate:
  /// `None` = never gated (pre-gate file, or a crash before the gate ran) — healed
  /// like calibration on the next warm; `Some(v)` = gated, `v` enables the list.
  /// Deterministic from index content, so freshness never consults it.
  bm25: Option<bool>,
}

fn persisted_tier_record(index_dir: &Path) -> Option<PersistedTierRecord> {
  let text =
    fs::read_to_string(vorpal_kg::resolve_index_dir(index_dir).join("ann.model.json")).ok()?;
  let value: serde_json::Value = serde_json::from_str(&text).ok()?;
  let provenance = ModelProvenance {
    model_id: value.get("model_id")?.as_str()?.to_string(),
    dim: value.get("dim")?.as_u64()? as usize,
    normalization: value.get("normalization")?.as_str()?.to_string(),
    version: value.get("version")?.as_u64()? as u32,
    learned: value.get("learned")?.as_bool()?,
  };
  // Files written by pre-tier builds carry no `tier` field: they are lexical by
  // construction (the only embedder that existed).
  let tier = value
    .get("tier")
    .and_then(|tier| tier.as_str())
    .unwrap_or("lexical")
    .to_string();
  let weights_hash = match value.get("weights_hash") {
    None | Some(serde_json::Value::Null) => None,
    Some(serde_json::Value::String(hex)) => Some(u128::from_str_radix(hex, 16).ok()?),
    Some(_) => return None,
  };
  let note = match value.get("note") {
    None | Some(serde_json::Value::Null) => None,
    Some(serde_json::Value::String(note)) => Some(note.clone()),
    Some(_) => return None,
  };
  // Pre-retrofit files carry no field: they were built unretrofitted.
  let retrofit = value
    .get("retrofit")
    .and_then(|form| form.as_str())
    .unwrap_or("none")
    .to_string();
  // Pre-gate files carry no field: never gated (None) — distinct from a gated OFF.
  let bm25 = match value.get("bm25") {
    None | Some(serde_json::Value::Null) => None,
    Some(serde_json::Value::Bool(enabled)) => Some(*enabled),
    Some(_) => return None,
  };
  // The `bm25_gate` sibling in the file is the verdict's evidence line for humans
  // and benches — deliberately unread: a mangled evidence string must never wedge
  // an otherwise coherent tier.
  // Pre-policy files carry no field: they were trained on every kind ("all").
  let train_kinds = value
    .get("train_kinds")
    .and_then(|label| label.as_str())
    .unwrap_or("all")
    .to_string();
  Some(PersistedTierRecord {
    provenance,
    tier,
    weights_hash,
    note,
    retrofit,
    bm25,
    train_kinds,
  })
}

/// The provenance persisted beside `index_dir`'s vector tier, if any — what the tier's
/// vectors were actually built with (may differ from [`model_provenance`] until a re-warm).
pub fn persisted_model_provenance(index_dir: &Path) -> Option<ModelProvenance> {
  persisted_tier_record(index_dir).map(|record| record.provenance)
}

/// Persist the tier record — written before the stamp commit, so a committed stamp
/// always implies a readable record. Canonical field order keeps the file
/// byte-reproducible; `note` appears only on stated fallbacks.
#[allow(clippy::too_many_arguments)] // one parameter per identity field of the tier record
fn write_model_provenance(
  index_dir: &Path,
  provenance: &ModelProvenance,
  tier: &str,
  weights_hash: Option<u128>,
  retrofit: &str,
  train_kinds: &str,
  bm25: Option<(bool, &str)>,
  note: Option<&str>,
) -> io::Result<()> {
  let weights = match weights_hash {
    Some(hash) => format!("\"{hash:032x}\""),
    None => "null".to_string(),
  };
  let bm25_fields = match bm25 {
    Some((enabled, evidence)) => format!(
      ",\"bm25\":{enabled},\"bm25_gate\":{}",
      serde_json::Value::String(evidence.to_string())
    ),
    None => String::new(),
  };
  let note_field = match note {
    Some(text) => format!(",\"note\":{}", serde_json::Value::String(text.to_string())),
    None => String::new(),
  };
  let json = format!(
    "{{\"model_id\":{},\"dim\":{},\"normalization\":{},\"version\":{},\"learned\":{},\"tier\":{},\"weights_hash\":{},\"retrofit\":{},\"train_kinds\":{}{}{}}}\n",
    serde_json::Value::String(provenance.model_id.clone()),
    provenance.dim,
    serde_json::Value::String(provenance.normalization.clone()),
    provenance.version,
    provenance.learned,
    serde_json::Value::String(tier.to_string()),
    weights,
    serde_json::Value::String(retrofit.to_string()),
    serde_json::Value::String(train_kinds.to_string()),
    bm25_fields,
    note_field,
  );
  let tmp = index_dir.join("ann.model.json.tmp");
  fs::write(&tmp, json)?;
  fs::rename(tmp, index_dir.join("ann.model.json"))
}


/// Query-side coherence: are the persisted vector artifacts internally consistent for
/// `current_stamp` — stamp file, bin header, tier record, and (learned) the model file
/// verified against the recorded checksum? Returns the embedder the tier was BUILT
/// with, so the handle queries through exactly that model: mixing embedders in one
/// pool is structurally impossible. Read-only; never triggers a build.
fn coherent_persisted_embedder(index_dir: &Path, current_stamp: u64) -> Option<ActiveEmbedder> {
  let stamp_ok = fs::read(index_dir.join("ann.stamp"))
    .ok()
    .and_then(|bytes| bytes.try_into().ok().map(u64::from_le_bytes))
    .is_some_and(|stored| stored == current_stamp);
  if !stamp_ok {
    return None;
  }
  let record = persisted_tier_record(index_dir)?;
  // The bin's own header must carry the same generation AND the record's dimension: a
  // rebuild window can rename the new bin before the new stamp lands, and a carried-
  // forward bin under a changed model must never pass.
  let (bin_dim, bin_stamp) = AnnIndex::peek_header(&index_dir.join("ann.bin"))?;
  if bin_stamp != current_stamp || bin_dim != record.provenance.dim {
    return None;
  }
  match record.tier.as_str() {
    "lexical" => {
      let lexical = LexicalEmbedder::default();
      (record.provenance == lexical.provenance()).then_some(ActiveEmbedder::Lexical(lexical))
    }
    "learned" => {
      let expected = record.weights_hash?;
      let path = index_dir.join("ann.model.bin");
      // Zero-copy mapped open: header + sealed checksum validated, bulk tables stay
      // on the page cache — a kernel-scale model (hundreds of MB) never materializes
      // per Searcher open.
      let (learned, stored) = LearnedStaticEmbedder::open_mapped(&path).ok()?;
      if stored != expected || learned.dim() != record.provenance.dim {
        return None;
      }
      (record.provenance == learned.provenance())
        .then(|| ActiveEmbedder::Learned(Box::new(learned)))
    }
    _ => None,
  }
}

/// Build-side freshness: coherent artifacts AND the persisted tier matches the
/// selection (a tier flip is staleness — the next warm rebuilds under the selected
/// model). The learned check verifies the model file's checksum without loading it.
/// Optional-model packaging (semantic-tier Stage 6): global install/enable paths
/// (always available — every consumer honors a global enable) and the
/// checksum-pinned installer (behind the `model-install` feature) backing
/// `vorpal enable` and the SDK install APIs.
pub mod models;

/// Programmatic tune — the `vorpal tune` core shared by the CLI and the SDK
/// bindings: paired one-search measurements, the verdict rule, the switch writes.
pub mod tune;

/// Reranker embedding-cache bound: ~12 MB of rows at the encoder's 768 dims. A
/// shipped cap in the `cached_searcher` LRU-8 precedent (a small documented bound,
/// not a derived law) — revisit under a recorded sweep if the reranker becomes a
/// hot path.
const ENCODER_CACHE_ROWS: usize = 4096;

/// FIFO-bounded map of node id → L2-normalized candidate-surface embedding.
#[derive(Default)]
struct EncoderCache {
  map: HashMap<u64, Vec<f32>>,
  order: std::collections::VecDeque<u64>,
}

impl EncoderCache {
  fn insert(&mut self, id: u64, row: Vec<f32>) {
    if self.map.contains_key(&id) {
      return;
    }
    if self.map.len() == ENCODER_CACHE_ROWS
      && let Some(evicted) = self.order.pop_front()
    {
      self.map.remove(&evicted);
    }
    self.order.push_back(id);
    self.map.insert(id, row);
  }
}

/// Open the root's selected encoder, if any — shared by both `Searcher` opens.
/// Selection precedence: the index root's `encoder.dir`, else the GLOBAL enable
/// written by `vorpal enable` / the SDK APIs (see [`models`]).
/// A failed open is a STATED degradation, never a failed search.
fn open_selected_encoder(
  index_root: &Path,
) -> (Option<Box<vorpal_ann::encoder::CodeEncoder>>, Option<String>) {
  let selection = match encoder_selection(index_root) {
    Some(EncoderSelection::Off) => None, // deliberate per-index opt-out
    Some(EncoderSelection::Model(model_dir)) => Some(model_dir),
    None => models::global_encoder_selection(),
  };
  match selection {
    None => (None, None),
    Some(model_dir) => match vorpal_ann::encoder::CodeEncoder::open(&model_dir) {
      Ok(encoder) => (Some(Box::new(encoder)), None),
      Err(reason) => (
        None,
        Some(format!("encoder disabled: {reason} ({})", model_dir.display())),
      ),
    },
  }
}

/// The per-index vendored-encoder selection (semantic-tier Stage 6): `<root>/encoder.dir`
/// names a LOCAL model directory and turns the opt-in query-time reranker on;
/// missing/empty = off. Like `semantic.tier`, this is a ROOT artifact — generation
/// dirs never carry one, so internal opens of a pinned generation (the BM25 gate's
/// probes, overlay assembly) always measure the UN-reranked fusion their records
/// describe. Never a download: the directory must already exist locally.
/// A root's explicit selection: a model directory, or the literal sentinel `off`
/// — a per-index OPT-OUT that shadows the global enable (what `vorpal tune`
/// writes when the reranker measures worse on that index).
enum EncoderSelection {
  Off,
  Model(PathBuf),
}

fn encoder_selection(index_root: &Path) -> Option<EncoderSelection> {
  let text = fs::read_to_string(index_root.join("encoder.dir")).ok()?;
  let trimmed = text.trim();
  if trimmed.is_empty() {
    None
  } else if trimmed == "off" {
    Some(EncoderSelection::Off)
  } else {
    Some(EncoderSelection::Model(PathBuf::from(trimmed)))
  }
}

/// Persist the encoder selection at `<root>/encoder.dir` (tmp + rename) — the
/// Stage-6 reranker's opt-in, written by index-shaped commands only (the
/// `semantic.tier` discipline: warms and searches are pure readers; removing the
/// file turns the reranker off).
pub fn write_encoder_selection(index_root: &Path, model_dir: &Path) -> io::Result<()> {
  let tmp = index_root.join("encoder.dir.tmp");
  fs::write(&tmp, format!("{}\n", model_dir.display()))?;
  fs::rename(tmp, index_root.join("encoder.dir"))
}

/// Persist the per-index OPT-OUT sentinel (`encoder.dir` = `off`): this index
/// deliberately shadows any global enable — `vorpal tune`'s verdict when the
/// reranker measures worse here. Remove the file to fall back to the global.
pub fn write_encoder_opt_out(index_root: &Path) -> io::Result<()> {
  let tmp = index_root.join("encoder.dir.tmp");
  fs::write(&tmp, "off\n")?;
  fs::rename(tmp, index_root.join("encoder.dir"))
}

/// The dense sidecar a handle serves, and whether its channel is ON: fresh for
/// this generation and the OPENED encoder (else `None`). ON by default wherever
/// a fresh sidecar exists (owner decision 2026-09-02, BENCHMARKS "Doc-side dense
/// channel": the labelled harness measured +0.100 / +0.016 / +0.123 all-NDCG on
/// vorpal / cpython / kernel, while the warm's label-free self-probe gate voted
/// OFF on all three — its probes are name-token queries blind to the descriptive
/// and subword-identifier classes the channel serves; that verdict stays in the
/// record as evidence only). The root's `dense.channel` override (`vorpal tune`'s
/// labelled verdict) is the per-index opt-out/opt-in. `VORPAL_DENSE_CHANNEL=on|off`
/// forces it under `bench-internals` only — never a production knob.
fn load_dense_sidecar(
  index_root: &Path,
  generation_dir: &Path,
  stamp: u64,
  encoder: Option<&vorpal_ann::encoder::CodeEncoder>,
) -> (Option<dense::DenseSidecar>, bool, usize) {
  let Some(encoder) = encoder else {
    return (None, false, 0);
  };
  // The fresh record carries the sidecar's own eligible population (the fill's stop
  // rule's whole set) — the denominator of the population-scaled fusion laws.
  let Some(record) = dense::fresh_record(generation_dir, stamp, encoder) else {
    return (None, false, 0);
  };
  let Some(sidecar) = dense::DenseSidecar::load(generation_dir, stamp) else {
    return (None, false, 0);
  };
  let eligible = record.eligible;
  // Precedence: the root's `dense.channel` override (`vorpal tune`'s labelled
  // verdict) over the default ON — the same shape as `encoder.dir` over the
  // global enable. The record's label-free gate verdict is evidence, not a switch.
  let verdict = dense_channel_override(index_root).unwrap_or(true);
  #[cfg(feature = "bench-internals")]
  let verdict = match std::env::var("VORPAL_DENSE_CHANNEL").as_deref() {
    Ok("on") => true,
    Ok("off") => false,
    _ => verdict,
  };
  (Some(sidecar), verdict, eligible)
}

/// The root's per-index dense-channel override (`<root>/dense.channel` = `on` |
/// `off`), if any — written by `vorpal tune` from a labelled verdict; missing or
/// unparseable = defer to the record's gate.
pub fn dense_channel_override(index_root: &Path) -> Option<bool> {
  match fs::read_to_string(index_root.join("dense.channel")).ok()?.trim() {
    "on" => Some(true),
    "off" => Some(false),
    _ => None,
  }
}

/// Persist the per-index dense-channel override (tmp + rename) — `vorpal tune`'s
/// write path; remove the file to fall back to the warm gate's verdict.
pub fn write_dense_channel_override(index_root: &Path, enabled: bool) -> io::Result<()> {
  let tmp = index_root.join("dense.channel.tmp");
  fs::write(&tmp, if enabled { "on\n" } else { "off\n" })?;
  fs::rename(tmp, index_root.join("dense.channel"))
}

/// Warm options beyond the tier selection: an explicit CAP on this round of the
/// dense fill (`dense.rs`; seconds, from a human duration). `None` consults the
/// root's `dense.budget` file; no cap anywhere = the fill runs to the stop rule.
#[derive(Clone, Copy, Debug, Default)]
pub struct WarmOptions {
  pub dense_budget_secs: Option<f64>,
}

/// The root's persisted dense cap (`<root>/dense.budget`, a human duration such
/// as `5m30s` or plain seconds), if present and well-formed.
pub fn dense_budget_selection(index_root: &Path) -> Option<f64> {
  duration::parse_duration(fs::read_to_string(index_root.join("dense.budget")).ok()?.trim()).ok()
}

/// Persist the root's dense cap (tmp + rename, rendered as a canonical duration)
/// — an index-shaped write, like `semantic.tier`; warms are pure readers of it.
pub fn write_dense_budget(index_root: &Path, secs: f64) -> io::Result<()> {
  let tmp = index_root.join("dense.budget.tmp");
  fs::write(&tmp, format!("{}\n", duration::render_duration(secs)))?;
  fs::rename(tmp, index_root.join("dense.budget"))
}

/// Fill (or resume) the dense sidecar whenever an encoder is selected — after
/// the core tiers committed, so a search is served by them meanwhile and picks
/// up each checkpoint as it lands. Every problem is a stated degradation
/// (phase-stamped), never a failed warm.
fn ensure_dense(
  index_dir: &Path,
  kg: &Kg,
  stamp: u64,
  encoder: Option<Box<vorpal_ann::encoder::CodeEncoder>>,
  cap_secs: Option<f64>,
) {
  let Some(encoder) = encoder else {
    return;
  };
  if dense::fresh_record(index_dir, stamp, &encoder).is_some_and(|record| record.complete) {
    return;
  }
  if let Err(reason) = dense::fill(kg, index_dir, stamp, &encoder, cap_secs) {
    vorpal_kg::phase_stamp(&format!(
      "dense: fill stopped ({reason}) — the committed checkpoint keeps serving"
    ));
  }
}

fn ann_is_fresh(index_dir: &Path, current_stamp: u64, selection: SemanticTier) -> bool {
  let stamp_ok = fs::read(index_dir.join("ann.stamp"))
    .ok()
    .and_then(|bytes| bytes.try_into().ok().map(u64::from_le_bytes))
    .is_some_and(|stored| stored == current_stamp);
  if !stamp_ok {
    return false;
  }
  let Some(record) = persisted_tier_record(index_dir) else {
    return false;
  };
  let header_ok = AnnIndex::peek_header(&index_dir.join("ann.bin"))
    .is_some_and(|(bin_dim, bin_stamp)| bin_dim == record.provenance.dim && bin_stamp == current_stamp);
  if !header_ok {
    return false;
  }
  match record.tier.as_str() {
    // The record must describe EXACTLY the (selection, outcome) pair. A lexical record
    // satisfies a Lexical selection only without a fallback note (a lingering note
    // would misdescribe a deliberate selection), and satisfies a Learned selection
    // only WITH one — the stated small-corpus fallback, which rebuilding cannot help
    // until content changes (re-warms retry naturally then). The note split is what
    // lets a deliberate lexical→learned flip retrain instead of no-op'ing.
    "lexical" => {
      record.provenance == LexicalEmbedder::default().provenance()
        && match selection {
          SemanticTier::Lexical => record.note.is_none(),
          SemanticTier::Learned => record.note.is_some(),
        }
    }
    "learned" => {
      // The freshness gate IS the query-side open: a model counts as fresh only if
      // the mapped view opens (magic, version, layout, checksum) with the recorded
      // hash. A cheaper prefix check drifted from the reader once — version-accepted
      // bytes that misparsed past the header wedged the tier ("fresh" to the builder,
      // unloadable to every query) — so build and query share ONE criterion forever.
      // Cost: one checksum pass per freshness check (~30 ms at kernel scale).
      // The retrofit form is part of the tier's identity: a form change in code
      // retrains old tiers. A record of "none" WITH a stated disable is a valid
      // outcome under any form (a rebuild under the same pressure would produce it
      // again — the fallback-note law, so a persistent disable never rebuild-loops).
      let retrofit_ok = record.retrofit == active_retrofit_label()
        || (record.retrofit == "none"
          && record
            .note
            .as_deref()
            .is_some_and(|note| note.contains("retrofit disabled")));
      // The training kind policy is identity too: a pre-policy record reads "all",
      // so flipping the pinned policy in code retrains every existing learned tier
      // (the forgotten-bump lesson, applied before the fact).
      selection == SemanticTier::Learned
        && record.provenance.learned
        && retrofit_ok
        && record.train_kinds == active_train_kinds_label()
        && record.weights_hash.is_some_and(|expected| {
          LearnedStaticEmbedder::open_mapped(&index_dir.join("ann.model.bin"))
            .is_ok_and(|(_, stored)| stored == expected)
        })
    }
    _ => false,
  }
}

/// Build the ANN tier iff its stamp no longer matches the persisted graph (or it does not
/// exist). Queries call this before touching `ann.bin`; `vorpal index` never does.
/// Build the ANN tier now if it is stale — the daemon calls this eagerly (in the
/// background, right after an index refresh) so interactive searches stop paying the
/// build; a search that arrives mid-build serializes on the same lock and proceeds the
/// moment the tier is fresh.
pub fn warm_ann(index_dir: &Path) -> Result<(), Box<dyn Error>> {
  warm_ann_with(index_dir, WarmOptions::default())
}

/// [`warm_ann`] with explicit [`WarmOptions`] (the CLI's `--dense-budget-secs`).
pub fn warm_ann_with(index_dir: &Path, options: WarmOptions) -> Result<(), Box<dyn Error>> {
  // The tier selection lives at the ROOT (it survives generations); read it before
  // resolving. A caller handing a raw generation dir gets the lexical default — only
  // index-shaped commands write selections, and they operate on roots.
  let selection = tier_selection(index_dir)?;
  // The dense sidecar embeds through the ROOT's encoder selection (per-index
  // `encoder.dir`, else the global enable); no encoder selected = no sidecar.
  // A cap (explicit option, else the root's `dense.budget`) only shortens a round.
  let dense_budget = options
    .dense_budget_secs
    .or_else(|| dense_budget_selection(index_dir));
  let dense_encoder = open_selected_encoder(index_dir).0;
  // Warm the generation CURRENT names right now; its artifacts land inside that generation
  // (the ANN tier is the one stamp-validated sidecar an existing generation admits). If a
  // rebuild supersedes it mid-warm, the work is simply for a generation about to retire —
  // harmless, and the next search on the new generation triggers its own warm.
  let index_dir = &vorpal_kg::resolve_index_dir(index_dir);
  // Cross-process exclusion: concurrent warms (a daemon thread + a detached CLI child, or
  // two cold CLIs) must not build twice. The loser skips entirely — whoever asked has
  // already been served by the fallback tier, and the winner's commit flips freshness.
  // fd-lock is advisory and released by the OS on process death, so a crashed warm never
  // wedges the tier.
  let lock_path = index_dir.join("ann.build.lock");
  let lock_file = fs::OpenOptions::new()
    .create(true)
    .truncate(false)
    .write(true)
    .open(&lock_path)?;
  let mut lock = fd_lock::RwLock::new(lock_file);
  let Ok(_guard) = lock.try_write() else {
    return Ok(());
  };
  ensure_ann(index_dir, selection, dense_encoder, dense_budget)
}

/// Run the warm-time per-corpus BM25 gate iff the committed record carries no
/// verdict, and rewrite the record with one (atomic tmp+rename, same canonical
/// writer). Deterministic: probes derive from the node-segment stamp, so every
/// re-warm of the same content reaches the same verdict — healing converges and
/// double-warm byte-identity extends over the gated record.
fn ensure_bm25_gate(index_dir: &Path, current_stamp: u64) -> Result<(), Box<dyn Error>> {
  let Some(record) = persisted_tier_record(index_dir) else {
    return Ok(()); // no committed tier — nothing to gate
  };
  if record.bm25.is_some() {
    return Ok(());
  }
  // The stamp must match: gating a half-replaced generation would persist a verdict
  // for bytes the record does not describe. A mismatch just leaves the record
  // ungated; the next fresh warm heals it.
  let stamp_ok = fs::read(index_dir.join("ann.stamp"))
    .ok()
    .and_then(|bytes| bytes.try_into().ok().map(u64::from_le_bytes))
    .is_some_and(|stored| stored == current_stamp);
  if !stamp_ok {
    return Ok(());
  }
  let searcher = Searcher::open(index_dir)?;
  let (enabled, evidence) = searcher.bm25_self_probe_verdict(current_stamp)?;
  write_model_provenance(
    index_dir,
    &record.provenance,
    &record.tier,
    record.weights_hash,
    &record.retrofit,
    &record.train_kinds,
    Some((enabled, &evidence)),
    record.note.as_deref(),
  )?;
  Ok(())
}

/// Manually override the per-corpus BM25 verdict (`vorpal tune`'s write path):
/// rewrite the committed tier record with the given verdict and a stated
/// manual-evidence line, through the same canonical writer as the warm gate.
/// The override holds until the index content changes and retrains (the warm
/// gate then re-derives its own verdict) — callers state that.
pub fn set_bm25_override(index_dir: &Path, enabled: bool, evidence: &str) -> Result<(), String> {
  let index_dir = vorpal_kg::resolve_index_dir(index_dir);
  let record = persisted_tier_record(&index_dir)
    .ok_or("no committed tier record — build/warm the index first")?;
  write_model_provenance(
    &index_dir,
    &record.provenance,
    &record.tier,
    record.weights_hash,
    &record.retrofit,
    &record.train_kinds,
    Some((enabled, evidence)),
    record.note.as_deref(),
  )
  .map_err(|e| format!("rewriting tier record: {e}"))
}

fn ensure_ann(
  index_dir: &Path,
  selection: SemanticTier,
  dense_encoder: Option<Box<vorpal_ann::encoder::CodeEncoder>>,
  dense_budget: Option<f64>,
) -> Result<(), Box<dyn Error>> {
  // One build at a time **per index directory**: an eager background warm and a foreground
  // search on the same index must not both build (duplicate work, racing writes), but a
  // host serving several indexes warms them concurrently — the old process-wide mutex
  // serialized unrelated indexes for no reason. Late entrants re-check freshness under the
  // dir lock and find the first builder's work done. The key map is bounded by the distinct
  // index dirs this process ever warms.
  static ANN_BUILDS: OnceLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> = OnceLock::new();
  let dir_lock = {
    let mut map = ANN_BUILDS
      .get_or_init(|| Mutex::new(HashMap::new()))
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner());
    map.entry(index_dir.to_path_buf()).or_default().clone()
  };
  let _guard = dir_lock
    .lock()
    .unwrap_or_else(|poisoned| poisoned.into_inner());
  // Load first, stamp the loaded bytes: the stamp this build writes must describe the KG it
  // actually embedded, even if `nodes.vseg` is replaced underneath us mid-decision.
  let kg = Kg::load(index_dir)?;
  let current = stamp_of(&kg);
  if ann_is_fresh(index_dir, current, selection) {
    // The vector tier is current, but the lexical tier heals independently (it can be
    // missing on indexes warmed by older builds, or after a partial cleanup).
    if !postings::postings_are_fresh(index_dir, current) {
      postings::build_postings(&kg, index_dir, current)?;
    }
    ensure_communities(&kg, index_dir, current)?;
    // The engine calibration heals independently too (older warms never measured one)
    // — probing through the embedder the persisted tier was actually built with.
    if load_ann_calibration(index_dir, current, kg.node_count()).is_none()
      && let Ok(ann) = AnnIndex::load(&index_dir.join("ann.bin"))
      && let Some(embedder) = coherent_persisted_embedder(index_dir, current)
    {
      let crossover = calibrate_semantic_cutover(&kg, &ann, &embedder);
      write_ann_calibration(index_dir, current, crossover)?;
    }
    // The per-corpus BM25 gate heals independently too (pre-gate records carry no
    // verdict) — deterministic from content, so healing converges in one pass.
    ensure_bm25_gate(index_dir, current)?;
    // The dense sidecar heals independently (stamp-gated; budget- and
    // encoder-conditional — see `dense.rs`).
    ensure_dense(index_dir, &kg, current, dense_encoder, dense_budget);
    return Ok(());
  }
  // Build under the SELECTED tier. A learned selection whose corpus is below the
  // learned tier's floor falls back to lexical with the reason STATED in the persisted
  // record — never a silent zero.
  let (embedder, note) = match selection {
    SemanticTier::Lexical => (ActiveEmbedder::Lexical(LexicalEmbedder::default()), None),
    SemanticTier::Learned => match train_learned_model(&kg, index_dir) {
      Ok(learned) => (ActiveEmbedder::Learned(Box::new(learned)), None),
      Err(reason) => (
        ActiveEmbedder::Lexical(LexicalEmbedder::default()),
        Some(format!("learned tier fell back to lexical: {reason}")),
      ),
    },
  };
  // Stage 2: relation-aware retrofit of the learned rows over the graph's typed,
  // grade-passed, hub-pruned edges — learned tier only. ANY problem states itself in
  // the provenance note and the build proceeds unrefined (the auto-disable law).
  let (retrofitted, retrofit_note) = match &embedder {
    ActiveEmbedder::Learned(_) => match retrofit_learned_rows(&kg, index_dir, &embedder) {
      Ok(rows) => (Some(rows), None),
      Err(reason) => (None, Some(format!("retrofit disabled: {reason}"))),
    },
    ActiveEmbedder::Lexical(_) => (None, None),
  };
  let note = match (note, retrofit_note) {
    (Some(a), Some(b)) => Some(format!("{a}; {b}")),
    (a, b) => a.or(b),
  };
  let retrofit_label = if retrofitted.is_some() {
    active_retrofit_label()
  } else {
    "none".to_string()
  };
  build_ann(&kg, index_dir, current, &embedder, retrofitted.as_ref())
    .map_err(|err| err as Box<dyn Error>)?;
  if let Some(rows) = retrofitted {
    // The tier has consumed the refined rows; scratch is done (crash residue is
    // swept at the next retrofit entry).
    rows
      .delete()
      .map_err(|e| format!("removing retrofit scratch: {e}"))?;
    let _ = fs::remove_dir_all(index_dir.join("retrofit.scratch"));
  }
  // Commit order: ann.bin → ann.files (both inside build_ann) → ann.model.bin →
  // ann.model.json → ann.stamp. The stamp is the commit point (a committed tier always
  // has a readable record); a crash anywhere earlier leaves a mismatch that routes
  // searches to the exhaustive fallback until the next warm heals it.
  let weights_hash = match &embedder {
    ActiveEmbedder::Learned(learned) => {
      // The build side always constructs its embedder from the freshly trained OWNED
      // model (`LearnedStaticEmbedder::new`); a mapped backing here is impossible by
      // construction, but the error stays typed — never a panic.
      let Some(model) = learned.model() else {
        return Err("freshly trained learned embedder lost its owned model (invariant)".into());
      };
      Some(save_model(model, &index_dir.join("ann.model.bin"))?)
    }
    ActiveEmbedder::Lexical(_) => {
      // A lexical build leaves no model file behind: a stale ann.model.bin beside a
      // lexical record would be dead weight the coherence gate ignores — remove it.
      let _ = fs::remove_file(index_dir.join("ann.model.bin"));
      None
    }
  };
  write_model_provenance(
    index_dir,
    &embedder.provenance(),
    embedder.tier_label(),
    weights_hash,
    &retrofit_label,
    active_train_kinds_label(),
    // The BM25 verdict gates AFTER the stamp commits (the gate probes a fully
    // servable generation); `None` = not yet gated — filled below, healed on
    // later warms if a crash lands between.
    None,
    note.as_deref(),
  )?;
  let stamp_path = index_dir.join("ann.stamp");
  let stamp_tmp = index_dir.join("ann.stamp.tmp");
  fs::write(&stamp_tmp, current.to_le_bytes())?;
  fs::rename(&stamp_tmp, &stamp_path)?;
  // The lexical tier warms alongside (IMPROVEMENTS #9): same stamp discipline, same
  // fallback correctness — a search whose postings are stale takes the exhaustive
  // name scan and stays exact.
  if !postings::postings_are_fresh(index_dir, current) {
    postings::build_postings(&kg, index_dir, current)?;
  }
  ensure_communities(&kg, index_dir, current)?;
  // Calibrate the semantic-engine crossover on the just-built tier — measured on this
  // machine over this index's rows, through the tier's own embedder.
  let ann = AnnIndex::load(&index_dir.join("ann.bin"))?;
  let crossover = calibrate_semantic_cutover(&kg, &ann, &embedder);
  write_ann_calibration(index_dir, current, crossover)?;
  // Gate the per-corpus BM25 fourth list on the now-fully-servable generation and
  // rewrite the record with the verdict (deterministic — see `ensure_bm25_gate`).
  ensure_bm25_gate(index_dir, current)?;
  // Last: the dense sidecar + its gate, over the committed tier (`dense.rs`).
  ensure_dense(index_dir, &kg, current, dense_encoder, dense_budget);
  Ok(())
}

/// Seed of the learned training run — an IDENTIFIER of the deterministic protocol
/// (like the Vamana build seed), not a tuned quantity: any fixed value defines a valid
/// deterministic pipeline.
const LEARNED_TRAIN_SEED: u64 = 1;

/// Train the Tier-1 model over this generation's node surfaces (the same
/// name/signature/basename recipe the embedder hashes; Imports excluded like the row
/// set), with bounded-memory resources derived from the machine and corpus probes. The
/// training scratch lives inside the generation dir, swept before and removed after —
/// the scratch lifecycle law.
fn train_learned_model(kg: &Kg, index_dir: &Path) -> Result<LearnedStaticEmbedder, String> {
  let scratch = index_dir.join("train.scratch");
  let _ = fs::remove_dir_all(&scratch);
  fs::create_dir_all(&scratch).map_err(|e| format!("creating {}: {e}", scratch.display()))?;
  let policy = vorpal_mem::ResourcePolicy::new(
    vorpal_mem::HardwareProbe::detect(),
    vorpal_mem::CorpusProbe::new(kg.node_segment_slabs().map(|s| s.len() as u64).sum::<u64>(), kg.node_count() as u64),
  );
  let resources = TrainResources {
    scratch_dir: scratch.clone(),
    page_bytes: policy.hardware().base_page_bytes,
    arena_chunk_bytes: policy.arena_chunk_bytes(kg.node_segment_slabs().map(|s| s.len() as u64).sum::<u64>()),
    // Training sub-steps land in the same phase-stamp stream as every warm phase
    // (`VORPAL_PHASE_TRACE=1`) — the kernel-scale attribution discipline.
    progress: vorpal_kg::phase_stamp,
  };
  // Kind policy (see `TrainKindPolicy`): under `BalanceToCallables` every kind's
  // document count is capped at the population of the largest callable kind, by a
  // deterministic stride over id order — the cap is read off this corpus, never set.
  let policy = train_kind_policy();
  // Per-kind tallies indexed by the kind's discriminant (SymbolKind is a fieldless enum).
  let mut kind_counts: Vec<u64> = vec![0; 256];
  let mut callable_max: u64 = 0;
  if policy == TrainKindPolicy::BalanceToCallables {
    for id in 0..kg.node_count() as u64 {
      if let Some(view) = kg.node(NodeId::new(id)) {
        kind_counts[view.kind as u8 as usize] += 1;
      }
    }
    for kind in [
      vorpal_kg::SymbolKind::Function,
      vorpal_kg::SymbolKind::Method,
      vorpal_kg::SymbolKind::Constructor,
    ] {
      callable_max = callable_max.max(kind_counts[kind as u8 as usize]);
    }
  }
  let cap: u64 = if callable_max == 0 { u64::MAX } else { callable_max };
  let corpus = |callback: &mut dyn FnMut(&[String])| {
    let mut seen_per_kind: Vec<u64> = vec![0; 256];
    for id in 0..kg.node_count() as u64 {
      let Some(view) = kg.node(NodeId::new(id)) else {
        continue;
      };
      if view.kind == vorpal_kg::SymbolKind::Import {
        continue;
      }
      match policy {
        TrainKindPolicy::All => {}
        TrainKindPolicy::ExcludeMacros => {
          if view.kind == vorpal_kg::SymbolKind::Macro {
            continue;
          }
        }
        TrainKindPolicy::BalanceToCallables => {
          let slot = view.kind as u8 as usize;
          let total = kind_counts[slot];
          if total > cap {
            // Keep the i-th node of this kind iff the stride bucket advances:
            // floor(i·cap/total) changes exactly `cap` times over i in [0, total).
            let ordinal = seen_per_kind[slot];
            let keep = (ordinal * cap) / total != ((ordinal + 1) * cap) / total;
            seen_per_kind[slot] += 1;
            if !keep {
              continue;
            }
          }
        }
      }
      let basename = view.path.rsplit('/').next().unwrap_or(view.path);
      let mut doc = tokenize(view.name);
      doc.extend(tokenize(view.signature));
      doc.extend(tokenize(basename));
      callback(&doc);
    }
  };
  vorpal_kg::phase_stamp("ann: train start");
  let (model, _report) = LearnedModel::train(&corpus, LEARNED_TRAIN_SEED, &resources)?;
  vorpal_kg::phase_stamp("ann: train done");
  let _ = fs::remove_dir_all(&scratch);
  Ok(LearnedStaticEmbedder::new(model))
}

/// The community sidecar warms beside the search tiers: same stamp discipline, same
/// absent-tolerant read (queries answer `null` for `community` until it exists).
/// Member cap for the community dendrogram cut: `VORPAL_COMMUNITY_CAP` (default
/// [`vorpal_kg::communities::DEFAULT_CAP`]; 0 reports the top Louvain level). A value that
/// is not a non-negative integer is an error, never a silent default.
fn community_cap() -> Result<u32, Box<dyn Error>> {
  match std::env::var_os("VORPAL_COMMUNITY_CAP") {
    None => Ok(vorpal_kg::communities::DEFAULT_CAP),
    Some(raw) => {
      let text = raw.to_string_lossy();
      text.trim().parse::<u32>().map_err(|e| {
        format!("VORPAL_COMMUNITY_CAP must be a non-negative integer (0 disables the cap), got {text:?}: {e}")
          .into()
      })
    }
  }
}

fn ensure_communities(kg: &Kg, index_dir: &Path, current: u64) -> Result<(), Box<dyn Error>> {
  let cap = community_cap()?;
  if vorpal_kg::communities::is_fresh(index_dir, current, cap) {
    return Ok(());
  }
  vorpal_kg::phase_stamp("communities: build start");
  let membership = vorpal_kg::communities::compute(kg, cap);
  vorpal_kg::communities::save(index_dir, current, cap, &membership)?;
  vorpal_kg::phase_stamp("communities: saved");
  Ok(())
}

/// The Stage-2 retrofit working set: three scratch-backed n×dim f32 regions (anchors
/// Q, the X double-buffer) in `<gen>/retrofit.scratch/` — the OS pager carries the
/// multi-GB matrices, anonymous RSS stays bounded. Held open until `build_ann` has
/// consumed the refined rows; deleted on success (a crash leaves only dead scratch,
/// swept at the next retrofit's entry).
struct RetrofittedRows {
  anchors: vorpal_mem::ScratchMmap,
  refined: vorpal_mem::ScratchMmap,
  spare: vorpal_mem::ScratchMmap,
  dim: usize,
}

impl RetrofittedRows {
  /// Row `i` of the refined matrix. An empty slice is unreachable by construction
  /// (page-aligned mapping, sized at creation); the fill treats it as a zero row
  /// rather than ever panicking.
  fn row(&self, i: usize) -> &[f32] {
    bytemuck::try_cast_slice(self.refined.as_bytes())
      .ok()
      .and_then(|rows: &[f32]| rows.get(i * self.dim..(i + 1) * self.dim))
      .unwrap_or(&[])
  }

  fn delete(self) -> std::io::Result<()> {
    self.anchors.delete()?;
    self.refined.delete()?;
    self.spare.delete()
  }
}

/// Assemble the retrofit adjacency over the semantic ROW space from the graph's
/// typed, confidence-graded edges — everything edge-specific folds into the weights
/// here, so the solver (`vorpal_ann::retrofit`) stays pure algebra with no graph
/// dependency. Every inclusion rule is structural or learned from this corpus:
///
/// - both endpoints must own semantic rows AND carry signal (unit anchors; a
///   zero-signal row must stay anchored, never invent a vector from neighbors);
/// - resolution grade: structural edges (confidence 0 — containment, certain by
///   construction) and grades ≥ Constrained pass; Heuristic/Unresolved are exactly
///   the noisy edges the retrofitting literature measures as harmful;
/// - hubs: rows whose included degree exceeds √(2m) hold out entirely — the same
///   null-model hub scale the communities builder derives (crates/kg/src/communities.rs);
/// - relation weight: LEARNED — the mean anchor cosine over each base edge type's
///   included directed edges, clamped at 0 (anchors are unit-or-zero, so cosine =
///   dot; a relation whose endpoints do not agree in the trained space contributes
///   nothing);
/// - grade factor: the resolver's own scale (confidence/100; structural = 1);
/// - degree normalization: SYMMETRIC 1/√(dᵢ·dⱼ) over final degrees — the normalized
///   graph-Laplacian form. Symmetry is load-bearing: the solver's Jacobi-descent
///   proof needs a symmetric weight matrix (Faruqui's 1/deg(i) is a Gauss–Seidel
///   prescription).
fn build_retro_edges(
  kg: &Kg,
  ids: &[u64],
  anchors: &[f32],
  dim: usize,
) -> Result<vorpal_ann::retrofit::FunctionalEdges, String> {
  use rayon::prelude::*;
  let n = ids.len();
  let graph = kg.graph();
  let mut row_of = vec![u32::MAX; kg.node_count()];
  for (row, &id) in ids.iter().enumerate() {
    row_of[id as usize] = row as u32;
  }
  let graded = |raw: u16| -> Option<f32> {
    let edge = vorpal_kg::EdgeType(raw);
    let confidence = edge.confidence();
    if confidence == 0 {
      return Some(1.0); // structural: certain by construction
    }
    let grade =
      vorpal_ingest::ResolutionGrade::from_confidence(vorpal_ingest::Confidence(confidence));
    (grade >= vorpal_ingest::ResolutionGrade::Constrained).then_some(confidence as f32 / 100.0)
  };
  let live: Vec<bool> = anchors
    .par_chunks(dim)
    .map(|row| row.iter().any(|v| *v != 0.0))
    .collect();
  // Enumerate row `i`'s included edges: both directions of every directed edge, each
  // undirected edge seen exactly once per endpoint (out view + in view), so degrees
  // and CSR slices are entirely ROW-LOCAL — no cross-row writes, determinism by
  // construction. `out_view` marks the out direction so per-type stats count each
  // DIRECTED edge exactly once.
  let each_included = |row: usize, f: &mut dyn FnMut(u32, u8, f32, bool)| {
    if !live[row] {
      return;
    }
    let node = ids[row] as u32;
    for (slot, &target) in graph.out_targets(node).iter().enumerate() {
      let neighbor = row_of[target as usize];
      if neighbor == u32::MAX || !live[neighbor as usize] {
        continue;
      }
      let raw = graph.out_edge_types(node)[slot];
      if let Some(grade) = graded(raw) {
        f(neighbor, vorpal_kg::EdgeType(raw).base().0 as u8, grade, true);
      }
    }
    for (slot, &source) in graph.in_targets(node).iter().enumerate() {
      let neighbor = row_of[source as usize];
      if neighbor == u32::MAX || !live[neighbor as usize] {
        continue;
      }
      let raw = graph.in_edge_types(node)[slot];
      if let Some(grade) = graded(raw) {
        f(neighbor, vorpal_kg::EdgeType(raw).base().0 as u8, grade, false);
      }
    }
  };

  // Pass 1 — preliminary degrees + per-base-type anchor-cosine sums, in fixed
  // 4096-row chunks folded in chunk order (the crate-standard deterministic shape).
  const CHUNK_ROWS: usize = 4096;
  let chunk_count = n.div_ceil(CHUNK_ROWS).max(1);
  let partials: Vec<(Vec<u32>, [f64; 256], [u64; 256])> = (0..chunk_count)
    .into_par_iter()
    .map(|chunk| {
      let start = chunk * CHUNK_ROWS;
      let end = ((chunk + 1) * CHUNK_ROWS).min(n);
      let mut degrees = vec![0u32; end.saturating_sub(start)];
      let mut dots = [0.0f64; 256];
      let mut counts = [0u64; 256];
      for row in start..end {
        let own = &anchors[row * dim..(row + 1) * dim];
        each_included(row, &mut |neighbor, base, _grade, out_view| {
          degrees[row - start] += 1;
          if out_view {
            let other = &anchors[neighbor as usize * dim..(neighbor as usize + 1) * dim];
            let mut dot = 0.0f64;
            for (a, b) in own.iter().zip(other) {
              dot += *a as f64 * *b as f64;
            }
            dots[base as usize] += dot;
            counts[base as usize] += 1;
          }
        });
      }
      (degrees, dots, counts)
    })
    .collect();
  let mut degree = Vec::with_capacity(n);
  let mut type_dot = [0.0f64; 256];
  let mut type_count = [0u64; 256];
  for (chunk_degrees, dots, counts) in &partials {
    degree.extend_from_slice(chunk_degrees);
    for slot in 0..256 {
      type_dot[slot] += dots[slot];
      type_count[slot] += counts[slot];
    }
  }
  // √(2m): each undirected edge contributed one degree per endpoint above.
  let two_m: u64 = degree.iter().map(|&d| d as u64).sum();
  if two_m == 0 {
    return Ok(vorpal_ann::retrofit::FunctionalEdges {
      offsets: vec![0; n + 1],
      targets: Vec::new(),
      weights: Vec::new(),
      relation: Vec::new(),
      outgoing: Vec::new(),
    });
  }
  let hub_threshold = two_m.isqrt();
  let hub: Vec<bool> = degree.iter().map(|&d| d as u64 > hub_threshold).collect();
  let mut type_weight = [0.0f32; 256];
  for slot in 0..256 {
    if type_count[slot] > 0 {
      type_weight[slot] = (type_dot[slot] / type_count[slot] as f64).max(0.0) as f32;
    }
  }

  // Pass 2 — final degrees under the full rule set (hub endpoints and zero-weight
  // relations drop), same deterministic chunk shape.
  let final_partials: Vec<Vec<u32>> = (0..chunk_count)
    .into_par_iter()
    .map(|chunk| {
      let start = chunk * CHUNK_ROWS;
      let end = ((chunk + 1) * CHUNK_ROWS).min(n);
      let mut degrees = vec![0u32; end.saturating_sub(start)];
      for row in start..end {
        if hub[row] {
          continue;
        }
        each_included(row, &mut |neighbor, base, _grade, _| {
          if !hub[neighbor as usize] && type_weight[base as usize] > 0.0 {
            degrees[row - start] += 1;
          }
        });
      }
      degrees
    })
    .collect();
  let mut final_degree = Vec::with_capacity(n);
  for chunk_degrees in &final_partials {
    final_degree.extend_from_slice(chunk_degrees);
  }

  let mut offsets = Vec::with_capacity(n + 1);
  let mut total = 0u64;
  offsets.push(0);
  for &d in &final_degree {
    total += d as u64;
    offsets.push(total);
  }
  let mut targets = Vec::with_capacity(total as usize);
  let mut weights = Vec::with_capacity(total as usize);
  let mut relation_column = Vec::with_capacity(total as usize);
  let mut outgoing = Vec::with_capacity(total as usize);
  // Fill — serial (row-local slices are irregular; one linear pass over the edges is
  // a small fraction of the sweeps that follow).
  for row in 0..n {
    if hub[row] {
      continue;
    }
    let own_degree = final_degree[row] as f64;
    each_included(row, &mut |neighbor, base, grade, out_view| {
      let relation = type_weight[base as usize];
      if hub[neighbor as usize] || relation <= 0.0 {
        return;
      }
      let neighbor_degree = final_degree[neighbor as usize] as f64;
      targets.push(neighbor);
      weights.push(relation * grade / ((own_degree * neighbor_degree).sqrt() as f32));
      relation_column.push(base);
      outgoing.push(out_view);
    });
  }
  Ok(vorpal_ann::retrofit::FunctionalEdges {
    offsets,
    targets,
    weights,
    relation: relation_column,
    outgoing,
  })
}

/// Which pairwise penalty the retrofit uses — BOTH forms ship (the plan's A/B
/// requirement) and production pins the measured winner. Identity: ‖xᵢ−xⱼ‖².
/// Functional: ‖a_r∘xᵢ−xⱼ‖² with per-relation DIAGONAL maps fitted closed-form from
/// this corpus's own edges (vorpal_ann::retrofit::fit_diagonal_maps). Not a runtime
/// knob: a form change is a code change, recorded in the persisted tier record so
/// the freshness gate retrains old tiers instead of serving mixed semantics.
// Both variants are shipped surface even though only the pinned one is constructed:
// which arm is live depends solely on RETROFIT_FORM, and the A/B flips it — the
// "unused" variant is the other measured form, not dead design.
#[allow(dead_code)]
#[derive(Clone, Copy, PartialEq, Eq)]
enum RetrofitForm {
  Identity,
  Functional,
}

impl RetrofitForm {
  fn label(self) -> &'static str {
    match self {
      RetrofitForm::Identity => "identity",
      RetrofitForm::Functional => "functional",
    }
  }
}

/// PINNED BY MEASUREMENT (2026-08-31, recorded in BENCHMARKS "penalty-form A/B"):
/// identity beat the diagonal functional form — kernel all-classes NDCG 0.298 vs
/// 0.267 (the fitted maps absorb the relation's systematic component and cancel the
/// neighbor pull that produced the gains); cpython tied. The functional
/// implementation ships measured, not speculative.
const RETROFIT_FORM: RetrofitForm = RetrofitForm::Identity;

/// Stage 2.5 (conditional, semantic-tier Stage 5): NUDGE-style constrained direct
/// step after the retrofit — ball radius = this scale × the retrofit's own median
/// displacement (content-derived; see `vorpal_ann::retrofit::nudge_into`).
///
/// MEASURED-AND-REJECTED (2026-08-31, scale sweep 0.5/1/2 at kernel + cpython,
/// recorded in BENCHMARKS "Stage 5"): 0.5 changed nothing at eval granularity;
/// 1 and 2 regressed BOTH corpora (kernel all 0.298 → 0.267, cpython 0.308 →
/// 0.261). Mechanism: the retrofit already CONVERGES to the Ψ-optimum over these
/// same grade-weighted edges — NUDGE's gains come from held-out query labels, an
/// independent signal; graph positives are the signal Stage 2 already absorbed,
/// so the extra step can only leave the optimum. The stage ships pinned off as a
/// measured seam (the functional-form precedent), not dead design.
const NUDGE_STAGE: Option<f64> = None;

/// The ACTIVE nudge scale: the pinned const, overridable only in measurement builds
/// (`bench-internals`) via `VORPAL_NUDGE_SCALE` — the same sweep seam as the BM25
/// gate's probe count; production binaries carry no knob.
fn active_nudge_scale() -> Option<f64> {
  #[cfg(feature = "bench-internals")]
  if let Ok(value) = std::env::var("VORPAL_NUDGE_SCALE") {
    return value.parse().ok().filter(|scale: &f64| *scale > 0.0);
  }
  NUDGE_STAGE
}

/// The retrofit-form label the ACTIVE configuration produces — what the tier record
/// carries and the freshness gate compares. With the nudge enabled the label carries
/// the stage and its scale, so flipping it (a code change) retrains old tiers
/// instead of ever serving mixed row semantics.
fn active_retrofit_label() -> String {
  match active_nudge_scale() {
    Some(scale) => format!("{}+nudge@{scale}", RETROFIT_FORM.label()),
    None => RETROFIT_FORM.label().to_string(),
  }
}

/// Learned-tier Stage 2: refine the Tier-1 rows over the graph before the tier
/// builds. Any problem is a typed reason for the AUTO-DISABLE seam — the caller
/// states it in provenance and builds unrefined; a retrofit issue may never fail a
/// build or bend a result silently.
fn retrofit_learned_rows(
  kg: &Kg,
  index_dir: &Path,
  embedder: &ActiveEmbedder,
) -> Result<RetrofittedRows, String> {
  use rayon::prelude::*;
  let dim = embedder.dim();
  let ids = semantic_row_ids(kg);
  let n = ids.len();
  if n == 0 || dim == 0 {
    return Err("no semantic rows to refine".to_string());
  }
  let region_bytes = n
    .checked_mul(dim)
    .and_then(|cells| cells.checked_mul(4))
    .ok_or_else(|| "region size overflow".to_string())?;
  // Typed insufficient-space error BEFORE creating multi-GB scratch (the scratch
  // law); platforms without statvfs proceed and rely on create() failing cleanly.
  #[cfg(unix)]
  {
    let needed = (region_bytes as u64).saturating_mul(3);
    let available = vorpal_mem::available_disk_bytes(index_dir)
      .map_err(|e| format!("scratch space probe: {e}"))?;
    if available < needed {
      return Err(format!(
        "insufficient scratch space: 3 retrofit regions need {needed} bytes, {available} available"
      ));
    }
  }
  let scratch_dir = index_dir.join("retrofit.scratch");
  let _ = fs::remove_dir_all(&scratch_dir);
  fs::create_dir_all(&scratch_dir).map_err(|e| format!("creating {}: {e}", scratch_dir.display()))?;
  let region = |name: &str, access: vorpal_mem::AccessPattern| {
    vorpal_mem::ScratchMmap::create(&scratch_dir.join(name), region_bytes, access)
      .map_err(|e| format!("scratch region {name}: {e}"))
  };
  // Q is swept sequentially every sweep; the X double-buffer is read by NEIGHBOR
  // index — random — in alternating roles.
  let mut anchors = region("q.f32", vorpal_mem::AccessPattern::Sequential)?;
  let mut refined = region("a.f32", vorpal_mem::AccessPattern::Random)?;
  let mut spare = region("b.f32", vorpal_mem::AccessPattern::Random)?;
  let embed_root = embedding_root(kg);
  {
    let rows: &mut [f32] = bytemuck::try_cast_slice_mut(anchors.as_mut_bytes())
      .map_err(|e| format!("scratch alignment: {e}"))?;
    rows
      .par_chunks_mut(dim)
      .enumerate()
      .for_each(|(i, row)| embed_node_into(kg, embedder, ids[i], row, &embed_root));
  }
  vorpal_kg::phase_stamp("retrofit: anchors embedded");
  let edges = {
    let anchor_rows: &[f32] =
      bytemuck::try_cast_slice(anchors.as_bytes()).map_err(|e| format!("scratch alignment: {e}"))?;
    build_retro_edges(kg, &ids, anchor_rows, dim)?
  };
  vorpal_kg::phase_stamp("retrofit: edges assembled");
  refined.as_mut_bytes().copy_from_slice(anchors.as_bytes());
  let report = {
    let anchor_rows: &[f32] =
      bytemuck::try_cast_slice(anchors.as_bytes()).map_err(|e| format!("scratch alignment: {e}"))?;
    let x: &mut [f32] = bytemuck::try_cast_slice_mut(refined.as_mut_bytes())
      .map_err(|e| format!("scratch alignment: {e}"))?;
    let next: &mut [f32] =
      bytemuck::try_cast_slice_mut(spare.as_mut_bytes()).map_err(|e| format!("scratch alignment: {e}"))?;
    match RETROFIT_FORM {
      RetrofitForm::Identity => {
        // The identity penalty ignores direction and relation ids: fold to the
        // plain weighted CSR (a column move, not a copy).
        let identity_edges = vorpal_ann::retrofit::RetroEdges {
          offsets: edges.offsets.clone(),
          targets: edges.targets.clone(),
          weights: edges.weights.clone(),
        };
        vorpal_ann::retrofit::retrofit_into(anchor_rows, x, next, dim, &identity_edges)?
      }
      RetrofitForm::Functional => {
        let maps = vorpal_ann::retrofit::fit_diagonal_maps(anchor_rows, dim, &edges)?;
        vorpal_kg::phase_stamp(&format!(
          "retrofit: {} relations fitted diagonal maps",
          maps.iter().flatten().count()
        ));
        vorpal_ann::retrofit::retrofit_functional_into(anchor_rows, x, next, dim, &edges, &maps)?
      }
    }
  };
  vorpal_kg::phase_stamp(&format!(
    "retrofit({}): {} sweeps, Ψ {:.6e} → {:.6e}, {} edges",
    RETROFIT_FORM.label(),
    report.sweeps,
    report.initial_psi,
    report.final_psi,
    edges.targets.len()
  ));
  // Stage 2.5 (conditional): one NUDGE step in Stage-2 space — γ scaled by the
  // retrofit's own median displacement, refined → spare, then adopted back. A zero
  // median (retrofit moved nothing) skips the stage and says so.
  if let Some(scale) = active_nudge_scale() {
    let (median, gamma, moved) = {
      let anchor_rows: &[f32] = bytemuck::try_cast_slice(anchors.as_bytes())
        .map_err(|e| format!("scratch alignment: {e}"))?;
      let x2: &[f32] = bytemuck::try_cast_slice(refined.as_bytes())
        .map_err(|e| format!("scratch alignment: {e}"))?;
      let median = vorpal_ann::retrofit::median_displacement(anchor_rows, x2, dim)?;
      if median > 0.0 {
        let gamma = scale * median;
        let out: &mut [f32] = bytemuck::try_cast_slice_mut(spare.as_mut_bytes())
          .map_err(|e| format!("scratch alignment: {e}"))?;
        let nudge_edges = vorpal_ann::retrofit::RetroEdges {
          offsets: edges.offsets.clone(),
          targets: edges.targets.clone(),
          weights: edges.weights.clone(),
        };
        let nudge = vorpal_ann::retrofit::nudge_into(x2, out, dim, &nudge_edges, gamma)?;
        (median, gamma, nudge.moved)
      } else {
        (0.0, 0.0, 0)
      }
    };
    if median > 0.0 {
      refined.as_mut_bytes().copy_from_slice(spare.as_bytes());
      vorpal_kg::phase_stamp(&format!(
        "retrofit: nudge@{scale} γ={gamma:.6e} moved {moved} rows (median displacement {median:.6e})"
      ));
    } else {
      vorpal_kg::phase_stamp("retrofit: nudge skipped (zero median displacement)");
    }
  }
  Ok(RetrofittedRows {
    anchors,
    refined,
    spare,
    dim,
  })
}

fn build_ann(
  kg: &Kg,
  out: &Path,
  base_stamp: u64,
  embedder: &ActiveEmbedder,
  retrofitted: Option<&RetrofittedRows>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
  vorpal_kg::phase_stamp("ann: build start");
  let dim = embedder.dim();
  let root = embedding_root(kg);
  let ids = semantic_row_ids(kg);
  let row_ids = ids.clone();
  // Rows embed straight into the index's storage (i8 codes at scale, in parallel): the
  // full-precision matrix never materializes — 2.9 GB of pure transient at kernel scale.
  // Under Stage 2 the rows come from the retrofit's refined scratch matrix instead of a
  // re-embed (same row universe by construction).
  let index = match retrofitted {
    Some(rows) => AnnIndex::build_rows(dim, ids, |i, row| {
      let source = rows.row(i);
      if source.len() == row.len() {
        row.copy_from_slice(source);
      } else {
        row.fill(0.0);
      }
    }),
    None => AnnIndex::build_rows(dim, ids, |i, row| {
      embed_node_into(kg, embedder, row_ids[i], row, &root)
    }),
  };
  let calibration = index.calibration();
  vorpal_kg::phase_stamp("ann: save start");
  index
    .with_base_stamp(base_stamp)
    .save(&out.join("ann.bin"))?;
  // Persist the Phase-2a calibration beside the tier so provenance survives the process:
  // a sidecar line the model-provenance reader ignores and humans/tools can read.
  if let Some((l_build, recall)) = calibration {
    let _ = fs::write(
      out.join("ann.calibration.json"),
      format!("{{\"l_build\":{l_build},\"pool_recall\":{recall:.4},\"probes\":32,\"k\":10,\"search_l\":200}}\n"),
    );
  }
  // The per-file identity map for this generation — what lets a later search remap
  // unchanged files and overlay changed ones instead of rebuilding (§ overlay).
  annfiles::save(out, base_stamp, &annfiles::file_runs_of(kg))?;
  vorpal_kg::phase_stamp("ann: done");
  Ok(())
}

/// A discovered `.vorpal/index` a search can bank products into: where its products live, and
/// how to spell file keys the way that index's `build_index` runs will (§3.4).
struct WarmRoot {
  products_dir: PathBuf,
  /// Canonical directory containing `.vorpal` — file keys are derived relative to it.
  canonical_root: PathBuf,
  /// The manifest's path-spelling prefix (`"./"`, `""`, `"sub/dir/"`, an absolute base…):
  /// `key(file) = prefix + (file relative to root)` reproduces the walker's exact strings.
  key_prefix: String,
}

/// Process-wide cache of discovered warm roots (one per index root encountered).
static WARM_ROOTS: OnceLock<Mutex<HashMap<PathBuf, Option<Arc<WarmRoot>>>>> = OnceLock::new();

/// Bank one file's extraction product into the nearest existing `.vorpal/index` cache — the
/// **search-feeds-index** hook (§3.4). A search has already walked, read, and matched the
/// file; this persists the extraction so the next `vorpal index` replays it instead of
/// re-parsing (products are self-validating via their source stat stamp).
///
/// Deliberately conservative: it only feeds an index that already exists (searches never
/// create index state in un-indexed trees), silently skips unsupported or unreadable files,
/// and returns whether a product was written. A fresh product for the file's current stat is
/// left untouched, so repeated matches are near-free.
pub fn warm_product_cache(file: &Path) -> io::Result<bool> {
  let Ok(canonical) = file.canonicalize() else {
    return Ok(false);
  };
  let Some(index_root) = find_index_root(&canonical) else {
    return Ok(false);
  };
  let Some(warm) = warm_root_for(index_root)? else {
    return Ok(false);
  };
  let Ok(rel) = canonical.strip_prefix(&warm.canonical_root) else {
    return Ok(false);
  };
  let keyed = format!("{}{}", warm.key_prefix, rel.display());

  let meta = fs::metadata(&canonical)?;
  let mtime_ns = meta
    .modified()?
    .duration_since(UNIX_EPOCH)
    .map(|d| d.as_nanos() as u64)
    .unwrap_or(0);
  let cache = warm.products_dir.join(cache_file_name(&keyed));
  if let Ok(existing) = load_product(&cache) {
    if existing.source_size == meta.len() && existing.source_mtime_ns == mtime_ns {
      return Ok(false);
    }
  }

  let Ok(source) = fs::read_to_string(&canonical) else {
    return Ok(false);
  };
  let extractor = OutlineExtractor::new().map_err(io::Error::other)?;
  // Bank products written by a broken binary replay into healthy runs (same digests, same
  // source) — the self-check keeps a bad build from feeding the bank at all. Scoped to the
  // one language this product is for (memoized process-wide, like the index path).
  let stat_for_lang = [vorpal_ingest::FileStat {
    path: keyed.clone(),
    size: 0,
    mtime_ns: 0,
  }];
  vorpal_ingest::verify_extraction_for_manifest(&extractor, &stat_for_lang)
    .map_err(io::Error::other)?;
  let Some(mut product) = extractor.extract_product(&keyed, &source) else {
    return Ok(false);
  };
  product.source_size = meta.len();
  product.source_mtime_ns = mtime_ns;
  fs::create_dir_all(&warm.products_dir)?;
  save_product(&cache, &product)?;
  Ok(true)
}

/// The nearest ancestor directory holding an existing default-location index — a generation
/// root (carrying `CURRENT`) or a legacy flat dir (carrying `manifest.bin` directly).
fn find_index_root(file: &Path) -> Option<PathBuf> {
  let mut dir = file.parent()?;
  loop {
    let index_dir = dir.join(".vorpal/index");
    if index_dir.join("CURRENT").is_file() || index_dir.join("manifest.bin").is_file() {
      return Some(dir.to_path_buf());
    }
    dir = dir.parent()?;
  }
}

/// Get-or-build the cached [`WarmRoot`] for an index root. `None` (also cached) means the
/// root's key spelling could not be established — warming is skipped rather than guessed.
fn warm_root_for(index_root: PathBuf) -> io::Result<Option<Arc<WarmRoot>>> {
  let roots = WARM_ROOTS.get_or_init(|| Mutex::new(HashMap::new()));
  if let Some(cached) = roots.lock().unwrap().get(&index_root) {
    return Ok(cached.clone());
  }
  let built = build_warm_root(&index_root)?.map(Arc::new);
  let mut lock = roots.lock().unwrap();
  Ok(lock.entry(index_root).or_insert_with(|| built).clone())
}

/// Establish how this index spells file keys: resolve one manifest entry to its canonical
/// path, take its root-relative form, and split the entry string into
/// `prefix + root-relative suffix` (`"./"` for `vorpal index .`, `"sub/"` for
/// `vorpal index sub`, an absolute base for absolute invocations). String-suffix matching —
/// not path joining — so spelling variants like `"./"` survive. An entry that cannot be
/// verified (deleted file, `..`/symlink spelling) yields `None` and warming stays off for
/// this root rather than guessing keys.
fn build_warm_root(index_root: &Path) -> io::Result<Option<WarmRoot>> {
  let index_dir = index_root.join(".vorpal").join("index");
  // Key spelling comes from the live generation's manifest; the bank itself stays at the
  // root-level `products/` inbox, which every generation's build consumes.
  let manifest = Manifest::load(&vorpal_kg::resolve_index_dir(&index_dir).join("manifest.bin"))?;
  let canonical_root = index_root.canonicalize()?;
  for entry in manifest.entries() {
    let Ok(canonical_entry) = index_root.join(&entry.path).canonicalize() else {
      continue;
    };
    let Ok(rel) = canonical_entry.strip_prefix(&canonical_root) else {
      continue;
    };
    let rel = rel.display().to_string();
    if let Some(prefix) = entry.path.strip_suffix(&rel) {
      return Ok(Some(WarmRoot {
        products_dir: index_dir.join("products"),
        canonical_root,
        key_prefix: prefix.to_string(),
      }));
    }
  }
  Ok(None)
}

/// The fused ranking's channel names, positional: the first three always fuse; the
/// fourth exists only where the per-corpus warm-time gate enabled it
/// (`PersistedTierRecord::bm25`, written by [`ensure_bm25_gate`]); rank vectors zip
/// against this list, so a 3-list fusion simply never reaches the fourth label.
/// The global-default story (the pin this gate replaces): at kernel scale the BM25
/// list regressed every graded gate — true answers tokenize to subwords exact-token
/// BM25 cannot see (sock ≠ socket) — while cpython IMPROVED (all 0.308 → 0.392);
/// both rounds recorded in BENCHMARKS "Stage 4".
/// The fifth is the doc-side DENSE channel (`dense.rs`): present only where a
/// fresh sidecar exists and its per-corpus gate enabled it; when it fuses
/// without BM25, an EMPTY fourth list holds BM25's position (an empty list adds
/// no RRF mass), so ranks stay positional against this table.
const SEARCH_CHANNELS: [&str; 5] = ["name", "vector", "graph", "bm25", "dense"];

/// One fused-ranking row: `(node id, RRF score, per-channel 0-based ranks)` — the shape
/// [`Searcher::run`] returns.
type FusedHit = (u64, f32, Vec<Option<usize>>);

/// One query's three ranked candidate channels at a pool depth, with the semantic
/// channel's exact distances kept alongside (ascending, so positive-similarity rows are
/// a prefix — see [`POSITIVE_BOUNDARY`]).
struct Channels {
  named: Vec<u64>,
  semantic: Vec<u64>,
  /// Squared L2 to the query, aligned with `semantic`.
  semantic_dist2: Vec<f32>,
  by_degree: Vec<u64>,
  /// Okapi BM25 over name tokens (semantic-tier Stage 4): union semantics — partial
  /// matches score — ranked (score desc, id asc). The fourth rank-only RRF list,
  /// appended after graph so tier-0 name hits keep 1/(60+0): BM25 adds mass, never
  /// demotes the short-query supremacy invariant.
  bm25: Vec<u64>,
  /// The doc-side dense channel (`dense.rs`): encoder cosine over the sidecar's
  /// covered definitions — empty when no query embedding was supplied.
  dense: Vec<u64>,
}

/// The orthogonality boundary of the semantic space: embeddings are L2-normalized, so
/// for unit query q and unit row v, `‖q−v‖² = 2 − 2·(q·v)` — squared distance below 2.0
/// means strictly positive similarity; at/above it, zero or negative. An algebraic
/// identity of the normalization, not a tunable. The multi-phrase rungs trim each
/// phrase's semantic RANK list here: rows past the boundary are the no-signal tie
/// region Stage 0 diagnosed and carry no rank information worth fusing. (What counts as
/// a conjunction MATCH is decided lexically — see `node_lexical_support_bits` — because
/// hashed-bucket collisions make vector-space sign meaningless as a match criterion.
/// Measured a THIRD time in the TRAINED space (bench `positivity`, kernel scale):
/// real phrases are "positive" to 46–52% of all rows and NONSENSE phrases to 55–57% —
/// OOV gram composition lands near the corpus's central direction — so sign can never
/// gate a match under any tier; see docs/wip/BENCHMARKS.md "Conjunction support under
/// the learned tier".)
const POSITIVE_BOUNDARY: f32 = 2.0;

/// The single-phrase rerank/fusion pool for a requested k — ONE source of truth shared
/// by the single-phrase path and the conjunction's shallow rung (which therefore does,
/// by construction, exactly the work a single-phrase query of the same k does). The
/// values predate this seam (IMPROVEMENTS #9 era); re-deriving them is its own sweep
/// item, not something to silently duplicate or adjust.
fn rerank_pool(k: usize) -> usize {
  (k * 4).max(50)
}

/// `ann.calib` format version — the warm-time semantic-engine calibration sidecar.
const ANN_CALIB_VERSION: u32 = 1;
/// `ann.calib` magic.
const ANN_CALIB_MAGIC: &[u8; 4] = b"VCAL";

/// Measure, on THIS machine over THIS index's rows, the fetch width where the beam
/// stops being faster than the flat exact scan — the mid-range routing crossover,
/// LEARNED from ingested data at warm time. The recorded sweep in docs/wip/BENCHMARKS.md
/// motivates the design; its numbers stay out of the product.
///
/// Protocol (statistical methodology, not tuned quantities): 3 deterministic probe
/// queries (seeded splitmix64 unit vectors — the seed is an identifier, any fixed value
/// defines a valid protocol), median of 3 reps per point. The scan reference is
/// `exhaustive_semantic` at take = 1 — the n-driven floor; real scans cost at least
/// this, so the error direction always prefers the EXACT engine. Beam probes walk the
/// geometric ×2 ladder 1, 2, 4, … and stop at the first width whose median exceeds the
/// scan reference — early exit keeps probing cheap by construction, because every probe
/// before the crossover is below the crossover. No crossing before `node_count` → the
/// structural floor stands.
fn calibrate_semantic_cutover(kg: &Kg, ann: &AnnIndex, embedder: &ActiveEmbedder) -> usize {
  let dim = embedder.dim();
  let node_count = kg.node_count();
  let rows = semantic_row_ids(kg);
  if rows.is_empty() || node_count == 0 {
    return node_count.max(1);
  }
  // Deterministic probe vectors: splitmix64 → uniform [-1, 1) components → L2-normalize.
  let mut state = 0x5EEDu64;
  let mut next = move || {
    state = state.wrapping_add(0x9E3779B97F4A7C15);
    let mut z = state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
    z ^ (z >> 31)
  };
  let mut queries: Vec<Vec<f32>> = Vec::with_capacity(3);
  for _ in 0..3 {
    let mut vector: Vec<f32> = Vec::with_capacity(dim);
    for _ in 0..dim {
      vector.push((next() >> 11) as f32 / (1u64 << 53) as f32 * 2.0 - 1.0);
    }
    let norm = vector.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
      for x in &mut vector {
        *x /= norm;
      }
    }
    queries.push(vector);
  }
  let median = |samples: &mut Vec<f64>| -> f64 {
    samples.sort_by(f64::total_cmp);
    samples.get(samples.len() / 2).copied().unwrap_or(0.0)
  };
  let embed_root = embedding_root(kg);
  let mut scan_samples = Vec::new();
  for _ in 0..3 {
    for query in &queries {
      let started = std::time::Instant::now();
      // Measure the tier's PRODUCTION exhaustive engine, mirroring `channel_lists`'s
      // dispatch exactly: the learned tier walks the persisted codes (a full-population
      // re-embed through the model measured 191 s of a 405 s kernel warm before this
      // split); the lexical tier re-embeds (hash-cheap, byte-exact). A learned FLAT
      // tier has no codes and falls through to the re-embed scan — exactly what its
      // queries pay.
      let code_walk = match embedder {
        ActiveEmbedder::Learned(_) => ann.scan_codes(query, 1, |_| true),
        ActiveEmbedder::Lexical(_) => None,
      };
      if code_walk.is_none() {
        std::hint::black_box(vorpal_ann::exhaustive_semantic(
          dim,
          &rows,
          |i, row| embed_node_into(kg, embedder, rows[i], row, &embed_root),
          query,
          1,
        ));
      }
      std::hint::black_box(code_walk);
      scan_samples.push(started.elapsed().as_secs_f64());
    }
  }
  let scan_floor = median(&mut scan_samples);
  let mut width = 1usize;
  while width < node_count {
    let mut beam_samples = Vec::new();
    for _ in 0..3 {
      for query in &queries {
        let started = std::time::Instant::now();
        std::hint::black_box(ann.search(query, width));
        beam_samples.push(started.elapsed().as_secs_f64());
      }
    }
    if median(&mut beam_samples) > scan_floor {
      return width.clamp(1, node_count);
    }
    width = width.saturating_mul(2);
  }
  node_count
}

/// Persist the calibrated crossover beside the tier it was measured on: 32 bytes —
/// magic, version, node-segment stamp, crossover — sealed by an xxh3 self-checksum.
/// Machine-local like every warm sidecar and EXCLUDED from byte-identity determinism
/// gates (it is a measurement); a stale or torn file reads as absent, never as a value.
fn write_ann_calibration(
  index_dir: &Path,
  stamp: u64,
  crossover: usize,
) -> Result<(), Box<dyn Error>> {
  let mut bytes = Vec::with_capacity(32);
  bytes.extend_from_slice(ANN_CALIB_MAGIC);
  bytes.extend_from_slice(&ANN_CALIB_VERSION.to_le_bytes());
  bytes.extend_from_slice(&stamp.to_le_bytes());
  bytes.extend_from_slice(&(crossover as u64).to_le_bytes());
  let checksum = xxhash_rust::xxh3::xxh3_64(&bytes);
  bytes.extend_from_slice(&checksum.to_le_bytes());
  let tmp = index_dir.join("ann.calib.tmp");
  fs::write(&tmp, &bytes)?;
  fs::rename(tmp, index_dir.join("ann.calib"))?;
  Ok(())
}

/// The calibrated crossover for `stamp`'s tier, if a valid one is persisted — clamped
/// to `[1, node_count]`. Anything else (absent, torn, foreign stamp, bad checksum,
/// wrong version) is None, and routing stands on the structural floor.
fn load_ann_calibration(index_dir: &Path, stamp: u64, node_count: usize) -> Option<usize> {
  let bytes = fs::read(index_dir.join("ann.calib")).ok()?;
  if bytes.len() != 32 || bytes.get(0..4)? != ANN_CALIB_MAGIC {
    return None;
  }
  let field = |range: std::ops::Range<usize>| -> Option<u64> {
    Some(u64::from_le_bytes(bytes.get(range)?.try_into().ok()?))
  };
  let version = u32::from_le_bytes(bytes.get(4..8)?.try_into().ok()?);
  if version != ANN_CALIB_VERSION
    || field(8..16)? != stamp
    || field(24..32)? != xxhash_rust::xxh3::xxh3_64(bytes.get(0..24)?)
  {
    return None;
  }
  Some((field(16..24)? as usize).clamp(1, node_count.max(1)))
}

/// Reciprocal Rank Fusion constant (the standard K=60): dampens the head of each list so no
/// single signal dominates, while rank-1 placements still carry the most weight.
const RRF_K: f32 = 60.0;

/// How the encoder's verdict combines with the fusion in `rerank_with_encoder`:
/// `Reorder` = the encoder's cosine order REPLACES the fused order of the tail (rank-0
/// pinned); `BlendPinned` = the encoder enters as one more reciprocal-rank list over the
/// tail, added to each hit's fused RRF mass (rank-0 pinned); `Blend` = the same over the
/// whole pool, rank 0 included. Production pins `Reorder` by measurement (BENCHMARKS
/// "Rerank mode A/B", 2026-09-02): the blends recover most of the kernel's
/// short-keyword loss (0.143 → 0.184 NDCG) but damp exactly the deep pulls that buy
/// recall elsewhere (cpython recall@5 0.50 → 0.33, this repo 0.75 → 0.55) — no mode
/// dominates, and per-index `vorpal tune` remains the instrument that decides whether
/// the encoder is on at all. `VORPAL_RERANK_MODE=reorder|blend-pinned|blend` sweeps
/// it under `bench-internals`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
// The non-default modes are selected only by the bench-internals sweep seam.
#[cfg_attr(not(feature = "bench-internals"), allow(dead_code))]
enum RerankMode {
  Reorder,
  BlendPinned,
  Blend,
  /// Bench-only (`VORPAL_RERANK_MODE=off`): the encoder stays open (the dense
  /// channel still embeds the query) but the fused order is returned untouched —
  /// the "channel without rerank" arm of the dense A/B.
  Off,
}

const RERANK_MODE: RerankMode = RerankMode::Reorder;

fn rerank_mode() -> RerankMode {
  #[cfg(feature = "bench-internals")]
  if let Ok(value) = std::env::var("VORPAL_RERANK_MODE") {
    return match value.as_str() {
      "reorder" => RerankMode::Reorder,
      "blend-pinned" => RerankMode::BlendPinned,
      "blend" => RerankMode::Blend,
      "off" => RerankMode::Off,
      _ => RERANK_MODE,
    };
  }
  RERANK_MODE
}

/// The dense list's position in [`SEARCH_CHANNELS`] — the one list the
/// [`DenseFusion`] laws touch.
const DENSE_LIST: usize = 4;

/// How the doc-side dense list fuses. RRF is scale-free: a list's rank-r mass is
/// 1/(K + r) whether the list was drawn from 25 K rows or 700 K, so as the sidecar's
/// coverage grows, the dense head fills with high-cosine lookalikes that carry the same
/// mass as the exact-name and graph evidence — measured on the kernel as a quality slide
/// with coverage (all-NDCG@10 0.345 at 24 K rows → 0.308 at 143 K; BENCHMARKS "Doc-side
/// dense channel", "always-on fill"). Each law below is DERIVED from the sidecar's own
/// counts or the query's own scores — no tuned constant — and every law leaves the other
/// four lists at the fusion's K and is byte-identical to the plain fusion when no dense
/// list is present. `VORPAL_DENSE_FUSION=flat|quantile|subset|cutoff|gap|degree` sweeps
/// them under `bench-internals`; production pins [`DENSE_FUSION`] by the coverage-curve
/// measurement recorded in BENCHMARKS "Size-aware dense fusion".
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
// The non-default laws are selected only by the bench-internals sweep seam.
#[cfg_attr(not(feature = "bench-internals"), allow(dead_code))]
enum DenseFusion {
  /// The shipped fusion: one more reciprocal-rank list at K, full pool depth.
  Flat,
  /// (a) Population-scaled RANK — the independent-prefilter law. A rank r in a list
  /// drawn from n_c covered rows is the quantile r/n_c; its rank-equivalent in the
  /// list's own eligible population n_e is r·n_e/n_c, and that is the rank K sees:
  /// mass 1/(K + r·n_e/n_c). Units: rows/rows. The head keeps 1/K, the tail damps by
  /// the coverage fraction, and at complete coverage the law is `Flat`.
  Quantile,
  /// (a') Population-scaled K — the prefix-conjunction law. A sidecar filled in
  /// coverage order is the conjunction of two rankings (referential in-degree, then
  /// cosine), so a list drawn from the fraction φ = n_c/n_e of its population has a
  /// rank offset that shrinks with it: K_dense = K·φ. At complete coverage `Flat`.
  Subset,
  /// (b) Rank cutoff — the equal-share law. The pool depth (`rerank_pool(k)`) is
  /// shared equally among the non-empty lists; the dense list nominates at most its
  /// share m = pool / lists, and nothing below rank m adds mass.
  Cutoff,
  /// (c) Score-gap admission — self-referenced. Over the dense list's own rescored
  /// candidates (`RESCORE_OVERSAMPLE × pool` rows), a row is admitted only when its
  /// cosine sits closer to the query's best row than to the candidates' median:
  /// c ≥ (c_max + c_median)/2 — the upper half of the query's own score range.
  Gap,
  /// (e) Degree support. The graph list (the in-degree disambiguator) ranks the union
  /// of the name-matched and dense candidates, so a dense nominee that is a referenced
  /// hub earns graph mass and a lookalike nobody calls does not — the fusion's own
  /// graph evidence applied to the dense list instead of a weight on it.
  Degree,
  /// (f) Hub admission — the coverage order made explicit. The fill covers
  /// definitions in referential in-degree order, so every row a later checkpoint adds
  /// has in-degree ≤ every row before it: the lookalikes that pile into the dense head
  /// as coverage grows are, by construction, the least-referenced covered rows. The
  /// dense list keeps, of its cosine top-pool, the equal share m = pool / lists with the
  /// highest referential in-degree (cosine order preserved among them) — at any
  /// coverage, the dense list a young fill would have produced.
  Hub,
  /// (g) Support admission — the conjunction support law applied to the dense list.
  /// A dense row is admitted when a SIBLING list also surfaces it (independent
  /// evidence: name, semantic, graph or BM25 at pool depth) or when it falls in the
  /// dense channel's equal share of most-referenced rows (`Hub`). A single-source
  /// nomination needs the structural evidence; a corroborated one does not — so the
  /// cosine-best answers of a small corpus keep their place (vorpal: `Hub` alone
  /// dropped `ingest_traces` at dense#1, in-degree 1, vector#85) while a large
  /// corpus's dense-only lookalikes, uncorroborated and unreferenced, stay out.
  Support,
}

/// The production law: `Flat`, PINNED by the coverage-curve measurement (BENCHMARKS
/// "Size-aware dense fusion", 2026-09-03; kernel sidecars at 40.7 K / 99.3 K / 190.7 K
/// of 716.7 K eligible rows, cpython and vorpal at complete referenced coverage).
/// What the sweep established: (1) with the rerank on, the fusion decides only pool
/// MEMBERSHIP — the encoder's cosine order over the pool sets the final positions —
/// and under `Flat` every labelled answer is already in the pool at every coverage; the
/// v0.7.0 8-query slide (0.345 → 0.245 all-NDCG@10) is the answers' own dense rank
/// slipping behind newly covered, higher-cosine, lower-in-degree lookalikes. (2) `Hub`
/// is the one derived law that repairs it (0.415 / 0.340 / 0.332, ≥ the 0.313 gate at
/// every point) but it drops cosine-best low-in-degree answers elsewhere (vorpal all
/// 0.685 → 0.542) and on the expanded 54-query kernel set trades the short-keyword gain
/// for the conjunctive class (net −0.009 at every point). (3) On the 54-query set the
/// shipped fusion does not slide at all (0.404 / 0.416 / 0.414) and no law beats it
/// beyond one query's worth. The population-scaled, cutoff and score-gap laws are
/// measured-and-rejected; the seam stays for the next sweep.
const DENSE_FUSION: DenseFusion = DenseFusion::Flat;

fn dense_fusion() -> DenseFusion {
  #[cfg(feature = "bench-internals")]
  if let Ok(value) = std::env::var("VORPAL_DENSE_FUSION") {
    return match value.as_str() {
      "flat" => DenseFusion::Flat,
      "quantile" => DenseFusion::Quantile,
      "subset" => DenseFusion::Subset,
      "cutoff" => DenseFusion::Cutoff,
      "gap" => DenseFusion::Gap,
      "degree" => DenseFusion::Degree,
      "hub" => DenseFusion::Hub,
      "support" => DenseFusion::Support,
      _ => DENSE_FUSION,
    };
  }
  DENSE_FUSION
}

/// The dense list's cut under `law` from its rescored candidates (cosine descending),
/// `pool` deep at most: `Gap` admits the upper half of the query's own score range,
/// `[c_median, c_max]`; every other law takes the first `pool` — byte-identical to
/// [`dense::DenseSidecar::search`].
fn admit_dense(law: DenseFusion, scored: &[(u64, f32)], pool: usize) -> Vec<u64> {
  let admitted = if law == DenseFusion::Gap && !scored.is_empty() {
    let top = scored[0].1;
    let median = if scored.len() % 2 == 1 {
      scored[scored.len() / 2].1
    } else {
      (scored[scored.len() / 2 - 1].1 + scored[scored.len() / 2].1) / 2.0
    };
    let floor = (top + median) / 2.0;
    scored.iter().take_while(|(_, cosine)| *cosine >= floor).count()
  } else {
    scored.len()
  };
  scored
    .iter()
    .take(admitted.min(pool))
    .map(|(id, _)| *id)
    .collect()
}

/// Which definitions feed the learned tier's distributional training corpus
/// (`train_learned_model`): every non-import node; every non-import node except
/// macros; or every kind capped at the population of the largest CALLABLE kind
/// (deterministic stride subsample) — the balance rule is data-derived, no constant.
/// Measured 2026-09-02 (BENCHMARKS "Learned-tier kind policy A/B"): excluding macros
/// loses on the kernel (macros are retrieval targets and real signal); balance lifts
/// the kernel (all 0.295 → 0.313 NDCG@10, protected short-keyword 0.202 → 0.222), is
/// bit-identical on cpython, and costs this repo one conjunctive query (0.559 →
/// 0.529). `All` stays pinned pending the owner's decision; pinning `Balance` must
/// also carry the policy label in `ann.model.json` so freshness retrains on a flip.
/// `VORPAL_LEARNED_KIND_POLICY=all|exclude-macros|balance` sweeps it under
/// `bench-internals`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
// The non-default policies are selected only by the bench-internals sweep seam.
#[cfg_attr(not(feature = "bench-internals"), allow(dead_code))]
enum TrainKindPolicy {
  All,
  ExcludeMacros,
  BalanceToCallables,
}

/// PINNED `BalanceToCallables` by measurement (owner decision 2026-09-02, BENCHMARKS
/// "Learned-tier kind policy A/B"): kernel learned tier 0.295 → 0.313 NDCG@10 (first
/// time above lexical on the 8.9 M-node graph), protected short-keyword 0.202 →
/// 0.222; cpython bit-identical; this repo −0.03 on one conjunctive query (the stated
/// exception). The label rides in `ann.model.json` (`train_kinds`) so a policy flip
/// retrains every existing learned tier.
const LEARNED_TRAIN_KINDS: TrainKindPolicy = TrainKindPolicy::BalanceToCallables;

impl TrainKindPolicy {
  fn label(self) -> &'static str {
    match self {
      TrainKindPolicy::All => "all",
      TrainKindPolicy::ExcludeMacros => "exclude-macros",
      TrainKindPolicy::BalanceToCallables => "balance",
    }
  }
}

/// The training kind policy label written into the tier record and demanded by the
/// learned freshness gate.
fn active_train_kinds_label() -> &'static str {
  train_kind_policy().label()
}

fn train_kind_policy() -> TrainKindPolicy {
  #[cfg(feature = "bench-internals")]
  if let Ok(value) = std::env::var("VORPAL_LEARNED_KIND_POLICY") {
    return match value.as_str() {
      "all" => TrainKindPolicy::All,
      "exclude-macros" => TrainKindPolicy::ExcludeMacros,
      "balance" => TrainKindPolicy::BalanceToCallables,
      _ => LEARNED_TRAIN_KINDS,
    };
  }
  LEARNED_TRAIN_KINDS
}

/// Fuse ranked candidate lists by RRF: `score(d) = Σ 1/(K + rank_in_list)`. Ties break by id for
/// determinism. Lists may overlap and have different lengths; absence from a list adds nothing.
/// [`rrf_fuse`] with provenance: each fused hit carries its per-channel rank (0-based;
/// `None` where a channel did not surface it) — §11's "expose which rankers contributed".
fn rrf_fuse_explained(lists: &[Vec<u64>], k: usize) -> Vec<(u64, f32, Vec<Option<usize>>)> {
  rrf_fuse_with(lists, k, |_, rank| 1.0 / (RRF_K + rank as f32))
}

/// [`rrf_fuse_explained`] with the per-placement mass supplied by `mass(channel, rank)`
/// — the seam the dense-list laws weight one list through; the plain fusion passes the
/// standard `1/(K + rank)` for every channel.
fn rrf_fuse_with(
  lists: &[Vec<u64>],
  k: usize,
  mass: impl Fn(usize, usize) -> f32,
) -> Vec<(u64, f32, Vec<Option<usize>>)> {
  let mut scores: std::collections::HashMap<u64, (f32, Vec<Option<usize>>)> =
    std::collections::HashMap::new();
  for (channel, list) in lists.iter().enumerate() {
    for (rank, &id) in list.iter().enumerate() {
      let entry = scores
        .entry(id)
        .or_insert_with(|| (0.0, vec![None; lists.len()]));
      entry.0 += mass(channel, rank);
      entry.1[channel] = Some(rank);
    }
  }
  let mut ranked: Vec<(u64, f32, Vec<Option<usize>>)> = scores
    .into_iter()
    .map(|(id, (score, ranks))| (id, score, ranks))
    .collect();
  ranked.sort_by(|a, b| {
    b.1
      .partial_cmp(&a.1)
      .unwrap_or(std::cmp::Ordering::Equal)
      .then(a.0.cmp(&b.0))
  });
  ranked.truncate(k);
  ranked
}

/// Accumulate RRF mass (`Σ 1/(K + rank)`, the identical formula
/// [`rrf_fuse_explained`] applies, in the identical channel-then-rank order) into a
/// dense id-indexed table — the multi-phrase rungs' fused view, O(n) memory with no
/// per-hit allocation. A present id always ends up with a strictly positive score, so
/// `> 0.0` is the presence test.
fn rrf_accumulate_dense(lists: &[Vec<u64>], table: &mut [f32]) {
  for list in lists {
    for (rank, &id) in list.iter().enumerate() {
      if let Some(slot) = table.get_mut(id as usize) {
        *slot += 1.0 / (RRF_K + rank as f32);
      }
    }
  }
}

/// Hybrid search (§3.5): three ranked lists fused by Reciprocal Rank Fusion —
/// 1. **name**: exact-name matches, then token-identical names, then names containing every
///    query token (shorter names first) — so querying a symbol by name always surfaces it;
/// 2. **semantic**: the ANN tier's lexical-embedding ranking — descriptive queries;
/// 3. **graph**: the candidates from (1)+(2) ranked by in-degree — heavily called/referenced
///    symbols outrank dead-weight lookalikes.
pub fn search_index(index_dir: &Path, query: &str, k: usize) -> Result<String, Box<dyn Error>> {
  search_index_impl(index_dir, query, k, false)
}

/// [`search_index`] with structured pre-ranking filters (the CLI's `--path`/`--prefix`/
/// `--kind`/`--lang`/`--exported` flags): same rendering contract, narrowed population.
pub fn search_index_filtered(
  index_dir: &Path,
  query: &str,
  k: usize,
  filter: &SearchFilter,
) -> Result<String, Box<dyn Error>> {
  cached_searcher(index_dir)?.search_rendered_filtered(query, k, false, filter)
}

/// [`search_index`] with ranking provenance: each line gains `(id N; name#r vector#r graph#r)`
/// — the node id (for `fetch_span`/selector refinement) and each contributing channel's
/// 1-based rank. The plain renderer stays byte-stable for humans and captured docs.
pub fn search_index_explained(
  index_dir: &Path,
  query: &str,
  k: usize,
) -> Result<String, Box<dyn Error>> {
  search_index_impl(index_dir, query, k, true)
}

/// Structured pre-ranking filters (IMPROVEMENTS #9): applied to every channel's candidate
/// population **before** ranking and fusion, so `k` results means `k` *matching* results —
/// a filtered query is never starved because unfiltered hits crowded the pool.
///
/// Dimensions: definition-path prefix (package/subtree scoping) and suffix, symbol kind,
/// language (canonical name or alias, resolved by defining-file extension), and visibility
/// (`exported_only`). Resolution *grade* is deliberately absent here: search hits are
/// definitions, not edges — grade floors live on the traversal/edge surfaces
/// (`min_grade` on `reachable`, graph predicates in rules).
#[derive(Default, Clone, Debug)]
pub struct SearchFilter {
  /// Definition file path must start with this prefix.
  pub path_prefix: Option<String>,
  /// Definition file path must end with this suffix.
  pub path_suffix: Option<String>,
  /// Symbol kind (function, method, struct, field, …).
  pub kind: Option<String>,
  /// Language name or alias (rust, py, ts, …), matched by defining-file extension.
  pub lang: Option<String>,
  /// Only exported definitions.
  pub exported_only: bool,
  /// Exclude test-classified paths (`path_class` == Test): tests reference everything, so
  /// production-signal queries filter them out rather than demote them.
  pub exclude_tests: bool,
}

impl SearchFilter {
  pub fn is_empty(&self) -> bool {
    self.path_prefix.is_none()
      && self.path_suffix.is_none()
      && self.kind.is_none()
      && self.lang.is_none()
      && !self.exported_only
      && !self.exclude_tests
  }
}

/// Conservative cross-language path classification. A **filter** facet, never a ranking
/// signal — ranking stays bit-stable; `--no-tests` and friends narrow populations instead.
/// Conservative means: only unambiguous conventions classify away from `Source`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PathClass {
  Source,
  Test,
  Vendored,
  Generated,
}

impl PathClass {
  pub fn label(self) -> &'static str {
    match self {
      PathClass::Source => "source",
      PathClass::Test => "test",
      PathClass::Vendored => "vendored",
      PathClass::Generated => "generated",
    }
  }
}

pub fn path_class(path: &str) -> PathClass {
  let mut basename = path;
  for component in path.split(['/', '\\']) {
    if component.is_empty() {
      continue;
    }
    basename = component;
    match component {
      "vendor" | "vendored" | "third_party" | "third-party" | "node_modules" => {
        return PathClass::Vendored;
      }
      "tests" | "test" | "__tests__" | "spec" | "testdata" => return PathClass::Test,
      "generated" | "__generated__" => return PathClass::Generated,
      _ => {}
    }
  }
  let stem = basename;
  let is_test_basename = stem.starts_with("test_")
    || stem.starts_with("conftest.")
    || [".test.", ".spec.", "_test.", "_spec."].iter().any(|m| stem.contains(m));
  if is_test_basename {
    return PathClass::Test;
  }
  if stem.contains(".generated.") || stem.contains("_generated.") {
    return PathClass::Generated;
  }
  PathClass::Source
}

/// The filter compiled for the hot path: kind/lang parsed once, applied per node view.
struct CompiledSearchFilter<'f> {
  /// `Cow` because an existing prefix path re-spells to the graph's canonical form at
  /// compile time (the build canonicalizes its root, so `/tmp/...` filters must meet
  /// `/private/tmp/...` node paths); a prefix that names nothing stays verbatim.
  path_prefix: Option<std::borrow::Cow<'f, str>>,
  path_suffix: Option<&'f str>,
  kind: Option<vorpal_kg::SymbolKind>,
  lang: Option<String>,
  exported_only: bool,
  exclude_tests: bool,
}

impl<'f> CompiledSearchFilter<'f> {
  fn compile(filter: &'f SearchFilter) -> Result<Self, Box<dyn Error>> {
    let kind = match filter.kind.as_deref() {
      Some(text) => Some(
        vorpal_kg::SymbolKind::parse(text)
          .ok_or_else(|| format!("unknown symbol kind '{text}'"))?,
      ),
      None => None,
    };
    let lang = match filter.lang.as_deref() {
      Some(text) => Some(
        vorpal_ingest::canonical_language(text)
          .ok_or_else(|| format!("unknown language '{text}'"))?,
      ),
      None => None,
    };
    let path_prefix = filter.path_prefix.as_deref().map(|prefix| {
      // Canonicalize a prefix that names a real path (preserving an intentional trailing
      // slash); partial prefixes cannot resolve and keep their verbatim spelling.
      match Path::new(prefix).canonicalize() {
        Ok(canonical) => {
          let mut spelled = canonical.to_string_lossy().into_owned();
          if prefix.ends_with('/') && !spelled.ends_with('/') {
            spelled.push('/');
          }
          if spelled == prefix {
            std::borrow::Cow::Borrowed(prefix)
          } else {
            std::borrow::Cow::Owned(spelled)
          }
        }
        Err(_) => std::borrow::Cow::Borrowed(prefix),
      }
    });
    Ok(Self {
      path_prefix,
      path_suffix: filter.path_suffix.as_deref(),
      kind,
      lang,
      exported_only: filter.exported_only,
      exclude_tests: filter.exclude_tests,
    })
  }

  fn admits(&self, kg: &Kg, id: u64) -> bool {
    let Some(view) = kg.node(NodeId::new(id)) else {
      return false;
    };
    if let Some(prefix) = &self.path_prefix {
      if !view.path.starts_with(prefix.as_ref()) {
        return false;
      }
    }
    if let Some(suffix) = self.path_suffix {
      if !view.path.ends_with(suffix) {
        return false;
      }
    }
    if let Some(kind) = self.kind {
      if view.kind != kind {
        return false;
      }
    }
    if self.exported_only && !view.exported {
      return false;
    }
    if self.exclude_tests && path_class(view.path) == PathClass::Test {
      return false;
    }
    if let Some(lang) = &self.lang {
      if vorpal_ingest::language_name_of(view.path).as_deref() != Some(lang.as_str()) {
        return false;
      }
    }
    true
  }
}

/// The typed twin of [`search_index_explained`]: the same pinned-generation hybrid ranking,
/// returned as records instead of rendered lines (IMPROVEMENTS #7).
pub fn search_records(
  index_dir: &Path,
  query: &str,
  k: usize,
) -> Result<Vec<records::SearchHitRecord>, Box<dyn Error>> {
  search_records_filtered(index_dir, query, k, &SearchFilter::default())
}

/// [`search_records_filtered`] with the semantic candidate pool supplied by the daemon's
/// live ANN tier. Filters, the full-precision rerank, and fusion are the shared path — the
/// live tier only replaces WHERE the approximate pool came from.
pub fn search_records_filtered_live(
  index_dir: &Path,
  query: &str,
  k: usize,
  filter: &SearchFilter,
  tier: &live_ann::LiveAnnTier,
) -> Result<Vec<records::SearchHitRecord>, Box<dyn Error>> {
  let searcher = cached_searcher(index_dir)?;
  let pool = (k * 4).max(50);
  let take = pool * 2 * if filter.is_empty() { 1 } else { 4 };
  let query_vec = active_embedder().embed(query);
  let semantic_pool = tier.search_ids(&query_vec, take);
  let ranked = searcher.run_with_semantic_pool(query, k, filter, semantic_pool)?;
  Ok(hit_records(&searcher.kg, ranked))
}

/// Map a ranked `(row, score, per-channel ranks)` list into hit records — shared by every
/// search-records surface.
fn hit_records(kg: &Kg, ranked: Vec<(u64, f32, Vec<Option<usize>>)>) -> Vec<records::SearchHitRecord> {
  const CHANNELS: [&str; 3] = ["name", "vector", "graph"];
  ranked
    .into_iter()
    .filter_map(|(row, score, ranks)| {
      Some(records::SearchHitRecord {
        node: records::node_record(kg, NodeId::new(row))?,
        score,
        channels: CHANNELS
          .iter()
          .zip(&ranks)
          .filter_map(|(&channel, rank)| {
            rank.map(|rank| records::ChannelRank {
              channel,
              rank: rank + 1,
              phrase: None,
            })
          })
          .collect(),
      })
    })
    .collect()
}

/// [`search_records`] with structured pre-ranking filters.
pub fn search_records_filtered(
  index_dir: &Path,
  query: &str,
  k: usize,
  filter: &SearchFilter,
) -> Result<Vec<records::SearchHitRecord>, Box<dyn Error>> {
  cached_searcher(index_dir)?.records(query, k, filter)
}

/// The typed search answer with conjunction provenance: `hits` plus `multi_phrase` when
/// the query used the `"…" AND "…"` syntax. [`search_records_filtered`] is its
/// `hits`-only shim.
pub fn search_report_filtered(
  index_dir: &Path,
  query: &str,
  k: usize,
  filter: &SearchFilter,
) -> Result<records::SearchReport, Box<dyn Error>> {
  cached_searcher(index_dir)?.report(query, k, filter)
}

/// [`search_report_filtered`] with the semantic candidate pool served by the daemon's
/// LIVE ANN tier — pool sizing mirrors the persisted dispatch, and the query vector
/// comes from the searcher's own embedder so live rows and rerank rows share one model.
pub fn search_report_filtered_live(
  index_dir: &Path,
  query: &str,
  k: usize,
  filter: &SearchFilter,
  tier: &live_ann::LiveAnnTier,
) -> Result<records::SearchReport, Box<dyn Error>> {
  let searcher = cached_searcher(index_dir)?;
  let pool = (k * 4).max(50);
  let take = pool * 2 * if filter.is_empty() { 1 } else { 4 };
  let query_vec = searcher.embedder.embed(query);
  let semantic_pool = tier.search_ids(&query_vec, take);
  searcher.report_live(query, k, filter, semantic_pool)
}

/// Parse the conjunctive search syntax: the ENTIRE query must be two or more double-quoted
/// phrases joined by whitespace-delimited literal `AND` (uppercase), with nothing but
/// whitespace outside the quotes — `"retry logic" AND "connection pool"`. Anything else —
/// unquoted terms, a single quoted phrase, lowercase `and`, missing whitespace, stray
/// text, empty phrases, unterminated quotes — returns `None`, and callers route the
/// ORIGINAL query bytes through the single-phrase path: the syntax claims no ordinary
/// query (`tokenize` already treated `"` as a plain boundary, so quoted input never had
/// distinct semantics to collide with).
pub fn parse_and_phrases(query: &str) -> Option<Vec<String>> {
  let mut phrases: Vec<String> = Vec::new();
  let mut rest = query.trim();
  loop {
    rest = rest.strip_prefix('"')?;
    let end = rest.find('"')?;
    let phrase = rest[..end].trim();
    if phrase.is_empty() {
      return None;
    }
    phrases.push(phrase.to_string());
    rest = &rest[end + 1..];
    if rest.is_empty() {
      break;
    }
    // Separator: at least one whitespace, `AND`, at least one whitespace.
    if !rest.starts_with(char::is_whitespace) {
      return None;
    }
    rest = rest.trim_start().strip_prefix("AND")?;
    if !rest.starts_with(char::is_whitespace) {
      return None;
    }
    rest = rest.trim_start();
  }
  if phrases.len() < 2 { None } else { Some(phrases) }
}

fn search_index_impl(
  index_dir: &Path,
  query: &str,
  k: usize,
  explain: bool,
) -> Result<String, Box<dyn Error>> {
  cached_searcher(index_dir)?.search_rendered(query, k, explain)
}

/// A persistent, reusable search handle for one **immutable** generation: it mmaps the graph,
/// the ANN tier, and the posting tier **once** and answers many queries against them. Because
/// generations are content-addressed (`gen/<id>/`), an open handle can never go stale — a
/// rebuild mints a new generation dir, which [`cached_searcher`] opens as a fresh entry.
/// Reusing the mappings removes the per-query mmap/munmap storm that serialized concurrent
/// searches on the kernel address-space lock (~10× system-time blow-up at 32 concurrent
/// queries before this).
pub struct Searcher {
  generation_dir: PathBuf,
  kg: Kg,
  /// The persisted ANN tier — present only when fresh for this generation (the common warm
  /// case). Absent → `run` takes the overlay/exhaustive tiers (cold, degraded, load per call).
  ann: Option<AnnIndex>,
  /// The persisted lexical posting tier — present only when its stamp matches this generation.
  postings: Option<postings::Postings>,
  /// The embedder this handle queries with — the PERSISTED tier's model when coherent
  /// (learned models checksum-verified at open), else the lexical default. Every query
  /// vector, overlay embed, and rerank goes through it, so vectors from different
  /// models can never meet in one pool.
  embedder: ActiveEmbedder,
  /// Fetch width at/above which the semantic channel takes the flat exact scan instead
  /// of the beam. LEARNED at warm time from the ingested index on the running machine
  /// (`ann.calib`); an absent/stale/torn calibration falls back to the structural floor
  /// `node_count` (a beam has no completeness guarantee at take ≥ n). Never a shipped
  /// constant.
  semantic_cutover: usize,
  /// Per-corpus BM25 fourth-list verdict from the persisted record's warm-time gate
  /// (`None` verdict or absent record → off): whether fusion appends the BM25 list.
  bm25_enabled: bool,
  /// Stage-6 opt-in reranker: the vendored code encoder, opened from the root's
  /// `encoder.dir` selection. `None` = unconfigured, or the open failed — then the
  /// reason is stated in `encoder_note` and searches keep serving the fused
  /// ranking (an optional tier may degrade, never fail a search).
  encoder: Option<Box<vorpal_ann::encoder::CodeEncoder>>,
  /// Present exactly when an encoder SELECTION could not be honored (stated
  /// degradation — surfaced by [`Searcher::encoder_status`]).
  encoder_note: Option<String>,
  /// Session cache of candidate-surface embeddings for the reranker — node
  /// surfaces are immutable per pinned generation, so rows never go stale within
  /// a handle. FIFO-bounded at [`ENCODER_CACHE_ROWS`].
  encoder_cache: Mutex<EncoderCache>,
  /// The doc-side dense sidecar (`dense.rs`) — present only when fresh for this
  /// generation AND the opened encoder; absent → the fifth list never forms.
  dense: Option<dense::DenseSidecar>,
  /// The sidecar record's per-corpus verdict (`None`/absent → off).
  dense_enabled: bool,
  /// The sidecar's eligible population (the fill's stop-rule set, `DenseRecord::eligible`)
  /// — with `dense.len()` (rows covered), the coverage fraction the population-scaled
  /// [`DenseFusion`] laws derive their weight from. 0 without a sidecar.
  dense_eligible: usize,
  /// Eval/measurement seam: refuse every approximate tier (base ANN, overlay) and every side
  /// effect (autowarm) so queries take the exact reference paths only. Set by
  /// [`Searcher::open_exact`]; never used in serving.
  exact_only: bool,
}

impl Searcher {
  /// Open (mmap) every tier for `index_dir`'s current generation, once.
  pub fn open(index_dir: &Path) -> Result<Searcher, Box<dyn Error>> {
    let (encoder, encoder_note) = open_selected_encoder(index_dir);
    Self::open_with_encoder(index_dir, encoder, encoder_note)
  }

  /// [`Searcher::open`] over an ALREADY-opened encoder (or a stated absence) —
  /// the dense gate's entry, which probes a pinned generation through the
  /// encoder the warm resolved from the root.
  fn open_with_encoder(
    index_dir: &Path,
    encoder: Option<Box<vorpal_ann::encoder::CodeEncoder>>,
    encoder_note: Option<String>,
  ) -> Result<Searcher, Box<dyn Error>> {
    // Pin the generation for the handle's lifetime: an immutable, content-addressed snapshot,
    // so nothing this handle serves can ever be a mixed or stale pair.
    let generation_dir = vorpal_kg::resolve_index_dir(index_dir);
    let kg = Kg::load(&generation_dir)?;
    let stamp = stamp_of(&kg);
    // The handle queries through EXACTLY the embedder the persisted tier was built
    // with (coherence-verified; learned models checksum-verified); incoherent or
    // absent artifacts fall back to the lexical default over the exact paths.
    let (ann, embedder) = match coherent_persisted_embedder(&generation_dir, stamp) {
      Some(embedder) => {
        let ann = AnnIndex::load(&generation_dir.join("ann.bin")).ok();
        // A LEARNED tier is servable only WITH its ann tier: exhaustive fallbacks
        // under the model would re-embed the population per query (minutes at kernel
        // scale, measured) — a torn/unloadable ann.bin under a learned record routes
        // the whole handle to the lexical default over the exact paths instead.
        match (&ann, &embedder) {
          (None, ActiveEmbedder::Learned(_)) => {
            (None, ActiveEmbedder::Lexical(LexicalEmbedder::default()))
          }
          _ => (ann, embedder),
        }
      }
      None => (None, ActiveEmbedder::Lexical(LexicalEmbedder::default())),
    };
    let postings = postings::Postings::load(&generation_dir).filter(|p| p.stamp() == stamp);
    let semantic_cutover =
      load_ann_calibration(&generation_dir, stamp, kg.node_count()).unwrap_or(kg.node_count());
    let bm25_enabled = persisted_tier_record(&generation_dir)
      .and_then(|record| record.bm25)
      .unwrap_or(false);
    let (dense, dense_enabled, dense_eligible) =
      load_dense_sidecar(index_dir, &generation_dir, stamp, encoder.as_deref());
    Ok(Searcher {
      generation_dir,
      kg,
      ann,
      postings,
      embedder,
      semantic_cutover,
      bm25_enabled,
      encoder,
      encoder_note,
      encoder_cache: Mutex::new(EncoderCache::default()),
      dense,
      dense_enabled,
      dense_eligible,
      exact_only: false,
    })
  }

  /// [`Searcher::open`] with every approximate tier deliberately refused: queries take the
  /// exact reference paths (exhaustive semantic scan + full name scan) no matter what
  /// sidecars exist on disk, and nothing is mutated (no autowarm). This is the measurement
  /// seam — `cargo xtask searcheval --overlap` compares tier answers against it — never a
  /// serving path, and compiled only under the non-default `bench-internals` feature so
  /// production builds carry no measurement entry points.
  #[cfg(feature = "bench-internals")]
  pub fn open_exact(index_dir: &Path) -> Result<Searcher, Box<dyn Error>> {
    let generation_dir = vorpal_kg::resolve_index_dir(index_dir);
    let kg = Kg::load(&generation_dir)?;
    let stamp = stamp_of(&kg);
    // The reference path embeds with the persisted tier's model when one is coherent
    // (so exact answers stay comparable to tier answers), else the lexical default.
    let embedder = coherent_persisted_embedder(&generation_dir, stamp)
      .unwrap_or_else(|| ActiveEmbedder::Lexical(LexicalEmbedder::default()));
    let semantic_cutover = kg.node_count();
    // The exact path honors the same per-corpus verdict, so reference answers stay
    // comparable to tier answers under whatever fusion this index actually serves.
    let bm25_enabled = persisted_tier_record(&generation_dir)
      .and_then(|record| record.bm25)
      .unwrap_or(false);
    // Same reranker semantics as serving: exact-vs-tier comparisons measure the
    // pipeline the index actually answers with — the dense channel included,
    // under the same per-corpus verdict.
    let (encoder, encoder_note) = open_selected_encoder(index_dir);
    let (dense, dense_enabled, dense_eligible) =
      load_dense_sidecar(index_dir, &generation_dir, stamp, encoder.as_deref());
    Ok(Searcher {
      generation_dir,
      kg,
      ann: None,
      postings: None,
      embedder,
      semantic_cutover,
      bm25_enabled,
      encoder,
      encoder_note,
      encoder_cache: Mutex::new(EncoderCache::default()),
      dense,
      dense_enabled,
      dense_eligible,
      exact_only: true,
    })
  }

  /// The fusion's list set for one query, positional against [`SEARCH_CHANNELS`]:
  /// the three always-on channels; BM25 where the record enabled it; and, when a
  /// query embedding produced a dense list, that list — behind an EMPTY BM25
  /// placeholder if BM25 is off (an empty list adds no RRF mass).
  fn fused_lists(&self, channels: Channels, dense_on: bool, k: usize) -> Vec<Vec<u64>> {
    let mut lists = vec![channels.named, channels.semantic, channels.by_degree];
    if self.bm25_enabled || dense_on {
      lists.push(channels.bm25);
    }
    if dense_on {
      let mut dense = channels.dense;
      let law = dense_fusion();
      if matches!(law, DenseFusion::Cutoff | DenseFusion::Hub | DenseFusion::Support) {
        // The equal-share cut: the pool depth divided among the lists that actually
        // nominate (the dense list itself counted).
        let nominating = lists.iter().filter(|list| !list.is_empty()).count() + usize::from(!dense.is_empty());
        let share = rerank_pool(k) / nominating.max(1);
        if law == DenseFusion::Cutoff {
          dense.truncate(share);
        } else if dense.len() > share {
          // The `share` most-referenced positions of the cosine top-pool: rank the
          // positions by (in-degree desc, cosine position asc) and take the share.
          let mut by_hub: Vec<(std::cmp::Reverse<usize>, usize)> = dense
            .iter()
            .enumerate()
            .map(|(position, &id)| (std::cmp::Reverse(self.kg.in_degree_referential(NodeId::new(id))), position))
            .collect();
          by_hub.sort_unstable();
          let mut keep: Vec<bool> = vec![false; dense.len()];
          for &(_, position) in by_hub.iter().take(share) {
            keep[position] = true;
          }
          if law == DenseFusion::Support {
            // Corroborated rows are admitted regardless of degree.
            let supported: std::collections::HashSet<u64> = lists.iter().flatten().copied().collect();
            for (position, id) in dense.iter().enumerate() {
              if supported.contains(id) {
                keep[position] = true;
              }
            }
          }
          // Cosine order is preserved among the admitted rows.
          dense = dense
            .iter()
            .zip(&keep)
            .filter_map(|(&id, &kept)| kept.then_some(id))
            .collect();
        }
      }
      lists.push(dense);
    }
    lists
  }

  /// [`rrf_fuse_explained`] with the dense list ([`DENSE_LIST`]) weighted by the active
  /// [`DenseFusion`] law: `Quantile` scales its ranks by n_eligible/n_covered, `Subset`
  /// scales its K by n_covered/n_eligible; every other law (and any list set without a
  /// dense list, or a sidecar without a population) is the plain fusion, byte for byte.
  fn fuse(&self, lists: &[Vec<u64>], k: usize) -> Vec<FusedHit> {
    let covered = self.dense.as_ref().map_or(0, dense::DenseSidecar::len);
    if lists.len() <= DENSE_LIST || covered == 0 || self.dense_eligible == 0 {
      return rrf_fuse_explained(lists, k);
    }
    let fraction = covered as f32 / self.dense_eligible as f32;
    match dense_fusion() {
      DenseFusion::Quantile => rrf_fuse_with(lists, k, |channel, rank| {
        if channel == DENSE_LIST {
          1.0 / (RRF_K + rank as f32 / fraction)
        } else {
          1.0 / (RRF_K + rank as f32)
        }
      }),
      DenseFusion::Subset => rrf_fuse_with(lists, k, |channel, rank| {
        if channel == DENSE_LIST {
          1.0 / (RRF_K * fraction + rank as f32)
        } else {
          1.0 / (RRF_K + rank as f32)
        }
      }),
      DenseFusion::Flat
      | DenseFusion::Cutoff
      | DenseFusion::Gap
      | DenseFusion::Degree
      | DenseFusion::Hub
      | DenseFusion::Support => rrf_fuse_explained(lists, k),
    }
  }

  /// The fixed-order QUERY embedding for the dense channel — computed ONCE per
  /// search and handed to the rerank too (`rerank_with_encoder`), so the channel
  /// adds no encoder work beyond the sidecar scan. `None` = the channel is off
  /// for this handle (no fresh sidecar, gate off, or no encoder); a runtime embed
  /// failure degrades to the un-channelled fusion for this query.
  fn dense_query(&self, query: &str) -> Option<Vec<f32>> {
    if !self.dense_enabled || self.dense.is_none() {
      return None;
    }
    self.encoder.as_ref()?.embed_query(query).ok()
  }

  /// Which warm tiers this handle holds, as `(ann, postings)` — fresh-and-open vs
  /// absent/stale. Lets measurement tools state which path served their numbers.
  /// Bench-only, like [`Searcher::open_exact`].
  #[cfg(feature = "bench-internals")]
  pub fn tiers(&self) -> (bool, bool) {
    (self.ann.is_some(), self.postings.is_some())
  }

  /// The dense sidecar this handle holds — `(covered rows, channel on)` — or
  /// `None` when no fresh sidecar exists for the opened encoder. Bench-only.
  #[cfg(feature = "bench-internals")]
  pub fn dense_status(&self) -> Option<(usize, bool)> {
    self.dense.as_ref().map(|sidecar| (sidecar.len(), self.dense_enabled))
  }

  /// The typed-record surface over [`Searcher::run`]: one record per hit with the fused
  /// score and per-channel provenance ranks. Shared by [`search_records_filtered`] and bulk
  /// callers holding a persistent handle. The `hits`-only shim over [`Searcher::report`].
  pub fn records(
    &self,
    query: &str,
    k: usize,
    filter: &SearchFilter,
  ) -> Result<Vec<records::SearchHitRecord>, Box<dyn Error>> {
    Ok(self.report(query, k, filter)?.hits)
  }

  /// ONE search, two orderings — the `--ranked` surface: the base fused top-k and,
  /// when this handle carries the encoder, the reranked ordering derived from the
  /// SAME fusion (a single channel pass and fuse; the only extra cost is the
  /// encoder step — never a second search). `None` reranked = no active encoder
  /// (the caller can state why via [`Searcher::encoder_status`]). Conjunction
  /// queries keep their min-RRF contract un-reranked and return `None` too.
  #[allow(clippy::type_complexity)]
  pub fn records_ranked(
    &self,
    query: &str,
    k: usize,
    filter: &SearchFilter,
  ) -> Result<
    (Vec<records::SearchHitRecord>, Option<Vec<records::SearchHitRecord>>),
    Box<dyn Error>,
  > {
    if parse_and_phrases(query).is_some() {
      return Ok((self.records(query, k, filter)?, None));
    }
    let dense_query = self.dense_query(query);
    let channels = self.channel_lists_with_bm25(
      query,
      rerank_pool(k),
      filter,
      self.bm25_enabled,
      None,
      dense_query.as_deref(),
    )?;
    let lists = self.fused_lists(channels, dense_query.is_some(), k);
    let fused = self.fuse(&lists, k);
    let reranked = self
      .encoder
      .is_some()
      .then(|| self.hits_from_ranked(self.rerank_with_encoder(query, fused.clone(), dense_query), None));
    Ok((self.hits_from_ranked(fused, None), reranked))
  }

  /// ONE search, BM25 off/on — `vorpal tune`'s paired view, the warm gate's own
  /// trick surfaced: a single channel pass computes the BM25 list regardless of
  /// the record's verdict, then the 3-list and 4-list fusions rank side by side.
  /// The active encoder (if any) reranks BOTH, so this isolates exactly the
  /// fourth list's effect. Conjunctions keep their contract: both sides equal.
  #[allow(clippy::type_complexity)]
  pub fn records_bm25_pair(
    &self,
    query: &str,
    k: usize,
    filter: &SearchFilter,
  ) -> Result<(Vec<records::SearchHitRecord>, Vec<records::SearchHitRecord>), Box<dyn Error>> {
    if parse_and_phrases(query).is_some() {
      // Both sides equal by the conjunction contract; two calls only materialize
      // the two vecs (record types are deliberately Clone-free).
      return Ok((self.records(query, k, filter)?, self.records(query, k, filter)?));
    }
    // The pair isolates the BM25 list: the dense channel (when on) rides in
    // BOTH fusions, so it cancels out of the comparison.
    let dense_query = self.dense_query(query);
    let channels =
      self.channel_lists_with_bm25(query, rerank_pool(k), filter, true, None, dense_query.as_deref())?;
    let mut without = vec![
      channels.named.clone(),
      channels.semantic.clone(),
      channels.by_degree.clone(),
    ];
    let mut with_bm25 = without.clone();
    with_bm25.push(channels.bm25);
    if dense_query.is_some() {
      without.push(Vec::new());
      without.push(channels.dense.clone());
      with_bm25.push(channels.dense);
    }
    let off = self.rerank_with_encoder(query, self.fuse(&without, k), dense_query.clone());
    let on = self.rerank_with_encoder(query, self.fuse(&with_bm25, k), dense_query);
    Ok((
      self.hits_from_ranked(off, None),
      self.hits_from_ranked(on, None),
    ))
  }

  /// ONE search, dense channel off/on — `vorpal tune`'s paired view for the
  /// fifth list: a single channel pass (with the query embedding) feeds both the
  /// four-list and the five-list fusion; the active encoder reranks BOTH, so
  /// this isolates exactly the dense list's effect. `None` = no fresh sidecar
  /// or no encoder on this handle (the channel is untestable, not "off").
  /// Conjunctions keep their contract: both sides equal.
  #[allow(clippy::type_complexity)]
  pub fn records_dense_pair(
    &self,
    query: &str,
    k: usize,
    filter: &SearchFilter,
  ) -> Result<Option<(Vec<records::SearchHitRecord>, Vec<records::SearchHitRecord>)>, Box<dyn Error>> {
    if self.dense.is_none() {
      return Ok(None);
    }
    let Some(encoder) = self.encoder.as_ref() else {
      return Ok(None);
    };
    if parse_and_phrases(query).is_some() {
      return Ok(Some((self.records(query, k, filter)?, self.records(query, k, filter)?)));
    }
    let query_vec = encoder.embed_query(query)?;
    let channels = self.channel_lists_with_bm25(
      query,
      rerank_pool(k),
      filter,
      self.bm25_enabled,
      None,
      Some(&query_vec),
    )?;
    let mut without = vec![
      channels.named.clone(),
      channels.semantic.clone(),
      channels.by_degree.clone(),
    ];
    if self.bm25_enabled {
      without.push(channels.bm25.clone());
    }
    let with_dense = self.fused_lists(channels, true, k);
    let off = self.rerank_with_encoder(query, self.fuse(&without, k), Some(query_vec.clone()));
    let on = self.rerank_with_encoder(query, self.fuse(&with_dense, k), Some(query_vec));
    Ok(Some((
      self.hits_from_ranked(off, None),
      self.hits_from_ranked(on, None),
    )))
  }

  /// The full typed search answer — the ONE dispatch point below every surface (CLI, MCP,
  /// napi, pyo3, the async pool): parses the conjunction syntax (`"…" AND "…"`, see
  /// [`parse_and_phrases`]) and routes. Anything that does not parse flows through the
  /// single-phrase path with the ORIGINAL query bytes — byte-identity for ordinary
  /// queries is structural, not behavioral.
  pub fn report(
    &self,
    query: &str,
    k: usize,
    filter: &SearchFilter,
  ) -> Result<records::SearchReport, Box<dyn Error>> {
    match parse_and_phrases(query) {
      None => Ok(records::SearchReport {
        hits: self.hits_from_ranked(self.run(query, k, filter)?, None),
        multi_phrase: None,
      }),
      Some(phrases) => self.report_multi(&phrases, k, filter),
    }
  }

  /// [`Searcher::report`] with the semantic pool served by the daemon's live ANN
  /// tier. Single-phrase queries ride [`Searcher::run_with_semantic_pool`];
  /// conjunctions keep the persisted-tier multi-phrase path (the live tier serves
  /// one pool per query — per-phrase live pools are a recorded seam, not silently
  /// approximated).
  pub fn report_live(
    &self,
    query: &str,
    k: usize,
    filter: &SearchFilter,
    semantic_pool: Vec<u64>,
  ) -> Result<records::SearchReport, Box<dyn Error>> {
    match parse_and_phrases(query) {
      None => Ok(records::SearchReport {
        hits: self.hits_from_ranked(self.run_with_semantic_pool(query, k, filter, semantic_pool)?, None),
        multi_phrase: None,
      }),
      Some(phrases) => self.report_multi(&phrases, k, filter),
    }
  }

  /// Build hit records from one fused ranking; `phrase` tags every channel rank when the
  /// ranking is one phrase of a conjunction.
  fn hits_from_ranked(
    &self,
    ranked: Vec<FusedHit>,
    phrase: Option<usize>,
  ) -> Vec<records::SearchHitRecord> {
    let kg = &self.kg;
    ranked
      .into_iter()
      .filter_map(|(row, score, ranks)| {
        Some(records::SearchHitRecord {
          node: records::node_record(kg, NodeId::new(row))?,
          score,
          channels: SEARCH_CHANNELS
            .iter()
            .zip(&ranks)
            .filter_map(|(&channel, rank)| {
              rank.map(|rank| records::ChannelRank {
                channel,
                rank: rank + 1,
                phrase,
              })
            })
            .collect(),
        })
      })
      .collect()
  }

  /// Conjunctive (multi-phrase AND) execution: every phrase runs the FULL three-channel
  /// single-phrase pass, candidates are intersected left-to-right (recording the phrase
  /// that emptied the set — the eliminator), and each survivor scores the MINIMUM of its
  /// per-phrase RRF scores. Min is the exact fuzzy-AND: monotone in every phrase, and
  /// invariant to per-phrase pool-size asymmetries — a product would double-count
  /// correlated phrases and reorder under pool changes. Ties: score sum descending (a hit
  /// strong on every phrase beats one merely not-weak), then id ascending.
  ///
  /// Depth has exactly two rungs, both computed from the data at hand: the shallow rung
  /// uses the single-phrase rerank pool for this k, the deep rung uses the node count
  /// itself. Truncated pools set a recall floor on intersections — at kernel scale a
  /// depth-50 pool for a broad phrase holds none of the conjunctive answers (measured
  /// 2026-08-30: `"socket buffer" AND "alloc"` came back empty) — so the deep rung
  /// structurally exhausts every channel, and an empty intersection there means the
  /// FULL supports are disjoint — support means sharing at least one real token with
  /// the phrase over the exact embedded surface (see [`node_lexical_support_bits`]; the
  /// learned tier re-derives the criterion with its embedder) — with no truncation
  /// asterisk. It runs only when the shallow
  /// rung is starved (< k survivors), and its fetch width covers the full population —
  /// where a beam is categorically the wrong tool (no completeness guarantee at
  /// take ≥ n) — so it rides the flat exact scan: exact by construction and, per the
  /// recorded sweep, faster there. Per-phrase RRF
  /// scores are depth-relative (the graph channel re-sorts the name pool, which is not
  /// prefix-stable), so scores compare within one answer, never across rungs; rung
  /// choice is a pure function of (query, k, index), so results stay deterministic.
  fn report_multi(
    &self,
    phrases: &[String],
    k: usize,
    filter: &SearchFilter,
  ) -> Result<records::SearchReport, Box<dyn Error>> {
    if phrases.len() > u64::BITS as usize {
      return Err("conjunction supports at most 64 phrases (support-mask width)".into());
    }
    // One tokenization per phrase, shared by every rung's support tests.
    let phrase_tokens: Vec<Vec<String>> = phrases.iter().map(|p| tokenize(p)).collect();
    let node_count = self.kg.node_count();
    let shallow = rerank_pool(k);
    let rungs: Vec<usize> = if shallow >= node_count {
      vec![node_count]
    } else {
      vec![shallow, node_count]
    };
    for (rung, &depth) in rungs.iter().enumerate() {
      // Per phrase: the three channel lists at this depth, folded into a dense
      // id-indexed RRF table — O(n) f32s, no per-hit allocation, so even the deep rung
      // stays lean at kernel scale.
      let mut per_phrase_lists: Vec<Vec<Vec<u64>>> = Vec::with_capacity(phrases.len());
      let mut tables: Vec<Vec<f32>> = Vec::with_capacity(phrases.len());
      for phrase in phrases {
        let mut channels = self.channel_lists(phrase, depth, filter)?;
        // Trim the semantic RANK list at the orthogonality boundary (an algebraic
        // identity — see `POSITIVE_BOUNDARY`): rows past it are the no-signal tie
        // region and carry no rank information worth fusing. Positive rows are a
        // prefix of the ascending list, so the cut changes no surviving rank. (What
        // counts as a MATCH is decided lexically below, never by vector sign.)
        let positive = channels
          .semantic_dist2
          .partition_point(|&dist2| dist2 < POSITIVE_BOUNDARY);
        channels.semantic.truncate(positive);
        let mut lists = vec![channels.named, channels.semantic, channels.by_degree];
        if self.bm25_enabled {
          lists.push(channels.bm25);
        }
        let mut table = vec![0.0f32; node_count];
        rrf_accumulate_dense(&lists, &mut table);
        per_phrase_lists.push(lists);
        tables.push(table);
      }
      // Lexical support: a phrase MATCHES a row iff they share at least one real token
      // over the exact surface the embedder hashes (see `node_lexical_support_bits`).
      // Vector-space sign cannot define a match — hashed-bucket collisions hand
      // unrelated rows positive dot products — so support is computed exactly, over the
      // rows any channel scored at this rung.
      let candidate_ids: Vec<u64> = (0..node_count as u64)
        .filter(|&id| {
          tables
            .iter()
            .any(|table| table.get(id as usize).copied().unwrap_or(0.0) > 0.0)
        })
        .collect();
      let support_bits: Vec<(u64, u64)> = {
        use rayon::prelude::*;
        candidate_ids
          .par_iter()
          .map(|&id| (id, node_lexical_support_bits(&self.kg, id, &phrase_tokens)))
          .collect()
      };
      let mut mask: Vec<u64> = vec![0; node_count];
      for (id, bits) in support_bits {
        if let Some(slot) = mask.get_mut(id as usize) {
          *slot = bits;
        }
      }
      let supported = |id: u64, phrase: usize, table: &[f32]| -> bool {
        mask.get(id as usize).copied().unwrap_or(0) & (1u64 << phrase) != 0
          && table.get(id as usize).copied().unwrap_or(0.0) > 0.0
      };
      let per_phrase_pool: Vec<usize> = tables
        .iter()
        .enumerate()
        .map(|(phrase, table)| {
          candidate_ids
            .iter()
            .filter(|&&id| supported(id, phrase, table))
            .count()
        })
        .collect();

      // Left-to-right intersection; the eliminator is the first phrase that emptied it.
      let Some(first) = tables.first() else {
        return Err("multi-phrase conjunction with no phrases (parser invariant)".into());
      };
      let mut survivors: Vec<u64> = candidate_ids
        .iter()
        .copied()
        .filter(|&id| supported(id, 0, first))
        .collect();
      let mut eliminated_by = if survivors.is_empty() { Some(0) } else { None };
      if eliminated_by.is_none() {
        for (index, table) in tables.iter().enumerate().skip(1) {
          survivors.retain(|&id| supported(id, index, table));
          if survivors.is_empty() {
            eliminated_by = Some(index);
            break;
          }
        }
      }

      let last_rung = rung + 1 == rungs.len();
      if survivors.len() < k && !last_rung {
        continue; // starved at the shallow rung — take the exhaustive one
      }

      // Score survivors: min RRF desc (the exact fuzzy-AND), then sum desc, then id asc.
      let mut scored: Vec<(u64, f32, f32)> = Vec::with_capacity(survivors.len());
      for id in survivors {
        let mut min = f32::INFINITY;
        let mut sum = 0.0f32;
        for table in &tables {
          let score = table.get(id as usize).copied().unwrap_or(0.0);
          if score <= 0.0 {
            return Err(
              "multi-phrase invariant violated: survivor missing from a phrase table".into(),
            );
          }
          min = min.min(score);
          sum += score;
        }
        scored.push((id, min, sum));
      }
      scored.sort_by(|a, b| {
        b.1
          .total_cmp(&a.1)
          .then(b.2.total_cmp(&a.2))
          .then(a.0.cmp(&b.0))
      });
      scored.truncate(k);

      // Channel provenance for the winners only: one membership pass over each phrase's
      // channel lists, collected phrase-major then channel-major (the record order).
      let winners: std::collections::HashSet<u64> =
        scored.iter().map(|&(id, _, _)| id).collect();
      let mut provenance: std::collections::HashMap<u64, Vec<records::ChannelRank>> =
        std::collections::HashMap::new();
      for (phrase, lists) in per_phrase_lists.iter().enumerate() {
        for (channel, list) in lists.iter().enumerate() {
          let Some(&name) = SEARCH_CHANNELS.get(channel) else {
            continue;
          };
          for (rank, id) in list.iter().enumerate() {
            if winners.contains(id) {
              provenance.entry(*id).or_default().push(records::ChannelRank {
                channel: name,
                rank: rank + 1,
                phrase: Some(phrase),
              });
            }
          }
        }
      }

      let kg = &self.kg;
      let mut hits = Vec::with_capacity(scored.len());
      for (id, min, _) in scored {
        let Some(node) = records::node_record(kg, NodeId::new(id)) else {
          continue;
        };
        hits.push(records::SearchHitRecord {
          node,
          score: min,
          channels: provenance.remove(&id).unwrap_or_default(),
        });
      }
      return Ok(records::SearchReport {
        hits,
        multi_phrase: Some(records::MultiPhraseReport {
          phrases: phrases.to_vec(),
          per_phrase_pool,
          intersection_depth: depth,
          eliminated_by,
        }),
      });
    }
    Err("multi-phrase rung loop ended without answering (invariant violation)".into())
  }

  /// The pinned-generation hybrid ranking shared by the rendered and typed search surfaces:
  /// name/semantic/in-degree channels fused by RRF, each hit carrying its per-channel ranks.
  /// Reads only the handle's already-open mappings — no `Kg::load`/`AnnIndex::load` per call.
  pub fn run(&self, query: &str, k: usize, filter: &SearchFilter) -> Result<Vec<FusedHit>, Box<dyn Error>> {
    let dense_query = self.dense_query(query);
    let channels = self.channel_lists_with_bm25(
      query,
      rerank_pool(k),
      filter,
      self.bm25_enabled,
      None,
      dense_query.as_deref(),
    )?;
    let lists = self.fused_lists(channels, dense_query.is_some(), k);
    Ok(self.rerank_with_encoder(query, self.fuse(&lists, k), dense_query))
  }

  /// The stated reason a configured encoder is NOT active (`None` = active, or no
  /// selection exists).
  pub fn encoder_status(&self) -> Option<&str> {
    self.encoder_note.as_deref()
  }

  /// Stage-6 opt-in rerank: stable-reorder the fused top-k by encoder cosine
  /// between the PREFIXED query and each hit's embedded surface — the same
  /// name/signature/basename recipe the tiers hash, as text. RRF scores and
  /// per-channel ranks in the records stay UNTOUCHED (the ORDER is the reranker's
  /// verdict); encoder off, an un-viewable node, or a runtime embed failure leave
  /// the fused order exactly (open() already validated the model — a runtime
  /// failure degrades one query, never fails a search). Conjunctions
  /// (`run_multi`) keep their own min-RRF contract un-reranked.
  /// `query_vec`: the fixed-order query embedding already computed for the dense
  /// channel (`dense_query`) — reused here so the channel costs no second query
  /// forward; `None` embeds the prefixed query in the candidate batch as before
  /// (bitwise equal either way, by the batch oracle).
  fn rerank_with_encoder(
    &self,
    query: &str,
    mut fused: Vec<FusedHit>,
    query_vec: Option<Vec<f32>>,
  ) -> Vec<FusedHit> {
    let Some(encoder) = &self.encoder else {
      return fused;
    };
    // The FUSED-WINNER PIN, structural: RRF's rank-0 consensus is the fusion's
    // strongest evidence, so the encoder arbitrates only the uncertain TAIL
    // (positions 1..k) and can never demote a hit the channels already agreed on
    // — measured: unpinned reranking collapsed the protected short-keyword class
    // on both bench corpora by demoting exactly those consensus winners. With the
    // pin, the kernel gates GREEN (all-NDCG +5%, the protected class itself +8%)
    // while cpython measures mixed — the per-corpus tables live in BENCHMARKS
    // "Stage 6", and `encoder.dir` is the per-corpus opt-in that expresses them.
    let mode = rerank_mode();
    if fused.len() <= 1 || mode == RerankMode::Off {
      return fused;
    }
    // ONE batched forward serves the whole query: the prefixed query plus every
    // cache-missed candidate surface concatenate into a single token matrix
    // (`embed_batch` — bitwise equal to solo embeds by the batch oracle), and the
    // per-generation-immutable candidate rows persist in the FIFO session cache.
    let prefixed = format!("{}{query}", vorpal_ann::encoder::QUERY_PREFIX);
    // The pool the encoder arbitrates: the tail under the pinned modes, everything
    // under the unpinned blend.
    let first = if mode == RerankMode::Blend { 0 } else { 1 };
    let tail_ids: Vec<u64> = fused.iter().skip(first).map(|hit| hit.0).collect();
    let mut rows: HashMap<u64, Vec<f32>> = HashMap::with_capacity(tail_ids.len());
    {
      let cache = self
        .encoder_cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
      for &id in &tail_ids {
        if let Some(row) = cache.map.get(&id) {
          rows.insert(id, row.clone());
        }
      }
    }
    // Candidate surfaces under the RERANK field of the surface pair (two-field
    // law, `dense::SurfacePair`): the pinned head recipe, so the rerank's
    // per-query encode cost and its exact-name evidence stay the shipped ones
    // whatever the sidecar embeds; the sweep's richer rerank recipes read the
    // candidates' source here (digest-verified; a changed file falls back to head).
    let mut miss_ids: Vec<u64> = Vec::new();
    let mut miss_surfaces: Vec<String> = Vec::new();
    let mut builder =
      dense::SurfaceBuilder::new(&self.generation_dir, dense::active_surface_pair().rerank);
    for &id in &tail_ids {
      if rows.contains_key(&id) {
        continue;
      }
      if self.kg.node(NodeId::new(id)).is_some() {
        miss_ids.push(id);
        miss_surfaces.push(builder.surface(&self.kg, id, encoder));
      }
    }
    // One batch: the prefixed query (unless already embedded) plus every missed
    // candidate surface; nothing to embed at all when the query is in hand and
    // every candidate is cached.
    let query_in_batch = query_vec.is_none();
    let mut sequences: Vec<&str> = Vec::with_capacity(1 + miss_surfaces.len());
    if query_in_batch {
      sequences.push(&prefixed);
    }
    sequences.extend(miss_surfaces.iter().map(String::as_str));
    let embedded = if sequences.is_empty() {
      Vec::new()
    } else {
      match encoder.embed_batch(&sequences) {
        Ok(embedded) => embedded,
        // open() validated the model; a runtime failure degrades one query, never
        // fails the search.
        Err(_) => return fused,
      }
    };
    let mut embedded = embedded.into_iter();
    let query_vec = match query_vec {
      Some(vec) => vec,
      None => match embedded.next() {
        Some(vec) => vec,
        None => return fused,
      },
    };
    {
      let mut cache = self
        .encoder_cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
      for (id, row) in miss_ids.iter().zip(embedded) {
        cache.insert(*id, row.clone());
        rows.insert(*id, row);
      }
    }
    let mut keyed: Vec<(f64, usize)> = Vec::with_capacity(fused.len().saturating_sub(first));
    for (position, hit) in fused.iter().enumerate().skip(first) {
      let cosine = rows.get(&hit.0).map_or(f64::NEG_INFINITY, |row| {
        query_vec
          .iter()
          .zip(row)
          .map(|(a, b)| *a as f64 * *b as f64)
          .sum()
      });
      keyed.push((cosine, position));
    }
    // The encoder's own rank list over the pool: cosine descending, fused position
    // breaking ties (a missing row sorts last, deterministically).
    keyed.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
    let order: Vec<usize> = match mode {
      RerankMode::Reorder | RerankMode::Off => keyed.iter().map(|&(_, position)| position).collect(),
      RerankMode::BlendPinned | RerankMode::Blend => {
        // One more reciprocal-rank list, added to each hit's fused mass with the
        // fusion's own K — the encoder re-weights within the pool but can never inject
        // a candidate the channels did not surface. Records keep their fused scores;
        // the blend is a local sort key. Ties break by id, as the fusion does.
        let mut blended: Vec<(f32, u64, usize)> = keyed
          .iter()
          .enumerate()
          .map(|(encoder_rank, &(_, position))| {
            let hit = &fused[position];
            (hit.1 + 1.0 / (RRF_K + encoder_rank as f32), hit.0, position)
          })
          .collect();
        blended.sort_by(|a, b| {
          b.0
            .partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.1.cmp(&b.1))
        });
        blended.into_iter().map(|(_, _, position)| position).collect()
      }
    };
    let mut reordered = Vec::with_capacity(fused.len());
    if first == 1 {
      reordered.push(std::mem::replace(&mut fused[0], (0, 0.0, Vec::new())));
    }
    for &position in &order {
      reordered.push(std::mem::replace(&mut fused[position], (0, 0.0, Vec::new())));
    }
    reordered
  }

  /// Label-free per-corpus BM25 gate (semantic-tier directive 4): known-item
  /// self-probes, the fused ranking with and without the fourth list PAIRED from one
  /// channel computation. Every choice below is derived or precedented, none tuned:
  ///
  /// * Probe nodes — seeded by the node-segment stamp (content-derived), taken in
  ///   `xxh3_64(id bytes, seed=stamp)` ascending order, so the set is deterministic
  ///   and stable under unrelated edits; eligible = non-Import nodes whose names
  ///   carry ≥ 3 distinct tokens, because a STRICT token subset then lands each
  ///   probe in the token-subset name regime where the channels genuinely disagree
  ///   (a full-name probe is decided by the byte-exact tier and carries no signal —
  ///   the tier ladder itself, not a tuning).
  /// * Probe queries — 2 or 3 leading name tokens (alternating): a strict token
  ///   subset of the probed name, landing each probe in the token-subset regime
  ///   where the channels genuinely disagree and BM25's TF/length-norm evidence
  ///   reorders competing subset-matches — the mechanism behind BOTH graded
  ///   outcomes (the kernel's demotions and cpython's gains). A second,
  ///   signature-token family was tried and MEASURED-AND-REJECTED: signature
  ///   tokens are shared by thousands of nodes, the probed node never reaches
  ///   the fused top 10 (paired means 0.0000 on both bench corpora), so that
  ///   family carries no signal — recorded in BENCHMARKS.
  /// * Metric — reciprocal rank of the probe's own node in the fused top 10
  ///   (MRR@10; 10 is the campaign's standing ranking cutoff).
  /// * Verdict — enable iff the paired mean STRICTLY improves AND wins − losses
  ///   clears the two-sided 95% sign-test bound `1.96·√(wins+losses)` (standard
  ///   normal approximation — a cited bound, not a tuned threshold). A corpus
  ///   where the list demotes true answers fails both clauses; weak evidence
  ///   stays conservatively off.
  fn bm25_self_probe_verdict(&self, stamp: u64) -> Result<(bool, String), Box<dyn Error>> {
    /// Probe count: pinned by the recorded sweep at two scales (BENCHMARKS
    /// "per-corpus BM25 gate") — verdicts were identical across 64–512 on both
    /// bench corpora; the largest swept count ships (maximum evidence per verdict,
    /// ~2 s once per generation at kernel scale).
    const PROBES: usize = 512;
    // Sweep seam — measurement builds only, never a production knob (the
    // selection-file lesson: env is a hijack surface; `bench-internals` binaries
    // are never shipped).
    #[cfg(feature = "bench-internals")]
    let probe_target: usize = std::env::var("VORPAL_BM25_GATE_PROBES")
      .ok()
      .and_then(|v| v.parse().ok())
      .unwrap_or(PROBES);
    #[cfg(not(feature = "bench-internals"))]
    let probe_target: usize = PROBES;
    /// MRR cutoff — the campaign's standing @10 ranking cutoff.
    const CUTOFF: usize = 10;
    let kg = &self.kg;
    let mut order: Vec<(u64, u64)> = (0..kg.node_count() as u64)
      .map(|id| {
        (
          xxhash_rust::xxh3::xxh3_64_with_seed(&id.to_le_bytes(), stamp),
          id,
        )
      })
      .collect();
    order.sort_unstable();
    let filter = SearchFilter::default();
    let mut probes = 0usize;
    let mut wins = 0usize;
    let mut losses = 0usize;
    let (mut mean_on, mut mean_off) = (0.0f64, 0.0f64);
    for &(_, id) in &order {
      if probes == probe_target {
        break;
      }
      let Some(view) = kg.node(NodeId::new(id)) else {
        continue;
      };
      if view.kind == vorpal_kg::SymbolKind::Import {
        continue;
      }
      let tokens = tokenize(view.name);
      let mut distinct = tokens.clone();
      distinct.sort_unstable();
      distinct.dedup();
      if distinct.len() < 3 {
        continue;
      }
      let take = 2 + (probes & 1);
      let query = tokens
        .iter()
        .take(take)
        .cloned()
        .collect::<Vec<_>>()
        .join(" ");
      // Both fusions PAIRED from one channel computation.
      let channels =
        self.channel_lists_with_bm25(&query, rerank_pool(CUTOFF), &filter, true, None, None)?;
      let base = vec![channels.named, channels.semantic, channels.by_degree];
      let rank_of = |lists: &[Vec<u64>]| -> f64 {
        rrf_fuse_explained(lists, CUTOFF)
          .iter()
          .position(|hit| hit.0 == id)
          .map_or(0.0, |rank| 1.0 / (rank as f64 + 1.0))
      };
      let rr_off = rank_of(&base);
      let mut with_bm25 = base;
      with_bm25.push(channels.bm25);
      let rr_on = rank_of(&with_bm25);
      mean_on += rr_on;
      mean_off += rr_off;
      if rr_on > rr_off {
        wins += 1;
      } else if rr_on < rr_off {
        losses += 1;
      }
      probes += 1;
    }
    if probes == 0 {
      // Too small to measure: the fourth list stays off, and the record says why.
      return Ok((false, "probes=0 (no eligible nodes)".to_string()));
    }
    mean_on /= probes as f64;
    mean_off /= probes as f64;
    let contested = (wins + losses) as f64;
    let significant = wins as f64 - losses as f64 > 1.96 * contested.sqrt();
    let enabled = significant && mean_on > mean_off;
    let evidence = format!(
      "probes={probes} wins={wins} losses={losses} mean_on={mean_on:.4} mean_off={mean_off:.4}"
    );
    Ok((enabled, evidence))
  }

  /// The three ranked candidate channels (name, semantic, graph) at one pool depth —
  /// the shared body behind [`Searcher::run`] (which RRF-fuses them at k) and the
  /// multi-phrase rungs (which accumulate them into dense per-phrase score tables
  /// without materializing a fused list per phrase).
  fn channel_lists(
    &self,
    query: &str,
    pool: usize,
    filter: &SearchFilter,
  ) -> Result<Channels, Box<dyn Error>> {
    self.channel_lists_with_bm25(query, pool, filter, self.bm25_enabled, None, None)
  }

  /// [`Searcher::run`] with the semantic candidate pool SUPPLIED by the caller (the
  /// daemon's live ANN tier) instead of chosen by the tier dispatch below. Everything
  /// downstream — filters, the full-precision re-embedding rerank, RRF fusion, the
  /// encoder rerank — is the shared path, so a live-tier answer differs from a
  /// persisted-tier answer only in which approximate pool fed the exact rerank.
  pub fn run_with_semantic_pool(
    &self,
    query: &str,
    k: usize,
    filter: &SearchFilter,
    semantic_pool: Vec<u64>,
  ) -> Result<Vec<FusedHit>, Box<dyn Error>> {
    let dense_query = self.dense_query(query);
    let channels = self.channel_lists_with_bm25(
      query,
      rerank_pool(k),
      filter,
      self.bm25_enabled,
      Some(semantic_pool),
      dense_query.as_deref(),
    )?;
    let lists = self.fused_lists(channels, dense_query.is_some(), k);
    Ok(self.rerank_with_encoder(query, self.fuse(&lists, k), dense_query))
  }

  /// [`Searcher::channel_lists`] with the BM25 list computed on demand — the serving
  /// path passes the record's per-corpus verdict; the warm-time gate passes `true`
  /// so both fusions probe from ONE channel computation (paired by construction).
  /// `supplied_pool` (the daemon's live ANN tier) replaces the semantic tier
  /// dispatch when present; every other channel is unchanged. `dense_query` (the
  /// fixed-order encoder embedding of the prefixed query) turns the dense
  /// sidecar channel on for this call; `None` leaves that list empty.
  fn channel_lists_with_bm25(
    &self,
    query: &str,
    pool: usize,
    filter: &SearchFilter,
    want_bm25: bool,
    supplied_pool: Option<Vec<u64>>,
    dense_query: Option<&[f32]>,
  ) -> Result<Channels, Box<dyn Error>> {
    let kg = &self.kg;
    let index_dir = self.generation_dir.as_path();
    let compiled_filter = CompiledSearchFilter::compile(filter)?;
    // The handle's embedder — the persisted tier's own model (see `Searcher::open`),
    // so query vectors, overlay embeds, and the rerank always match the stored rows.
    let embedder = &self.embedder;
    let query_vec = embedder.embed(query);
    // One root per query session: every node vector this search computes strips the same
    // canonical tree prefix (see `embedding_root`).
    let embed_root = embedding_root(kg);

  // Semantic candidate pool, by tier — a search NEVER waits on an ANN build:
  // 1. **Base-fresh**: the persisted tier matches this KG generation → beam search.
  // 2. **Overlay**: the base is one or more edits behind, but `ann.files` reconciles —
  //    unchanged files' candidates remap to current ids, changed/new files' rows score
  //    exactly, dead rows tombstone. Milliseconds, and recall for edited code is exact.
  // 3. **Fallback**: anything else — no base, torn artifacts, overlay too large — takes the
  //    fused exhaustive scan (~0.2s at kernel scale, exact recall) and kicks a detached
  //    background warm. Correctness never depends on which tier answered.
  //
  // Filters shrink the pool AFTER the approximate tiers (the ANN cannot pre-filter), so a
  // filtered query overfetches to keep its post-filter pool honest; the exhaustive fallback
  // filters BEFORE scoring and needs no slack.
  let take = pool * 2 * if filter.is_empty() { 1 } else { 4 };
  // Route between the two semantic engines by THIS index's warm-time calibration
  // (`ann.calib`: the crossover measured on the running machine over the ingested rows —
  // see `calibrate_semantic_cutover`). Without a calibration, the floor is the proven
  // structural rule alone: a fetch covering the full population is categorically not
  // ANN work (a beam's reach is a graph traversal with NO completeness guarantee at
  // take ≥ n), while the flat scan is exact by construction. No frozen machine numbers
  // exist on either path.
  let take_exhaustive = take >= self.semantic_cutover;
  enum SemanticCandidates {
    /// From an approximate tier (beam / overlay): the exact rerank below orders them.
    Approx(Vec<u64>),
    /// From the exhaustive scan: already exactly scored, pre-filtered, and in the
    /// `(dist, id)` total order the rerank would produce — re-embedding them would
    /// re-derive byte-identical results, so the rerank is skipped.
    Exact(Vec<(u64, f32)>),
    /// From the persisted code walk (LEARNED tier): complete over the admitted
    /// population and ordered by the beam's own code-space distance
    /// ([`AnnIndex::scan_codes`]) — full precision re-scores winners and bounded
    /// pools, never n rows through the model. Distances (and the
    /// [`POSITIVE_BOUNDARY`] trim they feed) are the same algebra at code precision.
    Quantized(Vec<(u64, f32)>),
  }
  let candidates: SemanticCandidates = if let Some(supplied) = supplied_pool {
    // The daemon's live tier already picked the pool; the exact rerank below
    // orders it — identical downstream to any approximate tier.
    SemanticCandidates::Approx(supplied)
  } else if !take_exhaustive && let Some(ann) = &self.ann {
    SemanticCandidates::Approx(
      ann
        .search(&query_vec, take)
        .into_iter()
        .map(|(id, _)| id)
        .collect(),
    )
  } else if let Some(overlay) = (!take_exhaustive
    && !self.exact_only
    // Provenance gap fix (the mixing hazard found in planning): a carried-forward
    // ann.bin under a CHANGED model must never assemble — the overlay only exists
    // when the persisted record matches the embedder this handle queries with, and
    // overlay rows embed through that same embedder below.
    && persisted_model_provenance(index_dir).as_ref() == Some(&embedder.provenance()))
    .then(|| annfiles::OverlayView::assemble(index_dir, kg, embedder.dim()))
    .flatten()
  {
    autowarm::maybe_spawn(index_dir);
    // Overfetch by the dead-row count (bounded) so tombstoning cannot starve the pool.
    let bump = (overlay.tombstoned_nodes as usize).min(take);
    let mut ids: Vec<u64> = AnnIndex::load(&index_dir.join("ann.bin"))?
      .search(&query_vec, take + bump)
      .into_iter()
      .filter_map(|(base_id, _)| overlay.remap(base_id))
      .collect();
    let overlay_hits = vorpal_ann::exhaustive_semantic(
      embedder.dim(),
      &overlay.overlay_ids,
      |i, row| embed_node_into(kg, embedder, overlay.overlay_ids[i], row, &embed_root),
      &query_vec,
      take,
    );
    // Disjoint by construction (remap targets are unchanged files; overlay ids are
    // changed/new files) — a plain union; the exact rerank below orders everything.
    ids.extend(overlay_hits.into_iter().map(|(id, _)| id));
    SemanticCandidates::Approx(ids)
  } else {
    // Kick a detached warm so the *next* search takes the fast tier — gated (registered
    // binaries only, opt-out, once per process) and best-effort; see `autowarm`. The
    // exact-only seam measures the index as-is and must not mutate it, and a deep-pool
    // bypass over FRESH tiers (`ann` present) has nothing to warm.
    if !self.exact_only && self.ann.is_none() {
      autowarm::maybe_spawn(index_dir);
    }
    // Under the LEARNED tier a full-population re-embed is unaffordable (the model
    // pools subwords per token — measured: calibration's scan probes alone cost 191 s
    // of a 405 s kernel warm), so exhaustive fetches walk the persisted i8 codes:
    // the same distance the beam ranks by, complete over the admitted population,
    // full precision returning for bounded pools and winners. `Searcher::open`
    // guarantees a learned handle carries its ann tier, so the walk is available
    // wherever it matters (flat tiers fall through — they exist only at sizes where
    // the re-embed scan is cheap). The exact-only reference seam still re-embeds:
    // it IS the full-precision truth, cost accepted.
    let quantized = match (&self.embedder, &self.ann) {
      (ActiveEmbedder::Learned(_), Some(ann)) if !self.exact_only => ann.scan_codes(
        &query_vec,
        take,
        |id| filter.is_empty() || compiled_filter.admits(kg, id),
      ),
      _ => None,
    };
    match quantized {
      Some(scored) => SemanticCandidates::Quantized(scored),
      None => {
        let mut ids = semantic_row_ids(kg);
        // The exhaustive path filters BEFORE scoring: exact recall over exactly the
        // admitted population, no overfetch slack needed.
        if !filter.is_empty() {
          ids.retain(|&id| compiled_filter.admits(kg, id));
        }
        SemanticCandidates::Exact(vorpal_ann::exhaustive_semantic(
          embedder.dim(),
          &ids,
          |i, row| embed_node_into(kg, embedder, ids[i], row, &embed_root),
          &query_vec,
          take,
        ))
      }
    }
  };

  // Exactly order the pool as (distance, id). Approximate candidates are re-embedded at
  // full precision — approximation chooses the pool, never the final semantic order
  // (§10's rerank bar) — and, since the tiers cannot pre-filter, non-admitted rows drop
  // here. Exhaustive candidates arrive already exact, pre-filtered, and in this precise
  // total order (crates/ann/src/scan.rs sorts by `(dist, id)` under `total_cmp`), so
  // re-deriving them would be pure duplicate work at full-population sizes.
  let (semantic, semantic_dist2): (Vec<u64>, Vec<f32>) = {
    let mut scored: Vec<(f32, u64)> = match candidates {
      // Quantized lists arrive complete and ordered by the code-space distance the
      // beam itself ranks by; re-embedding them here would put n rows through the
      // model — full precision instead re-scores the bounded winners downstream.
      SemanticCandidates::Exact(scored) | SemanticCandidates::Quantized(scored) => {
        scored.into_iter().map(|(id, dist)| (dist, id)).collect()
      }
      SemanticCandidates::Approx(ids) => {
        let mut scored: Vec<(f32, u64)> = ids
          .into_iter()
          .filter(|&id| filter.is_empty() || compiled_filter.admits(kg, id))
          .map(|id| {
            let mut row = vec![0.0f32; embedder.dim()];
            embed_node_into(kg, embedder, id, &mut row, &embed_root);
            // The ONE shared distance tree (vorpal_ann::l2_sq — lane-decomposed SIMD,
            // bit-deterministic): the exhaustive arm's rerank-skip is sound only
            // because scan and rerank distances agree bit-for-bit.
            (vorpal_ann::l2_sq(&row, &query_vec), id)
          })
          .collect();
        scored.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
        scored
      }
    };
    scored.truncate(pool);
    scored.into_iter().map(|(dist, id)| (id, dist)).unzip()
  };

  let query_tokens = tokenize(query);
  // Classify one candidate into the name channel's tiers (exact string, token-equal,
  // token-subset) — shared verbatim by the posting-index path and the exhaustive scan, so
  // the two paths cannot diverge in what they admit.
  let classify = |i: u64| -> Option<(u64, (u8, usize))> {
    if !filter.is_empty() && !compiled_filter.admits(kg, i) {
      return None;
    }
    let view = kg.node(NodeId::new(i))?;
    let name_tokens = tokenize(view.name);
    let tier = if view.name == query {
      0
    } else if !query_tokens.is_empty() && name_tokens == query_tokens {
      1
    } else if !query_tokens.is_empty() && query_tokens.iter().all(|t| name_tokens.contains(t)) {
      2
    } else {
      return None;
    };
    Some((i, (tier, view.name.len())))
  };
  // Deduplicated query tokens for the posting intersection (lists store distinct tokens).
  let lookup_tokens = {
    let mut t = query_tokens.clone();
    t.sort_unstable();
    t.dedup();
    t
  };
  // The persisted lexical tier (IMPROVEMENTS #9): when the posting index matches this
  // generation, the name channel classifies only the intersection candidates instead of
  // tokenizing every node. Every scan hit's name tokens are a superset of the query
  // tokens, so the intersection provably contains all of them — identical results, and
  // the fallback below stays the correctness anchor whenever the tier is missing/stale
  // (a background warm heals it).
  let posting_candidates: Option<Vec<u64>> = if query_tokens.is_empty() {
    None
  } else {
    self
      .postings
      .as_ref()
      .and_then(|p| p.candidates(&lookup_tokens))
      .map(|ids| ids.into_iter().map(|id| id as u64).collect())
  };
  let mut named: Vec<(u64, (u8, usize))> = match posting_candidates {
    Some(candidates) => candidates.into_iter().filter_map(classify).collect(),
    None => {
      // Parallel scan over the node rows; per-row work is pure, and the indexed flatten
      // keeps ascending-id order, so the collected list is identical to the serial loop's.
      use rayon::prelude::*;
      (0..kg.node_count() as u64)
        .into_par_iter()
        .filter_map(classify)
        .collect()
    }
  };
  named.sort_by_key(|&(id, key)| (key, id));
  named.truncate(pool);
  let named: Vec<u64> = named.into_iter().map(|(id, _)| id).collect();

  // In-degree is a *disambiguator among name-matched candidates* (three `seal` methods → the
  // most-called one first), never a global popularity prior — a union with the semantic pool
  // let popular-but-irrelevant symbols outrank the semantically-best hit on descriptive queries
  // (caught by dogfood: a heavily-referenced `stdin` field beat `Manifest`).
  let mut by_degree: Vec<u64> = named.clone();
  by_degree.sort_by_key(|&id| {
    (
      // Referential degree only: the flow-era edge families (similar_to, changes_with,
      // data_flows, …) are topology, not usage — counting them silently redefined this
      // disambiguator when they landed (an 8-partner clone family gains 16 raw in-edges
      // that say nothing about how called the symbol is).
      std::cmp::Reverse(kg.in_degree_referential(NodeId::new(id))),
      id,
    )
  });

  // The BM25 channel (semantic-tier Stage 4): TF + length normalization discriminate
  // the 2–6-token names that the token-subset tier and in-degree fusion drown at
  // kernel scale (the measured short-keyword collapse this channel exists to fix).
  // Postings fresh → the persisted walk; anything else → the bit-identical exhaustive
  // pass, so results never depend on which tier answered.
  let bm25 = if want_bm25 {
    let bm25_admit = |id: u32| filter.is_empty() || compiled_filter.admits(kg, id as u64);
    match &self.postings {
      Some(postings) => postings
        .bm25_ranked(&query_tokens, pool, bm25_admit)
        .unwrap_or_else(|| postings::bm25_exhaustive(kg, &query_tokens, pool, bm25_admit)),
      None => postings::bm25_exhaustive(kg, &query_tokens, pool, bm25_admit),
    }
  } else {
    Vec::new()
  };

  // The dense channel (`dense.rs`): int8 scan + f16 rescore over the sidecar's
  // covered definitions, filtered BEFORE ranking (exact, no overfetch slack).
  // The list's LENGTH is the sidecar's derived depth (`DenseSidecar::depth`, the
  // two-field surfaces' depth seam); the fusion LAW then admits from that scored
  // list against the fusion pool (`admit_dense`, the size-aware fusion seam) —
  // both pinned at their measured defaults (depth factor 1, `Flat`), under which
  // this composes to the shipped fifth list.
  let law = dense_fusion();
  let dense = match (dense_query, &self.dense) {
    (Some(query_vec), Some(sidecar)) => {
      let scored = sidecar.search_scored(query_vec, sidecar.depth(pool), &|id| {
        filter.is_empty() || compiled_filter.admits(kg, id)
      });
      admit_dense(law, &scored, pool)
    }
    _ => Vec::new(),
  };
  if law == DenseFusion::Degree && !dense.is_empty() {
    // Degree support: the in-degree disambiguator ranks the name-matched AND the
    // dense candidates (union, deduplicated), by the same referential-degree key.
    let mut seen: std::collections::HashSet<u64> = by_degree.iter().copied().collect();
    by_degree.extend(dense.iter().copied().filter(|id| seen.insert(*id)));
    by_degree.sort_by_key(|&id| (std::cmp::Reverse(kg.in_degree_referential(NodeId::new(id))), id));
  }

    Ok(Channels {
      named,
      semantic,
      semantic_dist2,
      by_degree,
      bm25,
      dense,
    })
  }

  /// Run a search and render it to the CLI's exact text format — shared by `search_index`
  /// and the batched async path so there is one rendering contract everywhere.
  pub fn search_rendered(
    &self,
    query: &str,
    k: usize,
    explain: bool,
  ) -> Result<String, Box<dyn Error>> {
    self.search_rendered_filtered(query, k, explain, &SearchFilter::default())
  }

  /// [`Searcher::search_rendered`] with structured pre-ranking filters — the rendering
  /// contract is identical; only the candidate population narrows.
  pub fn search_rendered_filtered(
    &self,
    query: &str,
    k: usize,
    explain: bool,
    filter: &SearchFilter,
  ) -> Result<String, Box<dyn Error>> {
    if parse_and_phrases(query).is_some() {
      return self.render_multi(query, k, explain, filter);
    }
    let ranked = self.run(query, k, filter)?;
    let kg = &self.kg;
    let mut out = String::new();
    for (row, score, ranks) in ranked {
      if let Some(view) = kg.node(NodeId::new(row)) {
        if explain {
          let channels = ["name", "vector", "graph"];
          let mut provenance = format!("id {row}");
          for (channel, rank) in channels.iter().zip(&ranks) {
            if let Some(rank) = rank {
              let _ = write!(provenance, "; {channel}#{}", rank + 1);
            }
          }
          let _ = writeln!(
            out,
            "{score:.4}  {} [{:?}] {}  ({provenance})",
            view.name, view.kind, view.path
          );
        } else {
          let _ = writeln!(
            out,
            "{score:.4}  {} [{:?}] {}",
            view.name, view.kind, view.path
          );
        }
      }
    }
    Ok(out)
  }

  /// Render a conjunctive query: identical line format to the single-phrase renderer
  /// (hit records carry the same `{:?}`-formatted kind), phrase-tagged provenance under
  /// explain (`p1:name#3`), and an explicit eliminator line instead of silence when the
  /// intersection is empty.
  fn render_multi(
    &self,
    query: &str,
    k: usize,
    explain: bool,
    filter: &SearchFilter,
  ) -> Result<String, Box<dyn Error>> {
    let report = self.report(query, k, filter)?;
    let mut out = String::new();
    if report.hits.is_empty()
      && let Some(mp) = &report.multi_phrase
      && let Some(index) = mp.eliminated_by
    {
      let pools = mp
        .per_phrase_pool
        .iter()
        .map(usize::to_string)
        .collect::<Vec<_>>()
        .join(", ");
      let _ = writeln!(
        out,
        "(no results: phrase {}/{} {:?} eliminated all candidates; per-phrase pools: {pools} \
         at depth {})",
        index + 1,
        mp.phrases.len(),
        mp.phrases.get(index).map(String::as_str).unwrap_or("?"),
        mp.intersection_depth,
      );
      return Ok(out);
    }
    for hit in &report.hits {
      if explain {
        let mut provenance = format!("id {}", hit.node.id);
        for rank in &hit.channels {
          match rank.phrase {
            Some(phrase) => {
              let _ = write!(provenance, "; p{}:{}#{}", phrase + 1, rank.channel, rank.rank);
            }
            None => {
              let _ = write!(provenance, "; {}#{}", rank.channel, rank.rank);
            }
          }
        }
        let _ = writeln!(
          out,
          "{:.4}  {} [{}] {}  ({provenance})",
          hit.score, hit.node.name, hit.node.kind, hit.node.path
        );
      } else {
        let _ = writeln!(
          out,
          "{:.4}  {} [{}] {}",
          hit.score, hit.node.name, hit.node.kind, hit.node.path
        );
      }
    }
    Ok(out)
  }
}

/// A reusable handle to an index's current generation: opens (mmaps) every tier once and
/// answers many queries lock-free. Ideal for bulk/concurrent work — hold one and fan queries
/// across threads, instead of paying a cache lookup (and, historically, a full re-`mmap`) per
/// call. Backed by the same immutable-generation cache the one-shot search functions use.
pub fn open_searcher(index_dir: &Path) -> Result<Arc<Searcher>, Box<dyn Error>> {
  cached_searcher(index_dir)
}

/// Process-wide LRU cache of open [`Searcher`]s, keyed by the immutable generation dir.
/// Repeated searches (a daemon, MCP, the async pool) reuse one set of mappings instead of
/// re-`mmap`ing every tier per call. Safe by construction: generation dirs are
/// content-addressed and immutable, so a cached entry is never stale — a rebuild resolves to a
/// new dir and opens a fresh entry. Bounded, so retired generations' mappings are released.
/// Newest-first LRU of open searchers, keyed by immutable generation dir.
type SearcherCache = Mutex<Vec<(PathBuf, Arc<Searcher>)>>;

fn cached_searcher(index_dir: &Path) -> Result<Arc<Searcher>, Box<dyn Error>> {
  const CAP: usize = 8;
  static CACHE: OnceLock<SearcherCache> = OnceLock::new();
  let generation_dir = vorpal_kg::resolve_index_dir(index_dir);
  let cache = CACHE.get_or_init(|| Mutex::new(Vec::new()));
  {
    let mut guard = cache.lock().unwrap();
    if let Some(pos) = guard.iter().position(|(dir, _)| *dir == generation_dir) {
      let entry = guard.remove(pos);
      let searcher = entry.1.clone();
      guard.push(entry); // most-recently-used to the back
      return Ok(searcher);
    }
  }
  // Open (mmap all tiers) outside the lock — a load must not serialize searches on other
  // generations.
  let searcher = Arc::new(Searcher::open(&generation_dir)?);
  let mut guard = cache.lock().unwrap();
  // A concurrent caller may have opened the same generation meanwhile; keep a single entry.
  if let Some(pos) = guard.iter().position(|(dir, _)| *dir == generation_dir) {
    return Ok(guard[pos].1.clone());
  }
  guard.push((generation_dir, searcher.clone()));
  if guard.len() > CAP {
    guard.remove(0);
  }
  Ok(searcher)
}

/// Benchmark-only internal seams, compiled ONLY under the non-default `bench-internals`
/// feature — release builds (`-p vorpal-index`, `-p vorpal`, crates.io consumers) carry
/// none of this. Consumers: `examples/sweep_semantic.rs` (the recorded engine-cost
/// sweep behind the `exhaustive_cutover` fit) and `cargo xtask searcheval`
/// (tier-vs-exact reference mode via [`Searcher::open_exact`]).
#[cfg(feature = "bench-internals")]
#[doc(hidden)]
pub mod bench {
  use std::error::Error;
  use std::path::Path;

  use vorpal_ann::Embedder;
  use vorpal_kg::Kg;

  /// The semantic row population (every non-Import node), as production selects it.
  pub fn semantic_rows(kg: &Kg) -> Vec<u64> {
    super::semantic_row_ids(kg)
  }

  /// Embed one node exactly as production does under the DEFAULT (lexical) model —
  /// the sweep must time the real fill cost, never a drifted replica. (Learned-tier
  /// fill costs are the training/warm phase stamps' business.)
  pub fn embed_row(kg: &Kg, id: u64, row: &mut [f32]) {
    let embedder = super::ActiveEmbedder::Lexical(super::active_embedder());
    let root = super::embedding_root(kg);
    super::embed_node_into(kg, &embedder, id, row, &root);
  }

  /// A query embedding, as production computes it.
  pub fn embed_query(query: &str) -> Vec<f32> {
    super::active_embedder().embed(query)
  }

  pub fn embed_dim() -> usize {
    super::active_embedder().dim()
  }

  /// True iff the persisted ann tier is coherent for this index (stamp, header, tier
  /// record, and — learned — the checksum-verified model) — the sweep refuses stale
  /// tiers so it can never silently time the wrong engine.
  pub fn ann_tier_fresh(index_dir: &Path) -> Result<bool, Box<dyn Error>> {
    let generation_dir = vorpal_kg::resolve_index_dir(index_dir);
    let kg = Kg::load(&generation_dir)?;
    Ok(super::coherent_persisted_embedder(&generation_dir, super::stamp_of(&kg)).is_some())
  }

  /// Dense-channel rank probe (the paraphrase question): the sidecar's FULL ranking
  /// of every covered row for `query`, reported as the 0-based dense rank of each
  /// named node (`None` = not covered) — with the encoder cosine at that rank and
  /// the top few names for context. Opens the index as serving does (encoder from
  /// the root/global selection), so a missing sidecar or encoder is a typed error.
  #[allow(clippy::type_complexity)] // (per-name rank, top-10 names) — a bench report tuple
  pub fn dense_ranks(
    index_dir: &Path,
    query: &str,
    names: &[&str],
  ) -> Result<(Vec<(String, Option<usize>)>, Vec<String>), Box<dyn Error>> {
    let searcher = super::Searcher::open(index_dir)?;
    let Some(sidecar) = &searcher.dense else {
      return Err("no fresh dense sidecar for this index/encoder".into());
    };
    let Some(encoder) = searcher.encoder.as_deref() else {
      return Err("no encoder selected".into());
    };
    let query_vec = encoder.embed_query(query)?;
    let ranked = sidecar.search(&query_vec, sidecar.len(), &|_| true);
    let name_of = |id: u64| -> String {
      searcher
        .kg
        .node(vorpal_kg::NodeId::new(id))
        .map(|view| view.name.to_string())
        .unwrap_or_default()
    };
    let ranks = names
      .iter()
      .map(|name| {
        let at = ranked.iter().position(|&id| name_of(id) == *name);
        (name.to_string(), at)
      })
      .collect();
    let head = ranked.iter().take(10).map(|&id| name_of(id)).collect();
    Ok((ranks, head))
  }

  /// Positivity probe for the conjunction-support question: for one phrase, how many
  /// rows sit strictly inside [`super::POSITIVE_BOUNDARY`] (dist² < 2 ⇔ cos > 0)
  /// under the index's PERSISTED tier, versus how many rows the lexical token-overlap
  /// support admits. The Stage-AND veto of vector-sign support was measured against
  /// the hashed lexical space (collisions made nonsense phrases near-globally
  /// "positive"); this measures the TRAINED space before any support re-derivation.
  /// Returns (positive rows, lexical-support rows, total rows).
  pub fn positivity(index_dir: &Path, phrase: &str) -> Result<(usize, usize, usize), Box<dyn Error>> {
    let generation_dir = vorpal_kg::resolve_index_dir(index_dir);
    let kg = Kg::load(&generation_dir)?;
    let stamp = super::stamp_of(&kg);
    let Some(embedder) = super::coherent_persisted_embedder(&generation_dir, stamp) else {
      return Err("no coherent persisted tier — warm first".into());
    };
    let embed_root = super::embedding_root(&kg);
    let query = embedder.embed(phrase);
    let rows = super::semantic_row_ids(&kg);
    let ann = vorpal_ann::AnnIndex::load(&generation_dir.join("ann.bin"))?;
    let positive = match ann.scan_codes(&query, rows.len(), |_| true) {
      Some(scored) => scored
        .iter()
        .filter(|(_, dist)| *dist < super::POSITIVE_BOUNDARY)
        .count(),
      None => vorpal_ann::exhaustive_semantic(
        embedder.dim(),
        &rows,
        |i, row| super::embed_node_into(&kg, &embedder, rows[i], row, &embed_root),
        &query,
        rows.len(),
      )
      .iter()
      .filter(|(_, dist)| *dist < super::POSITIVE_BOUNDARY)
      .count(),
    };
    let token_sets = vec![super::tokenize(phrase)];
    let lexical = rows
      .iter()
      .filter(|&&id| super::node_lexical_support_bits(&kg, id, &token_sets) != 0)
      .count();
    Ok((positive, lexical, rows.len()))
  }
}

/// Run one graph query verb against a persisted index and render the results — the shared
/// implementation behind the `vorpal-index` binary and the `vorpal graph` subcommand.
/// How a graph query names its target symbol — the CLI/MCP argument surface over
/// [`vorpal_kg::SymbolSelector`]. `merge_all` restores the historical union-over-namesakes
/// behavior explicitly; without it, an ambiguous name yields the candidate list instead.
#[derive(Debug, Clone, Default)]
pub struct GraphTarget {
  pub name: String,
  pub id: Option<u64>,
  /// Durable external id (32 hex chars) — the cross-generation bookmark form. Also accepted
  /// on every surface as an `eid:<hex>` name.
  pub external_id: Option<u128>,
  pub path_suffix: Option<String>,
  pub kind: Option<String>,
  pub merge_all: bool,
  /// Suffix every result line with its node id (scripting/agents; default keeps the
  /// human-stable format).
  pub show_ids: bool,
}

pub fn graph_query(index_dir: &Path, verb: &str, name: &str) -> Result<String, Box<dyn Error>> {
  graph_query_selected(
    index_dir,
    verb,
    &GraphTarget {
      name: name.to_string(),
      ..GraphTarget::default()
    },
  )
}

/// Run one graph verb against a selected target. Identity contract (IMPROVEMENTS §1): a
/// name that matches several definitions is answered with the **candidates**, not a silent
/// union — refine with `id`/`path_suffix`/`kind`, or pass `merge_all` to union explicitly.
/// Answer "why does this relation exist?" for the edge(s) `from_id → to_id` (§5): every
/// retained occurrence's edge type, resolution grade + resolver reason, candidate count, and
/// the source span of the referencing token — with a best-effort snippet of the *current* file
/// bytes at that span (labeled as such; files may have drifted since indexing, exactly as with
/// `fetch_span`).
pub fn explain_edge(
  index_dir: &Path,
  from_id: u64,
  to_id: u64,
) -> Result<String, Box<dyn Error>> {
  // Pin one generation for the whole answer: the graph, the evidence rows, and the snippet
  // digest check all come from the directory CURRENT names right now.
  let dir = vorpal_kg::resolve_index_dir(index_dir);
  let kg = Kg::load(&dir)?;
  explain_edge_on(&kg, Some(&dir), from_id, to_id)
}

/// [`explain_edge`] over an already-loaded graph — the daemon form, which answers from its
/// pinned generation instead of re-resolving the index path (a concurrent `CURRENT` swap can
/// never split the answer from the query that produced the ids). `artifacts_dir` is the
/// generation directory the graph was loaded from: when given, the snippet is shown only if
/// the file's current bytes still match the digest that generation indexed — otherwise the
/// snippet is omitted with an explicit note, never silently inconsistent.
pub fn explain_edge_on(
  kg: &Kg,
  artifacts_dir: Option<&Path>,
  from_id: u64,
  to_id: u64,
) -> Result<String, Box<dyn Error>> {
  let (from, to) = (NodeId::new(from_id), NodeId::new(to_id));
  let (Some(from_view), Some(to_view)) = (kg.node(from), kg.node(to)) else {
    return Err(format!("no such node id ({from_id} and/or {to_id})").into());
  };
  let rows = kg.edge_evidence(from, to);
  if rows.is_empty() {
    return Ok(format!(
      "(no recorded evidence for {} → {} — structural edges carry none, and generations \
       written before the evidence sidecar record none)\n",
      from_view.name, to_view.name
    ));
  }
  let mut out = String::new();
  let _ = writeln!(
    out,
    "{} → {}  ({} occurrence{})",
    from_view.name,
    to_view.name,
    rows.len(),
    if rows.len() == 1 { "" } else { "s" }
  );
  let from_path = from_view.path.to_string();
  for row in rows {
    let reason = vorpal_ingest::ResolveReason::from_tag(row.reason).label();
    let grade = confidence_label(row.confidence);
    let _ = writeln!(
      out,
      "  {}  [{grade}; {reason}; {} candidate{}]  {}:{}..{}",
      vorpal_kg::EdgeType(row.etype).name(),
      row.candidates,
      if row.candidates == 1 { "" } else { "s" },
      from_path,
      row.span_start,
      row.span_end
    );
    // "Why this target and not the alternatives?" — the retained tie-set losers, by identity.
    if !row.alternatives.is_empty() {
      let listed: Vec<String> = row
        .alternatives
        .iter()
        .map(|&alt| match kg.node(NodeId::new(alt as u64)) {
          Some(view) => format!("id {alt} ({} {})", view.name, view.path),
          None => format!("id {alt}"),
        })
        .collect();
      let more = (row.candidates as usize).saturating_sub(1 + row.alternatives.len());
      let suffix = if more > 0 {
        format!(" (+{more} more not retained)")
      } else {
        String::new()
      };
      let _ = writeln!(out, "    beat: {}{}", listed.join(", "), suffix);
    }
    // Snippet, digest-verified against the pinned generation (IMPROVEMENTS 07-29 §4): shown
    // only when the file's current bytes still match the digest this generation indexed, so
    // the rendered token can never be silently inconsistent with the edge. Without a pack
    // digest to check (older generation), the snippet is labeled as current-file contents.
    if let Ok(bytes) = fs::read(&from_path) {
      let indexed_digest = artifacts_dir
        .and_then(open_generation_pack)
        .and_then(|pack| {
          pack
            .get(&from_path)
            .and_then(vorpal_ingest::peek_product_digest)
        });
      let (verdict, show) = match indexed_digest {
        Some(digest) if xxhash_rust::xxh3::xxh3_64(&bytes) == digest => ("source verified", true),
        Some(_) => ("", false),
        None => ("current file contents", true),
      };
      if show {
        let (s, e) = (row.span_start as usize, row.span_end as usize);
        if e <= bytes.len() && s < e && e - s <= 200 {
          if let Ok(text) = std::str::from_utf8(&bytes[s..e]) {
            let _ = writeln!(out, "    `{}` ({verdict})", text.trim());
          }
        }
      } else {
        let _ = writeln!(out, "    (file changed since indexing — snippet omitted)");
      }
    }
  }
  Ok(out)
}

mod compose;

pub use vorpal_kg::resolve_index_dir;

/// Open the pack of a committed generation for QUERY surfaces that hold only the index
/// directory (no src root in scope), deriving the bucketed layout's stripping root
/// **exactly** from the generation's own manifest — never by guessing.
///
/// Derivation: take the first manifest entry's absolute path and try every `/` boundary as
/// the root; a candidate is accepted only if stripping it maps EVERY manifest entry onto a
/// pack hit and the pack's live-entry count equals the manifest's. That acceptance is
/// unique: two accepting roots would compose into a length-shifting bijection of a finite
/// key set (every key `k` would need `x/k` present too), which cannot exist — so a false
/// root can never pass, and a pack that fails derivation is returned rootless (absolute
/// lookups miss, callers degrade to "unverified", wrong bytes are impossible).
pub(crate) fn open_generation_pack(dir: &Path) -> Option<PackReader> {
  let pack = PackReader::open(dir)?;
  if !pack.is_bucketed() {
    return Some(pack);
  }
  let Ok(manifest) = Manifest::load(&dir.join("manifest.bin")) else {
    return Some(pack);
  };
  let entries = manifest.entries();
  if entries.is_empty() || entries.len() != pack.live_entries() {
    return Some(pack);
  }
  let probe = entries[0].path.as_str();
  let mut root: Option<String> = None;
  for (at, _) in probe.match_indices('/') {
    let candidate = &probe[..at];
    let accepts = entries.iter().all(|entry| {
      let key = vorpal_kg::identity::tree_relative(&entry.path, candidate);
      key != entry.path && pack.get(key).is_some()
    });
    if accepts {
      root = Some(candidate.to_string());
      break;
    }
  }
  Some(pack.with_root(root))
}

/// Process-wide cache of open [`PackReader`]s, keyed by the **immutable** generation dir —
/// the read-few query surfaces (`snippet`, `fetch_span`, `why`) verify one or two files per
/// call, and opening the pack (a full sidecar parse: one entry per indexed file) costs more
/// than the query itself at kernel scale (~5 ms / 72K entries). Content-addressed generation
/// dirs make the cache safe by construction, exactly like [`cached_searcher`]. Never used by
/// the build path, which owns its reader for the whole run.
pub(crate) fn cached_pack(generation_dir: &Path) -> Option<Arc<PackReader>> {
  const CAP: usize = 8;
  type PackCache = Mutex<Vec<(PathBuf, Arc<PackReader>)>>;
  static CACHE: OnceLock<PackCache> = OnceLock::new();
  let cache = CACHE.get_or_init(|| Mutex::new(Vec::new()));
  {
    let mut guard = cache.lock().unwrap();
    if let Some(pos) = guard.iter().position(|(dir, _)| dir == generation_dir) {
      let entry = guard.remove(pos);
      let pack = entry.1.clone();
      guard.push(entry); // most-recently-used to the back
      return Some(pack);
    }
  }
  // Open (mmap + sidecar parse) outside the lock.
  let pack = Arc::new(open_generation_pack(generation_dir)?);
  let mut guard = cache.lock().unwrap();
  if let Some(pos) = guard.iter().position(|(dir, _)| dir == generation_dir) {
    return Some(guard[pos].1.clone());
  }
  guard.push((generation_dir.to_path_buf(), pack.clone()));
  if guard.len() > CAP {
    guard.remove(0);
  }
  Some(pack)
}

/// Process-wide cache of per-generation FILE RUNS (the path-sorted per-file id ranges every
/// whole-graph surface starts from) — deriving them is a full boundary scan (~300 ms at
/// kernel scale), pure per generation, so immutable content-addressed dirs make the cache
/// safe exactly like [`cached_pack`]/[`cached_searcher`].
pub(crate) fn cached_runs(kg: &vorpal_kg::Kg, generation_dir: Option<&Path>) -> Arc<Vec<annfiles::FileRun>> {
  const CAP: usize = 8;
  type RunsCache = Mutex<Vec<(PathBuf, Arc<Vec<annfiles::FileRun>>)>>;
  static CACHE: OnceLock<RunsCache> = OnceLock::new();
  let Some(dir) = generation_dir else {
    return Arc::new(annfiles::file_runs_of(kg));
  };
  let cache = CACHE.get_or_init(|| Mutex::new(Vec::new()));
  {
    let mut guard = cache.lock().unwrap();
    if let Some(pos) = guard.iter().position(|(cached, _)| cached == dir) {
      let entry = guard.remove(pos);
      let runs = entry.1.clone();
      guard.push(entry);
      return runs;
    }
  }
  let runs = Arc::new(annfiles::file_runs_of(kg));
  let mut guard = cache.lock().unwrap();
  if let Some(pos) = guard.iter().position(|(cached, _)| cached == dir) {
    return guard[pos].1.clone();
  }
  guard.push((dir.to_path_buf(), runs.clone()));
  if guard.len() > CAP {
    guard.remove(0);
  }
  runs
}

/// A source read checked against the generation that indexed it (IMPROVEMENTS #7): persisted
/// byte offsets are only meaningful against the bytes they were computed from.
pub enum IndexedRead {
  /// The file's current bytes match the indexed digest — offsets are exact.
  Verified(Vec<u8>),
  /// No digest is available to check (generation predates pack digests) — the caller may
  /// slice, labeled as current-file contents.
  Unverified(Vec<u8>),
  /// The file changed since indexing: slicing persisted offsets would return bytes
  /// inconsistent with the node. Callers must refuse, not guess.
  Changed,
}

/// Read `path` and verify it against the digest `artifacts_dir`'s product pack recorded at
/// indexing time (the same check `why` snippets use).
pub fn read_indexed_source(
  artifacts_dir: Option<&Path>,
  path: &str,
) -> Result<IndexedRead, String> {
  read_indexed_source_with(artifacts_dir.and_then(cached_pack).as_deref(), path)
}

/// [`read_indexed_source`] against an already-open pack: bulk callers (multi-node snippet
/// selections) open the pack once instead of re-mapping it per file.
pub fn read_indexed_source_with(
  pack: Option<&PackReader>,
  path: &str,
) -> Result<IndexedRead, String> {
  let bytes = fs::read(path).map_err(|err| format!("read {path}: {err}"))?;
  let indexed_digest = pack.and_then(|pack| pack.get(path).and_then(vorpal_ingest::peek_product_digest));
  Ok(match indexed_digest {
    Some(digest) if xxhash_rust::xxh3::xxh3_64(&bytes) == digest => IndexedRead::Verified(bytes),
    Some(_) => IndexedRead::Changed,
    None => IndexedRead::Unverified(bytes),
  })
}

/// The parse-health query (IMPROVEMENTS #11): per-file damage with everything a consumer
/// needs before treating missing relations as meaningful absence — covered-byte ratios,
/// representative merged error spans, parser/language context (language name + the
/// extraction-identity digest the product was built under), and the graph entities whose
/// definition spans intersect damaged regions.
pub fn parse_health_report(index_dir: &Path) -> Result<String, Box<dyn Error>> {
  let dir = vorpal_kg::resolve_index_dir(index_dir);
  let kg = Kg::load(&dir)?;
  let pack = open_generation_pack(&dir).ok_or("no product pack in this generation")?;

  // One pass over the nodes: file paths, and per-path entity lists for overlap checks.
  let mut files: Vec<(u64, String)> = Vec::new();
  let mut by_path: HashMap<String, Vec<u64>> = HashMap::new();
  for id in 0..kg.node_count() as u64 {
    let Some(view) = kg.node(NodeId::new(id)) else {
      continue;
    };
    if view.kind == vorpal_kg::SymbolKind::File {
      files.push((id, view.path.to_string()));
    } else {
      by_path.entry(view.path.to_string()).or_default().push(id);
    }
  }
  files.sort_by(|a, b| a.1.cmp(&b.1));

  let mut out = String::new();
  let mut unhealthy = 0usize;
  let mut total_error_bytes = 0u64;
  for (_, path) in &files {
    let Some(bytes) = pack.get(path) else {
      continue;
    };
    let Ok(product) = vorpal_ingest::decode_product_view(bytes) else {
      continue;
    };
    if product.error_nodes == 0 {
      continue;
    }
    unhealthy += 1;
    total_error_bytes += product.error_bytes;
    let ratio = if product.source_size == 0 {
      100.0
    } else {
      product.error_bytes as f64 / product.source_size as f64 * 100.0
    };
    let language = vorpal_ingest::language_name_of(path).unwrap_or_else(|| "?".to_string());
    let _ = writeln!(
      out,
      "{path} [{language}; extraction-id {:016x}]: {} ERROR nodes, {} of {} bytes ({ratio:.1}%)",
      product.grammar_digest, product.error_nodes, product.error_bytes, product.source_size
    );
    for &(start, end) in &product.error_spans {
      let _ = writeln!(out, "  error span {start}..{end}");
    }
    // Entities whose definition span intersects any damaged region: relations from these
    // may be incomplete — the difference between "no edge" and "unknowable here".
    let mut affected: Vec<String> = Vec::new();
    for &id in by_path.get(path).into_iter().flatten() {
      let Some(view) = kg.node(NodeId::new(id)) else {
        continue;
      };
      let (s, e) = view.span;
      if (s, e) == (0, 0) {
        continue;
      }
      if product
        .error_spans
        .iter()
        .any(|&(es, ee)| s < ee && es < e)
      {
        affected.push(format!("{} [{:?}] (id {id})", view.name, view.kind));
      }
    }
    if !affected.is_empty() {
      let _ = writeln!(out, "  entities in damaged regions: {}", affected.join(", "));
    }
  }
  if unhealthy == 0 {
    return Ok("parse health: clean — every indexed file parsed without ERROR nodes\n".into());
  }
  Ok(format!(
    "parse health: {unhealthy} of {} files carry ERROR nodes ({total_error_bytes} damaged bytes total)\n{out}",
    files.len()
  ))
}

/// Answer "why is there NO edge from this node to anything named `name`?" — the no-edge
/// occurrences (external/masked) the resolver retained for that referenced name, plus any real
/// edges that DO exist to nodes of that name (so a partial answer is never mistaken for none).
pub fn explain_absence_on(kg: &Kg, from_id: u64, name: &str) -> Result<String, Box<dyn Error>> {
  let from = NodeId::new(from_id);
  let Some(from_view) = kg.node(from) else {
    return Err(format!("no such node id {from_id}").into());
  };
  let name_hash = xxhash_rust::xxh3::xxh3_64(name.as_bytes()) as u32;
  let absences = kg.evidence_absences(from, name_hash);
  let mut out = String::new();
  // Real edges to same-named nodes first: absence of SOME occurrences ≠ absence of all.
  let mut edges = 0usize;
  for target in kg.nodes_named(name) {
    for row in kg.edge_evidence(from, target) {
      edges += 1;
      let _ = writeln!(
        out,
        "  edge exists: {} → {}  ({}; {})",
        from_view.name,
        name,
        vorpal_kg::EdgeType(row.etype).name(),
        confidence_label(row.confidence)
      );
    }
  }
  if absences.is_empty() && edges == 0 {
    return Ok(format!(
      "(no retained occurrences from {} referencing '{name}' — the reference may not exist,        or the generation predates unresolved-evidence rows)
",
      from_view.name
    ));
  }
  for row in absences {
    let verdict = match row.outcome {
      vorpal_kg::EvidenceOutcome::External => {
        "external: no definition with this name exists in the indexed tree".to_string()
      }
      _ => format!(
        "masked: {} definition{} exist{} but none is safely attributable from this site",
        row.candidates,
        if row.candidates == 1 { "" } else { "s" },
        if row.candidates == 1 { "s" } else { "" }
      ),
    };
    let _ = writeln!(
      out,
      "  no {} edge  [{verdict}]  {}:{}..{}",
      vorpal_kg::EdgeType(row.etype).name(),
      from_view.path,
      row.span_start,
      row.span_end
    );
  }
  Ok(out)
}

/// Map a grade label to the confidence floor traversal enforces (`exact` > `constrained` >
/// `heuristic`); `None`/empty = no floor (structural edges included).
pub fn min_confidence_for_grade(grade: Option<&str>) -> Result<u8, Box<dyn Error>> {
  Ok(match grade.map(str::to_ascii_lowercase).as_deref() {
    None | Some("") => 0,
    Some("heuristic") => 1,
    // 90 -> 85 with TYPE_BOUND (G-M0): the constrained floor now admits typed-receiver edges.
    Some("constrained") => 85,
    Some("exact") => 100,
    Some(other) => {
      return Err(format!("unknown grade '{other}' (exact | constrained | heuristic)").into());
    }
  })
}

/// The relation-specific, selector-consistent, grade-filtered traversal every surface shares
/// (IMPROVEMENTS 07-29 §6): resolve `target` through the same selector contract as the direct
/// graph verbs (ambiguous names list candidates), traverse only `relations` at
/// `min_confidence`+ up to `max_depth`, and render each reached node **with its path** back to
/// the seed — per-edge relation names included, so a traversal answer is auditable, not a bare
/// node set.
/// Resolve a [`GraphTarget`] to its matching node ids — the one selector implementation every
/// surface shares (rendered queries, typed [`records`], and the `eid:<hex>`-as-name wire
/// form).
pub fn resolve_target(kg: &Kg, target: &GraphTarget) -> Result<Vec<NodeId>, Box<dyn Error>> {
  let kind = match target.kind.as_deref() {
    Some(text) => Some(
      vorpal_kg::SymbolKind::parse(text).ok_or_else(|| format!("unknown symbol kind '{text}'"))?,
    ),
    None => None,
  };
  let (name, eid_from_name) = match target.name.strip_prefix("eid:") {
    Some(hex) => (
      "",
      Some(u128::from_str_radix(hex, 16).map_err(|_| format!("malformed external id '{hex}'"))?),
    ),
    None => (target.name.as_str(), None),
  };
  let selector = vorpal_kg::SymbolSelector {
    id: target.id,
    name: (!name.is_empty()).then_some(name),
    path_suffix: target.path_suffix.as_deref(),
    kind,
    external_id: target.external_id.or(eid_from_name),
  };
  Ok(kg.select(&selector))
}

/// Rendered pattern listing (the text twin of [`records::pattern_records`]): candidate
/// lines with ids/eids, capped for readability — the full set pages through records.
pub fn pattern_query_on(kg: &Kg, pattern: &str, cap: usize) -> Result<String, Box<dyn Error>> {
  let records = records::pattern_records(kg, pattern)?;
  if records.is_empty() {
    return Ok(format!("(no names match /{pattern}/)\n"));
  }
  let ids: Vec<NodeId> = records.iter().take(cap).map(|r| NodeId::new(r.id)).collect();
  let mut out = render_candidates(kg, &ids);
  if records.len() > cap {
    use std::fmt::Write;
    let _ = writeln!(out, "… {} more — tighten the pattern or page the records surface", records.len() - cap);
  }
  Ok(out)
}

/// Rendered selector-driven snippets (the text twin of [`records::snippet_records`]):
/// `path:line  name [Kind] (verification)` headers over digest-verified span bodies, with
/// the same no-match/ambiguity wording as every other selector verb.
pub fn snippet_query_on(
  kg: &Kg,
  artifacts_dir: Option<&Path>,
  target: &GraphTarget,
  context_lines: usize,
  max_bytes: usize,
) -> Result<String, records::SnippetError> {
  match records::snippet_records(kg, artifacts_dir, target, context_lines, max_bytes)? {
    records::Selected::NoMatch => Ok(format!(
      "(no results for '{}' — no symbol matches that selector)\n",
      target.name
    )),
    records::Selected::Ambiguous(candidates) => {
      let mut out = format!(
        "ambiguous: '{}' matches {} definitions — refine with --path/--kind/--id, or --all to merge:\n",
        target.name,
        candidates.len()
      );
      let ids: Vec<NodeId> = candidates.iter().map(|c| NodeId::new(c.id)).collect();
      out.push_str(&render_candidates(kg, &ids));
      Ok(out)
    }
    records::Selected::Hits(hits) => Ok(records::render_snippets(&hits)),
  }
}

/// Open the data-flow sidecar when a traversal will render `data_flows` hops. Absent or
/// unreadable sidecars degrade to no annotations — the hop itself still renders.
pub(crate) fn flow_store_for(
  dir: Option<&std::path::Path>,
  relations: &[vorpal_kg::EdgeType],
) -> Option<vorpal_kg::DataflowStore> {
  let dir = dir?;
  if !relations.iter().any(|e| e.base() == vorpal_kg::EdgeType::DATA_FLOWS) {
    return None;
  }
  vorpal_kg::DataflowStore::load(dir).ok().filter(|s| !s.is_empty())
}

/// The `expr→param#k` annotations for one traversal hop with sidecar rows on its
/// (from, to) pair. Deliberately NOT gated on the hop's winning edge type: a call edge and
/// its derived DATA_FLOWS edge share endpoints, and BFS crowns whichever the CSR lists
/// first — the rows describe the call either way. `parent`/`node` orient by `inbound`
/// (the stored edge always points from → to).
pub(crate) fn flow_exprs_for_hop(
  store: Option<&vorpal_kg::DataflowStore>,
  parent: u32,
  node: u32,
  inbound: bool,
) -> Vec<String> {
  let Some(store) = store else {
    return Vec::new();
  };
  let (from, to) = if inbound { (node, parent) } else { (parent, node) };
  store
    .flows_between(from, to)
    .iter()
    .map(|flow| {
      let expr = flow.expr.unwrap_or(match flow.class {
        2 => "(call-result)",
        _ => "(arg)",
      });
      if flow.param_index == u16::MAX {
        format!("{expr}→#?")
      } else {
        format!("{expr}→#{}", flow.param_index)
      }
    })
    .collect()
}

pub fn reachable_query_on(
  kg: &Kg,
  flows_dir: Option<&std::path::Path>,
  target: &GraphTarget,
  dir: vorpal_kg::Direction,
  relations: &[vorpal_kg::EdgeType],
  max_depth: Option<u32>,
  min_confidence: u8,
) -> Result<String, Box<dyn Error>> {
  let matches = resolve_target(kg, target)?;
  if matches.is_empty() {
    return Ok(format!(
      "(no results for '{}' — no symbol matches that selector)
",
      target.name
    ));
  }
  if matches.len() > 1 && !target.merge_all {
    let mut out = format!(
      "ambiguous: '{}' matches {} definitions — refine with --path/--kind/--id, or --all to merge:
",
      target.name,
      matches.len()
    );
    out.push_str(&render_candidates(kg, &matches));
    return Ok(out);
  }

  let flow_store = flow_store_for(flows_dir, relations);
  let mut out = String::new();
  for &seed in &matches {
    // Parent-edge map for path reconstruction; steps arrive in BFS order (deterministic).
    let steps = kg.reachable_via_paths(seed, dir, relations, max_depth, min_confidence);
    let mut parent: HashMap<u32, (u32, vorpal_kg::EdgeType, bool)> = HashMap::new();
    for step in &steps {
      parent.insert(step.node, (step.via.0, step.via.1, step.inbound));
    }
    let seed_raw = seed.raw() as u32;
    let name_of = |id: u32| {
      kg.node(NodeId::new(id as u64))
        .map(|v| v.name.to_string())
        .unwrap_or_else(|| format!("<{id}>"))
    };
    for step in &steps {
      let Some(view) = kg.node(NodeId::new(step.node as u64)) else {
        continue;
      };
      // Reconstruct the BFS path: node ← … ← seed, rendered seed-first.
      let mut chain: Vec<String> = Vec::new();
      let mut at = step.node;
      while at != seed_raw {
        let Some(&(up, edge, inbound)) = parent.get(&at) else {
          break;
        };
        // Pure In/Out keeps the historical arrow; Both labels each hop's real orientation
        // (`←rel-` = the stored edge points from this node toward its parent).
        let flow_note = {
          let exprs = flow_exprs_for_hop(flow_store.as_ref(), up, at, inbound);
          if exprs.is_empty() {
            String::new()
          } else {
            format!("[{}]", exprs.join(", "))
          }
        };
        if matches!(dir, vorpal_kg::Direction::Both) && inbound {
          chain.push(format!("←{}{}- {}", edge.name(), flow_note, name_of(at)));
        } else {
          chain.push(format!("-{}{}→ {}", edge.name(), flow_note, name_of(at)));
        }
        at = up;
      }
      chain.reverse();
      let _ = writeln!(
        out,
        "{} [{:?}] {} (id {}; depth {}; {} {})",
        view.name,
        view.kind,
        view.path,
        step.node,
        step.depth,
        name_of(seed_raw),
        chain.join(" ")
      );
    }
  }
  if out.is_empty() {
    out = format!("(nothing reachable from '{}' under those filters)
", target.name);
  }
  Ok(out)
}

pub fn graph_query_selected(
  index_dir: &Path,
  verb: &str,
  target: &GraphTarget,
) -> Result<String, Box<dyn Error>> {
  let kg = Kg::load(index_dir)?;
  graph_query_on(&kg, verb, target)
}

/// [`graph_query_selected`] over an already-loaded graph — the daemon path, which serves
/// from its warm cached [`Kg`] instead of re-opening artifacts per tool call.
pub fn graph_query_on(kg: &Kg, verb: &str, target: &GraphTarget) -> Result<String, Box<dyn Error>> {
  let matches = resolve_target(kg, target)?;
  if matches.is_empty() {
    return Ok(format!(
      "(no results for '{}' — no symbol matches that selector)\n",
      target.name
    ));
  }

  let edge = match verb {
    "callers" => Some(vorpal_kg::EdgeType::CALLS),
    "refs" | "references" => Some(vorpal_kg::EdgeType::REFERENCES),
    "importers" => Some(vorpal_kg::EdgeType::IMPORTS),
    "implementors" => Some(vorpal_kg::EdgeType::IMPLEMENTS),
    "typeusers" => Some(vorpal_kg::EdgeType::OF_TYPE),
    "similar" => Some(vorpal_kg::EdgeType::SIMILAR_TO),
    "node" => None,
    other => return Err(format!("unknown graph verb '{other}'").into()),
  };

  // `node` is a listing verb: every match IS the answer (ids attached — this verb exists to
  // discover identities for refinement).
  let Some(edge) = edge else {
    return Ok(render_candidates(kg, &matches));
  };

  if matches.len() > 1 && !target.merge_all {
    let mut out = format!(
      "ambiguous: '{}' matches {} definitions — refine with --path/--kind/--id, or --all to merge:\n",
      target.name,
      matches.len()
    );
    out.push_str(&render_candidates(kg, &matches));
    return Ok(out);
  }

  let mut hits: Vec<(NodeId, u8)> = Vec::new();
  for &target_id in &matches {
    for (from, confidence) in kg.incoming_with_confidence(target_id, edge) {
      hits.push((from, confidence));
    }
  }
  hits.sort_unstable_by_key(|&(n, c)| (n.raw(), std::cmp::Reverse(c)));
  hits.dedup_by_key(|&mut (n, _)| n);
  Ok(if hits.is_empty() {
    format!("(no results for '{}')\n", target.name)
  } else if target.show_ids {
    // Identity + evidence mode: each result carries its node id and the edge's resolution
    // confidence label — approximate edges are visibly approximate (IMPROVEMENTS §5).
    let mut out = String::new();
    for &(id, confidence) in &hits {
      if let Some(view) = kg.node(id) {
        let _ = writeln!(
          out,
          "{} [{:?}] {} (id {}; {})",
          view.name,
          view.kind,
          view.path,
          id.raw(),
          confidence_label(confidence)
        );
      }
    }
    out
  } else if edge.base() == vorpal_kg::EdgeType::SIMILAR_TO {
    // Near-clones: the edge confidence is the estimated similarity — show it.
    let mut out = String::new();
    for &(id, confidence) in &hits {
      if let Some(view) = kg.node(id) {
        let _ = writeln!(
          out,
          "{} [{:?}] {} (~{}% similar)",
          view.name, view.kind, view.path, confidence
        );
      }
    }
    out
  } else {
    let ids: Vec<NodeId> = hits.iter().map(|&(id, _)| id).collect();
    format_nodes(kg, &ids)
  })
}

/// The grade of a packed edge confidence. A confidence of `0` is a *structural* edge (containment
/// like `defines`/`has_method`, certain by construction) — not an unresolved reference, which
/// never becomes an edge; everything else carries a resolution grade from the single shared
/// vocabulary ([`vorpal_ingest::ResolutionGrade`]), so a caller can tell an exact binding from a
/// heuristic guess.
fn confidence_label(confidence: u8) -> &'static str {
  if confidence == 0 {
    return "structural";
  }
  vorpal_ingest::ResolutionGrade::from_confidence(vorpal_ingest::Confidence(confidence)).label()
}

/// Render selector candidates with their identities — enough to refine to exactly one.
fn render_candidates(kg: &Kg, ids: &[NodeId]) -> String {
  let mut out = String::new();
  for &id in ids {
    if let Some(view) = kg.node(id) {
      let signature = if view.signature.is_empty() {
        String::new()
      } else {
        format!("  {}", view.signature)
      };
      let eid = view
        .external_id
        .map(|e| format!("  eid:{e:032x}"))
        .unwrap_or_default();
      let _ = writeln!(
        out,
        "id {}  {} [{:?}] {}{}{}",
        id.raw(),
        view.name,
        view.kind,
        view.path,
        signature,
        eid
      );
    }
  }
  out
}

/// Render nodes as `name [Kind] path` lines.
pub fn format_nodes(kg: &Kg, ids: &[NodeId]) -> String {
  let mut out = String::new();
  for &id in ids {
    if let Some(view) = kg.node(id) {
      let _ = writeln!(out, "{} [{:?}] {}", view.name, view.kind, view.path);
    }
  }
  out
}

#[cfg(test)]
mod tests {
  use super::{DENSE_LIST, DenseFusion, RRF_K, admit_dense, rrf_fuse_explained, rrf_fuse_with};

  fn rrf_fuse(lists: &[Vec<u64>], k: usize) -> Vec<(u64, f32)> {
    rrf_fuse_explained(lists, k)
      .into_iter()
      .map(|(id, score, _)| (id, score))
      .collect()
  }

  #[test]
  fn rrf_fuses_and_breaks_ties_deterministically() {
    // Doc 7 is rank 0 in two lists; doc 3 is rank 0 in one and absent elsewhere.
    let ranked = rrf_fuse(&[vec![7, 3], vec![7, 9], vec![3, 7]], 10);
    assert_eq!(ranked[0].0, 7, "{ranked:?}");
    assert_eq!(ranked[1].0, 3, "{ranked:?}");
    assert_eq!(ranked[2].0, 9, "{ranked:?}");

    // A third-list (graph) placement tips otherwise-mirrored candidates.
    let ranked = rrf_fuse(&[vec![1, 2], vec![2, 1], vec![2, 1]], 10);
    assert_eq!(ranked[0].0, 2, "graph list breaks the tie: {ranked:?}");

    // Exact ties break by id.
    let ranked = rrf_fuse(&[vec![5], vec![4]], 10);
    assert_eq!(ranked[0].0, 4, "{ranked:?}");
  }

  /// The plain fusion through the mass seam is the standard formula, bit for bit, on
  /// every list — the byte-identity the dense laws promise for absent dense lists.
  #[test]
  fn mass_seam_reproduces_the_standard_fusion_bitwise() {
    let lists = [vec![7, 3, 11], vec![7, 9], vec![3, 7, 9, 11], vec![], vec![11, 2]];
    let plain = rrf_fuse_explained(&lists, 10);
    let seamed = rrf_fuse_with(&lists, 10, |_, rank| 1.0 / (RRF_K + rank as f32));
    assert_eq!(plain.len(), seamed.len());
    for (a, b) in plain.iter().zip(&seamed) {
      assert_eq!(a.0, b.0);
      assert_eq!(a.1.to_bits(), b.1.to_bits(), "{a:?} vs {b:?}");
      assert_eq!(a.2, b.2);
    }
    // Hand-checked masses: 7 sits at rank 0 in lists 0 and 1 and rank 1 in list 2.
    let seven = plain.iter().find(|hit| hit.0 == 7).expect("7 fused");
    assert_eq!(seven.1.to_bits(), (1.0 / 60.0 + 1.0 / 60.0 + 1.0 / 61.0f32).to_bits());
  }

  /// The population-scaled laws touch only the dense list's mass: `Quantile` scales
  /// its ranks by 1/fraction, `Subset` scales its K by fraction — and both are the
  /// plain fusion at complete coverage (fraction = 1).
  #[test]
  fn population_laws_weight_only_the_dense_list() {
    let lists = [vec![1], vec![2], vec![3], vec![], vec![4, 5]];
    let fraction = 0.25f32;
    let quantile = rrf_fuse_with(&lists, 10, |channel, rank| {
      if channel == DENSE_LIST { 1.0 / (RRF_K + rank as f32 / fraction) } else { 1.0 / (RRF_K + rank as f32) }
    });
    let subset = rrf_fuse_with(&lists, 10, |channel, rank| {
      if channel == DENSE_LIST { 1.0 / (RRF_K * fraction + rank as f32) } else { 1.0 / (RRF_K + rank as f32) }
    });
    let mass = |fused: &[(u64, f32, Vec<Option<usize>>)], id: u64| fused.iter().find(|h| h.0 == id).map(|h| h.1);
    // Non-dense placements are untouched under both laws.
    for id in [1u64, 2, 3] {
      assert_eq!(mass(&quantile, id), Some(1.0 / 60.0));
      assert_eq!(mass(&subset, id), Some(1.0 / 60.0));
    }
    // Quantile: dense rank 0 keeps 1/K; dense rank 1 reads as rank 4.
    assert_eq!(mass(&quantile, 4), Some(1.0 / 60.0));
    assert_eq!(mass(&quantile, 5), Some(1.0 / 64.0));
    // Subset: K_dense = 15 — the head outweighs every other list's head.
    assert_eq!(mass(&subset, 4), Some(1.0 / 15.0));
    assert_eq!(mass(&subset, 5), Some(1.0 / 16.0));
    assert_eq!(subset[0].0, 4, "{subset:?}");
    // Complete coverage collapses both to the plain fusion, bitwise.
    let plain = rrf_fuse_explained(&lists, 10);
    let complete = rrf_fuse_with(&lists, 10, |channel, rank| {
      if channel == DENSE_LIST { 1.0 / (RRF_K + rank as f32 / 1.0) } else { 1.0 / (RRF_K + rank as f32) }
    });
    for (a, b) in plain.iter().zip(&complete) {
      assert_eq!((a.0, a.1.to_bits()), (b.0, b.1.to_bits()));
    }
  }

  /// Score-gap admission keeps the rows in the upper half of the query's own score
  /// range, `[median, max]`, and never more than the pool; every other law takes the
  /// first `pool` ids exactly as the sidecar's own cut does.
  #[test]
  fn gap_admission_is_self_referenced() {
    // Cosines descending; median of the 6 = (0.70 + 0.50)/2 = 0.60; floor = (0.90 + 0.60)/2 = 0.75.
    let scored = [(10u64, 0.90f32), (11, 0.80), (12, 0.70), (13, 0.50), (14, 0.40), (15, 0.10)];
    assert_eq!(admit_dense(DenseFusion::Gap, &scored, 4), vec![10, 11]);
    assert_eq!(admit_dense(DenseFusion::Gap, &scored, 1), vec![10], "pool still caps");
    assert_eq!(admit_dense(DenseFusion::Flat, &scored, 4), vec![10, 11, 12, 13]);
    assert_eq!(admit_dense(DenseFusion::Cutoff, &scored, 10), vec![10, 11, 12, 13, 14, 15]);
    // Odd count: the middle element is the median; floor = (0.9 + 0.7)/2 = 0.8 admits two.
    assert_eq!(admit_dense(DenseFusion::Gap, &scored[..5], 10), vec![10, 11]);
    assert!(admit_dense(DenseFusion::Gap, &[], 10).is_empty());
  }
}
