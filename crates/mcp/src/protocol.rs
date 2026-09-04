//! The JSON-RPC / MCP shell shared by the single-project [`Server`](crate::Server) and the
//! multi-project router: one line in, at most one line out, and every protocol rule in one
//! place so the tool layers only ever see `(method, params)` and return results or errors.
//!
//! **Dual-era.** MCP revision `2026-07-28` removed the `initialize` handshake: every request
//! carries its protocol version and the client's capabilities in `params._meta`, `server/discover`
//! advertises the server, every result states `resultType`, and list results carry caching
//! hints. Clients built against `2025-11-25` and earlier still open with `initialize`
//! (Claude Code 2.1.260 does, measured 2026-09-04) — the versioning page explicitly permits a
//! server to serve both: a request carrying the modern `_meta` fields is served statelessly
//! under the modern rules; an `initialize` request selects the legacy envelope for that
//! client. Era is decided per request from the message itself, never from connection state,
//! which is what the modern revision demands and what the legacy one tolerates.
//!
//! Framing rules (both eras): one message per line; a JSON array (batch) or any non-object is
//! `-32600`; a request id must be a string or integer (`null` is `-32600`); a message without
//! an id is a notification and is never answered; unknown methods are `-32601`; malformed
//! params are `-32602`. Unknown tool names are protocol errors (`-32602`), tool execution
//! failures are in-band `isError` results — the split the tools page draws.

use serde_json::{Map, Value, json};

/// The stateless revision this server implements in full.
pub const MODERN_VERSION: &str = "2026-07-28";

/// Handshake-era revisions the legacy path answers. An `initialize` naming any of these gets
/// it echoed; any other requested version gets the newest of them (the lifecycle rule of
/// those revisions: "respond with the latest version it supports"). `2024-11-05` is not
/// offered — it requires JSON-RPC batching, which no later revision does.
pub const LEGACY_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26"];

/// How long a client may treat the tool list and the discover result as fresh. The set of
/// tools a process serves is fixed at launch (profile and enrolment are launch arguments and
/// nothing on the surface can change them), and the modern versioning page tells stdio
/// clients to cache `server/discover` for the lifetime of the process anyway — so the hint
/// is "a day", i.e. longer than any editor session, rather than a tuned interval.
pub const LIST_TTL_MS: u64 = 86_400_000;

const META_PROTOCOL_VERSION: &str = "io.modelcontextprotocol/protocolVersion";
const META_CLIENT_CAPABILITIES: &str = "io.modelcontextprotocol/clientCapabilities";
const META_SERVER_INFO: &str = "io.modelcontextprotocol/serverInfo";

pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const INTERNAL_ERROR: i64 = -32603;
/// `UnsupportedProtocolVersionError` (2026-07-28): `data.supported` lists what we speak.
pub const UNSUPPORTED_PROTOCOL_VERSION: i64 = -32022;

/// A JSON-RPC error response body.
#[derive(Debug, Clone, PartialEq)]
pub struct RpcError {
  pub code: i64,
  pub message: String,
  pub data: Option<Value>,
}

impl RpcError {
  pub fn new(code: i64, message: impl Into<String>) -> Self {
    Self {
      code,
      message: message.into(),
      data: None,
    }
  }

  pub fn invalid_params(message: impl Into<String>) -> Self {
    Self::new(INVALID_PARAMS, message)
  }

  pub fn method_not_found(method: &str) -> Self {
    Self::new(METHOD_NOT_FOUND, format!("method not found: {method}"))
  }

  fn with_data(mut self, data: Value) -> Self {
    self.data = Some(data);
    self
  }
}

/// Which envelope a request is served under (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Era {
  /// `2026-07-28`: per-request `_meta`, `resultType`, caching hints, `server/discover`.
  Modern,
  /// `2025-11-25` and earlier: `initialize` handshake, bare results.
  Legacy,
}

/// What the tool layer must provide; the shell does everything else.
pub trait Handler {
  /// The tools this daemon serves, in the deterministic order `tools/list` returns them
  /// (already filtered to the profile, already decorated).
  fn tools(&self) -> Vec<Value>;
  /// Run one tool call. `Ok` is the `CallToolResult` body (`content`, `structuredContent`,
  /// `isError`); `Err` is a protocol error (unknown tool, malformed request).
  fn call_tool(&mut self, name: &str, params: &Value) -> Result<Value, RpcError>;
  /// Natural-language guidance for `server/discover` (and legacy `initialize`), or `None`.
  fn instructions(&self) -> Option<String> {
    None
  }
}

