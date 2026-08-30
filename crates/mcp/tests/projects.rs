//! D4: multi-project routing, the human-only enrollment boundary, and the un-enrolled-source
//! refusal — exercised through the router exactly as a client would.

use std::fs;

use serde_json::{Value, json};
use vorpal_mcp::registry;

fn call(server: &mut vorpal_mcp::MultiServerForTest, line: &str) -> Value {
  let response = server.handle_line(line).expect("request gets a response");
  serde_json::from_str(&response).expect("valid json")
}

fn tool_call(server: &mut vorpal_mcp::MultiServerForTest, name: &str, args: Value) -> (String, bool) {
  let line = json!({"jsonrpc": "2.0", "id": 9, "method": "tools/call",
                    "params": {"name": name, "arguments": args}})
  .to_string();
  let value = call(server, &line);
  let result = &value["result"];
  let text = result["content"][0]["text"].as_str().unwrap_or("").to_string();
  let is_err = result["isError"].as_bool().unwrap_or(false);
  (text, is_err)
}

#[test]
fn routes_by_project_and_refuses_unenrolled_sources() {
  let base = std::env::temp_dir().join(format!("vorpal-mcp-projects-{}", std::process::id()));
  let _ = fs::remove_dir_all(&base);
  let alpha = base.join("alpha");
  let beta = base.join("beta");
  fs::create_dir_all(alpha.join("src")).unwrap();
  fs::create_dir_all(beta.join("src")).unwrap();
  fs::write(alpha.join("src/a.rs"), "pub fn alpha_only() {}\n").unwrap();
  fs::write(beta.join("src/b.rs"), "pub fn beta_only() {}\n").unwrap();
  // SAFETY: test-scoped registry file.
  unsafe { std::env::set_var("VORPAL_PROJECTS_FILE", base.join("projects.yml")) };
  registry::enroll(&alpha, Some("alpha"), None).unwrap();
  registry::enroll(&beta, Some("beta"), None).unwrap();

  let mut server = vorpal_mcp::multi_server_for_test();

  // tools/list carries the injected `project` selector and the listing tool.
  let listing = call(
    &mut server,
    r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}"#,
  );
  let tools = listing["result"]["tools"].as_array().unwrap();
  assert!(tools.iter().any(|t| t["name"] == "list_projects"));
  let node_tool = tools.iter().find(|t| t["name"] == "node").expect("node tool");
  assert!(
    node_tool["inputSchema"]["properties"]["project"].is_object(),
    "every tool gains the project selector"
  );

  // list_projects names both, neither indexed yet.
  let (text, is_err) = tool_call(&mut server, "list_projects", json!({}));
  assert!(!is_err);
  assert!(text.contains("alpha") && text.contains("beta"), "{text}");
  assert!(text.contains("no index yet"), "{text}");

  // Ambiguity is an error that teaches, not a silent default.
  let (text, is_err) = tool_call(&mut server, "schema", json!({}));
  assert!(is_err);
  assert!(text.contains("pass \"project\""), "{text}");

  // Index each project through its route; then queries answer per project.
  let (text, is_err) = tool_call(&mut server, "index", json!({"project": "alpha"}));
  assert!(!is_err, "{text}");
  let (text, is_err) = tool_call(&mut server, "index", json!({"project": "beta"}));
  assert!(!is_err, "{text}");
  let (text, is_err) = tool_call(&mut server, "node", json!({"project": "alpha", "name": "alpha_only"}));
  assert!(!is_err && text.contains("alpha_only"), "{text}");
  let (text, is_err) = tool_call(&mut server, "node", json!({"project": "beta", "name": "alpha_only"}));
  // Honest absence on the other project: not an error, just no such node there.
  assert!(text.contains("no ") || is_err || !text.contains("alpha_only"), "{text}");

  // The un-enrolled-source refusal: an explicit src outside the enrolled root is refused
  // with the human-gate explanation.
  let outside = base.join("outside");
  fs::create_dir_all(&outside).unwrap();
  let (text, is_err) = tool_call(
    &mut server,
    "index",
    json!({"project": "alpha", "src": outside.to_string_lossy()}),
  );
  assert!(is_err);
  assert!(text.contains("not the enrolled source"), "{text}");
  assert!(text.contains("vorpal mcp allow"), "{text}");

  // Unknown project: names the enrolled set and the human-only rule.
  let (text, is_err) = tool_call(&mut server, "schema", json!({"project": "gamma"}));
  assert!(is_err);
  assert!(text.contains("human-only"), "{text}");

  unsafe { std::env::remove_var("VORPAL_PROJECTS_FILE") };
  let _ = fs::remove_dir_all(&base);
}
