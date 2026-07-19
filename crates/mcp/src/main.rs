//! `vorpal-mcp` binary: a warm-index MCP server over stdio (one JSON-RPC message per line).

use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use vorpal_mcp::Server;

const USAGE: &str = "usage: vorpal-mcp <index-dir>
Serves vorpal knowledge-graph queries as MCP tools over stdio.
<index-dir> holds the persisted index (created by the 'index' tool if absent).";

fn main() -> ExitCode {
  let mut args = std::env::args().skip(1);
  let (Some(dir), None) = (args.next(), args.next()) else {
    eprintln!("{USAGE}");
    return ExitCode::FAILURE;
  };
  let mut server = Server::new(PathBuf::from(dir));
  let stdin = io::stdin();
  let mut stdout = io::stdout().lock();
  for line in stdin.lock().lines() {
    let Ok(line) = line else { break };
    if line.trim().is_empty() {
      continue;
    }
    if let Some(response) = server.handle_line(&line) {
      // One message per line; flush so the client sees each response immediately.
      if writeln!(stdout, "{response}")
        .and_then(|()| stdout.flush())
        .is_err()
      {
        break;
      }
    }
  }
  ExitCode::SUCCESS
}
