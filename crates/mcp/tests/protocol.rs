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
      "health",
      "schema",
      "coverage",
      "architecture",
      "compare_generations",
      "impact",
      "dead_code",
      "node",
      "callers",
      "references",
      "importers",
      "implementors",
      "type_users",
      "reachable",
      "structural_search",
      "rule_search",
      "ast_dump",
      "fetch_span",
      "snippet",
      "why",
      "search"
    ]
  );
  for tool in tools {
    assert_eq!(tool["inputSchema"]["type"], "object", "{}", tool["name"]);
    assert!(tool["description"].as_str().is_some_and(|d| !d.is_empty()));
  }
}

#[test]
fn profiles_gate_both_the_listing_and_the_calls() {
  use vorpal_mcp::Profile;
  let (src, idx) = temp_tree("profile");
  // Build once with a full server so the scout server has a graph to read.
  let mut full = Server::new(idx.clone());
  let (_, is_err) = call_tool(&mut full, 1, "index", json!({"src": src.to_str().unwrap()}));
  assert!(!is_err);

  let mut scout = Server::with_profile(idx, Profile::Scout);
  let response = request(&mut scout, 2, "tools/list", Value::Null);
  let names: Vec<&str> = response["result"]["tools"]
    .as_array()
    .unwrap()
    .iter()
    .map(|t| t["name"].as_str().unwrap())
    .collect();
  assert_eq!(names, ["schema", "node", "fetch_span", "snippet", "search"]);

  // Advertised tools answer; unlisted tools refuse with the stable code — the listing and
  // the gate can never drift apart.
  let (_, is_err) = call_tool(&mut scout, 3, "node", json!({"name": "target"}));
  assert!(!is_err);
  let response = request(
    &mut scout,
    4,
    "tools/call",
    json!({"name": "index", "arguments": {"src": src.to_str().unwrap()}}),
  );
  assert_eq!(response["result"]["isError"], true);
  assert_eq!(response["result"]["structuredContent"]["code"], "bad-argument");
}

#[test]
fn schema_reports_vocabulary_with_counts() {
  let (src, idx) = temp_tree("schema");
  let mut server = Server::new(idx);
  let (_, is_err) = call_tool(&mut server, 1, "index", json!({"src": src.to_str().unwrap()}));
  assert!(!is_err);

  let response = request(&mut server, 2, "tools/call", json!({"name": "schema", "arguments": {}}));
  let result = &response["result"];
  assert_eq!(result["isError"], false);
  let data = &result["structuredContent"];
  assert!(data["nodes"].as_u64().unwrap() >= 4, "two files + two fns: {data}");
  assert_eq!(data["files"], 2);
  let kinds: Vec<&str> = data["kinds"]
    .as_array()
    .unwrap()
    .iter()
    .map(|row| row["name"].as_str().unwrap())
    .collect();
  assert!(kinds.contains(&"Function") && kinds.contains(&"File"), "{kinds:?}");
  let relations: Vec<&str> = data["relations"]
    .as_array()
    .unwrap()
    .iter()
    .map(|row| row["name"].as_str().unwrap())
    .collect();
  assert!(relations.contains(&"calls") && relations.contains(&"defines"), "{relations:?}");
  assert_eq!(data["grades"][0], "exact");
  let text = result["content"][0]["text"].as_str().unwrap();
  assert!(text.starts_with("generation "), "{text}");
  assert!(text.contains("kinds: "), "{text}");
}

