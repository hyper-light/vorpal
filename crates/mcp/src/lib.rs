//! `vorpal-mcp` — the warm-index MCP daemon (§3.6): knowledge-graph queries served to agents.
//!
//! The Model Context Protocol is JSON-RPC 2.0, one message per line, over stdio — revision
//! `2026-07-28`, with the `initialize`-era envelope kept for older clients (see [`protocol`]).
//! This server holds a loaded [`vorpal_kg::Kg`] in memory across calls (mmap cold-open once,
//! query many — the warm-index story), exposing the graph verbs as MCP tools: `index`, `node`,
//! `callers`, `references`, `importers`, `reachable`, and the rest of the tool list.
//!
//! The protocol layer is a pure function ([`Server::handle_line`]: line in, optional line out),
//! so the whole daemon is testable without a process; `main` is a thin stdio loop. The protocol
//! is implemented directly on `serde_json` — small, dependency-light, and swappable for an SDK
//! transport later without touching the tool logic.

pub mod protocol;
pub mod registry;
mod router;

/// Test-only handle: the multi-project router constructed from the CURRENT registry file
/// (which tests point at a scratch path via `VORPAL_PROJECTS_FILE`).
pub type MultiServerForTest = router::MultiServer;

pub fn multi_server_for_test() -> MultiServerForTest {
  let projects = registry::load().unwrap_or_default();
  router::MultiServer::new(projects, Profile::Full)
}

pub fn multi_server_for_test_with_envs(
  envs: std::collections::BTreeMap<String, vorpal_index::ExtractionEnv>,
) -> MultiServerForTest {
  let projects = registry::load().unwrap_or_default();
  router::MultiServer::with_envs(projects, Profile::Full, envs)
}
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
/// Serve every enrolled project from one daemon (D4): tools gain a `project` selector, a
/// `list_projects` tool lists the registry, and nothing on this surface can enroll anything
/// (human-only, via the CLI). This env-less form serves builtin grammars with default
/// environments; the CLI launcher uses [`serve_stdio_projects_with_envs`] after
/// union-registering every enrolled project's custom languages.
pub fn serve_stdio_projects(profile: Profile) -> io::Result<()> {
  serve_stdio_projects_with_envs(profile, std::collections::BTreeMap::new())
}

/// The enrolled registry, for launchers that must prepare per-project state (custom-language
/// union registration, extraction environments) BEFORE serving begins.
pub fn enrolled_projects() -> io::Result<Vec<(String, std::path::PathBuf, std::path::PathBuf)>> {
  let projects = registry::load().map_err(io::Error::other)?;
  Ok(
    projects
      .iter()
      .map(|(name, entry)| (name.clone(), entry.src.clone(), entry.index.clone()))
      .collect(),
  )
}

/// [`serve_stdio_projects`] with a per-project extraction environment (union-registered
/// custom languages): each project's rebuilds run under ITS OWN rules/specs/canaries. Any
/// dlopen happened in the CALLER at startup — one union registration for every enrolled
/// project — so the serving loop still can never load code.
pub fn serve_stdio_projects_with_envs(
  profile: Profile,
  envs: std::collections::BTreeMap<String, vorpal_index::ExtractionEnv>,
) -> io::Result<()> {
  let projects = registry::load().map_err(io::Error::other)?;
  if projects.is_empty() {
    return Err(io::Error::other(
      "no projects enrolled — a person can enroll one with `vorpal mcp allow <path>`",
    ));
  }
  let mut server = router::MultiServer::with_envs(projects, profile, envs);
  pump(|line| match line {
    Some(line) => server.handle_line(line),
    None => {
      server.tick();
      None
    }
  })
}

pub fn serve_stdio_env(
  index_dir: PathBuf,
  profile: Profile,
  env: vorpal_index::ExtractionEnv,
) -> io::Result<()> {
  serve_stdio_opts(index_dir, profile, env, true)
}

/// [`serve_stdio_env`] plus the D1 toggle: `watch_rebuild` gates proactive freshness — the
/// serve loop pulses [`Server::tick`] between requests, which rebuilds through the retained
/// tiers (or a supervised child) once the watch goes quiet. Also disable-able at runtime
/// with `VORPAL_WATCH_REBUILD=0`; query-path freshness is lazy and unaffected either way.
pub fn serve_stdio_opts(
  index_dir: PathBuf,
  profile: Profile,
  env: vorpal_index::ExtractionEnv,
  watch_rebuild: bool,
) -> io::Result<()> {
  let mut server = Server::with_profile_env_rebuild(index_dir, profile, env, watch_rebuild);
  pump(|line| match line {
    Some(line) => server.handle_line(line),
    None => {
      server.tick();
      None
    }
  })
}

/// The shared stdio pump: a reader thread forwards stdin lines over a channel so the serve
/// loop wakes on QUIET (250 ms) and pulses background freshness between requests — a
/// blocking read would pin proactivity to request arrival. Protocol behavior is unchanged:
/// one JSON-RPC message per line in, one per line out, EOF ends the daemon.
/// `step(Some(line))` handles a request; `step(None)` is the quiet pulse.
fn pump(mut step: impl FnMut(Option<&str>) -> Option<String>) -> io::Result<()> {
  let (tx, rx) = std::sync::mpsc::channel::<io::Result<String>>();
  std::thread::spawn(move || {
    let stdin = io::stdin();
    for line in stdin.lock().lines() {
      let failed = line.is_err();
      if tx.send(line).is_err() || failed {
        return;
      }
    }
    // EOF: dropping the sender disconnects the pump below, ending the daemon cleanly.
  });
  let mut stdout = io::stdout().lock();
  loop {
    match rx.recv_timeout(std::time::Duration::from_millis(250)) {
      Ok(line) => {
        let line = line?;
        if line.trim().is_empty() {
          continue;
        }
        if let Some(response) = step(Some(&line)) {
          // One message per line; flush so the client sees each response immediately.
          writeln!(stdout, "{response}")?;
          stdout.flush()?;
        }
      }
      Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
        let _ = step(None);
      }
      Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
    }
  }
}
