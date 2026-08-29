//! MCP protocol handling + the warm-index tool implementations.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use vorpal_index::build_index;
use vorpal_kg::Kg;

use crate::watch::SourceWatch;

/// Protocol revisions this server can speak; a client asking for one of these gets it echoed,
/// anything else is answered with the oldest (most widely supported) revision.
const PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];
const FALLBACK_PROTOCOL_VERSION: &str = "2024-11-05";

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
      "callers", "references", "importers", "implementors", "type_users", "reachable", "why",
      "health", "dead_code", "coverage", "impact", "compare_generations", "architecture",
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
  kg: Option<Kg>,
  /// The resolved generation directory the cached graph was loaded from — the artifacts a
  /// `why` snippet is digest-verified against, so a concurrent `CURRENT` swap can never split
  /// an answer from the pinned graph that produced its ids.
  kg_dir: Option<PathBuf>,
  watch: Option<SourceWatch>,
}

impl Server {
  pub fn new(index_dir: PathBuf) -> Self {
    Self::with_profile(index_dir, Profile::Full)
  }

  pub fn with_profile(index_dir: PathBuf, profile: Profile) -> Self {
    let watch = watch_root(&index_dir).and_then(|src| SourceWatch::start(&src));
    // Boot-time warm: if the persisted index exists with a stale (or absent) vector tier,
    // start building it now instead of on the first semantic search.
    if index_dir.join("nodes.vseg").exists() {
      let warm_dir = index_dir.clone();
      std::thread::spawn(move || {
        let _ = vorpal_index::warm_ann(&warm_dir);
      });
    }
    Self {
      index_dir,
      profile,
      kg: None,
      kg_dir: None,
      watch,
    }
  }

  /// Bring the in-memory graph up to date with the watched source tree. With a clean watch
  /// this is a single atomic load; with a dirty one it runs the incremental `build_index`
  /// (stat manifest + product replay — only changed files parse) and reloads. Any failure
  /// re-arms the dirty flag so the next query retries rather than serving stale data as fresh.
  fn ensure_fresh(&mut self) -> Result<(), String> {
    let Some(watch) = &self.watch else {
      return Ok(());
    };
    if self.kg.is_some() && !watch.take_dirty() {
      return Ok(());
    }
    let rebuilt = build_index(watch.src(), &self.index_dir)
      .map_err(|err| err.to_string())
      .and_then(|_| {
        let dir = vorpal_kg::resolve_index_dir(&self.index_dir);
        Kg::load(&dir)
          .map(|kg| (kg, dir))
          .map_err(|err| err.to_string())
      });
    match rebuilt {
      Ok((kg, dir)) => {
        self.kg = Some(kg);
        self.kg_dir = Some(dir);
        // Warm the vector tier in the background so the *next* semantic search never pays
        // the build. Best-effort: a failure just means the search that needs it builds it
        // (and reports its own error); the in-process build lock prevents duplicate work
        // if a search arrives mid-warm.
        let index_dir = self.index_dir.clone();
        std::thread::spawn(move || {
          let _ = vorpal_index::warm_ann(&index_dir);
        });
        Ok(())
      }
      Err(err) => {
        watch.mark_dirty();
        Err(format!("revalidating watched index failed: {err}"))
      }
    }
  }

  /// Handle one JSON-RPC message line. Requests return a response line; notifications (no `id`)
  /// and unparseable-but-ignorable input return `None` where the protocol says to stay silent.
  pub fn handle_line(&mut self, line: &str) -> Option<String> {
    let msg: Value = match serde_json::from_str(line) {
      Ok(v) => v,
      Err(_) => return Some(error_response(Value::Null, -32700, "parse error")),
    };
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    // No id → a notification (e.g. notifications/initialized): never answered.
    let id = msg.get("id").cloned()?;

    let result = match method {
      "initialize" => initialize(&params),
      "ping" => json!({}),
      "tools/list" => tools_list(self.profile),
      "tools/call" => self.tools_call(&params),
      _ => return Some(error_response(id, -32601, "method not found")),
    };
    Some(json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string())
  }