/// Handle one line for `handler`. `None` means "say nothing" (a notification, or a blank
/// line); `Some` is exactly one response line.
pub fn handle_line(handler: &mut impl Handler, line: &str) -> Option<String> {
  let msg: Value = match serde_json::from_str(line) {
    Ok(value) => value,
    Err(_) => {
      return Some(error_line(
        Value::Null,
        RpcError::new(PARSE_ERROR, "parse error"),
      ));
    }
  };
  let Some(obj) = msg.as_object() else {
    let why = if msg.is_array() {
      "JSON-RPC batches are not supported: send one message per line"
    } else {
      "a message must be a JSON object"
    };
    return Some(error_line(Value::Null, RpcError::new(INVALID_REQUEST, why)));
  };
  let method = obj.get("method").and_then(Value::as_str);
  let params = obj.get("params").cloned().unwrap_or(Value::Null);
  let id = match obj.get("id") {
    // No id: a notification. Never answered, whatever it says.
    None => {
      handle_notification(method.unwrap_or(""), &params);
      return None;
    }
    Some(id) if id.is_string() || id.is_i64() || id.is_u64() => id.clone(),
    Some(_) => {
      return Some(error_line(
        Value::Null,
        RpcError::new(INVALID_REQUEST, "request id must be a string or an integer"),
      ));
    }
  };
  let Some(method) = method else {
    return Some(error_line(
      id,
      RpcError::new(INVALID_REQUEST, "request has no method"),
    ));
  };
  let outcome = dispatch(handler, method, &params);
  Some(match outcome {
    Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string(),
    Err(err) => error_line(id, err),
  })
}

/// Notifications this server understands. `notifications/cancelled` is the only one the
/// modern revision defines client→server; nothing here is cancellable yet (a build runs to
/// completion or its supervised timeout), so the request is noted on stderr and the reply
/// still goes out — the cancellation page permits ignoring a cancellation the server cannot
/// honour. `notifications/initialized` (legacy) needs nothing.
fn handle_notification(method: &str, params: &Value) {
  if method == "notifications/cancelled" {
    let id = params.get("requestId").cloned().unwrap_or(Value::Null);
    let reason = params
      .get("reason")
      .and_then(Value::as_str)
      .unwrap_or("no reason given");
    eprintln!(
      "vorpal-mcp: cancellation requested for request {id} ({reason}); the request is not cancellable and will complete"
    );
  }
}

fn dispatch(handler: &mut impl Handler, method: &str, params: &Value) -> Result<Value, RpcError> {
  match era_of(method, params)? {
    Era::Modern => dispatch_modern(handler, method, params),
    Era::Legacy => dispatch_legacy(handler, method, params),
  }
}

/// Decide the era from the request itself. A request carrying the modern `_meta` protocol
/// field is modern and is validated in full (version supported, capabilities present);
/// anything else is legacy — including `server/discover` without `_meta`, which the modern
/// spec calls malformed (`-32602`) and which we therefore refuse under the modern rules.
fn era_of(method: &str, params: &Value) -> Result<Era, RpcError> {
  let meta = params.get("_meta").and_then(Value::as_object);
  let has_modern_meta = meta.is_some_and(|m| m.contains_key(META_PROTOCOL_VERSION));
  if !has_modern_meta {
    return if method == "server/discover" {
      Err(RpcError::invalid_params(format!(
        "server/discover requires params._meta.{META_PROTOCOL_VERSION} and \
         params._meta.{META_CLIENT_CAPABILITIES}"
      )))
    } else {
      Ok(Era::Legacy)
    };
  }
  let meta = meta.unwrap_or(&EMPTY_META);
  let requested = meta
    .get(META_PROTOCOL_VERSION)
    .and_then(Value::as_str)
    .ok_or_else(|| {
      RpcError::invalid_params(format!(
        "params._meta.{META_PROTOCOL_VERSION} must be a string"
      ))
    })?;
  if requested != MODERN_VERSION {
    return Err(
      RpcError::new(UNSUPPORTED_PROTOCOL_VERSION, "Unsupported protocol version")
        .with_data(json!({"supported": [MODERN_VERSION], "requested": requested})),
    );
  }
  if !meta
    .get(META_CLIENT_CAPABILITIES)
    .is_some_and(Value::is_object)
  {
    return Err(RpcError::invalid_params(format!(
      "params._meta.{META_CLIENT_CAPABILITIES} is required on every request (an empty object \
       declares no optional capabilities)"
    )));
  }
  Ok(Era::Modern)
}

