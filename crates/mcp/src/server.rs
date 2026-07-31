//! MCP protocol handling + the warm-index tool implementations.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use vorpal_index::{build_index, format_nodes};
use vorpal_kg::{Kg, NodeId};

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
pub struct Server {
  index_dir: PathBuf,
  kg: Option<Kg>,
  /// The resolved generation directory the cached graph was loaded from — the artifacts a
  /// `why` snippet is digest-verified against, so a concurrent `CURRENT` swap can never split
  /// an answer from the pinned graph that produced its ids.
  kg_dir: Option<PathBuf>,
  watch: Option<SourceWatch>,
}

impl Server {
  pub fn new(index_dir: PathBuf) -> Self {
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
      "tools/list" => tools_list(),
      "tools/call" => self.tools_call(&params),
      _ => return Some(error_response(id, -32601, "method not found")),
    };
    Some(json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string())
  }

  /// `tools/call`: run a tool, wrapping success and failure as MCP tool results (`isError`
  /// carries tool-level failures in-band, per spec; JSON-RPC errors are protocol-level only).
  fn tools_call(&mut self, params: &Value) -> Value {
    let tool = params.get("name").and_then(Value::as_str).unwrap_or("");
    let args = params
      .get("arguments")
      .cloned()
      .unwrap_or_else(|| json!({}));
    match self.run_tool(tool, &args) {
      Ok(text) => json!({"content": [{"type": "text", "text": text}], "isError": false}),
      Err(text) => json!({"content": [{"type": "text", "text": text}], "isError": true}),
    }
  }

  fn run_tool(&mut self, tool: &str, args: &Value) -> Result<String, String> {
    let str_arg = |key: &str| {
      args
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| format!("missing required argument '{key}'"))
    };
    // Query tools serve from a graph the watch keeps fresh; the explicit `index` tool builds
    // from its own `src` argument and needs no pre-validation.
    if tool != "index" {
      self.ensure_fresh()?;
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
        let report = vorpal_index::build_index_with(Path::new(&src), &self.index_dir, mode)
          .map_err(|err| err.to_string())?;
        // Reload so queries serve the fresh graph (a cheap mmap cold-open), pinning the
        // new generation directory alongside it.
        let dir = vorpal_kg::resolve_index_dir(&self.index_dir);
        self.kg = Some(Kg::load(&dir).map_err(|err| err.to_string())?);
        self.kg_dir = Some(dir);
        Ok(if report.reused {
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
        })
      }
      "node" | "callers" | "references" | "importers" | "implementors" | "type_users" => {
        let name = str_arg("name")?;
        // Symbol identity contract (IMPROVEMENTS §1): ambiguous names return the candidate
        // list (with node ids) instead of silently merging namesake neighborhoods; refine
        // with `path`/`kind`/`id`, or pass `all: true` to merge explicitly.
        let target = vorpal_index::GraphTarget {
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
        };
        let verb = match tool {
          "type_users" => "typeusers",
          "references" => "refs",
          other => other,
        };
        // `self.kg()` keeps the daemon contract: freshness revalidation, the warm cached
        // graph, and the "run the 'index' tool first" error when nothing is indexed yet.
        let kg = self.kg()?;
        vorpal_index::graph_query_on(kg, verb, &target).map_err(|err| err.to_string())
      }
      "search" => {
        let query = str_arg("query")?;
        let k = args.get("k").and_then(Value::as_u64).unwrap_or(10) as usize;
        // Agents get ranking provenance by default: which channels (name/vector/graph)
        // placed each hit, at which rank — §11's "expose which rankers contributed."
        let rendered = vorpal_index::search_index_explained(&self.index_dir, &query, k)
          .map_err(|err| err.to_string())?;
        Ok(if rendered.is_empty() {
          format!("(no results for '{query}')")
        } else {
          rendered
        })
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
      }
      "ast_dump" => {
        let lang;
        let source;
        match (args.get("source").and_then(Value::as_str), args.get("path").and_then(Value::as_str)) {
          (Some(inline), _) => {
            source = inline.to_string();
            lang = str_arg("lang")?;
          }
          (None, Some(path)) => {
            source =
              std::fs::read_to_string(path).map_err(|err| format!("read {path}: {err}"))?;
            lang = match args.get("lang").and_then(Value::as_str) {
              Some(lang) => lang.to_string(),
              None => <vorpal_language::SupportLang as vorpal_core::Language>::from_path(
                std::path::Path::new(path),
              )
              .map(|l: vorpal_language::SupportLang| l.to_string())
              .ok_or_else(|| format!("cannot infer language from {path}; pass lang"))?,
            };
          }
          (None, None) => return Err("pass source+lang, or path".to_string()),
        }
        let max_nodes = args
          .get("max_nodes")
          .and_then(Value::as_u64)
          .unwrap_or(500) as usize;
        crate::tools::ast_dump(&source, &lang, max_nodes.clamp(10, 5000))
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
        let kg = self.kg.as_ref().expect("pinned above");
        crate::tools::fetch_span(kg, dir.as_deref(), id, max_bytes.clamp(64, 262_144))
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
        let kg = self.kg.as_ref().expect("pinned above");
        match (to_id, name) {
          (Some(to_id), _) => vorpal_index::explain_edge_on(kg, dir.as_deref(), from_id, to_id)
            .map_err(|err| err.to_string()),
          (None, Some(name)) => vorpal_index::explain_absence_on(kg, from_id, &name)
            .map_err(|err| err.to_string()),
          (None, None) => Err("pass to_id (edge evidence) or name (absence evidence)".into()),
        }
      }
      "reachable" => {
        let name = str_arg("name")?;
        let direction = str_arg("direction")?;
        let dir = match direction.as_str() {
          "in" => vorpal_kg::Direction::In,
          "out" => vorpal_kg::Direction::Out,
          other => {
            return Err(format!("direction must be \"in\" or \"out\", got '{other}'"));
          }
        };
        // Selector-consistent (07-29 §6): same refinement contract as the direct graph tools —
        // ambiguous names return candidates; id/eid/path/kind refine; `all` merges explicitly.
        let target = vorpal_index::GraphTarget {
          name,
          id: args.get("id").and_then(Value::as_u64),
          external_id: args
            .get("eid")
            .and_then(Value::as_str)
            .and_then(|hex| u128::from_str_radix(hex, 16).ok()),
          path_suffix: args.get("path").and_then(Value::as_str).map(str::to_string),
          kind: args.get("kind").and_then(Value::as_str).map(str::to_string),
          merge_all: args.get("all").and_then(Value::as_bool).unwrap_or(false),
          show_ids: true,
        };
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
          Some(_) => return Err("relations must be an array of relation names".to_string()),
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
        vorpal_index::reachable_query_on(kg, &target, dir, &relations, max_depth, min_confidence)
          .map_err(|err| err.to_string())
      }
      other => Err(format!("unknown tool '{other}'")),
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
    Ok(self.kg.as_ref().expect("just loaded"))
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

