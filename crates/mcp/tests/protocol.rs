//! MCP protocol + warm-index tools, end to end against a real indexed source tree.

use std::fs;
use std::path::PathBuf;

use serde_json::{Value, json};
use vorpal_mcp::Server;

fn request(server: &mut Server, id: u64, method: &str, params: Value) -> Value {
  let line = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string();
  let response = server
    .handle_line(&line)
    .unwrap_or_else(|| panic!("request {method} must get a response"));
  serde_json::from_str(&response).expect("response is valid JSON")
}

fn call_tool(server: &mut Server, id: u64, tool: &str, args: Value) -> (String, bool) {
  let response = request(
    server,
    id,
    "tools/call",
    json!({"name": tool, "arguments": args}),
  );
  let result = &response["result"];
  let text = result["content"][0]["text"].as_str().expect("text content");
  (text.to_owned(), result["isError"].as_bool().unwrap_or(true))
}

/// A source tree with a cross-file call and an import.
fn temp_tree(tag: &str) -> (PathBuf, PathBuf) {
  let base = std::env::temp_dir().join(format!("vorpal-mcp-{tag}-{}", std::process::id()));
  let src = base.join("src");
  let idx = base.join("index");
  let _ = fs::remove_dir_all(&base);
  fs::create_dir_all(&src).unwrap();
  fs::write(src.join("b.rs"), "pub fn target() -> i32 {\n    0\n}\n").unwrap();
  fs::write(
    src.join("a.rs"),
    "use b::target;\n\npub fn caller() -> i32 {\n    target()\n}\n",
  )
  .unwrap();
  (src, idx)
}

#[test]
fn initialize_handshake_and_tool_listing() {
  let (_src, idx) = temp_tree("handshake");
  let mut server = Server::new(idx);

  let response = request(
    &mut server,
    1,
    "initialize",
    json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t"}}),
  );
  assert_eq!(response["jsonrpc"], "2.0");
  assert_eq!(response["id"], 1);
  assert_eq!(response["result"]["protocolVersion"], "2025-06-18");
  assert!(response["result"]["capabilities"]["tools"].is_object());
  assert_eq!(response["result"]["serverInfo"]["name"], "vorpal-mcp");

  // An unknown requested revision falls back instead of failing.
  let response = request(
    &mut server,
    2,
    "initialize",
    json!({"protocolVersion": "9999-01-01"}),
  );
  assert_eq!(response["result"]["protocolVersion"], "2024-11-05");

  // notifications get no response.
  assert!(
    server
      .handle_line(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}).to_string())
      .is_none()
  );

  // ping → empty result.
  let response = request(&mut server, 3, "ping", Value::Null);
  assert!(response["result"].as_object().unwrap().is_empty());

  let response = request(&mut server, 4, "tools/list", Value::Null);
  let tools = response["result"]["tools"].as_array().unwrap();
  let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
  assert_eq!(
    names,
    [
      "index",
      "node",
      "callers",
      "references",
      "importers",
      "implementors",
      "type_users",
      "reachable",
      "structural_search",
      "fetch_span",
      "search"
    ]
  );
  for tool in tools {
    assert_eq!(tool["inputSchema"]["type"], "object", "{}", tool["name"]);
    assert!(tool["description"].as_str().is_some_and(|d| !d.is_empty()));
  }
}

#[test]
fn warm_index_tools_answer_graph_queries() {
  let (src, idx) = temp_tree("tools");
  let mut server = Server::new(idx);

  let (text, is_err) = call_tool(
    &mut server,
    1,
    "index",
    json!({"src": src.to_string_lossy()}),
  );
  assert!(!is_err, "{text}");
  assert!(text.contains("indexed 2 files"), "{text}");

  let (text, is_err) = call_tool(&mut server, 2, "callers", json!({"name": "target"}));
  assert!(!is_err, "{text}");
  assert!(text.contains("caller"), "callers of target: {text}");

  let (text, is_err) = call_tool(&mut server, 3, "importers", json!({"name": "target"}));
  assert!(!is_err, "{text}");
  assert!(text.contains("a.rs"), "importers of target: {text}");

  let (text, is_err) = call_tool(&mut server, 4, "node", json!({"name": "target"}));
  assert!(!is_err, "{text}");
  assert!(text.contains("target [Function]"), "{text}");

  let (text, is_err) = call_tool(
    &mut server,
    5,
    "reachable",
    json!({"name": "target", "direction": "in"}),
  );
  assert!(!is_err, "{text}");
  assert!(text.contains("caller"), "transitive callers: {text}");

  let (text, is_err) = call_tool(&mut server, 6, "node", json!({"name": "missing"}));
  assert!(!is_err, "{text}");
  assert!(text.contains("no results"), "{text}");

  // Re-index of the unchanged tree is the near-instant reuse path.
  let (text, is_err) = call_tool(
    &mut server,
    7,
    "index",
    json!({"src": src.to_string_lossy()}),
  );
  assert!(!is_err, "{text}");
  assert!(text.contains("unchanged — reused"), "{text}");

  let _ = fs::remove_dir_all(src.parent().unwrap());
}

#[test]
fn protocol_and_tool_errors_are_explicit() {
  let (_src, idx) = temp_tree("errors");
  let mut server = Server::new(idx);

  // Unknown method → JSON-RPC error.
  let response = request(&mut server, 1, "resources/list", Value::Null);
  assert_eq!(response["error"]["code"], -32601);

  // Malformed JSON → parse error with null id.
  let response: Value = serde_json::from_str(
    &server
      .handle_line("{not json")
      .expect("parse error response"),
  )
  .unwrap();
  assert_eq!(response["error"]["code"], -32700);
  assert!(response["id"].is_null());

  // Unknown tool → in-band tool error, not a protocol error.
  let (text, is_err) = call_tool(&mut server, 2, "explode", json!({}));
  assert!(is_err);
  assert!(text.contains("unknown tool"), "{text}");

  // Query before any index exists → helpful in-band error.
  let (text, is_err) = call_tool(&mut server, 3, "callers", json!({"name": "x"}));
  assert!(is_err);
  assert!(text.contains("'index' tool"), "{text}");

  // Missing / invalid arguments.
  let (text, is_err) = call_tool(&mut server, 4, "callers", json!({}));
  assert!(is_err);
  assert!(text.contains("missing required argument"), "{text}");
  let (text, is_err) = call_tool(
    &mut server,
    5,
    "reachable",
    json!({"name": "x", "direction": "sideways"}),
  );
  assert!(is_err);
  assert!(text.contains("direction"), "{text}");
}