static EMPTY_META: std::sync::LazyLock<Map<String, Value>> = std::sync::LazyLock::new(Map::new);

fn dispatch_modern(
  handler: &mut impl Handler,
  method: &str,
  params: &Value,
) -> Result<Value, RpcError> {
  let mut result = match method {
    "server/discover" => {
      let mut result = json!({
        "supportedVersions": [MODERN_VERSION],
        "capabilities": capabilities(),
        "ttlMs": LIST_TTL_MS,
        "cacheScope": "public",
      });
      if let Some(text) = handler.instructions() {
        result["instructions"] = json!(text);
      }
      result
    }
    "tools/list" => {
      refuse_cursor(params)?;
      json!({
        "tools": handler.tools(),
        "ttlMs": LIST_TTL_MS,
        "cacheScope": "public",
      })
    }
    "tools/call" => call(handler, params)?,
    // Removed in this revision (and never ours): `ping`, `logging/setLevel`,
    // `resources/subscribe`, `initialize` under modern `_meta`, `tasks/*`.
    other => return Err(RpcError::method_not_found(other)),
  };
  result["resultType"] = json!("complete");
  result["_meta"] = json!({META_SERVER_INFO: server_info()});
  Ok(result)
}

fn dispatch_legacy(
  handler: &mut impl Handler,
  method: &str,
  params: &Value,
) -> Result<Value, RpcError> {
  match method {
    "initialize" => {
      let requested = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or("");
      let version = if LEGACY_VERSIONS.contains(&requested) {
        requested
      } else {
        LEGACY_VERSIONS[0]
      };
      let mut result = json!({
        "protocolVersion": version,
        "capabilities": capabilities(),
        "serverInfo": server_info(),
      });
      if let Some(text) = handler.instructions() {
        result["instructions"] = json!(text);
      }
      Ok(result)
    }
    "ping" => Ok(json!({})),
    "tools/list" => {
      refuse_cursor(params)?;
      Ok(json!({"tools": handler.tools()}))
    }
    "tools/call" => call(handler, params),
    // A modern-only client that skipped the discover probe lands here without `_meta`;
    // name what we speak so the failure is actionable (the versioning page's advice for
    // `initialize`, applied to the modern method the client should have used).
    "server/discover" => Err(RpcError::invalid_params(format!(
      "server/discover requires modern per-request _meta; supported: {MODERN_VERSION}"
    ))),
    other => Err(RpcError::method_not_found(other)),
  }
}

/// `tools/call` request validation (the `CallToolRequest` schema), then the handler.
fn call(handler: &mut impl Handler, params: &Value) -> Result<Value, RpcError> {
  let name = params
    .get("name")
    .and_then(Value::as_str)
    .ok_or_else(|| RpcError::invalid_params("tools/call requires a string `name`"))?;
  if params
    .get("arguments")
    .is_some_and(|args| !args.is_object() && !args.is_null())
  {
    return Err(RpcError::invalid_params(
      "tools/call `arguments` must be an object",
    ));
  }
  handler.call_tool(name, params)
}

/// `tools/list` is a single page: the server never issues a cursor, so any non-empty one
/// is invalid (`-32602`, the pagination page's rule). An empty string is a cursor too, but
/// the only cursor a client can legitimately hold is one we issued — none.
fn refuse_cursor(params: &Value) -> Result<(), RpcError> {
  match params.get("cursor") {
    None | Some(Value::Null) => Ok(()),
    Some(_) => Err(RpcError::invalid_params(
      "invalid cursor: tools/list is a single page and issues no cursors",
    )),
  }
}

fn capabilities() -> Value {
  // `listChanged` is deliberately absent: the tool set is fixed at launch and there is no
  // `subscriptions/listen` stream to deliver a change on.
  json!({"tools": {}})
}