fn tools_list() -> Value {
  let name_only = json!({
    "name": {"type": "string", "description": "Exact symbol name"},
    "path": {"type": "string", "description": "Refine: definition file path must end with this suffix"},
    "kind": {"type": "string", "description": "Refine: symbol kind (function, method, struct, field, …)"},
    "id": {"type": "integer", "description": "Query exactly this node id (from `node` output or an ambiguity listing)"},
    "eid": {"type": "string", "description": "Durable external id (32 hex chars from `node` output) — survives rebuilds; also accepted as a `name` of the form eid:<hex>"},
    "all": {"type": "boolean", "description": "Merge results across ALL same-named definitions instead of listing candidates"}
  });
  json!({"tools": [
    tool(
      "index",
      "Build or refresh the knowledge-graph index from a source directory (near-instant when \
       the tree is unchanged), then hold it warm for queries.",
      json!({
        "src": {"type": "string", "description": "Source directory to index"},
        "verify": {"type": "boolean", "description": "Content-authoritative cache validation: verify every replay against current file bytes (default fast-stat trusts size+mtime outside the racy window)"}
      }),
      &["src"],
    ),
    tool("node", "Nodes matching an exact symbol name.", name_only.clone(), &["name"]),
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
        "direction": {"type": "string", "enum": ["in", "out"]},
        "relations": {"type": "array", "items": {"type": "string"},
          "description": "Edge types to follow: calls, references, imports, implements, of_type, defines, has_method, has_field, overrides (default [\"calls\"])"},
        "max_depth": {"type": "integer", "description": "Maximum hops (0 or absent = unbounded)"},
        "min_grade": {"type": "string", "enum": ["exact", "constrained", "heuristic"],
          "description": "Only traverse edges at this resolution grade or better (absent = include structural edges too)"},
        "path": {"type": "string", "description": "Refine: seed's file path must end with this suffix"},
        "kind": {"type": "string", "description": "Refine: seed's symbol kind"},
        "id": {"type": "integer", "description": "Seed exactly this node id"},
        "eid": {"type": "string", "description": "Seed by durable external id (32 hex chars)"},
        "all": {"type": "boolean", "description": "Merge across ALL same-named seeds instead of listing candidates"}
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
      "why",
      "Evidence for the edge(s) from one node to another: each retained occurrence's edge \
       type, resolution grade, resolver reason, candidate count, and source span — why does \
       this relation exist?",
      json!({
        "from_id": {"type": "integer", "description": "Source node id (from any graph tool's id output)"},
        "to_id": {"type": "integer", "description": "Target node id (edge form: why does this edge exist?)"},
        "name": {"type": "string", "description": "Referenced name (absence form: why is there NO edge to anything with this name?)"}
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
        "k": {"type": "integer", "description": "Max results (default 10)"}
      }),
      &["query"],
    ),
  ]})
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

fn render(kg: &Kg, name: &str, ids: &[NodeId]) -> String {
  if ids.is_empty() {
    format!("(no results for '{name}')")
  } else {
    format_nodes(kg, ids)
  }
}

fn error_response(id: Value, code: i64, message: &str) -> String {
  json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}).to_string()
}
