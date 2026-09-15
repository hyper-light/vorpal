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
  assert_eq!(response["result"]["protocolVersion"], "2025-11-25");
  assert_eq!(response["result"]["serverInfo"]["title"], "vorpal");
  let instructions = response["result"]["instructions"].as_str().expect("instructions");
  assert!(!instructions.is_empty());
  // The fast-path note names THIS index, absolute, so a shell client can go straight there.
  assert!(instructions.contains("--index /"), "{instructions}");
  assert!(instructions.contains("graph callers <name>"), "{instructions}");
  // Legacy results carry no modern envelope fields.
  assert!(response["result"].get("resultType").is_none());

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
      "scope",
      "coverage",
      "code_search",
      "text_search",
      "architecture",
      "compare_generations",
      "impact",
      "dead_code",
      "node",
      "graph",
      "reachable",
      "structural_search",
      "rule_search",
      "ast_dump",
      "fetch_span",
      "data_flow",
      "query",
      "snippet",
      "why",
      "search"
    ]
  );
  for tool in tools {
    assert_eq!(tool["inputSchema"]["type"], "object", "{}", tool["name"]);
    assert!(tool["description"].as_str().is_some_and(|d| !d.is_empty()));
    assert!(tool["title"].as_str().is_some_and(|t| !t.is_empty()), "{}", tool["name"]);
    let read_only = tool["annotations"]["readOnlyHint"].as_bool().expect("readOnlyHint");
    assert_eq!(read_only, tool["name"] != "index", "{}", tool["name"]);
    assert_eq!(tool["annotations"]["openWorldHint"], false);
    // Every record-bearing tool declares the `format` switch it honours.
    if tool["inputSchema"]["properties"].get("cursor").is_some() {
      assert!(
        tool["inputSchema"]["properties"]["format"].is_object(),
        "{} pages records but does not declare format",
        tool["name"]
      );
    }
  }
  // The listing is a per-call token cost for every client; it is size-gated here. The
  // 44 KB listing of 2026-09-04 cost ~12 K tokens per turn when resident; the diet
  // landed at 10.8 KB. Descriptions live in docs/mcp.md, not on the wire.
  let bytes = serde_json::to_string(&response["result"]["tools"]).unwrap().len();
  assert!(bytes <= 12_000, "tools/list is {bytes} B; keep it under 12 KB");
}

/// `params._meta` every 2026-07-28 request carries.
fn modern_meta() -> Value {
  json!({
    "io.modelcontextprotocol/protocolVersion": vorpal_mcp::protocol::MODERN_VERSION,
    "io.modelcontextprotocol/clientCapabilities": {},
    "io.modelcontextprotocol/clientInfo": {"name": "t", "version": "0"}
  })
}