pub fn server_info() -> Value {
  json!({
    "name": "vorpal-mcp",
    "title": "vorpal",
    "version": env!("CARGO_PKG_VERSION"),
    "description": "Code knowledge graph, hybrid search, and structural search over an indexed repository",
    "websiteUrl": "https://github.com/hyper-light/vorpal",
  })
}

pub fn error_line(id: Value, err: RpcError) -> String {
  let mut error = json!({"code": err.code, "message": err.message});
  if let Some(data) = err.data {
    error["data"] = data;
  }
  json!({"jsonrpc": "2.0", "id": id, "error": error}).to_string()
}

/// Tool-declaration decoration shared by both handlers: display titles, annotation hints,
/// the `format` switch on every record-bearing tool, and an output schema for the structured
/// half of every result. Applied once at listing time so the declarations stay in one place
/// (the per-tool `tool(...)` calls) and the cross-cutting facts in another (here).
pub fn decorate_tools(tools: &mut [Value]) {
  for tool in tools.iter_mut() {
    let name = tool["name"].as_str().unwrap_or("").to_string();
    tool["title"] = json!(title_of(&name));
    let read_only = name != "index";
    tool["annotations"] = json!({
      "title": title_of(&name),
      "readOnlyHint": read_only,
      // `index` rewrites nothing a user wrote: it (re)builds the derived index directory.
      "destructiveHint": false,
      "idempotentHint": true,
      "openWorldHint": false,
    });
    let record_bearing = tool.pointer("/inputSchema/properties/cursor").is_some();
    if record_bearing {
      if let Some(props) = tool
        .pointer_mut("/inputSchema/properties")
        .and_then(Value::as_object_mut)
      {
        props.entry("format").or_insert_with(|| {
          json!({
            "type": "string",
            "enum": ["toon", "lean", "ids"],
            "description": "Rendering of the records page, text AND structuredContent: lean = identity and ranking columns only (cheapest), toon = lossless tab-grid grouped by directory, ids = durable handles only"
          })
        });
      }
    }
    tool["outputSchema"] = output_schema(record_bearing);
  }
}

/// The structured half of every result. Successes carry `generation` and, for record
/// tools, the paged envelope; failures (`isError: true`) carry the stable `code`. One schema
/// admits both so a validating client never rejects a well-formed failure.
fn output_schema(record_bearing: bool) -> Value {
  let mut properties = json!({
    "generation": {
      "type": ["string", "null"],
      "description": "Content id of the index generation this answer was read from (null before any graph is loaded)"
    },
    "code": {
      "type": "string",
      "enum": ["bad-argument", "bad-query", "index-unavailable", "no-watch", "stale-source", "internal", "tool-error"],
      "description": "Stable failure class; present only when isError is true"
    }
  });
  if record_bearing {
    let extra = json!({
      "outcome": {"type": "string", "description": "Selector outcome: found, no-match, ambiguous, …"},
      "base": {"type": "string", "description": "Absolute directory prefix every record's `path` is relative to; absent when paths share none"},
      "records": {"type": "array", "items": {"type": "object"}, "description": "One page of typed records in deterministic order; `format` shapes these too: lean drops signature/span/external_id, ids keeps id/external_id only"},
      "total": {"type": "integer", "minimum": 0},
      "truncated": {"type": "boolean"},
      "nextCursor": {"type": "string", "description": "Pass as `cursor` to fetch the next page; absent on the last page"}
    });
    if let (Some(props), Some(extra)) = (properties.as_object_mut(), extra.as_object()) {
      for (key, value) in extra {
        props.insert(key.clone(), value.clone());
      }
    }
  }
  json!({
    "type": "object",
    "properties": properties,
    "additionalProperties": true
  })
}

fn title_of(name: &str) -> String {
  match name {
    "index" => "Build or refresh the index",
    "health" => "Parse health",
    "schema" => "Index schema",
    "coverage" => "Parse coverage",
    "code_search" => "Pattern search, ranked",
    "architecture" => "Architecture summary",
    "compare_generations" => "Compare generations",
    "impact" => "Change impact",
    "dead_code" => "Dead code",
    "node" => "Find node",
    "graph" => "Graph neighbours",
    "reachable" => "Reachability",
    "structural_search" => "Structural search",
    "rule_search" => "Rule search",
    "ast_dump" => "AST dump",
    "fetch_span" => "Fetch source span",
    "data_flow" => "Data flow",
    "query" => "Graph query",
    "snippet" => "Snippet",
    "why" => "Edge evidence",
    "search" => "Search",
    "list_projects" => "List projects",
    other => other,
  }
  .to_string()
}

