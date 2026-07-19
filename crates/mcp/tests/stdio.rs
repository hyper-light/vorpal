//! Process-level proof: the real binary speaks MCP over stdio.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

use serde_json::{Value, json};

#[test]
fn binary_serves_mcp_over_stdio() {
  let base = std::env::temp_dir().join(format!("vorpal-mcp-stdio-{}", std::process::id()));
  let src = base.join("src");
  let idx = base.join("index");
  let _ = fs::remove_dir_all(&base);
  fs::create_dir_all(&src).unwrap();
  fs::write(src.join("b.rs"), "pub fn target() -> i32 {\n    0\n}\n").unwrap();
  fs::write(
    src.join("a.rs"),
    "pub fn caller() -> i32 {\n    target()\n}\n",
  )
  .unwrap();

  let mut child = Command::new(env!("CARGO_BIN_EXE_vorpal-mcp"))
    .arg(&idx)
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .spawn()
    .expect("spawn vorpal-mcp");
  let mut stdin = child.stdin.take().unwrap();
  let mut stdout = BufReader::new(child.stdout.take().unwrap());

  let mut send = |msg: Value| writeln!(stdin, "{msg}").unwrap();
  let mut recv = || {
    let mut line = String::new();
    stdout.read_line(&mut line).unwrap();
    serde_json::from_str::<Value>(&line).expect("response line is JSON")
  };

  send(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
              "params": {"protocolVersion": "2024-11-05", "capabilities": {}}}));
  let response = recv();
  assert_eq!(response["result"]["serverInfo"]["name"], "vorpal-mcp");

  send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));

  send(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
              "params": {"name": "index", "arguments": {"src": src.to_string_lossy()}}}));
  let response = recv();
  assert_eq!(response["result"]["isError"], false);

  send(json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call",
              "params": {"name": "callers", "arguments": {"name": "target"}}}));
  let response = recv();
  let text = response["result"]["content"][0]["text"].as_str().unwrap();
  assert!(text.contains("caller"), "callers over stdio: {text}");

  // Closing stdin ends the session; the server exits cleanly.
  drop(stdin);
  let status = child.wait().unwrap();
  assert!(status.success());

  let _ = fs::remove_dir_all(&base);
}
