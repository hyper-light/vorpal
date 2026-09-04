//! Multi-project serving (D4): one daemon, many enrolled roots. A thin JSON-RPC shell owns
//! the protocol loop and routes each `tools/call` to the per-project [`Server`] named by the
//! request's `project` argument (default: the sole enrolled project). The MCP surface can
//! LIST projects and nothing else about the registry — enrollment is the human-typed CLI's
//! exclusive power (see `registry.rs` for the threat model).
//!
//! Custom/dynamic languages in projects mode (v2): the LAUNCHER union-registers every
//! enrolled project's custom languages at startup (one one-shot dlopen set, name/extension
//! collisions refused loudly, builtin-extension shadowing refused — see the CLI's union
//! builder), and hands each project its own extraction environment, so rebuilds run under
//! that project's rules/specs/canaries. The grammar UNIVERSE is process-wide by necessity;
//! per-project behavior lives in the envs. Projects declaring languageGlobs are refused at
//! launch (globs rebind builtin routing process-wide — consent cannot be per-project).

use std::collections::BTreeMap;

use serde_json::{Value, json};

use crate::Profile;
use crate::protocol::{Handler, RpcError, decorate_tools};
use crate::registry::Projects;
use crate::server::{Server, tool_declarations};

pub struct MultiServer {
  servers: BTreeMap<String, Server>,
  projects: Projects,
  profile: Profile,
}

impl MultiServer {
  pub fn new(projects: Projects, profile: Profile) -> Self {
    Self::with_envs(projects, profile, BTreeMap::new())
  }

  pub fn with_envs(
    projects: Projects,
    profile: Profile,
    mut envs: BTreeMap<String, vorpal_index::ExtractionEnv>,
  ) -> Self {
    let servers = projects
      .iter()
      .map(|(name, entry)| {
        let env = envs.remove(name).unwrap_or_default();
        (
          name.clone(),
          Server::with_profile_env(entry.index.clone(), profile, env),
        )
      })
      .collect();
    Self {
      servers,
      projects,
      profile,
    }
  }

  /// The between-requests freshness pulse, fanned out to every enrolled project's server.
  pub fn tick(&mut self) {
    for server in self.servers.values_mut() {
      server.tick();
    }
  }

  pub fn handle_line(&mut self, line: &str) -> Option<String> {
    crate::protocol::handle_line(self, line)
  }

  /// The single-project tool list, with a `project` selector injected into every tool's
  /// schema, plus the registry-listing tool.
  fn tools_list_multi(&self) -> Vec<Value> {
    let mut tools = tool_declarations(self.profile);
    let names: Vec<&str> = self.projects.keys().map(String::as_str).collect();
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
    let mut extra = vec![json!({
      "name": "list_projects",
      "description": "The projects this daemon is enrolled to serve: name, source root, \
        index root, and whether an index exists yet. Enrollment itself is human-only — it \
        happens via the `vorpal mcp allow` CLI a person types, never through this surface, \
        because a confirmation delivered through MCP would be answered by the same agent \
        that may have been influenced.",
      "inputSchema": {"type": "object", "properties": {}, "required": []}
    })];
    decorate_tools(&mut extra);
    tools.extend(extra);
    tools
  }

  fn tools_call_multi(&mut self, tool: &str, params: &Value) -> Result<Value, RpcError> {
    if tool == "list_projects" {
      return Ok(self.list_projects_result());
    }
    // Unknown names are protocol errors (the tools page's split); the profile is the
    // same for every enrolled project, so any server's answer is the daemon's answer.
    if !self.servers.values().next().is_some_and(|s| s.serves(tool)) {
      return Err(RpcError::invalid_params(format!("Unknown tool: {tool}")));
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
        None => return Ok(tool_error("no projects are enrolled")),
      },
      None => {
        return Ok(tool_error(&format!(
          "this daemon serves {} projects — pass \"project\" (one of: {})",
          self.servers.len(),
          self.projects.keys().cloned().collect::<Vec<_>>().join(", ")
        )));
      }
    };
    let Some(entry) = self.projects.get(&name) else {
      return Ok(tool_error(&format!(
        "no enrolled project named '{name}' (enrolled: {}) — enrollment is human-only, via \
         `vorpal mcp allow <path>` typed on the CLI",
        self.projects.keys().cloned().collect::<Vec<_>>().join(", ")
      )));
    };
    // The index tool serves ONLY the enrolled root: an explicit src is honored only when it
    // is exactly that root; anything else is the un-enrolled-source refusal.
    if tool == "index" {
      if let Some(args) = params
        .pointer_mut("/arguments")
        .and_then(Value::as_object_mut)
      {
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
              return Ok(tool_error(&format!(
                "'{explicit}' is not the enrolled source of project '{name}' — this surface \
                 cannot index un-enrolled roots; a person can enroll it with `vorpal mcp \
                 allow {explicit}`"
              )));
            }
          }
        }
      }
    }
    match self.servers.get_mut(&name) {
      Some(server) => Ok(server.tool_result(tool, &params)),
      None => Ok(tool_error(&format!("project '{name}' has no server state"))),
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

impl Handler for MultiServer {
  fn tools(&self) -> Vec<Value> {
    self.tools_list_multi()
  }

  fn call_tool(&mut self, name: &str, params: &Value) -> Result<Value, RpcError> {
    self.tools_call_multi(name, params)
  }

  fn instructions(&self) -> Option<String> {
    Some(format!(
      "{} This daemon serves several enrolled projects: pass `project` on every call (see \
       `list_projects`).",
      crate::server::INSTRUCTIONS
    ))
  }
}

fn tool_error(message: &str) -> Value {
  json!({
    "content": [{"type": "text", "text": message}],
    "structuredContent": {"code": "bad-argument"},
    "isError": true,
  })
}
