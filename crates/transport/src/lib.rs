//! Vorpal fleet transports (docs/REMOTE.md §2).
//!
//! Everything a coordinator needs from a remote node reduces to **"run argv, pipe bytes, get exit
//! code"**. That is the [`Transport`] trait; `push_file`/`pull_file` default over `exec`. The
//! `dyn` boundary is coarse (control-plane granularity via `async_trait`); the per-byte result
//! copy runs through the monomorphized `AsyncRead`/`AsyncWrite` halves handed back from `exec`, so
//! boxing is amortized over whole streams, not per byte.
//!
//! The layer is **async on tokio** — three of the four planned backends (SSH, k8s, docker) are
//! async-native with no sync equivalent, and one runtime multiplexing I/O over hundreds of nodes ×
//! several streams is the only thing that scales. The sync scan/index engine is never touched: the
//! agent reuses it verbatim behind a thin async I/O shell, and the coordinator bridges async
//! results into the existing sync printer channel with a dedicated blocking forwarder.

mod command;
mod error;
pub mod negotiate;
mod policy;
mod process;
pub mod provision;
mod spawn;
mod subprocess;

#[cfg(feature = "ssh")]
pub mod ssh;

pub use command::CommandTransport;
pub use error::TransportError;
pub use negotiate::{ExecMode, ForcedMode, LandingSpot, NodeProbe, decide, probe};
pub use policy::{Backend, Redacted, RemotePolicy};
pub use process::{ExecSpec, Output, RemoteKiller, RemoteProcess};
pub use provision::{Provisioned, push_agent};
pub use subprocess::SubprocessTransport;

use async_trait::async_trait;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

/// A pulled byte stream that keeps its producing [`RemoteProcess`] alive for as long as the
/// reader exists (see `pull_file`).
struct PulledStream {
  stdout: Box<dyn AsyncRead + Send + Unpin>,
  _proc: RemoteProcess,
}

impl AsyncRead for PulledStream {
  fn poll_read(
    mut self: std::pin::Pin<&mut Self>,
    cx: &mut std::task::Context<'_>,
    buf: &mut tokio::io::ReadBuf<'_>,
  ) -> std::task::Poll<std::io::Result<()>> {
    std::pin::Pin::new(&mut self.stdout).poll_read(cx, buf)
  }
}

/// A redacted, secrets-free identity for a node — safe to log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeDescriptor {
  /// Stable transport scheme (`"subprocess"`, `"ssh"`, `"k8s"`, …).
  pub scheme: &'static str,
  /// Human-readable, redacted address (host, pod name — never credentials).
  pub address: String,
}

impl std::fmt::Display for NodeDescriptor {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    write!(f, "{}://{}", self.scheme, self.address)
  }
}

/// A-priori facts about a node, from the control plane where available — used by `negotiate` to
/// avoid a probe round-trip when it can.
#[derive(Clone, Debug, Default)]
pub struct TransportHints {
  pub arch: Option<String>,
  pub os: Option<String>,
  pub libc: Option<String>,
  /// The node is known to forbid executing pushed binaries (distroless/noexec) — force streaming.
  pub known_noexec: bool,
}