#[test]
fn snippet_selects_by_name_with_context_and_refuses_stale() {
  let (src, idx) = temp_tree("snippet");
  let mut server = Server::new(idx);
  let (_, is_err) = call_tool(&mut server, 1, "index", json!({"src": src.to_str().unwrap()}));
  assert!(!is_err);

  // By name: digest-verified whole-line body, records carry line + verification.
  let response = request(
    &mut server,
    2,
    "tools/call",
    json!({"name": "snippet", "arguments": {"name": "target"}}),
  );
  let text = response["result"]["content"][0]["text"].as_str().unwrap();
  assert!(text.contains("b.rs:1"), "header names the file+line: {text}");
  assert!(text.contains("(verified)"), "digest verdict present: {text}");
  assert!(text.contains("pub fn target() -> i32 {"), "body has the definition: {text}");
  let record = &response["result"]["structuredContent"]["records"][0];
  assert_eq!(record["line"], 1);
  assert_eq!(record["verification"], "verified");
  assert!(record["body"].as_str().unwrap().contains("pub fn target"));

  // Context expansion pulls in the neighboring line (the import above `caller`).
  let response = request(
    &mut server,
    3,
    "tools/call",
    json!({"name": "snippet", "arguments": {"name": "caller", "context_lines": 2}}),
  );
  let body = response["result"]["structuredContent"]["records"][0]["body"].as_str().unwrap();
  assert!(body.contains("use b::target;"), "context reaches the import: {body}");

  // A changed file refuses with the stable stale-source code, never inconsistent bytes.
  fs::write(
    src.join("b.rs"),
    "// shifted\npub fn target() -> i32 {\n    1\n}\n",
  )
  .unwrap();
  // Bypass the watch's rebuild-on-dirty so the pinned generation stays behind the edit:
  // query through a server whose watch never saw the tree (custom index location).
  let response = request(
    &mut server,
    4,
    "tools/call",
    json!({"name": "snippet", "arguments": {"name": "target"}}),
  );
  let result = &response["result"];
  if result["isError"].as_bool() == Some(true) {
    assert_eq!(result["structuredContent"]["code"], "stale-source");
  } else {
    // The watch rebuilt first (timing-dependent): the snippet must then be the NEW bytes.
    let body = result["structuredContent"]["records"][0]["body"].as_str().unwrap();
    assert!(body.contains("    1"), "rebuilt snippet reflects the edit: {body}");
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
fn rule_search_and_ast_dump_serve_the_full_rule_model() {
  // The structural tools need a watched tree: the daemon-default `<src>/.vorpal/index`.
  let base = std::env::temp_dir().join(format!("vorpal-mcp-struct-{}", std::process::id()));
  let src = base.join("src");
  let _ = fs::remove_dir_all(&base);
  fs::create_dir_all(&src).unwrap();
  fs::write(src.join("b.rs"), "pub fn target() -> i32 {\n    0\n}\n").unwrap();
  fs::write(
    src.join("a.rs"),
    "use b::target;\n\npub fn caller() -> i32 {\n    target()\n}\n",
  )
  .unwrap();
  let idx = src.join(".vorpal").join("index");
  let mut server = Server::new(idx);

  let (text, is_err) = call_tool(
    &mut server,
    1,
    "index",
    json!({"src": src.to_string_lossy()}),
  );
  assert!(!is_err, "{text}");

  // Full rule model: composite rule + constraints + fix, rendered as a dry run.
  let rule = "id: retarget\nlanguage: rust\nrule:\n  pattern: $F()\nconstraints:\n  F:\n    regex: ^target$\nfix: replaced()\n";
  let (text, is_err) = call_tool(&mut server, 2, "rule_search", json!({"rule": rule}));
  assert!(!is_err, "{text}");
  assert!(text.contains("[retarget]") && text.contains("a.rs"), "{text}");
  assert!(text.contains("fix (dry-run) → replaced()"), "{text}");
  // Dry run means dry: the file still holds the original call.
  let a_rs = fs::read_to_string(src.join("a.rs")).unwrap();
  assert!(a_rs.contains("target()") && !a_rs.contains("replaced"), "{a_rs}");

  // A malformed rule is a tool error, not a crash.
  let (text, is_err) = call_tool(&mut server, 3, "rule_search", json!({"rule": "rule: [nonsense"}));
  assert!(is_err, "{text}");

  // AST dump: inline source, named nodes with kinds and spans.
  let (text, is_err) = call_tool(
    &mut server,
    4,
    "ast_dump",
    json!({"source": "def f():\n    return g()\n", "lang": "python"}),
  );
  assert!(!is_err, "{text}");
  assert!(
    text.contains("function_definition") && text.contains("call"),
    "{text}"
  );

  let _ = fs::remove_dir_all(&base);
}

#[test]
fn fetch_span_is_digest_verified_and_refuses_stale_files() {
  // Non-watched layout: an edit after indexing must NOT be silently rebuilt away — that is
  // exactly the staleness this contract detects.
  let (src, idx) = temp_tree("fetchspan");
  let mut server = Server::new(idx);
  let (text, is_err) = call_tool(
    &mut server,
    1,
    "index",
    json!({"src": src.to_string_lossy()}),
  );
  assert!(!is_err, "{text}");

  let (text, is_err) = call_tool(&mut server, 2, "node", json!({"name": "target"}));
  assert!(!is_err, "{text}");
  let id: u64 = text
    .split("id ")
    .nth(1)
    .and_then(|rest| rest.split(|c: char| !c.is_ascii_digit()).next())
    .and_then(|digits| digits.parse().ok())
    .unwrap_or_else(|| panic!("no node id in: {text}"));

  let (text, is_err) = call_tool(&mut server, 3, "fetch_span", json!({"id": id}));
  assert!(!is_err, "{text}");
  assert!(text.contains("(source verified)"), "{text}");
  assert!(text.contains("pub fn target"), "{text}");

  // Change the file: persisted offsets are now stale, so the tool must refuse the slice.
  let b_rs = src.join("b.rs");
  let mut content = fs::read_to_string(&b_rs).unwrap();
  content.insert_str(0, "// shifted\n");
  fs::write(&b_rs, content).unwrap();
  let (text, is_err) = call_tool(&mut server, 4, "fetch_span", json!({"id": id}));
  assert!(is_err, "stale file must refuse, got: {text}");
  assert!(text.contains("changed since"), "{text}");

  let _ = fs::remove_dir_all(src.parent().unwrap());
}

#[test]
fn results_carry_generation_identity_and_stable_error_codes() {
  let (src, idx) = temp_tree("envelope");
  let mut server = Server::new(idx);

  // Success envelope: the pinned generation content id rides every result.
  let response = request(
    &mut server,
    1,
    "tools/call",
    json!({"name": "index", "arguments": {"src": src.to_string_lossy()}}),
  );
  let generation = response["result"]["structuredContent"]["generation"]
    .as_str()
    .expect("generation id on success")
    .to_string();
  assert!(!generation.is_empty());
  let response = request(
    &mut server,
    2,
    "tools/call",
    json!({"name": "node", "arguments": {"name": "target"}}),
  );
  assert_eq!(
    response["result"]["structuredContent"]["generation"].as_str(),
    Some(generation.as_str()),
    "query answers name the same generation the index call pinned"
  );

  // Error envelope: stable machine-readable codes, not just prose.
  let response = request(
    &mut server,
    3,
    "tools/call",
    json!({"name": "node", "arguments": {}}),
  );
  assert_eq!(response["result"]["isError"], true);
  assert_eq!(
    response["result"]["structuredContent"]["code"].as_str(),
    Some("bad-argument")
  );

  // The stale-source refusal carries its own code.
  let (text, _) = call_tool(&mut server, 4, "node", json!({"name": "target"}));
  let id: u64 = text
    .split("id ")
    .nth(1)
    .and_then(|rest| rest.split(|c: char| !c.is_ascii_digit()).next())
    .and_then(|digits| digits.parse().ok())
    .unwrap_or_else(|| panic!("no node id in: {text}"));
  let b_rs = src.join("b.rs");
  let mut content = fs::read_to_string(&b_rs).unwrap();
  content.insert_str(0, "// shifted\n");
  fs::write(&b_rs, content).unwrap();
  let response = request(
    &mut server,
    5,
    "tools/call",
    json!({"name": "fetch_span", "arguments": {"id": id}}),
  );
  assert_eq!(response["result"]["isError"], true);
  assert_eq!(
    response["result"]["structuredContent"]["code"].as_str(),
    Some("stale-source")
  );

  let _ = fs::remove_dir_all(src.parent().unwrap());
}

#[test]
fn typed_records_and_cursor_pagination() {
  let (src, idx) = temp_tree("records");
  let mut server = Server::new(idx);
  let (text, is_err) = call_tool(
    &mut server,
    1,
    "index",
    json!({"src": src.to_string_lossy()}),
  );
  assert!(!is_err, "{text}");

  // `node` returns typed records: full identity (dense id, durable eid, kind, path, span).
  let response = request(
    &mut server,
    2,
    "tools/call",
    json!({"name": "node", "arguments": {"name": "target"}}),
  );
  let data = &response["result"]["structuredContent"];
  assert_eq!(data["outcome"], "hits");
  assert_eq!(data["total"], 1);
  assert_eq!(data["truncated"], false);
  let record = &data["records"][0];
  assert_eq!(record["name"], "target");
  assert_eq!(record["kind"], "Function");
  assert!(record["path"].as_str().unwrap().ends_with("b.rs"));
  assert!(record["id"].as_u64().is_some());
  assert!(
    record["external_id"].as_str().unwrap().starts_with("eid:"),
    "{record}"
  );
  assert!(record["span"][1].as_u64().unwrap() > 0);

  // `callers` records carry the edge grade; `reachable` steps carry relation + via.
  let response = request(
    &mut server,
    3,
    "tools/call",
    json!({"name": "callers", "arguments": {"name": "target"}}),
  );
  let data = &response["result"]["structuredContent"];
  assert_eq!(data["outcome"], "hits");
  assert_eq!(data["records"][0]["name"], "caller");
  assert!(data["records"][0]["grade"].as_str().is_some());

  let response = request(
    &mut server,
    4,
    "tools/call",
    json!({"name": "reachable", "arguments": {"name": "target", "direction": "in"}}),
  );
  let data = &response["result"]["structuredContent"];
  assert_eq!(data["outcome"], "hits");
  let step = &data["records"][0];
  assert_eq!(step["name"], "caller");
  assert_eq!(step["relation"], "calls");
  assert_eq!(step["depth"], 1);
  assert!(step["via"].as_u64().is_some());

  // `why` typed evidence: relation, grade, reason, span — from the edge the graph holds.
  let from_id = step["id"].as_u64().unwrap();
  let target_id = record["id"].as_u64().unwrap();
  let response = request(
    &mut server,
    5,
    "tools/call",
    json!({"name": "why", "arguments": {"from_id": from_id, "to_id": target_id}}),
  );
  let data = &response["result"]["structuredContent"];
  assert_eq!(data["outcome"], "hits");
  let row = &data["records"][0];
  assert_eq!(row["relation"], "calls");
  assert_eq!(row["to"].as_u64(), Some(target_id));
  assert!(row["reason"].as_str().is_some());
  assert!(row["span"][1].as_u64().unwrap() > 0);

  // Pagination: limit=1 over the search records pages deterministically with a nextCursor.
  let response = request(
    &mut server,
    6,
    "tools/call",
    json!({"name": "search", "arguments": {"query": "target caller", "k": 5, "limit": 1}}),
  );
  let data = &response["result"]["structuredContent"];
  let total = data["total"].as_u64().unwrap();
  assert!(total >= 2, "{data}");
  assert_eq!(data["records"].as_array().unwrap().len(), 1);
  assert_eq!(data["truncated"], true);
  let cursor = data["nextCursor"].as_str().unwrap().to_string();
  let first_name = data["records"][0]["name"].as_str().unwrap().to_string();
  let response = request(
    &mut server,
    7,
    "tools/call",
    json!({"name": "search", "arguments": {"query": "target caller", "k": 5, "limit": 1, "cursor": cursor}}),
  );
  let data = &response["result"]["structuredContent"];
  let second_name = data["records"][0]["name"].as_str().unwrap();
  assert_ne!(first_name, second_name, "pages advance through the ranking");

  // A malformed cursor is a coded bad-argument, never a silent first page.
  let response = request(
    &mut server,
    8,
    "tools/call",
    json!({"name": "node", "arguments": {"name": "target", "cursor": "bogus"}}),
  );
  assert_eq!(response["result"]["isError"], true);
  assert_eq!(
    response["result"]["structuredContent"]["code"].as_str(),
    Some("bad-argument")
  );

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
