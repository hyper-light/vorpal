//! Live-watch freshness: a daemon on a default-layout index answers queries about changes it
//! was never explicitly told about, via the real OS watcher (FSEvents/inotify).

use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use vorpal_mcp::Server;

fn call_tool(server: &mut Server, id: u64, tool: &str, args: Value) -> (String, bool) {
  let line = json!({
    "jsonrpc": "2.0", "id": id, "method": "tools/call",
    "params": {"name": tool, "arguments": args}
  })
  .to_string();
  let response = server
    .handle_line(&line)
    .expect("tool call gets a response");
  let response: Value = serde_json::from_str(&response).expect("valid JSON");
  let result = &response["result"];
  let text = result["content"][0]["text"].as_str().expect("text content");
  (text.to_owned(), result["isError"].as_bool().unwrap_or(true))
}

#[test]
fn watched_daemon_serves_changes_it_was_never_told_about() {
  let base = std::env::temp_dir().join(format!("vorpal-mcp-watch-{}", std::process::id()));
  let src = base.join("repo");
  let index: PathBuf = src.join(".vorpal").join("index");
  let _ = fs::remove_dir_all(&base);
  fs::create_dir_all(&src).unwrap();
  fs::write(src.join("b.rs"), "pub fn target() -> i32 {\n    0\n}\n").unwrap();
  fs::write(
    src.join("a.rs"),
    "pub fn caller() -> i32 {\n    target()\n}\n",
  )
  .unwrap();
  vorpal_index::build_index(&src, &index).expect("initial index");

  let mut server = Server::new(index);

  // First query self-validates (the flag starts dirty: pre-startup changes have no events).
  let (text, is_error) = call_tool(&mut server, 1, "callers", json!({"name": "target"}));
  assert!(!is_error, "{text}");
  assert!(text.contains("caller"), "{text}");

  // A brand-new file with a brand-new symbol appears — no explicit `index` call ever made.
  fs::write(
    src.join("fresh.rs"),
    "pub fn brand_new_symbol() -> i32 {\n    target()\n}\n",
  )
  .unwrap();

  // The watcher delivers asynchronously; poll until the daemon serves the new symbol.
  let deadline = Instant::now() + Duration::from_secs(10);
  let mut id = 2;
  let found = loop {
    let (text, is_error) = call_tool(&mut server, id, "node", json!({"name": "brand_new_symbol"}));
    id += 1;
    if !is_error && text.contains("fresh.rs") {
      break true;
    }
    if Instant::now() > deadline {
      eprintln!("last response: {text}");
      break false;
    }
    std::thread::sleep(Duration::from_millis(50));
  };
  assert!(found, "watched daemon never picked up the new file");

  // And the new symbol participates in the graph, not just the node list.
  let (text, is_error) = call_tool(&mut server, id, "callers", json!({"name": "target"}));
  assert!(!is_error, "{text}");
  assert!(
    text.contains("brand_new_symbol"),
    "new caller must appear in the refreshed graph: {text}"
  );

  let _ = fs::remove_dir_all(&base);
}

#[test]
fn unwatchable_layout_keeps_explicit_index_semantics() {
  let base = std::env::temp_dir().join(format!("vorpal-mcp-nowatch-{}", std::process::id()));
  let src = base.join("repo");
  let index = base.join("custom-index-location");
  let _ = fs::remove_dir_all(&base);
  fs::create_dir_all(&src).unwrap();
  fs::write(src.join("a.rs"), "pub fn lonely() -> i32 {\n    1\n}\n").unwrap();
  vorpal_index::build_index(&src, &index).expect("initial index");

  let mut server = Server::new(index);
  let (text, is_error) = call_tool(&mut server, 1, "node", json!({"name": "lonely"}));
  assert!(!is_error, "{text}");

  // A new file is NOT picked up automatically (no derivable source root to watch)…
  fs::write(src.join("late.rs"), "pub fn late_symbol() {}\n").unwrap();
  std::thread::sleep(Duration::from_millis(300));
  let (text, _) = call_tool(&mut server, 2, "node", json!({"name": "late_symbol"}));
  assert!(text.contains("no results"), "{text}");

  // …until the explicit `index` tool runs, exactly as before.
  let (text, is_error) = call_tool(
    &mut server,
    3,
    "index",
    json!({"src": src.to_string_lossy()}),
  );
  assert!(!is_error, "{text}");
  let (text, is_error) = call_tool(&mut server, 4, "node", json!({"name": "late_symbol"}));
  assert!(!is_error, "{text}");
  assert!(text.contains("late.rs"), "{text}");

  let _ = fs::remove_dir_all(&base);
}

/// Steady-state daemon latency — run explicitly:
///   cargo test --release -p vorpal-mcp --test watch bench_steady -- --ignored --nocapture
#[test]
#[ignore = "benchmark: run explicitly with --ignored --nocapture"]
fn bench_steady_state_query_latency() {
  let base = std::env::temp_dir().join(format!("vorpal-mcp-bench-{}", std::process::id()));
  let src = base.join("repo");
  let index: PathBuf = src.join(".vorpal").join("index");
  let _ = fs::remove_dir_all(&base);
  fs::create_dir_all(&src).unwrap();
  fs::write(src.join("b.rs"), "pub fn target() -> i32 {\n    0\n}\n").unwrap();
  fs::write(
    src.join("a.rs"),
    "pub fn caller() -> i32 {\n    target()\n}\n",
  )
  .unwrap();
  vorpal_index::build_index(&src, &index).expect("initial index");

  let mut server = Server::new(index);
  let (text, is_error) = call_tool(&mut server, 1, "callers", json!({"name": "target"}));
  assert!(!is_error, "{text}");

  let rounds = 10_000u64;
  let start = Instant::now();
  for i in 0..rounds {
    let (_, is_error) = call_tool(&mut server, 2 + i, "callers", json!({"name": "target"}));
    assert!(!is_error);
  }
  let elapsed = start.elapsed();
  eprintln!(
    "steady-state watched tool call: {:.1} µs/call over {rounds} calls (full JSON-RPC parse + freshness check + graph query + render)",
    elapsed.as_secs_f64() * 1e6 / rounds as f64
  );

  let _ = fs::remove_dir_all(&base);
}
