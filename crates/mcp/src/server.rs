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
      .and_then(|_| Kg::load(&self.index_dir).map_err(|err| err.to_string()));
    match rebuilt {
      Ok(kg) => {
        self.kg = Some(kg);
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
        let report =
          build_index(Path::new(&src), &self.index_dir).map_err(|err| err.to_string())?;
        // Reload so queries serve the fresh graph (a cheap mmap cold-open).
        self.kg = Some(Kg::load(&self.index_dir).map_err(|err| err.to_string())?);
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
      "fetch_span" => {
        let id = args
          .get("id")
          .and_then(Value::as_u64)
          .ok_or_else(|| "missing required argument: id".to_string())?;
        let max_bytes = args
          .get("max_bytes")
          .and_then(Value::as_u64)
          .unwrap_or(16_384) as usize;
        let kg = self.kg()?;
        crate::tools::fetch_span(kg, id, max_bytes.clamp(64, 262_144))
      }
      "reachable" => {
        let name = str_arg("name")?;
        let direction = str_arg("direction")?;
        if direction != "in" && direction != "out" {
          return Err(format!(
            "direction must be \"in\" or \"out\", got '{direction}'"
          ));
        }
        let kg = self.kg()?;
        let mut ids: Vec<NodeId> = Vec::new();
        for seed in kg.nodes_named(&name) {
          let set = if direction == "in" {
            kg.reachable_in(seed)
          } else {
            kg.reachable_out(seed)
          };
          for id in set {
            if !ids.contains(&id) {
              ids.push(id);
            }
          }
        }
        Ok(render(kg, &name, &ids))
      }
      other => Err(format!("unknown tool '{other}'")),
    }
  }

  /// The warm graph: lazily cold-open the persisted index on first query, then reuse.
  fn kg(&mut self) -> Result<&Kg, String> {
    if self.kg.is_none() {
      let loaded = Kg::load(&self.index_dir).map_err(|err| {
        format!(
          "no index loaded from {} — call the 'index' tool first ({err})",
          self.index_dir.display()
        )
      })?;
      self.kg = Some(loaded);
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
    "all": {"type": "boolean", "description": "Merge results across ALL same-named definitions instead of listing candidates"}
  });
  json!({"tools": [
    tool(
      "index",
      "Build or refresh the knowledge-graph index from a source directory (near-instant when \
       the tree is unchanged), then hold it warm for queries.",
      json!({"src": {"type": "string", "description": "Source directory to index"}}),
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
      "Transitive closure from a symbol: direction \"in\" = everything reaching it \
       (transitive callers/containers), \"out\" = everything it reaches.",
      json!({
        "name": {"type": "string", "description": "Exact symbol name"},
        "direction": {"type": "string", "enum": ["in", "out"]}
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
      "fetch_span",
      "The defining source of a graph node, verbatim: pass a node id (from any graph tool's \
       output or an ambiguity listing) and get back path:line plus the definition's bytes.",
      json!({
        "id": {"type": "integer", "description": "Node id"},
        "max_bytes": {"type": "integer", "description": "Clamp returned source (default 16384)"}
      }),
      &["id"],
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
