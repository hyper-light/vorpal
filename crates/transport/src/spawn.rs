//! Shared process-spawn core: turn a prepared `std::process::Command` into a live
//! [`RemoteProcess`] with piped stdio, a wait future, and a group-killing kill handle. Both the
//! loopback [`SubprocessTransport`](crate::SubprocessTransport) and the
//! [`CommandTransport`](crate::CommandTransport) (kubectl/docker/…) spawn through here, so the exec
//! semantics — process-group isolation, kill-on-drop, exit-status plumbing — stay identical across
//! backends and can't drift.

use std::sync::Arc;

use async_trait::async_trait;
use tokio::process::{Child, Command as TokioCommand};
use tokio::sync::Mutex;

use crate::process::{RemoteKiller, RemoteProcess};
use crate::{ExitStatus, TransportError};

/// Configure stdio + process-group isolation on `std_cmd`, spawn it, and wrap the child as a
/// [`RemoteProcess`]. `want_stderr` pipes stderr back (else it is inherited/dropped).
pub(crate) fn spawn_piped(
  mut std_cmd: std::process::Command,
  want_stderr: bool,
) -> Result<RemoteProcess, TransportError> {
  std_cmd.stdin(std::process::Stdio::piped());
  std_cmd.stdout(std::process::Stdio::piped());
  std_cmd.stderr(if want_stderr {
    std::process::Stdio::piped()
  } else {
    std::process::Stdio::inherit()
  });
  // Own process group so killing takes down any grandchildren (a wrapper like `kubectl`/`ssh` that
  // spawns helpers); a lone `child.kill()` would leave them holding the stdout pipe open.
  #[cfg(unix)]
  {
    use std::os::unix::process::CommandExt;
    std_cmd.process_group(0);
  }

  let mut cmd = TokioCommand::from(std_cmd);
  cmd.kill_on_drop(true);
  let mut child = cmd.spawn().map_err(|e| TransportError::exec(format!("spawn failed: {e}")))?;

  let stdin = child.stdin.take().map(|s| Box::new(s) as Box<dyn tokio::io::AsyncWrite + Send + Unpin>);
  let stdout = child.stdout.take().map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Send + Unpin>);
  let stderr = child.stderr.take().map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Send + Unpin>);

  // The child is shared between the wait future and the kill handle — genuinely two independent
  // `'static` owners of one OS process, so an `Arc` here is load-bearing, not incidental.
  let child = Arc::new(Mutex::new(child));
  let killer = Arc::new(ChildKiller { child: child.clone() });
  let wait_child = child;
  let wait = Box::pin(async move {
    let mut guard = wait_child.lock().await;
    let status = guard.wait().await.map_err(TransportError::io)?;
    Ok(ExitStatus { code: status.code() })
  });
  Ok(RemoteProcess::new(stdin, stdout, stderr, wait, killer))
}

/// Kills the child's whole process group on unix; the direct child otherwise.
struct ChildKiller {
  child: Arc<Mutex<Child>>,
}

#[async_trait]
impl RemoteKiller for ChildKiller {
  async fn kill(&self) {
    let mut guard = self.child.lock().await;
    #[cfg(unix)]
    if let Some(pid) = guard.id() {
      // Spawned with `process_group(0)`, so the pgid equals the pid.
      unsafe {
        libc_kill_group(pid as i32);
      }
    }
    let _ = guard.start_kill();
  }
}

#[cfg(unix)]
unsafe fn libc_kill_group(pid: i32) {
  // SIGKILL the whole group. We avoid a `libc` dep in this crate by declaring the one symbol.
  unsafe extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
  }
  const SIGKILL: i32 = 9;
  unsafe {
    kill(-pid, SIGKILL);
  }
}
