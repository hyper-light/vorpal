//! `vorpal-mcp` binary: a warm-index MCP server over stdio (one JSON-RPC message per line).

use std::path::PathBuf;
use std::process::ExitCode;

/// Same allocator policy as `vorpal-index`: jemalloc with immediate page return, so a
/// long-lived daemon's RSS tracks its live set instead of its high-water mark.
#[cfg(not(any(target_env = "msvc", all(target_env = "musl", target_arch = "aarch64"))))]
#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[cfg(not(any(target_env = "msvc", all(target_env = "musl", target_arch = "aarch64"))))]
mod jemalloc_conf {
  #[repr(transparent)]
  pub struct SyncPtr(#[allow(dead_code)] *const u8);
  unsafe impl Sync for SyncPtr {}
  #[unsafe(export_name = "_rjem_malloc_conf")]
  pub static MALLOC_CONF: SyncPtr = SyncPtr(
    c"narenas:8,dirty_decay_ms:0,muzzy_decay_ms:0"
      .as_ptr()
      .cast(),
  );
}

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
