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
  let (text, is_error) = call_tool(&mut server, 1, "graph", json!({"relation": "callers", "name": "target"}));
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
  let (text, is_error) = call_tool(&mut server, id, "graph", json!({"relation": "callers", "name": "target"}));
  assert!(!is_error, "{text}");
  assert!(
    text.contains("brand_new_symbol"),
    "new caller must appear in the refreshed graph: {text}"
  );

  let _ = fs::remove_dir_all(&base);
}

/// Poll `node <name>` until the daemon serves it from `path` (or the deadline passes).
fn served_from(server: &mut Server, id: &mut u64, name: &str, path: &str, within: Duration) -> bool {
  let deadline = Instant::now() + within;
  loop {
    *id += 1;
    let (text, is_error) = call_tool(server, *id, "node", json!({"name": name}));
    if !is_error && text.contains(path) {
      return true;
    }
    if Instant::now() > deadline {
      eprintln!("last response for {name}: {text}");
      return false;
    }
    std::thread::sleep(Duration::from_millis(50));
  }
}

/// The freshness law: "unchanged" is measured against the SERVED state, never against the
/// generation on disk. A generation committed behind the daemon's back — here a plain
/// `build_index` beside it, standing in for an external `vorpal index` run or the daemon's
/// own background canonicalizer reading a tree that moved after the probe — puts `CURRENT`
/// ahead of the served graph. Before the fix the next probe of the moved file compared its
/// extraction against that newer pack, judged it "unchanged", noted the fresh stamps into
/// the overlay's manifest, and the daemon served the pre-edit graph indefinitely.
#[test]
fn a_generation_committed_behind_the_daemon_never_masks_an_edit() {
  let base = std::env::temp_dir().join(format!("vorpal-mcp-ahead-{}", std::process::id()));
  let src = base.join("repo");
  let index: PathBuf = src.join(".vorpal").join("index");
  let _ = fs::remove_dir_all(&base);
  fs::create_dir_all(&src).unwrap();
  fs::write(src.join("a.py"), "def alpha():\n    return 1\n").unwrap();
  fs::write(src.join("b.py"), "def beta_v1():\n    return alpha()\n").unwrap();
  vorpal_index::build_index(&src, &index).expect("initial index");

  let mut server = Server::new(index.clone());
  let mut id = 0u64;
  assert!(served_from(&mut server, &mut id, "beta_v1", "b.py", Duration::from_secs(10)));
  let before = fs::read_to_string(index.join("CURRENT")).unwrap_or_default();

  // v2 arrives through the live path: served from the overlay, persisted in the background.
  fs::write(src.join("b.py"), "def beta_v2():\n    return alpha()\n").unwrap();
  assert!(
    served_from(&mut server, &mut id, "beta_v2", "b.py", Duration::from_secs(10)),
    "live path must serve v2"
  );
  // Let the served persist land and be reaped, so the daemon's own committers are idle and
  // the next commit is unmistakably somebody else's.
  let deadline = Instant::now() + Duration::from_secs(10);
  while fs::read_to_string(index.join("CURRENT")).unwrap_or_default() == before {
    assert!(Instant::now() < deadline, "served persist never landed");
    id += 1;
    let _ = call_tool(&mut server, id, "node", json!({"name": "alpha"}));
    std::thread::sleep(Duration::from_millis(50));
  }
  for _ in 0..3 {
    id += 1;
    let _ = call_tool(&mut server, id, "node", json!({"name": "alpha"}));
    std::thread::sleep(Duration::from_millis(50));
  }

  // v3 is written AND committed by someone else before the daemon looks again.
  fs::write(src.join("b.py"), "def beta_v3():\n    return alpha()\n").unwrap();
  vorpal_index::build_index(&src, &index).expect("external index run");

  assert!(
    served_from(&mut server, &mut id, "beta_v3", "b.py", Duration::from_secs(10)),
    "the daemon must serve v3: the committed generation is not the served state, so the \
     edit is judged against what the daemon actually serves"
  );
  id += 1;
  let (text, is_error) = call_tool(&mut server, id, "node", json!({"name": "beta_v2"}));
  assert!(
    is_error || !text.contains("b.py"),
    "v2 must be gone from the served graph: {text}"
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
  let (text, is_error) = call_tool(&mut server, 1, "graph", json!({"relation": "callers", "name": "target"}));
  assert!(!is_error, "{text}");

  let rounds = 10_000u64;
  let start = Instant::now();
  for i in 0..rounds {
    let (_, is_error) = call_tool(&mut server, 2 + i, "graph", json!({"relation": "callers", "name": "target"}));
    assert!(!is_error);
  }
  let elapsed = start.elapsed();
  eprintln!(
    "steady-state watched tool call: {:.1} µs/call over {rounds} calls (full JSON-RPC parse + freshness check + graph query + render)",
    elapsed.as_secs_f64() * 1e6 / rounds as f64
  );

  let _ = fs::remove_dir_all(&base);
}
