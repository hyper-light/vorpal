//! A live remote process and the spec used to launch one.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::io::{AsyncRead, AsyncWrite};

use crate::{ExitStatus, TransportError};

/// What to run on a node. Either an argv (preferred — no shell parsing) or a shell line (needed for
/// pipelines/redirections like the default `push_file`).
#[derive(Clone, Debug)]
pub struct ExecSpec {
  pub command: Command,
  /// Extra environment (name, value). Secrets (the per-job token) ride here or on stdin — never a
  /// command-line argument, which is world-readable in `/proc`.
  pub env: Vec<(String, String)>,
  /// Whether stderr should be piped back (vs inherited/dropped).
  pub want_stderr: bool,
}

#[derive(Clone, Debug)]
pub enum Command {
  /// Argv with no shell involved.
  Argv(Vec<String>),
  /// A shell line run via `sh -c`.
  Shell(String),
}

impl ExecSpec {
  pub fn argv<I, S>(args: I) -> Self
  where
    I: IntoIterator<Item = S>,
    S: Into<String>,
  {
    Self {
      command: Command::Argv(args.into_iter().map(Into::into).collect()),
      env: Vec::new(),
      want_stderr: false,
    }
  }

  pub fn shell(line: String) -> Self {
    Self { command: Command::Shell(line), env: Vec::new(), want_stderr: false }
  }

  pub fn env(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
    self.env.push((name.into(), value.into()));
    self
  }

  pub fn with_stderr(mut self) -> Self {
    self.want_stderr = true;
    self
  }

  /// Render to a single shell line (for transports whose exec takes a command string).
  pub fn to_shell_line(&self) -> String {
    match &self.command {
      Command::Shell(line) => line.clone(),
      Command::Argv(argv) => {
        argv.iter().map(|a| crate::shell_quote(a)).collect::<Vec<_>>().join(" ")
      }
    }
  }
}

/// Kills a still-running remote process (used by the [`RemoteProcess`] Drop guard so a dropped
/// process never leaks on the node).
#[async_trait]
pub trait RemoteKiller: Send + Sync {
  async fn kill(&self);
}

/// The type of the exit-status future a transport hands back.
pub type WaitFuture = Pin<Box<dyn Future<Output = Result<ExitStatus, TransportError>> + Send>>;

/// A live remote process: piped stdio halves, a wait future, and a kill handle. Dropping it is
/// best-effort cleanup; transports also set kill-on-drop where the platform supports it.
pub struct RemoteProcess {
  pub stdin: Option<Box<dyn AsyncWrite + Send + Unpin>>,
  pub stdout: Option<Box<dyn AsyncRead + Send + Unpin>>,
  pub stderr: Option<Box<dyn AsyncRead + Send + Unpin>>,
  wait: Option<WaitFuture>,
  killer: Arc<dyn RemoteKiller>,
}

impl RemoteProcess {
  pub fn new(
    stdin: Option<Box<dyn AsyncWrite + Send + Unpin>>,
    stdout: Option<Box<dyn AsyncRead + Send + Unpin>>,
    stderr: Option<Box<dyn AsyncRead + Send + Unpin>>,
    wait: WaitFuture,
    killer: Arc<dyn RemoteKiller>,
  ) -> Self {
    Self { stdin, stdout, stderr, wait: Some(wait), killer }
  }

  /// Await the process exit status. Idempotent-ish: a second call returns an unknown status.
  pub async fn wait(&mut self) -> Result<ExitStatus, TransportError> {
    match self.wait.take() {
      Some(fut) => fut.await,
      None => Ok(ExitStatus { code: None }),
    }
  }

  /// A cheap clone of the kill handle, e.g. for a watchdog.
  pub fn killer(&self) -> Arc<dyn RemoteKiller> {
    self.killer.clone()
  }
}

/// Captured output of a completed remote process.
#[derive(Debug)]
pub struct Output {
  pub status: ExitStatus,
  pub stdout: Vec<u8>,
  pub stderr: Vec<u8>,
}
