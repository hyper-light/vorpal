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
mod supervised;
mod tools;
mod watch;

pub use server::{Profile, Server};

use std::io::{self, BufRead, Write};
use std::path::PathBuf;

/// Serve MCP over stdio (one JSON-RPC message per line) until stdin closes — the daemon loop
/// shared by the standalone `vorpal-mcp` binary and the `vorpal mcp` subcommand.
pub fn serve_stdio(index_dir: PathBuf) -> io::Result<()> {
  serve_stdio_profiled(index_dir, Profile::Full)
}

/// [`serve_stdio`] with a tool profile: `scout` (read-only navigation), `analysis`
/// (+ traversal/evidence/health), `full` (everything, including index builds and rule tools).
pub fn serve_stdio_profiled(index_dir: PathBuf, profile: Profile) -> io::Result<()> {
  serve_stdio_env(index_dir, profile, vorpal_index::ExtractionEnv::default())
}

/// [`serve_stdio_profiled`] under an explicit extraction environment (F-M6): the daemon's
/// rebuilds see the same custom-language rules/specs/canaries the CLI build would. Any dlopen
/// happened in the CALLER at startup; the serving loop can never load code.
pub fn serve_stdio_env(
  index_dir: PathBuf,
  profile: Profile,
  env: vorpal_index::ExtractionEnv,
) -> io::Result<()> {
  serve_stdio_opts(index_dir, profile, env, true)
}

/// [`serve_stdio_env`] plus the D1 toggle: `watch_rebuild` gates the proactive background
/// rebuild worker (also disable-able at runtime with `VORPAL_WATCH_REBUILD=0`).
pub fn serve_stdio_opts(
  index_dir: PathBuf,
  profile: Profile,
  env: vorpal_index::ExtractionEnv,
  watch_rebuild: bool,
) -> io::Result<()> {
  let mut server = Server::with_profile_env_rebuild(index_dir, profile, env, watch_rebuild);
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
