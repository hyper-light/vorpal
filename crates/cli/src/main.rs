use std::process::ExitCode;

use anyhow::Result;
use vorpal::execute_main;

/// Same allocator policy as `vorpal-index` (where it was measured): jemalloc with immediate
/// page return — a bulk index/scan's peak footprint tracks its live set instead of stacking
/// each phase's retained garbage (2.05 GB → 1.13 GB at kernel scale), and the thread-local
/// caches are faster under the pipeline's multithreaded churn.
#[cfg(not(any(target_env = "msvc", all(target_env = "musl", target_arch = "aarch64"))))]
#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[cfg(not(any(target_env = "msvc", all(target_env = "musl", target_arch = "aarch64"))))]
mod jemalloc_conf {
  #[repr(transparent)]
  pub struct SyncPtr(#[allow(dead_code)] *const u8);
  unsafe impl Sync for SyncPtr {}
  #[unsafe(export_name = "_rjem_malloc_conf")]
  pub static MALLOC_CONF: SyncPtr =
    SyncPtr(c"narenas:8,dirty_decay_ms:0,muzzy_decay_ms:0".as_ptr().cast());
}

/// Route tree-sitter's C-side allocations (parse trees) through jemalloc too — one
/// allocator, one decay policy; without this the trees age out in the default zone beyond
/// jemalloc's reach (~150–250 MB of retained pages at kernel scale).
#[cfg(not(any(target_env = "msvc", all(target_env = "musl", target_arch = "aarch64"))))]
fn unify_parser_allocator() {
  unsafe {
    tree_sitter::set_allocator(
      Some(tikv_jemalloc_sys::malloc),
      Some(tikv_jemalloc_sys::calloc),
      Some(tikv_jemalloc_sys::realloc),
      Some(tikv_jemalloc_sys::free),
    );
  }
}

fn main() -> Result<ExitCode> {
  // Restore default SIGPIPE so `vorpal … | head` ends quietly like every Unix tool, instead
  // of Rust's ignore-then-panic ("failed printing to stdout: Broken pipe"). The stdio
  // daemons (lsp/mcp) inherit this: a vanished client ends the process, which is the
  // correct daemon behavior too.
  #[cfg(unix)]
  unsafe {
    libc::signal(libc::SIGPIPE, libc::SIG_DFL);
  }
  #[cfg(not(any(target_env = "msvc", all(target_env = "musl", target_arch = "aarch64"))))]
  unify_parser_allocator();
  execute_main()
}