#[cfg(test)]
mod tests {
  use super::*;

  struct Echo;
  impl Handler for Echo {
    fn tools(&self) -> Vec<Value> {
      vec![
        json!({"name": "t", "description": "d", "inputSchema": {"type": "object", "properties": {}, "required": []}}),
      ]
    }
    fn call_tool(&mut self, name: &str, _params: &Value) -> Result<Value, RpcError> {
      if name != "t" {
        return Err(RpcError::invalid_params(format!("Unknown tool: {name}")));
      }
      Ok(json!({"content": [{"type": "text", "text": "ok"}], "isError": false}))
    }
  }

  fn modern(method: &str, extra: Value) -> String {
    let mut params = json!({"_meta": {
      "io.modelcontextprotocol/protocolVersion": MODERN_VERSION,
      "io.modelcontextprotocol/clientCapabilities": {}
    }});
    if let (Some(p), Some(e)) = (params.as_object_mut(), extra.as_object()) {
      for (k, v) in e {
        p.insert(k.clone(), v.clone());
      }
    }
    json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).to_string()
  }

  fn parse(line: Option<String>) -> Value {
    serde_json::from_str(&line.expect("a response")).unwrap()
  }

  #[test]
  fn framing_rules() {
    let mut h = Echo;
    assert_eq!(
      parse(handle_line(&mut h, "{nope"))["error"]["code"],
      PARSE_ERROR
    );
    let batch = parse(handle_line(&mut h, "[]"));
    assert_eq!(batch["error"]["code"], INVALID_REQUEST);
    assert!(batch["id"].is_null());
    assert_eq!(
      parse(handle_line(&mut h, "5"))["error"]["code"],
      INVALID_REQUEST
    );
    let null_id = parse(handle_line(
      &mut h,
      r#"{"jsonrpc":"2.0","id":null,"method":"ping"}"#,
    ));
    assert_eq!(null_id["error"]["code"], INVALID_REQUEST);
    assert!(
      handle_line(
        &mut h,
        r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":1}}"#
      )
      .is_none()
    );
    assert!(
      handle_line(
        &mut h,
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#
      )
      .is_none()
    );
    let no_method = parse(handle_line(&mut h, r#"{"jsonrpc":"2.0","id":"a"}"#));
    assert_eq!(no_method["error"]["code"], INVALID_REQUEST);
    assert_eq!(no_method["id"], "a");
  }

  #[test]
  fn modern_discover_list_call() {
    let mut h = Echo;
    let d = parse(handle_line(&mut h, &modern("server/discover", json!({}))));
    let r = &d["result"];
    assert_eq!(r["resultType"], "complete");
    assert_eq!(r["supportedVersions"], json!([MODERN_VERSION]));
    assert_eq!(r["cacheScope"], "public");
    assert_eq!(r["ttlMs"], LIST_TTL_MS);
    assert_eq!(r["_meta"][META_SERVER_INFO]["name"], "vorpal-mcp");
    let l = parse(handle_line(&mut h, &modern("tools/list", json!({}))));
    assert_eq!(l["result"]["tools"][0]["name"], "t");
    assert_eq!(l["result"]["ttlMs"], LIST_TTL_MS);
    let c = parse(handle_line(
      &mut h,
      &modern("tools/call", json!({"name": "t", "arguments": {}})),
    ));
    assert_eq!(c["result"]["resultType"], "complete");
    assert_eq!(c["result"]["isError"], false);
    // Removed methods are unknown under the modern era.
    assert_eq!(
      parse(handle_line(&mut h, &modern("ping", json!({}))))["error"]["code"],
      METHOD_NOT_FOUND
    );
    assert_eq!(
      parse(handle_line(&mut h, &modern("initialize", json!({}))))["error"]["code"],
      METHOD_NOT_FOUND
    );
  }

  #[test]
  fn modern_meta_validation() {
    let mut h = Echo;
    let wrong = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {"_meta": {
      "io.modelcontextprotocol/protocolVersion": "1900-01-01",
      "io.modelcontextprotocol/clientCapabilities": {}}}});
    let e = parse(handle_line(&mut h, &wrong.to_string()));
    assert_eq!(e["error"]["code"], UNSUPPORTED_PROTOCOL_VERSION);
    assert_eq!(e["error"]["data"]["supported"], json!([MODERN_VERSION]));
    assert_eq!(e["error"]["data"]["requested"], "1900-01-01");
    let no_caps = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {"_meta": {
      "io.modelcontextprotocol/protocolVersion": MODERN_VERSION}}});
    assert_eq!(
      parse(handle_line(&mut h, &no_caps.to_string()))["error"]["code"],
      INVALID_PARAMS
    );
    let bare_discover = json!({"jsonrpc": "2.0", "id": 1, "method": "server/discover"});
    assert_eq!(
      parse(handle_line(&mut h, &bare_discover.to_string()))["error"]["code"],
      INVALID_PARAMS
    );
  }

  #[test]
  fn legacy_handshake_and_versions() {
    let mut h = Echo;
    let init = |v: &str| {
      json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
             "params": {"protocolVersion": v, "capabilities": {}}})
      .to_string()
    };
    let r = parse(handle_line(&mut h, &init("2025-11-25")));
    assert_eq!(r["result"]["protocolVersion"], "2025-11-25");
    assert!(r["result"].get("resultType").is_none());
    assert_eq!(r["result"]["serverInfo"]["title"], "vorpal");
    let r = parse(handle_line(&mut h, &init("2025-06-18")));
    assert_eq!(r["result"]["protocolVersion"], "2025-06-18");
    let r = parse(handle_line(&mut h, &init("2024-11-05")));
    assert_eq!(r["result"]["protocolVersion"], "2025-11-25");
    let r = parse(handle_line(&mut h, &init("9999-01-01")));
    assert_eq!(r["result"]["protocolVersion"], "2025-11-25");
    let ping = parse(handle_line(
      &mut h,
      r#"{"jsonrpc":"2.0","id":2,"method":"ping"}"#,
    ));
    assert_eq!(ping["result"], json!({}));
    let list = parse(handle_line(
      &mut h,
      r#"{"jsonrpc":"2.0","id":3,"method":"tools/list"}"#,
    ));
    assert!(list["result"].get("ttlMs").is_none());
    assert_eq!(list["result"]["tools"][0]["name"], "t");
  }

  #[test]
  fn call_validation_and_cursor_refusal() {
    let mut h = Echo;
    let bad_name = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"arguments":{}}}"#;
    assert_eq!(
      parse(handle_line(&mut h, bad_name))["error"]["code"],
      INVALID_PARAMS
    );
    let bad_args =
      r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"t","arguments":[]}}"#;
    assert_eq!(
      parse(handle_line(&mut h, bad_args))["error"]["code"],
      INVALID_PARAMS
    );
    let unknown = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"zzz"}}"#;
    let e = parse(handle_line(&mut h, unknown));
    assert_eq!(e["error"]["code"], INVALID_PARAMS);
    assert!(
      e["error"]["message"]
        .as_str()
        .unwrap()
        .contains("Unknown tool")
    );
    let cursor = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{"cursor":"x"}}"#;
    assert_eq!(
      parse(handle_line(&mut h, cursor))["error"]["code"],
      INVALID_PARAMS
    );
  }

  #[test]
  fn decoration() {
    let mut tools = vec![
      json!({"name": "index", "description": "d", "inputSchema": {"type": "object", "properties": {"src": {}}, "required": ["src"]}}),
      json!({"name": "graph", "description": "d", "inputSchema": {"type": "object", "properties": {"relation": {}, "name": {}, "cursor": {}}, "required": ["relation", "name"]}}),
    ];
    decorate_tools(&mut tools);
    assert_eq!(tools[0]["annotations"]["readOnlyHint"], false);
    assert_eq!(tools[1]["annotations"]["readOnlyHint"], true);
    assert_eq!(tools[1]["title"], "Graph neighbours");
    assert!(tools[1]["inputSchema"]["properties"]["format"].is_object());
    assert!(
      tools[0]["inputSchema"]["properties"]
        .get("format")
        .is_none()
    );
    assert!(tools[1]["outputSchema"]["properties"]["records"].is_object());
    assert!(
      tools[0]["outputSchema"]["properties"]
        .get("records")
        .is_none()
    );
  }
}
