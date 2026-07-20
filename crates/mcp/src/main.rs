//! `vorpal-mcp` binary: a warm-index MCP server over stdio (one JSON-RPC message per line).

use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str = "usage: vorpal-mcp <index-dir>
Serves vorpal knowledge-graph queries as MCP tools over stdio.
<index-dir> holds the persisted index (created by the 'index' tool if absent).";

fn main() -> ExitCode {
  let mut args = std::env::args().skip(1);
  let (Some(dir), None) = (args.next(), args.next()) else {
    eprintln!("{USAGE}");
    return ExitCode::FAILURE;
  };
  match vorpal_mcp::serve_stdio(PathBuf::from(dir)) {
    Ok(()) => ExitCode::SUCCESS,
    Err(err) => {
      eprintln!("vorpal-mcp: {err}");
      ExitCode::FAILURE
    }
  }
}
