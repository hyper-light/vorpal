//! A span-only edit under the sync (no-overlay) path must move the served rows.
//!
//! Regression for the live-span coherence failure of 2026-09-05: the respan compose
//! rewrote node spans but reported `graph_reused`, so a daemon serving without an overlay
//! kept its loaded rows, repointed its artifact pin to the new generation, and `snippet`
//! sliced the OLD span out of the NEW file — and called it verified, because the pack's
//! digest matched the current bytes. The overlay is disabled here on purpose: with it,
//! the edit is absorbed by the retained tier and the compose lane never runs.

use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use vorpal_mcp::Server;

fn call(server: &mut Server, id: u64, tool: &str, args: Value) -> Value {
  let line = json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": tool, "arguments": args}}).to_string();
  let response = server.handle_line(&line).expect("tool call gets a response");
  serde_json::from_str(&response).expect("valid JSON")
}

const BODY: &str = "def target_fn():\n    x = alpha()\n    return x + 41\n";

#[test]
fn span_only_edit_moves_the_served_rows_without_an_overlay() {
  // Own test binary: the switch is process-global.
  unsafe { std::env::set_var("VORPAL_NO_LIVE_OVERLAY", "1") };
  let base = std::env::temp_dir().join(format!("vorpal-mcp-respan-{}", std::process::id()));
  let src = base.join("repo");
  let index: PathBuf = src.join(".vorpal").join("index");
  let _ = fs::remove_dir_all(&base);
  fs::create_dir_all(&src).unwrap();
  let original = format!("def alpha():\n    return 1\n\n{BODY}");
  fs::write(src.join("a.py"), &original).unwrap();
  vorpal_index::build_index(&src, &index).expect("initial index");

  let mut server = Server::new(index.clone());
  let before = call(&mut server, 1, "snippet", json!({"name": "target_fn"}));
  let text = before["result"]["content"][0]["text"].as_str().unwrap();
  assert!(text.contains("(verified)"), "{text}");
  assert!(text.ends_with(BODY), "{text}");
  let span_before = call(&mut server, 2, "node", json!({"name": "target_fn", "format": "toon"}));
  let start_before = span_before["result"]["structuredContent"]["records"][0]["span"][0]
    .as_u64()
    .expect("span start");

  // Span-only edit: a comment line ABOVE every definition shifts each span by its length.
  let comment = "# a comment line shifts every span below it\n";
  fs::write(src.join("a.py"), format!("{comment}{original}")).unwrap();
  let expected_start = start_before + comment.len() as u64;

  // The daemon must adopt the respanned generation: rows move, not just the artifact pin.
  let deadline = Instant::now() + Duration::from_secs(15);
  let mut id = 10;
  loop {
    id += 1;
    let response = call(&mut server, id, "node", json!({"name": "target_fn", "format": "toon"}));
    let start = response["result"]["structuredContent"]["records"][0]["span"][0].as_u64();
    if start == Some(expected_start) {
      break;
    }
    assert!(
      Instant::now() < deadline,
      "daemon never served the respanned rows: span start {start:?}, expected {expected_start} \
       (the respan compose reported the graph as reused and the daemon kept its rows)"
    );
    std::thread::sleep(Duration::from_millis(50));
  }
  let after = call(&mut server, id + 1, "snippet", json!({"name": "target_fn"}));
  let text = after["result"]["content"][0]["text"].as_str().unwrap();
  assert!(text.contains("(verified)"), "{text}");
  assert!(
    text.ends_with(BODY),
    "snippet body must be the function, not the old span sliced from the new file: {text}"
  );
  let _ = fs::remove_dir_all(&base);
}
