//! Multi-project serving (D4): one daemon, many enrolled roots. A thin JSON-RPC shell owns
//! the protocol loop and routes each `tools/call` to the per-project [`Server`] named by the
//! request's `project` argument (default: the sole enrolled project). The MCP surface can
//! LIST projects and nothing else about the registry — enrollment is the human-typed CLI's
//! exclusive power (see `registry.rs` for the threat model).
//!
//! v1 boundary, stated rather than hidden: projects mode serves the BUILTIN grammar set.
//! Custom-language registration is a process-wide one-shot, so per-project dynamic languages
//! wait on the registration-scoping rework (the F-track non-goal list); a project needing
//! them runs its own single-project daemon today.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use crate::registry::Projects;
use crate::server::{Server, error_response, initialize, tools_list};
use crate::Profile;

pub struct MultiServer {
  servers: BTreeMap<String, Server>,
  projects: Projects,
  profile: Profile,
}

impl MultiServer {
  pub fn new(projects: Projects, profile: Profile) -> Self {
    let servers = projects
      .iter()
      .map(|(name, entry)| {
        (
          name.clone(),
          Server::with_profile(entry.index.clone(), profile),
        )
      })
      .collect();
    Self {
      servers,
      projects,
      profile,
    }
  }

  pub fn handle_line(&mut self, line: &str) -> Option<String> {
    let msg: Value = match serde_json::from_str(line) {
      Ok(v) => v,
      Err(_) => return Some(error_response(Value::Null, -32700, "parse error")),
    };
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    let id = msg.get("id").cloned()?;

    let result = match method {
      "initialize" => initialize(&params),
      "ping" => json!({}),
      "tools/list" => self.tools_list_multi(),
      "tools/call" => self.tools_call_multi(&params),
      _ => return Some(error_response(id, -32601, "method not found")),
    };
    Some(json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string())
  }

  /// The single-project tool list, with a `project` selector injected into every tool's
  /// schema, plus the registry-listing tool.
  fn tools_list_multi(&self) -> Value {
    let mut listing = tools_list(self.profile);
    let names: Vec<&str> = self.projects.keys().map(String::as_str).collect();
    if let Some(tools) = listing.get_mut("tools").and_then(Value::as_array_mut) {
      for tool in tools.iter_mut() {
        if let Some(props) = tool
          .pointer_mut("/inputSchema/properties")
          .and_then(Value::as_object_mut)
        {
          props.insert(
            "project".to_string(),
            json!({
              "type": "string",
              "description": format!(
                "Enrolled project to serve this call from (default: the sole enrolled \
                 project). Enrolled: {}.",
                names.join(", ")
              ),
            }),
          );
        }
      }
      tools.push(json!({
        "name": "list_projects",
        "description": "The projects this daemon is enrolled to serve: name, source root, \
          index root, and whether an index exists yet. Enrollment itself is human-only — it \
          happens via the `vorpal mcp allow` CLI a person types, never through this surface, \
          because a confirmation delivered through MCP would be answered by the same agent \
          that may have been influenced.",
        "inputSchema": {"type": "object", "properties": {}, "required": []}
      }));
    }
    listing
  }

  fn tools_call_multi(&mut self, params: &Value) -> Value {
    let tool = params.get("name").and_then(Value::as_str).unwrap_or("");
    if tool == "list_projects" {
      return self.list_projects_result();
    }
    // Route by the `project` argument (removed before delegation — per-project servers know
    // nothing of routing), defaulting to the sole enrolled project.
    let mut params = params.clone();
    let requested = params
      .pointer_mut("/arguments")
      .and_then(Value::as_object_mut)
      .and_then(|args| args.remove("project"))
      .and_then(|v| v.as_str().map(str::to_string));
    let name = match requested {
      Some(name) => name,
      None if self.servers.len() == 1 => match self.servers.keys().next() {
        Some(sole) => sole.clone(),
        None => return tool_error("no projects are enrolled"),
      },
      None => {
        return tool_error(&format!(
          "this daemon serves {} projects — pass \"project\" (one of: {})",
          self.servers.len(),
          self.projects.keys().cloned().collect::<Vec<_>>().join(", ")
        ));
      }
    };
    let Some(entry) = self.projects.get(&name) else {
      return tool_error(&format!(
        "no enrolled project named '{name}' (enrolled: {}) — enrollment is human-only, via \
         `vorpal mcp allow <path>` typed on the CLI",
        self.projects.keys().cloned().collect::<Vec<_>>().join(", ")
      ));
    };
    // The index tool serves ONLY the enrolled root: an explicit src is honored only when it
    // is exactly that root; anything else is the un-enrolled-source refusal.
    if tool == "index" {
      if let Some(args) = params.pointer_mut("/arguments").and_then(Value::as_object_mut) {
        match args.get("src").and_then(Value::as_str) {
          None => {
            args.insert("src".into(), json!(entry.src.to_string_lossy()));
          }
          Some(explicit) => {
            let matches = std::path::Path::new(explicit)
              .canonicalize()
              .map(|p| p == entry.src)
              .unwrap_or(false);
            if !matches {
              return tool_error(&format!(
                "'{explicit}' is not the enrolled source of project '{name}' — this surface \
                 cannot index un-enrolled roots; a person can enroll it with `vorpal mcp \
                 allow {explicit}`"
              ));
            }
          }
        }
      }
    }
    match self.servers.get_mut(&name) {
      Some(server) => server.tools_call(&params),
      None => tool_error(&format!("project '{name}' has no server state")),
    }
  }

  fn list_projects_result(&self) -> Value {
    let mut lines = Vec::new();
    let mut records = Vec::new();
    for (name, entry) in &self.projects {
      let ready = entry.index.join("CURRENT").exists() || entry.index.join("nodes.vseg").exists();
      lines.push(format!(
        "{name}  src={}  index={}  {}",
        entry.src.display(),
        entry.index.display(),
        if ready { "indexed" } else { "no index yet" }
      ));
      records.push(json!({
        "name": name,
        "src": entry.src.to_string_lossy(),
        "index": entry.index.to_string_lossy(),
        "indexed": ready,
      }));
    }
    let text = if lines.is_empty() {
      "no projects enrolled (a person can enroll one: `vorpal mcp allow <path>`)".to_string()
    } else {
      lines.join("\n")
    };
    json!({
      "content": [{"type": "text", "text": text}],
      "structuredContent": {"records": records},
      "isError": false,
    })
  }
}

fn tool_error(message: &str) -> Value {
  json!({
    "content": [{"type": "text", "text": message}],
    "structuredContent": {"code": "bad-argument"},
    "isError": true,
  })
}