/// Exec-shaped async connection to one node. Coarse `dyn` boundary; the byte copy is monomorphized
/// over the halves in [`RemoteProcess`].
#[async_trait]
pub trait Transport: Send + Sync + 'static {
  /// Redacted identity — never secrets.
  fn descriptor(&self) -> &NodeDescriptor;

  /// A-priori facts from the control plane (empty if none).
  fn hints(&self) -> TransportHints {
    TransportHints::default()
  }

  /// Run `spec` and return a live process with piped stdio and a wait handle.
  async fn exec(&self, spec: &ExecSpec) -> Result<RemoteProcess, TransportError>;

  /// Run `spec` to completion, capturing stdout/stderr. Default drives [`Transport::exec`].
  async fn exec_capture(&self, spec: &ExecSpec) -> Result<Output, TransportError> {
    let mut proc = self.exec(spec).await?;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    if let Some(mut out) = proc.stdout.take() {
      out.read_to_end(&mut stdout).await.map_err(TransportError::io)?;
    }
    if let Some(mut err) = proc.stderr.take() {
      err.read_to_end(&mut stderr).await.map_err(TransportError::io)?;
    }
    let status = proc.wait().await?;
    Ok(Output { status, stdout, stderr })
  }

  /// Push `len` bytes from `body` to `dest` with mode `mode`. Default streams the bytes into
  /// `dd`/`cat` under `exec` — every backend that can exec gets this for free; backends with a
  /// native daemon-served copy (docker `put_archive`, k8s) override it.
  async fn push_file(
    &self,
    dest: &str,
    mode: u32,
    _len: u64,
    body: &mut (dyn AsyncRead + Send + Unpin),
  ) -> Result<(), TransportError> {
    // `cat > dest` is the most portable landing primitive; chmod separately (some shells lack a
    // combined form). The dest is a coordinator-controlled path, not user input.
    let spec = ExecSpec::shell(format!("cat > {q} && chmod {mode:o} {q}", q = shell_quote(dest)));
    let mut proc = self.exec(&spec).await?;
    let mut stdin = proc.stdin.take().ok_or_else(|| TransportError::other("no stdin for push"))?;
    tokio::io::copy(body, &mut stdin).await.map_err(TransportError::io)?;
    // Flush and cleanly shut down the write half so every byte is delivered and the remote reader
    // sees a proper EOF — a bare drop races byte delivery against pipe close (over SSH too).
    stdin.flush().await.map_err(TransportError::io)?;
    stdin.shutdown().await.map_err(TransportError::io)?;
    drop(stdin);
    let status = proc.wait().await?;
    if !status.success() {
      return Err(TransportError::exec(format!("push to {dest} failed with {status:?}")));
    }
    Ok(())
  }

  /// Read `src` back as a stream. Default `cat`s it under `exec`. The returned stream owns
  /// its producing process: backends spawn children kill-on-drop, so handing back the pipe
  /// alone dropped the last process handle and killed the remote `cat` mid-stream — the
  /// pull raced its own teardown and read empty. Early-dropping the stream still cancels
  /// the process, exactly as intended.
  async fn pull_file(
    &self,
    src: &str,
  ) -> Result<Box<dyn AsyncRead + Send + Unpin>, TransportError> {
    let spec = ExecSpec::shell(format!("cat {}", shell_quote(src)));
    let mut proc = self.exec(&spec).await?;
    let stdout = proc
      .stdout
      .take()
      .ok_or_else(|| TransportError::other("no stdout for pull"))?;
    Ok(Box::new(PulledStream { stdout, _proc: proc }))
  }

  /// Cheap liveness probe (default: `true`/exit-0).
  async fn health_check(&self) -> Result<(), TransportError> {
    let out = self.exec_capture(&ExecSpec::shell("exit 0".into())).await?;
    if out.status.success() {
      Ok(())
    } else {
      Err(TransportError::exec("health check nonzero exit"))
    }
  }

  /// Close the connection and free resources.
  async fn shutdown(&self) -> Result<(), TransportError> {
    Ok(())
  }
}

/// Single-quote a path for POSIX shells (wrap in `'…'`, escaping embedded quotes). Used only for
/// coordinator-controlled landing paths, never untrusted input, but done correctly regardless.
pub(crate) fn shell_quote(s: &str) -> String {
  let mut out = String::with_capacity(s.len() + 2);
  out.push('\'');
  for ch in s.chars() {
    if ch == '\'' {
      out.push_str("'\\''");
    } else {
      out.push(ch);
    }
  }
  out.push('\'');
  out
}

/// Exit status of a remote process, transport-neutral.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExitStatus {
  pub code: Option<i32>,
}

impl ExitStatus {
  pub fn success(&self) -> bool {
    self.code == Some(0)
  }
}