  /// `tools/call`: run a tool, wrapping success and failure as MCP tool results (`isError`
  /// carries tool-level failures in-band, per spec; JSON-RPC errors are protocol-level only).
  ///
  /// Every result carries `structuredContent` (IMPROVEMENTS #7): successes state the pinned
  /// **generation** content id the answer came from (`null` before any graph is loaded, e.g.
  /// pure-parse tools like `ast_dump`), so ids and spans are attributable to exactly one
  /// index state; failures state a **stable machine-readable code** alongside the message.
  fn tools_call(&mut self, params: &Value) -> Value {
    let tool = params.get("name").and_then(Value::as_str).unwrap_or("");
    let args = params
      .get("arguments")
      .cloned()
      .unwrap_or_else(|| json!({}));
    match self.run_tool(tool, &args) {
      Ok((text, mut data)) => {
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
        "bad-argument",
        format!("tool '{tool}' is not in this daemon's '{}' profile", self.profile.label()),
      ));
    }
    // Query tools serve from a graph the watch keeps fresh; the explicit `index` tool builds
    // from its own `src` argument and needs no pre-validation.
    if tool != "index" {
      self
        .ensure_fresh()
        .map_err(|message| ToolError::coded("index-unavailable", message))?;
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
        let report =
          vorpal_index::build_index_full(Path::new(&src), &self.index_dir, mode, policy)
            .map_err(|err| err.to_string())?;
        // Reload so queries serve the fresh graph (a cheap mmap cold-open), pinning the
        // new generation directory alongside it.
        let dir = vorpal_kg::resolve_index_dir(&self.index_dir);
        self.kg = Some(Kg::load(&dir).map_err(|err| err.to_string())?);
        self.kg_dir = Some(dir);
        let text = if report.reused {
          format!("unchanged — reused existing index ({} nodes)", report.nodes)
        } else {
          format!(
            "indexed {} files ({} skipped) → {} nodes; refs: {} resolved, {} ambiguous, {} external, {} masked",
            report.indexed,
            report.skipped,
            report.nodes,
            report.resolved,
            report.ambiguous,
            report.external,
            report.masked
          )
        };
        Ok((text, json!({})))
      }
      "architecture" => {
        let top = args.get("top").and_then(Value::as_u64).unwrap_or(20).clamp(1, 500) as usize;
        self.kg()?;
        let Some(kg) = self.kg.as_ref() else {
          return Err(ToolError::coded("index-unavailable", "no graph is loaded — run the 'index' tool first"));
        };
        let report = vorpal_index::records::architecture_report(kg, top);
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
        let Some(kg) = self.kg.as_ref() else {
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
      "node" | "callers" | "references" | "importers" | "implementors" | "type_users" => {
        // Pattern listing (node only): regex over names, matches ARE the answer.
        if tool == "node" {
          if let Some(pattern) = args.get("pattern").and_then(Value::as_str) {
            self.kg()?;
            let Some(kg) = self.kg.as_ref() else {
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
        let kg = self.kg()?;
        let data = if tool == "node" {
          let records =
            vorpal_index::records::listing_records(kg, &target).map_err(ToolError::from)?;
          paged(records, args, "hits")?
        } else {
          let selected =
            vorpal_index::records::related_records(kg, verb, &target).map_err(ToolError::from)?;
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
        // contributed").
        let records = vorpal_index::search_records_filtered(&self.index_dir, &query, k, &filter)
          .map_err(|err| err.to_string())?;
        let mut text = String::new();
        for hit in &records {
          let mut provenance = format!("id {}", hit.node.id);
          for channel in &hit.channels {
            provenance.push_str(&format!("; {}#{}", channel.channel, channel.rank));
          }
          text.push_str(&format!(
            "{:.4}  {} [{}] {}  ({provenance})\n",
            hit.score, hit.node.name, hit.node.kind, hit.node.path
          ));
        }
        if text.is_empty() {
          text = format!("(no results for '{query}')");
        }
        let data = paged(records, args, "hits")?;
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
            "structural_search needs a watched source tree (daemon started on a default \
             <src>/.vorpal/index location)"
              .to_string()
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
        let Some(kg) = self.kg.as_ref() else {
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
            "rule_search needs a watched source tree (daemon started on a default \
             <src>/.vorpal/index location)"
              .to_string()
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
              None => <vorpal_language::SupportLang as vorpal_core::Language>::from_path(
                std::path::Path::new(path),
              )
              .map(|l: vorpal_language::SupportLang| l.to_string())
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
        let Some(kg) = self.kg.as_ref() else {
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
        let Some(kg) = self.kg.as_ref() else {
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
        let Some(kg) = self.kg.as_ref() else {
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
        let Some(kg) = self.kg.as_ref() else {
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
        let kg = self.kg()?;
        // Page-materialized: the BFS runs whole (that IS the deterministic vector), but
        // record construction is paid per page — an undirected kernel walk reaches 200K+
        // nodes and building all their records to serve one page dominated this tool.
        let selected = vorpal_index::records::reach_records_page(
          kg,
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
        let text = vorpal_index::reachable_query_on(kg, &target, dir, &relations, max_depth, min_confidence)
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
      other => Err(ToolError::coded("bad-argument", format!("unknown tool '{other}'"))),
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
      self.kg = Some(loaded);
      self.kg_dir = Some(dir);
    }
    self
      .kg
      .as_ref()
      .ok_or_else(|| "index load raced away — retry the query".to_string())
  }
}

fn initialize(params: &Value) -> Value {
  let requested = params
    .get("protocolVersion")
    .and_then(Value::as_str)
    .unwrap_or("");
  let version = if PROTOCOL_VERSIONS.contains(&requested) {
    requested
  } else {
    FALLBACK_PROTOCOL_VERSION
  };
  json!({
    "protocolVersion": version,
    "capabilities": {"tools": {}},
    "serverInfo": {"name": "vorpal-mcp", "version": env!("CARGO_PKG_VERSION")}
  })
}

fn tools_list(profile: Profile) -> Value {
  let name_only = json!({
    "name": {"type": "string", "description": "Exact symbol name"},
    "path": {"type": "string", "description": "Refine: definition file path must end with this suffix"},
    "kind": {"type": "string", "description": "Refine: symbol kind (function, method, struct, field, …)"},
    "id": {"type": "integer", "description": "Query exactly this node id (from `node` output or an ambiguity listing)"},
    "eid": {"type": "string", "description": "Durable external id (32 hex chars from `node` output) — survives rebuilds; also accepted as a `name` of the form eid:<hex>"},
    "all": {"type": "boolean", "description": "Merge results across ALL same-named definitions instead of listing candidates"},
    "cursor": {"type": "string", "description": "Opaque page cursor from a previous result's nextCursor (structuredContent records only)"},
    "limit": {"type": "integer", "description": "Records per page in structuredContent (default 100, max 1000)"}
  });
  let tools: Vec<Value> = vec![
    tool(
      "index",
      "Build or refresh the knowledge-graph index from a source directory (near-instant when \
       the tree is unchanged), then hold it warm for queries.",
      json!({
        "src": {"type": "string", "description": "Source directory to index"},
        "verify": {"type": "boolean", "description": "Content-authoritative cache validation: verify every replay against current file bytes (default fast-stat trusts size+mtime outside the racy window)"},
        "parse_health": {"type": "string", "enum": ["warn", "exclude", "fail"], "description": "Policy for files whose parse produced ERROR nodes: warn reports (default), exclude drops them from the graph, fail aborts before committing"},
        "max_error_ratio": {"type": "number", "description": "Unhealthy threshold: error bytes / file size above this ratio (default 0.0 = any error byte)"}
      }),
      &["src"],
    ),
    tool(
      "health",
      "Per-file parse damage in the pinned generation: ERROR-node counts, covered-byte \
       ratios, representative error spans, language + extraction-identity context, and the \
       graph entities whose definitions overlap damaged regions — the difference between \
       'no edge' and 'unknowable here'.",
      json!({}),
      &[],
    ),
    tool(
      "schema",
      "What this graph contains, by vocabulary: node kinds, edge relations, and resolution \
       grades — each with counts — plus generation id and warm-tier state. Call this before \
       forming queries; it is the authority on which kind/relation/grade names exist.",
      json!({}),
      &[],
    ),
    tool(
      "coverage",
      "Per-file parse-coverage overview from the product bank: error nodes/bytes and damage \
       ratio per file, worst first (clean files counted, not listed). The cheap first stop \
       before trusting absence anywhere; `health` has span/entity detail. No bank → says \
       coverage is UNAVAILABLE, never that parses were clean.",
      json!({
        "cursor": {"type": "string", "description": "Opaque page cursor from a previous result's nextCursor (structuredContent records only)"},
        "limit": {"type": "integer", "description": "Records per page in structuredContent (default 100, max 1000)"}
      }),
      &[],
    ),
    tool(
      "architecture",
      "Orientation summary: modules by definition mass with cross-module import margins, \
       hub definitions by semantic in-degree, and entry-point candidates (exported, \
       semantically unreached). The first call to make in an unfamiliar codebase.",
      json!({
        "top": {"type": "integer", "description": "Rows per section (default 20, max 500)"}
      }),
      &[],
    ),
    tool(
      "compare_generations",
      "What changed between two retained generations of this index: files added/removed/\
       changed (unchanged files skip by digest), node-level adds/removes/modifications \
       aligned by durable eid, and per-relation edge-count deltas. A signature change on an \
       overloadable definition is an identity transition (removed + added) by the eid \
       contract; body-only edits alter no semantic content and diff as unchanged.",
      json!({
        "from": {"type": "string", "description": "Older generation: content id, path, or 'prev' (default)"},
        "to": {"type": "string", "description": "Newer generation: content id, path, or 'CURRENT' (default)"},
        "cursor": {"type": "string", "description": "Opaque page cursor from a previous result's nextCursor (structuredContent records only)"},
        "limit": {"type": "integer", "description": "Records per page in structuredContent (default 100, max 1000)"}
      }),
      &[],
    ),
    tool(
      "impact",
      "Blast radius of changed files: git-diff-seeded (merge-base vs `since`, or the \
       uncommitted worktree) transitive INBOUND closure over the chosen relations — every \
       impacted node with its minimum hop distance. Changed files missing from the index \
       are counted in missingFiles, never silently dropped.",
      json!({
        "since": {"type": "string", "description": "Git ref to diff against (merge-base semantics). Absent = uncommitted changes only"},
        "relations": {"type": "array", "items": {"type": "string"}, "description": "Edge types to follow (default [calls])"},
        "max_depth": {"type": "integer", "description": "Hop bound (0/absent = unbounded)"},
        "min_grade": {"type": "string", "description": "Only traverse edges at this grade or better (exact|constrained|heuristic)"},
        "cursor": {"type": "string", "description": "Opaque page cursor from a previous result's nextCursor (structuredContent records only)"},
        "limit": {"type": "integer", "description": "Records per page in structuredContent (default 100, max 1000)"}
      }),
      &[],
    ),
    tool(
      "dead_code",
      "Definitions with no semantic in-edges anywhere in the graph (calls/references/\
       imports/implements/of_type/overrides — containment doesn't count), with honest \
       suppression: candidates whose name appears in ANY evidence occurrence (fn-pointer \
       tables, dynamic dispatch, namesake ties) and candidates in parse-damaged files are \
       counted out, not listed. Leads, not verdicts: absence of an edge is not proof of \
       death — check `coverage`/`health` before deleting anything.",
      json!({
        "kind": {"type": "string", "description": "One symbol kind (default set: function, method, class, struct, enum, interface, constructor)"},
        "prefix": {"type": "string", "description": "Filter: definition file path starts with this prefix"},
        "path": {"type": "string", "description": "Filter: definition file path ends with this suffix"},
        "exported": {"type": "boolean", "description": "Only exported definitions"},
        "exclude_tests": {"type": "boolean", "description": "Exclude test-classified paths (tests/, __tests__/, *_test.*, …)"},
        "cursor": {"type": "string", "description": "Opaque page cursor from a previous result's nextCursor (structuredContent records only)"},
        "limit": {"type": "integer", "description": "Records per page in structuredContent (default 100, max 1000)"}
      }),
      &[],
    ),
    tool(
      "node",
      "Nodes matching an exact symbol name — or, with `pattern`, every node whose name \
       matches a regex (a listing; refine from its ids).",
      {
        let mut props = name_only.clone();
        props["pattern"] =
          json!({"type": "string", "description": "Regex over names (replaces `name`)"});
        props
      },
      &[],
    ),
    tool("callers", "Direct callers of a symbol (incoming `calls` edges).", name_only.clone(), &["name"]),
    tool("references", "Direct referrers of a symbol (incoming `references` edges).", name_only.clone(), &["name"]),
    tool("importers", "Files importing a symbol (incoming `imports` edges).", name_only.clone(), &["name"]),
    tool("implementors", "Types implementing/extending a trait, interface, or base type (incoming `implements` edges).", name_only.clone(), &["name"]),
    tool("type_users", "Definitions using a type in fields, params, returns, or annotations (incoming `of_type` edges).", name_only.clone(), &["name"]),
    tool(
      "reachable",
      "Relation-specific transitive traversal from a symbol, returning each reached node WITH \
       its path back to the seed (per-edge relation names). direction \"in\" = everything \
       reaching it, \"out\" = everything it reaches; `relations` restricts edge types (default \
       [\"calls\"]); `min_grade` sets a resolution-grade floor; the seed uses the same selector \
       contract as the direct graph tools (ambiguous names list candidates).",
      json!({
        "name": {"type": "string", "description": "Exact symbol name"},
        "direction": {"type": "string", "enum": ["in", "out", "both"]},
        "relations": {"type": "array", "items": {"type": "string"},
          "description": "Edge types to follow: calls, references, imports, implements, of_type, defines, has_method, has_field, overrides (default [\"calls\"])"},
        "max_depth": {"type": "integer", "description": "Maximum hops (0 or absent = unbounded)"},
        "min_grade": {"type": "string", "enum": ["exact", "constrained", "heuristic"],
          "description": "Only traverse edges at this resolution grade or better (absent = include structural edges too)"},
        "path": {"type": "string", "description": "Refine: seed's file path must end with this suffix"},
        "kind": {"type": "string", "description": "Refine: seed's symbol kind"},
        "id": {"type": "integer", "description": "Seed exactly this node id"},
        "eid": {"type": "string", "description": "Seed by durable external id (32 hex chars)"},
        "all": {"type": "boolean", "description": "Merge across ALL same-named seeds instead of listing candidates"},
        "cursor": {"type": "string", "description": "Opaque page cursor from a previous result's nextCursor (structuredContent records only)"},
        "limit": {"type": "integer", "description": "Records per page in structuredContent (default 100, max 1000)"}
      }),
      &["name", "direction"],
    ),
    tool(
      "structural_search",
      "ast-grep-style structural pattern search over the watched source tree: real code with \
       metavariables ($X, $$$ARGS), matched on the AST — returns path:line + matched text.",
      json!({
        "pattern": {"type": "string", "description": "Structural pattern (e.g. 'foo($A, $B)')"},
        "lang": {"type": "string", "description": "Language of the pattern (rust, c, python, …)"},
        "path": {"type": "string", "description": "Only search files whose path ends with this suffix"},
        "limit": {"type": "integer", "description": "Max matches (default 100, cap 1000)"}
      }),
      &["pattern", "lang"],
    ),
    tool(
      "rule_search",
      "Run full YAML rule(s) over the watched source tree — the complete rule model \
       (composite/relational rules, constraints, utils, transform), not just a bare pattern. \
       Rules with `fix` render each match's dry-run replacement; nothing on disk changes. \
       Separate multiple rule documents with `---`.",
      json!({
        "rule": {"type": "string", "description": "YAML rule document(s): id, language, rule, and optionally constraints/utils/transform/fix"},
        "path": {"type": "string", "description": "Only search files whose path ends with this suffix"},
        "limit": {"type": "integer", "description": "Max matches (default 100, cap 1000)"}
      }),
      &["rule"],
    ),
    tool(
      "ast_dump",
      "Parse source and print the named-node tree (kind, byte span, leaf text) — the ground \
       truth for choosing pattern/kind targets when authoring rules. Pass inline source+lang, \
       or a file path (language inferred from the extension).",
      json!({
        "source": {"type": "string", "description": "Inline source text (requires lang)"},
        "lang": {"type": "string", "description": "Language of the source (rust, c, python, …)"},
        "path": {"type": "string", "description": "File to parse instead of inline source"},
        "max_nodes": {"type": "integer", "description": "Cap printed nodes (default 500, max 5000)"}
      }),
      &[],
    ),
    tool(
      "fetch_span",
      "The defining source of a graph node, verbatim: pass a node id (from any graph tool's \
       output or an ambiguity listing) and get back path:line plus the definition's bytes, \
       digest-verified against the pinned generation (stale files refuse rather than return \
       inconsistent bytes).",
      json!({
        "id": {"type": "integer", "description": "Node id"},
        "max_bytes": {"type": "integer", "description": "Clamp returned source (default 16384)"}
      }),
      &["id"],
    ),
    tool(
      "snippet",
      "The defining source of a symbol by NAME (or id/eid): the selector-driven sibling of \
       fetch_span — same digest verification, plus whole-line context expansion. Ambiguous \
       names return the candidate list to refine (path/kind/id), never a guessed snippet. \
       Absence of a match is not proof the symbol doesn't exist: check `coverage` for parse \
       damage in its file.",
      json!({
        "name": {"type": "string", "description": "Exact symbol name (or eid:<hex>)"},
        "path": {"type": "string", "description": "Refine: definition file path ends with this suffix"},
        "kind": {"type": "string", "description": "Refine: symbol kind (function, method, struct, …)"},
        "id": {"type": "integer", "description": "Refine: exactly this node id"},
        "eid": {"type": "string", "description": "Refine: durable external id (32 hex chars)"},
        "all": {"type": "boolean", "description": "Return a snippet for every same-named match instead of an ambiguity listing"},
        "context_lines": {"type": "integer", "description": "Whole context lines around the span (default 0)"},
        "max_bytes": {"type": "integer", "description": "Byte cap per snippet body (default 16384, clamp 64..262144)"},
        "cursor": {"type": "string", "description": "Opaque page cursor from a previous result's nextCursor (structuredContent records only)"},
        "limit": {"type": "integer", "description": "Records per page in structuredContent (default 100, max 1000)"}
      }),
      &["name"],
    ),
    tool(
      "why",
      "Evidence for the edge(s) from one node to another: each retained occurrence's edge \
       type, resolution grade, resolver reason, candidate count, and source span — why does \
       this relation exist?",
      json!({
        "from_id": {"type": "integer", "description": "Source node id (from any graph tool's id output)"},
        "to_id": {"type": "integer", "description": "Target node id (edge form: why does this edge exist?)"},
        "name": {"type": "string", "description": "Referenced name (absence form: why is there NO edge to anything with this name?)"},
        "cursor": {"type": "string", "description": "Opaque page cursor from a previous result's nextCursor (structuredContent records only)"},
        "limit": {"type": "integer", "description": "Records per page in structuredContent (default 100, max 1000)"}
      }),
      &["from_id"],
    ),
    tool(
      "search",
      "Hybrid search over definitions: exact/token name matches, lexical-embedding similarity, \
       and graph in-degree fused by reciprocal rank fusion; returns the top-k matches with \
       scores.",
      json!({
        "query": {"type": "string", "description": "Free-text query"},
        "k": {"type": "integer", "description": "Max results (default 10)"},
        "path": {"type": "string", "description": "Filter: definition file path must end with this suffix"},
        "prefix": {"type": "string", "description": "Filter: definition file path must start with this prefix (package/subtree scoping)"},
        "kind": {"type": "string", "description": "Filter: symbol kind (function, method, struct, …)"},
        "lang": {"type": "string", "description": "Filter: language name or alias (rust, py, ts, …)"},
        "exported": {"type": "boolean", "description": "Filter: only exported definitions"},
        "exclude_tests": {"type": "boolean", "description": "Filter: exclude test-classified paths"},
        "cursor": {"type": "string", "description": "Opaque page cursor from a previous result's nextCursor (structuredContent records only)"},
        "limit": {"type": "integer", "description": "Records per page in structuredContent (default 100, max 1000)"}
      }),
      &["query"],
    ),
  ];
  // Advertise exactly what run_tool will accept: one membership authority.
  let tools: Vec<Value> = tools
    .into_iter()
    .filter(|t| profile.allows(t["name"].as_str().unwrap_or("")))
    .collect();
  json!({"tools": tools})
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
/// the MCP contract: `bad-argument` (caller passed something unusable), `index-unavailable`
/// (no graph to answer from — build/revalidate failed), `no-watch` (a source-tree tool on a
/// custom index location), `stale-source` (file changed since the pinned generation indexed
/// it), and `tool-error` (everything else, message-only).
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


fn error_response(id: Value, code: i64, message: &str) -> String {
  json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}).to_string()
}
