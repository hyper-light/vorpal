//! `vorpal-mcp` — the warm-index MCP daemon (§3.6): knowledge-graph queries served to agents.
//!
//! The Model Context Protocol is JSON-RPC 2.0, one message per line, over stdio. This server
//! holds a loaded [`vorpal_kg::Kg`] in memory across calls (mmap cold-open once, query many —
//! the warm-index story), exposing the graph verbs as MCP tools: `index`, `node`, `callers`,
//! `references`, `importers`, `reachable`.
//!
//! The protocol layer is a pure function ([`Server::handle_line`]: line in, optional line out),
//! so the whole daemon is testable without a process; `main` is a thin stdio loop. The protocol
//! is implemented directly on `serde_json` — small, dependency-light, and swappable for an SDK
//! transport later without touching the tool logic.

mod server;

pub use server::Server;

use std::io::{self, BufRead, Write};
use std::path::PathBuf;

/// Serve MCP over stdio (one JSON-RPC message per line) until stdin closes — the daemon loop
/// shared by the standalone `vorpal-mcp` binary and the `vorpal mcp` subcommand.
pub fn serve_stdio(index_dir: PathBuf) -> io::Result<()> {
  let mut server = Server::new(index_dir);
  let stdin = io::stdin();
  let mut stdout = io::stdout().lock();
  for line in stdin.lock().lines() {
    let line = line?;
    if line.trim().is_empty() {
      continue;
    }
    if let Some(response) = server.handle_line(&line) {
      // One message per line; flush so the client sees each response immediately.
      writeln!(stdout, "{response}")?;
      stdout.flush()?;
    }
  }
  Ok(())
}
