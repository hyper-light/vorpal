//! Detached background ANN warm-up — the "second search is fast" half of the cold-scan
//! story, without ever blocking the first.
//!
//! A cold search serves via the exhaustive fallback, then asks this module to warm the
//! tier in a **detached child process** so the work survives the one-shot CLI exiting.
//! Three containment rules keep this from ever surprising anyone:
//!
//! 1. **Only registered binaries spawn.** `register()` is called by the `vorpal` and
//!    `vorpal-index` mains — never by tests, language bindings, or embedding hosts — so a
//!    `cargo test` binary (whose `main` knows nothing of the sentinel) can never fork-bomb
//!    itself into running its own suite recursively.
//! 2. **`VORPAL_NO_AUTOWARM=1` is an unconditional veto** for users and CI.
//! 3. **One spawn per process, one builder per index.** A process-local flag stops a chatty
//!    caller from re-spawning per search; the cross-process file lock in [`warm-guarded`
//!    builds](super::warm_ann) makes every extra child a cheap no-op.
//!
//! The child is re-entered through an argv sentinel (`<exe> __warm-ann <abs-dir>`), the same
//! pre-clap pattern the CLI already uses for agent mode — no env-var hijack surface.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

/// The argv sentinel a registered binary handles before any argument parsing.
pub const WARM_SENTINEL: &str = "__warm-ann";

static REGISTERED: AtomicBool = AtomicBool::new(false);
static SPAWNED: AtomicBool = AtomicBool::new(false);

/// Declare that this process's `main` handles [`WARM_SENTINEL`] — called by the `vorpal`
/// and `vorpal-index` binaries only. Unregistered processes never spawn.
pub fn register() {
  REGISTERED.store(true, Ordering::Release);
}

/// If this process's argv is a warm-sentinel invocation, run the warm and exit the process.
/// Registered binaries call this first thing in `main`.
pub fn run_if_sentinel() {
  let mut args = std::env::args_os().skip(1);
  if args.next().as_deref() != Some(std::ffi::OsStr::new(WARM_SENTINEL)) {
    return;
  }
  let code = match args.next() {
    Some(dir) => match crate::warm_ann(Path::new(&dir)) {
      Ok(()) => 0,
      Err(_) => 1,
    },
    None => 2,
  };
  std::process::exit(code);
}

/// Spawn a detached warm of `index_dir`, if this process may: registered, not vetoed, not
/// already spawned this process. Best-effort by design — every failure mode just means the
/// next search also serves via the (correct) fallback.
pub fn maybe_spawn(index_dir: &Path) {
  if !REGISTERED.load(Ordering::Acquire) {
    return;
  }
  if std::env::var_os("VORPAL_NO_AUTOWARM").is_some_and(|v| v == "1") {
    return;
  }
  if SPAWNED.swap(true, Ordering::AcqRel) {
    return;
  }
  let Ok(exe) = std::env::current_exe() else {
    return;
  };
  // Canonicalize: the child's working directory is unrelated to ours, so a relative
  // `.vorpal/index` would warm the wrong place (or nothing).
  let Ok(dir) = index_dir.canonicalize() else {
    return;
  };
  let mut command = std::process::Command::new(exe);
  command
    .arg(WARM_SENTINEL)
    .arg(&dir)
    .stdin(std::process::Stdio::null())
    .stdout(std::process::Stdio::null())
    .stderr(std::process::Stdio::null());
  // Detach from the terminal's process group so Ctrl-C on the parent doesn't kill the
  // build mid-write (tmp+rename means it wouldn't corrupt anything — it would just waste
  // the work).
  #[cfg(unix)]
  {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
  }
  if let Ok(mut child) = command.spawn() {
    // Reap the child from a detached thread: one-shot CLI parents exit first (init adopts
    // the child), but a long-lived embedder would otherwise accumulate zombies.
    std::thread::spawn(move || {
      let _ = child.wait();
    });
  }
}
