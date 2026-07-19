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
