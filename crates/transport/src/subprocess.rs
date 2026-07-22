//! The subprocess transport: spawn a local child and pipe its stdio. This is the `loopback://`
//! backend — the differential-testing reference every other transport must match — now async
//! (`tokio::process`), driven by the same coordinator agent-session code SSH uses.

use async_trait::async_trait;

use crate::process::{Command, ExecSpec, RemoteProcess};
use crate::spawn::spawn_piped;
use crate::{NodeDescriptor, Transport, TransportError};

/// Runs commands as local child processes. `program`/`base_args` prefix every `exec` (e.g.
/// `vorpal __agent`), so the "node" is really this machine re-executing the vorpal binary.
pub struct SubprocessTransport {
  descriptor: NodeDescriptor,
  program: std::path::PathBuf,
  base_args: Vec<String>,
}

impl SubprocessTransport {
  /// A transport that runs `program base_args… <exec>`. For loopback agent mode, `program` is the
  /// current vorpal binary and `base_args` is `["__agent"]`.
  pub fn new(program: impl Into<std::path::PathBuf>, base_args: Vec<String>) -> Self {
    Self {
      descriptor: NodeDescriptor { scheme: "loopback", address: "local".into() },
      program: program.into(),
      base_args,
    }
  }

  fn build_command(&self, spec: &ExecSpec) -> std::process::Command {
    // The agent transport ignores the exec argv (the agent's behavior is driven by the wire
    // protocol on its stdio, not argv); a generic subprocess transport would honor it. We support
    // both: `base_args` runs first, then the spec's argv/shell as additional args if any.
    let mut std_cmd = std::process::Command::new(&self.program);
    std_cmd.args(&self.base_args);
    match &spec.command {
      Command::Argv(argv) => {
        std_cmd.args(argv);
      }
      Command::Shell(line) => {
        // Only used by push/pull/health defaults, which the agent path never invokes; still,
        // honor it by appending as a `sh -c` payload when there are no base_args (generic use).
        if self.base_args.is_empty() {
          std_cmd = std::process::Command::new("sh");
          std_cmd.arg("-c").arg(line);
        }
      }
    }
    for (k, v) in &spec.env {
      std_cmd.env(k, v);
    }
    std_cmd
  }
}

#[async_trait]
impl Transport for SubprocessTransport {
  fn descriptor(&self) -> &NodeDescriptor {
    &self.descriptor
  }

  async fn exec(&self, spec: &ExecSpec) -> Result<RemoteProcess, TransportError> {
    spawn_piped(self.build_command(spec), spec.want_stderr)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use tokio::io::AsyncReadExt;

  #[tokio::test]
  async fn subprocess_exec_pipes_stdout_and_exit() {
    let t = SubprocessTransport::new("sh", vec!["-c".into(), "printf hello".into()]);
    let mut proc = t.exec(&ExecSpec::argv(Vec::<String>::new())).await.unwrap();
    let mut out = String::new();
    proc.stdout.take().unwrap().read_to_string(&mut out).await.unwrap();
    assert_eq!(out, "hello");
    assert!(proc.wait().await.unwrap().success());
  }

  #[tokio::test]
  async fn subprocess_push_and_pull_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("pushed.bin");
    // A generic subprocess transport (no base_args) supports the shell-based push/pull defaults.
    let t = SubprocessTransport::new("sh", vec![]);
    let mut body = &b"payload-bytes"[..];
    t.push_file(dest.to_str().unwrap(), 0o600, 13, &mut body).await.unwrap();
    let got = std::fs::read(&dest).unwrap();
    assert_eq!(got, b"payload-bytes");
    let mut r = t.pull_file(dest.to_str().unwrap()).await.unwrap();
    let mut back = Vec::new();
    r.read_to_end(&mut back).await.unwrap();
    assert_eq!(back, b"payload-bytes");
  }
}