#[test]
fn modern_era_is_served_statelessly() {
  let (src, idx) = temp_tree("modern");
  let mut server = Server::new(idx);

  // No handshake: discover first (or not at all), every request self-describing.
  let response = request(&mut server, 1, "server/discover", json!({"_meta": modern_meta()}));
  let result = &response["result"];
  assert_eq!(result["resultType"], "complete");
  assert_eq!(result["supportedVersions"], json!([vorpal_mcp::protocol::MODERN_VERSION]));
  assert!(result["capabilities"]["tools"].is_object());
  assert_eq!(result["cacheScope"], "public");
  assert!(result["ttlMs"].as_u64().is_some());
  assert_eq!(result["_meta"]["io.modelcontextprotocol/serverInfo"]["name"], "vorpal-mcp");
  assert!(result["instructions"].as_str().is_some());

  let response = request(&mut server, 2, "tools/list", json!({"_meta": modern_meta()}));
  assert_eq!(response["result"]["resultType"], "complete");
  assert!(response["result"]["ttlMs"].as_u64().is_some());
  assert_eq!(response["result"]["cacheScope"], "public");
  assert_eq!(response["result"]["tools"][0]["name"], "index");

  let response = request(
    &mut server,
    3,
    "tools/call",
    json!({"name": "index", "arguments": {"src": src.to_str().unwrap()}, "_meta": modern_meta()}),
  );
  assert_eq!(response["result"]["resultType"], "complete");
  assert_eq!(response["result"]["isError"], false);
  assert_eq!(response["result"]["_meta"]["io.modelcontextprotocol/serverInfo"]["title"], "vorpal");
  let response = request(
    &mut server,
    4,
    "tools/call",
    json!({"name": "graph", "arguments": {"relation": "callers", "name": "target"}, "_meta": modern_meta()}),
  );
  assert!(response["result"]["content"][0]["text"].as_str().unwrap().contains("caller"));

  // Removed methods are unknown under the modern era.
  let response = request(&mut server, 5, "ping", json!({"_meta": modern_meta()}));
  assert_eq!(response["error"]["code"], -32601);
  let response = request(&mut server, 6, "initialize", json!({"_meta": modern_meta()}));
  assert_eq!(response["error"]["code"], -32601);

  // Version mismatch names what we speak; missing capabilities is malformed.
  let mut wrong = modern_meta();
  wrong["io.modelcontextprotocol/protocolVersion"] = json!("2025-11-25");
  let response = request(&mut server, 7, "tools/list", json!({"_meta": wrong}));
  assert_eq!(response["error"]["code"], -32022);
  assert_eq!(
    response["error"]["data"]["supported"],
    json!([vorpal_mcp::protocol::MODERN_VERSION])
  );
  assert_eq!(response["error"]["data"]["requested"], "2025-11-25");
  let response = request(
    &mut server,
    8,
    "tools/list",
    json!({"_meta": {"io.modelcontextprotocol/protocolVersion": vorpal_mcp::protocol::MODERN_VERSION}}),
  );
  assert_eq!(response["error"]["code"], -32602);
  // A bare discover (no _meta) is malformed too — the probe must carry the fields.
  let response = request(&mut server, 9, "server/discover", Value::Null);
  assert_eq!(response["error"]["code"], -32602);

  // Both eras on one process, interleaved: the legacy client still works.
  let response = request(
    &mut server,
    10,
    "initialize",
    json!({"protocolVersion": "2025-11-25", "capabilities": {}}),
  );
  assert_eq!(response["result"]["protocolVersion"], "2025-11-25");
  let (text, is_err) = call_tool(&mut server, 11, "graph", json!({"relation": "callers", "name": "target"}));
  assert!(!is_err);
  assert!(text.contains("caller"));

  // `mentions: true` adds the absence proof: b.rs defines target and a.rs calls it, so no
  // file outside the graph's answer spells the name — empty and complete.
  let response = request(
    &mut server,
    111,
    "tools/call",
    json!({"name": "graph", "arguments": {"relation": "callers", "name": "target", "mentions": true}}),
  );
  let mentions = &response["result"]["structuredContent"]["mentions"];
  assert_eq!(mentions["unattributedFiles"], 0, "{mentions}");
  assert_eq!(mentions["complete"], true, "{mentions}");
  assert_eq!(mentions["attributedFiles"], 2, "{mentions}");
  assert_eq!(mentions["records"].as_array().map(Vec::len), Some(0));

  let _ = fs::remove_dir_all(src.parent().unwrap());
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
  assert_eq!(names, ["schema", "scope", "text_search", "node", "fetch_span", "snippet", "search"]);

  // Advertised tools answer; unlisted tools are unknown to this daemon — a protocol error,
  // exactly as for a name that exists nowhere — so the listing and the gate never drift.
  let (_, is_err) = call_tool(&mut scout, 3, "node", json!({"name": "target"}));
  assert!(!is_err);
  let response = request(
    &mut scout,
    4,
    "tools/call",
    json!({"name": "index", "arguments": {"src": src.to_str().unwrap()}}),
  );
  assert_eq!(response["error"]["code"], -32602);
  assert!(response["error"]["message"].as_str().unwrap().contains("Unknown tool: index"));
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
fn toon_format_rewrites_record_tool_text() {
  let (src, idx) = temp_tree("toon");
  let mut server = Server::new(idx);
  let (_, is_err) = call_tool(&mut server, 1, "index", json!({"src": src.to_str().unwrap()}));
  assert!(!is_err);
  let (text, is_err) = call_tool(
    &mut server,
    2,
    "graph",
    json!({"relation": "callers", "name": "target", "format": "toon"}),
  );
  assert!(!is_err);
  assert!(text.starts_with("cols: "), "{text}");
  assert!(text.contains("caller"), "{text}");
  let (ids, _) = call_tool(&mut server, 3, "graph", json!({"relation": "callers", "name": "target", "format": "ids"}));
  assert!(ids.starts_with("eid:") || ids.starts_with("id:"), "{ids}");
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

  let (text, is_err) = call_tool(&mut server, 2, "graph", json!({"relation": "callers", "name": "target"}));
  assert!(!is_err, "{text}");
  assert!(text.contains("caller"), "callers of target: {text}");

  let (text, is_err) = call_tool(&mut server, 3, "graph", json!({"relation": "importers", "name": "target"}));
  assert!(!is_err, "{text}");
  assert!(text.contains("a.rs"), "importers of target: {text}");

  let (text, is_err) = call_tool(&mut server, 4, "node", json!({"name": "target"}));
  assert!(!is_err, "{text}");
  assert!(text.contains("target\tFunction"), "{text}");

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
  assert!(text.contains("records[0]:"), "{text}");

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

  // The same hits ride structuredContent as records (paged like every record tool).
  let response = request(
    &mut server,
    12,
    "tools/call",
    json!({"name": "rule_search", "arguments": {"rule": rule}}),
  );
  let records = response["result"]["structuredContent"]["records"].as_array().expect("rule records");
  assert_eq!(records.len(), 1, "{records:?}");
  assert_eq!(records[0]["rule"], "retarget");
  assert!(records[0]["path"].as_str().unwrap().ends_with("a.rs"));
  assert_eq!(records[0]["line"], 4);
  assert_eq!(records[0]["fixes"][0], "replaced()");
  assert_eq!(response["result"]["structuredContent"]["outcome"], "hits");

  // A bare-pattern rule (no fix/constraints) has span-only records: it is answered through
  // the reference shortcut or the chunk memo like `structural_search`, same record shape.
  let response = request(
    &mut server,
    121,
    "tools/call",
    json!({"name": "rule_search", "arguments": {"rule": "id: bare\nlanguage: rust\nrule:\n  pattern: target()\n"}}),
  );
  let sc = &response["result"]["structuredContent"];
  let bare = sc["records"].as_array().expect("bare rule records");
  assert_eq!(bare.len(), 1, "{sc}");
  assert_eq!(bare[0]["rule"], "bare");
  assert_eq!(bare[0]["line"], 4);
  assert_eq!(bare[0]["column"], 5);
  assert_eq!(bare[0]["text"], "target()");
  assert!(sc["callSiteFiles"].as_u64().is_some(), "{sc}");

  // Paging replays the remembered set: two definitions, one per page, a cursor between.
  let first = request(
    &mut server,
    122,
    "tools/call",
    json!({"name": "structural_search", "arguments": {"pattern": "pub fn $F() -> i32 { $$$ }", "lang": "rust", "limit": 1}}),
  );
  let sc1 = &first["result"]["structuredContent"];
  assert_eq!(sc1["total"], 2, "{sc1}");
  assert_eq!(sc1["records"].as_array().unwrap().len(), 1);
  let cursor = sc1["nextCursor"].as_str().expect("a second page").to_string();
  let second = request(
    &mut server,
    123,
    "tools/call",
    json!({"name": "structural_search", "arguments": {"pattern": "pub fn $F() -> i32 { $$$ }", "lang": "rust", "limit": 1, "cursor": cursor}}),
  );
  let sc2 = &second["result"]["structuredContent"];
  assert_eq!(sc2["total"], 2, "{sc2}");
  let (p1, p2) = (sc1["records"][0]["path"].as_str().unwrap(), sc2["records"][0]["path"].as_str().unwrap());
  assert_ne!(p1, p2, "each page holds a different definition: {sc1} {sc2}");
  assert!(sc2["nextCursor"].is_null(), "{sc2}");

  // structural_search: records with path/line/column/kind/text, and the honesty margins.
  let response = request(
    &mut server,
    13,
    "tools/call",
    json!({"name": "structural_search", "arguments": {"pattern": "target()", "lang": "rust"}}),
  );
  let sc = &response["result"]["structuredContent"];
  let records = sc["records"].as_array().expect("structural records");
  assert_eq!(records.len(), 1, "{sc}");
  assert!(records[0]["path"].as_str().unwrap().ends_with("a.rs"));
  assert_eq!(records[0]["line"], 4);
  assert_eq!(records[0]["kind"], "call_expression");
  assert_eq!(records[0]["text"], "target()");
  assert_eq!(sc["candidateFiles"], 2);
  assert_eq!(sc["scannedFiles"].as_u64().unwrap() + sc["prefilteredFiles"].as_u64().unwrap() + sc["prunedFiles"].as_u64().unwrap(), 2);
  assert!(sc["chunkParsedFiles"].as_u64().is_some(), "{sc}");
  let text = response["result"]["content"][0]["text"].as_str().unwrap();
  assert!(text.contains("a.rs:4:5  target()"), "{text}");

  // text_search: grep-shaped records over the indexed files, with the enclosing symbol.
  let response = request(
    &mut server,
    14,
    "tools/call",
    json!({"name": "text_search", "arguments": {"pattern": r"target\(\)"}}),
  );
  let sc = &response["result"]["structuredContent"];
  let records = sc["records"].as_array().expect("text records");
  // a.rs:4 (the call) and b.rs:1 (the definition `pub fn target() -> i32`), path order.
  assert_eq!(records.len(), 2, "{sc}");
  assert!(records[0]["path"].as_str().unwrap().ends_with("a.rs"));
  assert_eq!(records[0]["line"], 4);
  assert_eq!(records[0]["column"], 5);
  assert_eq!(records[0]["text"], "    target()");
  assert_eq!(records[0]["symbol"], "caller");
  assert!(records[1]["path"].as_str().unwrap().ends_with("b.rs"));
  assert_eq!(records[1]["symbol"], "target");
  assert_eq!(sc["totalMatches"], 2);
  assert_eq!(sc["matchedFiles"], 2);
  assert!(sc["index"] == "trigram" || sc["index"] == "full-scan", "{sc}");
  // No three-byte literal: an honest full scan, same answer.
  let response = request(
    &mut server,
    15,
    "tools/call",
    json!({"name": "text_search", "arguments": {"pattern": "t.rg.t"}}),
  );
  let sc = &response["result"]["structuredContent"];
  assert_eq!(sc["index"], "full-scan", "{sc}");
  assert!(sc["indexReason"].as_str().is_some());
  // `use b::target;`, the call, and the definition — one record per line.
  assert_eq!(sc["totalMatches"], 3, "{sc}");

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

  let response = request(
    &mut server,
    2,
    "tools/call",
    json!({"name": "node", "arguments": {"name": "target"}}),
  );
  assert_eq!(response["result"]["isError"], false);
  let id = response["result"]["structuredContent"]["records"][0]["id"].as_u64().unwrap();

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
  let response = request(
    &mut server,
    4,
    "tools/call",
    json!({"name": "node", "arguments": {"name": "target"}}),
  );
  let id = response["result"]["structuredContent"]["records"][0]["id"].as_u64().unwrap();
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

  // `node` returns compact typed records by default (dense id, kind, path).
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
  // Paths are relative to one `base`; bulky node metadata is omitted by default.
  assert!(data["base"].as_str().is_some_and(|b| b.starts_with('/') && b.ends_with('/')));
  assert!(!record["path"].as_str().unwrap().starts_with('/'));
  assert!(record.get("signature").is_none() && record.get("external_id").is_none());

  // `format` shapes structuredContent, not just the text — the client may feed the
  // model the structured half.
  let response = request(
    &mut server,
    20,
    "tools/call",
    json!({"name": "node", "arguments": {"name": "target", "format": "lean"}}),
  );
  let lean = &response["result"]["structuredContent"]["records"][0];
  assert_eq!(lean["name"], "target");
  // Callers rows carry the call site, so "who calls X" needs no snippet follow-up.
  let response = request(
    &mut server,
    22,
    "tools/call",
    json!({"name": "graph", "arguments": {"relation": "callers", "name": "target", "format": "lean"}}),
  );
  let caller = &response["result"]["structuredContent"]["records"][0];
  assert_eq!(caller["name"], "caller");
  assert_eq!(caller["site_line"], 4, "{caller}");
  assert_eq!(caller["site"], "target()");
  // Callees rows carry the site inside the caller's own body — "what does X call" is one
  // call too, no `reachable` detour, no snippet.
  let response = request(
    &mut server,
    23,
    "tools/call",
    json!({"name": "graph", "arguments": {"relation": "callees", "name": "caller", "format": "lean"}}),
  );
  let callee = &response["result"]["structuredContent"]["records"][0];
  assert_eq!(callee["name"], "target", "{callee}");
  assert!(callee["path"].as_str().unwrap().ends_with("b.rs"));
  assert_eq!(callee["site_line"], 4, "{callee}");
  assert_eq!(callee["site"], "target()");
  assert_eq!(response["result"]["structuredContent"]["total"], 1);
  assert_eq!(lean["kind"], "Function");
  assert!(lean.get("signature").is_none() && lean.get("span").is_none() && lean.get("external_id").is_none());
  let response = request(
    &mut server,
    21,
    "tools/call",
    json!({"name": "node", "arguments": {"name": "target", "format": "ids"}}),
  );
  let ids = &response["result"]["structuredContent"]["records"][0];
  assert!(ids["id"].is_u64() && ids["external_id"].is_string());
  assert_eq!(ids.as_object().unwrap().len(), 2);
  assert!(record["id"].as_u64().is_some());
  assert!(record.get("external_id").is_none());
  assert!(record.get("span").is_none());

  // `callers` records carry the edge grade; `reachable` steps carry relation + via.
  let response = request(
    &mut server,
    3,
    "tools/call",
    json!({"name": "graph", "arguments": {"relation": "callers", "name": "target"}}),
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

  // Unknown tool → protocol error (-32602), the tools page's rule; execution failures
  // below stay in-band.
  let response = request(&mut server, 2, "tools/call", json!({"name": "explode", "arguments": {}}));
  assert_eq!(response["error"]["code"], -32602);
  assert!(response["error"]["message"].as_str().unwrap().contains("Unknown tool: explode"));
  let response = request(&mut server, 21, "tools/call", json!({"arguments": {}}));
  assert_eq!(response["error"]["code"], -32602);
  let response = request(&mut server, 22, "tools/call", json!({"name": "graph", "arguments": 5}));
  assert_eq!(response["error"]["code"], -32602);

  // Framing: batches and null ids are invalid requests; unknown notifications are silent.
  let response: Value = serde_json::from_str(&server.handle_line("[]").unwrap()).unwrap();
  assert_eq!(response["error"]["code"], -32600);
  let response: Value = serde_json::from_str(
    &server
      .handle_line(r#"{"jsonrpc":"2.0","id":null,"method":"ping"}"#)
      .unwrap(),
  )
  .unwrap();
  assert_eq!(response["error"]["code"], -32600);
  assert!(server
    .handle_line(r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":1,"reason":"test"}}"#)
    .is_none());
  let response = request(&mut server, 23, "tools/list", json!({"cursor": "o:5"}));
  assert_eq!(response["error"]["code"], -32602);

  // Query before any index exists → helpful in-band error.
  let (text, is_err) = call_tool(&mut server, 3, "graph", json!({"relation": "callers", "name": "x"}));
  assert!(is_err);
  assert!(text.contains("'index' tool"), "{text}");

  // `graph` validates its relation in-band (an execution error the model can correct).
  let (text, is_err) = call_tool(&mut server, 30, "graph", json!({"name": "x"}));
  assert!(is_err);
  assert!(text.contains("relation"), "{text}");
  let (text, is_err) = call_tool(&mut server, 31, "graph", json!({"relation": "sideways", "name": "x"}));
  assert!(is_err);
  assert!(text.contains("unknown relation"), "{text}");

  // Missing / invalid arguments.
  let (text, is_err) = call_tool(&mut server, 4, "graph", json!({"relation": "callers", }));
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

#[test]
fn query_tool_answers_text_and_ir_and_refuses_typed() {
  let (src, idx) = temp_tree("query");
  let mut server = Server::new(idx);
  let (text, is_err) = call_tool(
    &mut server,
    1,
    "index",
    json!({"src": src.to_string_lossy()}),
  );
  assert!(!is_err, "{text}");

  let response = request(
    &mut server,
    2,
    "tools/call",
    json!({"name": "query", "arguments": {"text":
      "MATCH (f)-[:calls]->(g {name: \"target\"}) RETURN f.name, f.path"}}),
  );
  let data = &response["result"]["structuredContent"];
  assert_eq!(data["columns"][0], "f.name");
  assert_eq!(data["rows"][0][0], "caller");
  assert_eq!(data["total_rows"], 1);

  // The IR document form executes the same plan.
  let ir = json!({
    "pattern": {
      "left": {"var": "f"},
      "segments": [{
        "rel": {"types": ["calls"], "direction": "out"},
        "node": {"var": "g", "props": [["name", "target"]]}
      }]
    },
    "returns": {"items": [{"expr": {"prop": {"var": "f", "prop": "name"}}}]}
  });
  let response = request(
    &mut server,
    3,
    "tools/call",
    json!({"name": "query", "arguments": {"ir": ir}}),
  );
  assert_eq!(
    response["result"]["structuredContent"]["rows"][0][0],
    "caller"
  );

  // Typed refusals: unknown relation names the problem; ceilings name the ceiling.
  let (text, is_err) = call_tool(
    &mut server,
    4,
    "query",
    json!({"text": "MATCH (f)-[:frobnicates]->(g) RETURN f.name"}),
  );
  assert!(is_err && text.contains("unknown relation"), "{text}");
  let (text, is_err) = call_tool(
    &mut server,
    5,
    "query",
    json!({"text": "MATCH (f)-[:calls*1..99]->(g) RETURN f.name"}),
  );
  assert!(is_err && text.contains("ceiling"), "{text}");
}

/// A default-layout tree (`<src>/.vorpal/index`, so relative scope entries resolve against
/// the source root) with callers in two directories and a two-hop chain:
/// `outer → caller2 → target`, `caller → target`.
fn scoped_tree(tag: &str) -> (PathBuf, PathBuf) {
  let base = std::env::temp_dir().join(format!("vorpal-mcp-{tag}-{}", std::process::id()));
  let src = base.join("src");
  let idx = src.join(".vorpal").join("index");
  let _ = fs::remove_dir_all(&base);
  fs::create_dir_all(src.join("sub")).unwrap();
  fs::write(src.join("b.rs"), "pub fn target() -> i32 {\n    0\n}\n").unwrap();
  fs::write(
    src.join("a.rs"),
    "use b::target;\n\npub fn caller() -> i32 {\n    target()\n}\n",
  )
  .unwrap();
  fs::write(
    src.join("sub").join("c.rs"),
    "use b::target;\n\npub fn caller2() -> i32 {\n    target()\n}\n\npub fn outer() -> i32 {\n    caller2()\n}\n",
  )
  .unwrap();
  (src, idx)
}

/// A record's absolute path on the wire: `base` (the page's common directory) + `path`.
fn abs_path(data: &Value, row: usize) -> String {
  format!(
    "{}{}",
    data["base"].as_str().unwrap_or(""),
    data["records"][row]["path"].as_str().unwrap_or("")
  )
}

fn structured(server: &mut Server, id: u64, tool: &str, args: Value) -> Value {
  let response = request(server, id, "tools/call", json!({"name": tool, "arguments": args}));
  let result = &response["result"];
  assert_eq!(result["isError"], false, "{}", result["content"][0]["text"]);
  result["structuredContent"].clone()
}

#[test]
fn scope_bounds_answers_and_counts_the_rest() {
  let (src, idx) = scoped_tree("scope");
  let mut server = Server::new(idx);
  let (text, is_err) = call_tool(&mut server, 1, "index", json!({"src": src.to_str().unwrap()}));
  assert!(!is_err, "{text}");

  // Unscoped: both callers, nearest file first (a.rs shares target's directory).
  let all = structured(&mut server, 2, "graph", json!({"relation": "callers", "name": "target"}));
  assert_eq!(all["total"], 2, "{all}");
  assert!(abs_path(&all, 0).ends_with("/a.rs"), "{all}");
  assert!(abs_path(&all, 1).ends_with("/sub/c.rs"), "{all}");
  assert!(all.get("scope").is_none() && all.get("outsideScope").is_none(), "{all}");
  assert_eq!(all["radius"]["anchor"], "target", "{all}");
  assert_eq!(all["radius"]["files"], 2, "{all}");
  assert_eq!(all["radius"]["dirs"], 2, "{all}");

  // A relative `within` resolves against the source root; the rest is counted, not listed,
  // and the count plus the rows equals the unscoped answer.
  let sub = structured(
    &mut server,
    3,
    "graph",
    json!({"relation": "callers", "name": "target", "within": ["sub"]}),
  );
  assert_eq!(sub["total"], 1, "{sub}");
  assert_eq!(sub["outsideScope"], 1, "{sub}");
  assert!(abs_path(&sub, 0).ends_with("/sub/c.rs"), "{sub}");
  assert_eq!(sub["scope"], json!({"within": ["sub"], "source": "call"}), "{sub}");
  let response = request(
    &mut server,
    4,
    "tools/call",
    json!({"name": "graph", "arguments": {"relation": "callers", "name": "target", "within": ["sub"]}}),
  );
  let text = response["result"]["content"][0]["text"].as_str().unwrap();
  assert!(text.contains("outside scope: 1 rows not listed (within: sub)"), "{text}");

  // An entry that names nothing is an error, never a silent empty answer; a single
  // string works like a list; matching is segment-exact (`su` exists here as a directory
  // and is not a prefix of `sub/`).
  let response = request(
    &mut server,
    5,
    "tools/call",
    json!({"name": "graph", "arguments": {"relation": "callers", "name": "target", "within": "nope"}}),
  );
  assert_eq!(response["result"]["isError"], true, "{response}");
  assert!(response["result"]["content"][0]["text"].as_str().unwrap().contains("names nothing under"));
  fs::create_dir_all(src.join("su")).unwrap();
  let none = structured(
    &mut server,
    5,
    "graph",
    json!({"relation": "callers", "name": "target", "within": "su"}),
  );
  assert_eq!(none["total"], 0, "{none}");
  assert_eq!(none["outsideScope"], 2, "{none}");

  // Session scope: set once, applied to every scoped call that passes no `within`; an
  // explicit empty list is the unscoped view; `clear` removes it.
  let set = structured(&mut server, 6, "scope", json!({"within": ["sub/"]}));
  assert_eq!(set["outcome"], "scoped", "{set}");
  let session = structured(&mut server, 7, "graph", json!({"relation": "callers", "name": "target"}));
  assert_eq!(session["total"], 1, "{session}");
  assert_eq!(session["scope"]["source"], "session", "{session}");
  assert_eq!(session["outsideScope"], 1, "{session}");
  let explicit = structured(
    &mut server,
    8,
    "graph",
    json!({"relation": "callers", "name": "target", "within": []}),
  );
  assert_eq!(explicit["total"], 2, "{explicit}");
  assert!(explicit.get("scope").is_none(), "{explicit}");
  let hits = structured(&mut server, 9, "search", json!({"query": "caller", "k": 5}));
  assert!(hits["total"].as_u64().unwrap() >= 1, "{hits}");
  for row in 0..hits["records"].as_array().unwrap().len() {
    assert!(abs_path(&hits, row).contains("/sub/"), "search honours the session scope: {hits}");
  }
  assert_eq!(hits["scope"]["source"], "session", "{hits}");
  let cleared = structured(&mut server, 10, "scope", json!({"clear": true}));
  assert_eq!(cleared["outcome"], "unscoped", "{cleared}");
  let again = structured(&mut server, 11, "graph", json!({"relation": "callers", "name": "target"}));
  assert_eq!(again["total"], 2, "{again}");

  // Rings: one hop by default with the next ring counted; 0 walks the whole closure; the
  // scope is a view over the rows, so the two-hop node is still reached through sub/.
  let ring = structured(&mut server, 12, "reachable", json!({"name": "target", "direction": "in"}));
  assert_eq!(ring["total"], 2, "{ring}");
  assert_eq!(ring["frontier"], 1, "{ring}");
  assert_eq!(ring["maxDepth"], 1, "{ring}");
  assert!(ring["records"].as_array().unwrap().iter().all(|r| r["depth"] == 1), "{ring}");
  let response = request(
    &mut server,
    13,
    "tools/call",
    json!({"name": "reachable", "arguments": {"name": "target", "direction": "in"}}),
  );
  let text = response["result"]["content"][0]["text"].as_str().unwrap();
  assert!(text.contains("frontier: 1 more at depth 2"), "{text}");
  let whole = structured(
    &mut server,
    14,
    "reachable",
    json!({"name": "target", "direction": "in", "max_depth": 0}),
  );
  assert_eq!(whole["total"], 3, "{whole}");
  assert_eq!(whole["frontier"], 0, "{whole}");
  assert_eq!(whole["maxDepth"], 0, "{whole}");
  let scoped_ring = structured(
    &mut server,
    15,
    "reachable",
    json!({"name": "target", "direction": "in", "within": ["sub"]}),
  );
  assert_eq!(scoped_ring["total"], 1, "{scoped_ring}");
  assert_eq!(scoped_ring["outsideScope"], 1, "{scoped_ring}");
  assert_eq!(scoped_ring["frontier"], 1, "{scoped_ring}");

  // text_search and code_search take the same scope.
  let lines = structured(
    &mut server,
    16,
    "text_search",
    json!({"pattern": "target\\(\\)", "within": ["sub"]}),
  );
  assert_eq!(lines["total"], 1, "{lines}");
  assert!(abs_path(&lines, 0).ends_with("/sub/c.rs"), "{lines}");
  let code = structured(
    &mut server,
    17,
    "code_search",
    json!({"pattern": "target()", "lang": "rust", "within": ["sub"]}),
  );
  assert_eq!(code["total"], 1, "{code}");
  assert!(abs_path(&code, 0).ends_with("/sub/c.rs"), "{code}");

  let _ = fs::remove_dir_all(src.parent().unwrap());
}

#[test]
fn scope_refuses_relative_entries_without_a_source_root_and_local_profile_drops_closures() {
  use vorpal_mcp::Profile;
  let (src, idx) = temp_tree("scope-root");
  let mut server = Server::new(idx.clone());
  let (text, is_err) = call_tool(&mut server, 1, "index", json!({"src": src.to_str().unwrap()}));
  assert!(!is_err, "{text}");
  // A custom index location has no source root: a relative entry is an error naming it,
  // never a silent empty answer; an absolute entry works.
  let response = request(
    &mut server,
    2,
    "tools/call",
    json!({"name": "scope", "arguments": {"within": ["src"]}}),
  );
  assert_eq!(response["result"]["isError"], true, "{response}");
  assert_eq!(response["result"]["structuredContent"]["code"], "bad-argument");
  assert!(response["result"]["content"][0]["text"].as_str().unwrap().contains("no source root"));
  let abs = structured(&mut server, 3, "scope", json!({"within": [src.to_str().unwrap()]}));
  assert_eq!(abs["outcome"], "scoped", "{abs}");
  let callers = structured(&mut server, 4, "graph", json!({"relation": "callers", "name": "target"}));
  assert_eq!(callers["total"], 1, "{callers}");
  assert_eq!(callers["outsideScope"], 0, "{callers}");

  let local = Server::with_profile(idx, Profile::Local);
  let mut local = local;
  let response = request(&mut local, 5, "tools/list", Value::Null);
  let names: Vec<&str> = response["result"]["tools"]
    .as_array()
    .unwrap()
    .iter()
    .map(|t| t["name"].as_str().unwrap())
    .collect();
  assert!(names.contains(&"graph") && names.contains(&"structural_search") && names.contains(&"scope"), "{names:?}");
  assert!(!names.contains(&"reachable") && !names.contains(&"impact"), "{names:?}");
  let _ = fs::remove_dir_all(src.parent().unwrap());
}

/// The object form of a scope: exclusions, path classes, kind, anchor-relative entries,
/// deferred session binding, and files changed since a git ref.
#[test]
fn scope_object_facets_anchors_and_changed_files() {
  let (src, idx) = scoped_tree("facets");
  fs::create_dir_all(src.join("tests")).unwrap();
  fs::write(
    src.join("tests").join("t.rs"),
    "use b::target;\n\npub fn test_caller() -> i32 {\n    target()\n}\n",
  )
  .unwrap();
  fs::write(src.join("sub").join("Cargo.toml"), "[package]\nname = \"sub\"\nversion = \"0.0.0\"\n").unwrap();
  let git = |args: &[&str]| {
    let out = std::process::Command::new("git")
      .arg("-C")
      .arg(&src)
      .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
      .args(args)
      .output()
      .expect("git runs");
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
  };
  git(&["init", "-q"]);
  git(&["add", "-A"]);
  git(&["commit", "-q", "-m", "init"]);

  let mut server = Server::new(idx.clone());
  let (text, is_err) = call_tool(&mut server, 1, "index", json!({"src": src.to_str().unwrap()}));
  assert!(!is_err, "{text}");
  let callers = |server: &mut Server, id: u64, name: &str, scope: Value| {
    structured(server, id, "graph", json!({"relation": "callers", "name": name, "scope": scope}))
  };

  let all = callers(&mut server, 2, "target", json!({}));
  assert_eq!(all["total"], 3, "{all}");

  let except = callers(&mut server, 3, "target", json!({"except": ["sub"]}));
  assert_eq!(except["total"], 2, "{except}");
  assert_eq!(except["outsideScope"], 1, "{except}");
  assert_eq!(except["scope"]["except"], json!(["sub"]), "{except}");
  assert!(except["scope"].get("within").is_some_and(|w| w.as_array().is_some_and(Vec::is_empty)), "{except}");

  let source_only = callers(&mut server, 4, "target", json!({"classes": ["source"]}));
  assert_eq!(source_only["total"], 2, "{source_only}");
  assert_eq!(source_only["outsideScope"], 1, "{source_only}");
  for row in 0..2 {
    assert!(!abs_path(&source_only, row).contains("/tests/"), "{source_only}");
  }

  let functions = callers(&mut server, 5, "target", json!({"kind": "Function"}));
  assert_eq!(functions["total"], 3, "{functions}");
  let structs = callers(&mut server, 6, "target", json!({"kind": "Struct"}));
  assert_eq!(structs["total"], 0, "{structs}");
  assert_eq!(structs["outsideScope"], 3, "{structs}");

  // Anchor-relative: `@dir` is the symbol's own directory, `@package` its nearest manifest.
  let at_dir = callers(&mut server, 7, "target", json!({"within": ["@dir"]}));
  assert_eq!(at_dir["total"], 3, "{at_dir}");
  assert_eq!(at_dir["scope"]["within"], json!(["@dir"]), "{at_dir}");
  let sub_dir = callers(&mut server, 8, "caller2", json!({"within": ["@dir"]}));
  assert_eq!(sub_dir["total"], 1, "{sub_dir}");
  assert!(abs_path(&sub_dir, 0).ends_with("/sub/c.rs"), "{sub_dir}");
  let package = callers(&mut server, 9, "caller2", json!({"within": ["@package"]}));
  assert_eq!(package["total"], 1, "{package}");
  let response = request(
    &mut server,
    10,
    "tools/call",
    json!({"name": "graph", "arguments": {"relation": "callers", "name": "target", "within": ["@package"]}}),
  );
  assert_eq!(response["result"]["isError"], true, "{response}");
  assert!(response["result"]["content"][0]["text"].as_str().unwrap().contains("no package manifest above"));

  // Unknown scope fields are errors, never a silently unscoped answer.
  let response = request(
    &mut server,
    11,
    "tools/call",
    json!({"name": "graph", "arguments": {"relation": "callers", "name": "target", "scope": {"withn": ["sub"]}}}),
  );
  assert_eq!(response["result"]["isError"], true, "{response}");
  assert!(response["result"]["content"][0]["text"].as_str().unwrap().contains("unknown scope field 'withn'"));

  // Changed files: modify one caller's file, then scope to the worktree's changes.
  fs::write(
    src.join("sub").join("c.rs"),
    "use b::target;\n\npub fn caller2() -> i32 {\n    target()\n}\n\npub fn outer() -> i32 {\n    caller2()\n}\n// touched\n",
  )
  .unwrap();
  let changed = callers(&mut server, 12, "target", json!({"changed_since": "worktree"}));
  assert_eq!(changed["total"], 1, "{changed}");
  assert_eq!(changed["outsideScope"], 2, "{changed}");
  assert_eq!(changed["scope"]["changedFiles"], 1, "{changed}");
  assert!(abs_path(&changed, 0).ends_with("/sub/c.rs"), "{changed}");
  let since_head = callers(&mut server, 13, "target", json!({"changed_since": "HEAD"}));
  assert_eq!(since_head["total"], 1, "{since_head}");

  // A session scope with a deferred `@dir` binds per symbol: a search before any symbol
  // anchor exists is refused; after a graph call it answers inside that symbol's dir.
  let mut fresh = Server::new(idx);
  let set = structured(&mut fresh, 14, "scope", json!({"within": ["@dir"], "classes": ["source"]}));
  assert_eq!(set["outcome"], "scoped", "{set}");
  assert_eq!(set["deferred"], json!(["@dir"]), "{set}");
  let response = request(&mut fresh, 15, "tools/call", json!({"name": "search", "arguments": {"query": "caller", "k": 5}}));
  assert_eq!(response["result"]["isError"], true, "{response}");
  assert!(response["result"]["content"][0]["text"].as_str().unwrap().contains("bind to a symbol"), "{response}");
  let bound = structured(&mut fresh, 16, "graph", json!({"relation": "callers", "name": "caller2"}));
  assert_eq!(bound["total"], 1, "{bound}");
  assert_eq!(bound["scope"]["source"], "session", "{bound}");
  let hits = structured(&mut fresh, 17, "search", json!({"query": "caller", "k": 5}));
  assert!(hits["total"].as_u64().unwrap() >= 1, "{hits}");
  for row in 0..hits["records"].as_array().unwrap().len() {
    assert!(abs_path(&hits, row).contains("/sub/"), "{hits}");
  }
  assert_eq!(hits["scope"]["within"], json!(["@dir"]), "{hits}");
  let _ = fs::remove_dir_all(src.parent().unwrap());
}

/// A roots-capable client's workspace roots become the session's default scope when they
/// lie strictly inside the indexed tree; a root at the tree means no scope; a scope a
/// person sets wins over roots.
#[test]
fn client_roots_become_the_default_scope() {
  let (src, idx) = scoped_tree("roots");
  let mut server = Server::new(idx.clone());
  let (text, is_err) = call_tool(&mut server, 1, "index", json!({"src": src.to_str().unwrap()}));
  assert!(!is_err, "{text}");

  // initialize with the roots capability, then the initialized notification: the server
  // answers the notification with its own roots/list request.
  let response = request(
    &mut server,
    2,
    "initialize",
    json!({"protocolVersion": "2025-06-18", "capabilities": {"roots": {"listChanged": true}}, "clientInfo": {"name": "t"}}),
  );
  assert_eq!(response["jsonrpc"], "2.0");
  let line = server
    .handle_line(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}).to_string())
    .expect("a roots/list request goes out");
  let req: Value = serde_json::from_str(&line).unwrap();
  assert_eq!(req["method"], "roots/list", "{req}");
  let req_id = req["id"].clone();

  // The client's answer: one root, the `sub` directory inside the tree.
  let sub_uri = format!("file://{}", src.join("sub").display());
  let none = server.handle_line(
    &json!({"jsonrpc": "2.0", "id": req_id, "result": {"roots": [{"uri": sub_uri, "name": "sub"}]}}).to_string(),
  );
  assert!(none.is_none(), "a response is never answered");
  let callers = structured(&mut server, 3, "graph", json!({"relation": "callers", "name": "target"}));
  assert_eq!(callers["total"], 1, "{callers}");
  assert_eq!(callers["outsideScope"], 1, "{callers}");
  assert_eq!(callers["scope"]["source"], "roots", "{callers}");
  assert_eq!(callers["scope"]["within"], json!(["sub"]), "{callers}");

  // A root at the tree itself: the whole tree is in play, the roots scope is dropped.
  let line = server
    .handle_line(&json!({"jsonrpc": "2.0", "method": "notifications/roots/list_changed"}).to_string())
    .expect("re-asks for roots");
  let req: Value = serde_json::from_str(&line).unwrap();
  let root_uri = format!("file://{}", src.display());
  server.handle_line(
    &json!({"jsonrpc": "2.0", "id": req["id"], "result": {"roots": [{"uri": root_uri}]}}).to_string(),
  );
  let all = structured(&mut server, 4, "graph", json!({"relation": "callers", "name": "target"}));
  assert_eq!(all["total"], 2, "{all}");
  assert!(all.get("scope").is_none(), "{all}");

  // A person's scope wins: set one, then roots change again — the person's scope stays.
  let set = structured(&mut server, 5, "scope", json!({"within": ["sub"]}));
  assert_eq!(set["scope"]["source"], "session", "{set}");
  let line = server
    .handle_line(&json!({"jsonrpc": "2.0", "method": "notifications/roots/list_changed"}).to_string())
    .expect("re-asks for roots");
  let req: Value = serde_json::from_str(&line).unwrap();
  server.handle_line(
    &json!({"jsonrpc": "2.0", "id": req["id"], "result": {"roots": [{"uri": format!("file://{}", src.display())}]}}).to_string(),
  );
  let still = structured(&mut server, 6, "graph", json!({"relation": "callers", "name": "target"}));
  assert_eq!(still["scope"]["source"], "session", "{still}");
  assert_eq!(still["total"], 1, "{still}");
  let _ = fs::remove_dir_all(src.parent().unwrap());
}
