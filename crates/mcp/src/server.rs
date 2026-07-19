//! MCP protocol handling + the warm-index tool implementations.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use vorpal_index::{build_index, format_nodes};
use vorpal_kg::{Kg, NodeId};

/// Protocol revisions this server can speak; a client asking for one of these gets it echoed,
/// anything else is answered with the oldest (most widely supported) revision.
const PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];
const FALLBACK_PROTOCOL_VERSION: &str = "2024-11-05";

/// The warm-index MCP server: one persisted index directory, its graph held in memory across
/// calls (lazily cold-opened via mmap on first query, reloaded after each `index` tool call).
pub struct Server {
  index_dir: PathBuf,
  kg: Option<Kg>,
}

impl Server {
  pub fn new(index_dir: PathBuf) -> Self {
    Self {
      index_dir,
      kg: None,
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
            "indexed {} files ({} skipped) → {} nodes, {} calls resolved, {} unresolved",
            report.indexed, report.skipped, report.nodes, report.resolved, report.unresolved
          )
        })
      }
      "node" | "callers" | "references" | "importers" | "implementors" | "type_users" => {
        let name = str_arg("name")?;
        let kg = self.kg()?;
        let ids = match tool {
          "node" => kg.nodes_named(&name),
          "callers" => kg.callers_of(&name),
          "references" => kg.references_to(&name),
          "implementors" => kg.implementors_of(&name),
          "type_users" => kg.users_of_type(&name),
          _ => kg.importers_of(&name),
        };
        Ok(render(kg, &name, &ids))
      }
      "search" => {
        let query = str_arg("query")?;
        let k = args.get("k").and_then(Value::as_u64).unwrap_or(10) as usize;
        let rendered =
          vorpal_index::search_index(&self.index_dir, &query, k).map_err(|err| err.to_string())?;
        Ok(if rendered.is_empty() {
          format!("(no results for '{query}')")
        } else {
          rendered
        })
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
  let name_only = json!({"name": {"type": "string", "description": "Exact symbol name"}});
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
      "search",
      "Semantic search over definitions (lexical-embedding similarity on names, signatures, \
       and paths); returns the top-k matches with scores.",
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
