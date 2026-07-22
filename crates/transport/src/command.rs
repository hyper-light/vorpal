//! The generic command transport (docs/REMOTE.md §2, D2): turn any **exec-shaped CLI** —
//! `kubectl exec`, `docker exec`, a custom broker — into a [`Transport`] by spawning
//! `program prefix… <remote-command>` locally and piping its stdio. The wrapped CLI relays bytes to
//! the node, so the whole agent/stream/negotiation stack rides on top unchanged: the probe, the
//! agent handshake, `push_file` (`cat > dest`), and the stream enumerator all run as ordinary
//! `exec`s through the CLI.
//!
//! Kubernetes and Docker are first-class constructors here rather than bespoke API clients: for a
//! local cluster the `kubectl`/`docker` binaries are already present and authenticated (kubeconfig /
//! docker context), and shelling out is the robust, dependency-light path. `exec -i` (no `-t`, so no
//! TTY line-discipline to corrupt binary streams) gives a clean bidirectional byte pipe, and the
//! CLI forwards the remote process's exit status as its own.

use async_trait::async_trait;

use crate::process::{Command, ExecSpec, RemoteProcess};
use crate::spawn::spawn_piped;
use crate::{NodeDescriptor, Transport, TransportError};

/// A transport that runs `program <prefix…> <remote-command>` as a local child. `prefix` carries the
/// CLI's own flags up to (and including, for k8s) the `--` that separates them from the remote
/// command; the remote command (argv, or `sh -c <line>`) is appended per `exec`.
pub struct CommandTransport {
  descriptor: NodeDescriptor,
  program: String,
  prefix: Vec<String>,
}

impl CommandTransport {
  /// A fully-general command transport. `scheme`/`address` are the redacted node identity;
  /// `program` + `prefix` are the CLI invocation the remote command is appended to.
  pub fn new(
    scheme: &'static str,
    address: impl Into<String>,
    program: impl Into<String>,
    prefix: Vec<String>,
  ) -> Self {
    Self { descriptor: NodeDescriptor { scheme, address: address.into() }, program: program.into(), prefix }
  }

  /// `kubectl exec -i [-n ns] [-c container] <pod> -- <cmd>`. `program` is usually `"kubectl"` but
  /// may carry global flags via [`CommandTransport::new`] if a caller needs `--context`/`--kubeconfig`.
  pub fn kubectl(namespace: Option<&str>, pod: &str, container: Option<&str>) -> Self {
    let mut prefix = vec!["exec".to_string(), "-i".to_string()];
    if let Some(ns) = namespace {
      prefix.push("-n".into());
      prefix.push(ns.to_string());
    }
    if let Some(ctr) = container {
      prefix.push("-c".into());
      prefix.push(ctr.to_string());
    }
    prefix.push(pod.to_string());
    prefix.push("--".into()); // separate kubectl's flags from the remote command
    let address = match (namespace, container) {
      (Some(ns), Some(c)) => format!("{ns}/{pod}/{c}"),
      (Some(ns), None) => format!("{ns}/{pod}"),
      (None, Some(c)) => format!("{pod}/{c}"),
      (None, None) => pod.to_string(),
    };
    Self::new("k8s", address, kubectl_program(), prefix)
  }

  /// `docker exec -i <container> <cmd>` (docker takes no `--` separator).
  pub fn docker(container: &str) -> Self {
    let prefix = vec!["exec".to_string(), "-i".to_string(), container.to_string()];
    Self::new("docker", container.to_string(), docker_program(), prefix)
  }

  /// The full local invocation for `spec`: `[program, prefix…, remote-command…]`. The remote
  /// command is the spec's argv, or `sh -c <line>` for a shell spec; any `ExecSpec` env is applied
  /// **remotely** via an `env K=V …` prefix (setting it on the local CLI process would not reach
  /// the node). Split out so it can be asserted directly in tests.
  fn invocation(&self, spec: &ExecSpec) -> Vec<String> {
    let mut argv = Vec::with_capacity(self.prefix.len() + 4);
    argv.push(self.program.clone());
    argv.extend(self.prefix.iter().cloned());
    if !spec.env.is_empty() {
      argv.push("env".into());
      for (k, v) in &spec.env {
        argv.push(format!("{k}={v}"));
      }
    }
    match &spec.command {
      Command::Argv(a) => argv.extend(a.iter().cloned()),
      Command::Shell(line) => {
        argv.push("sh".into());
        argv.push("-c".into());
        argv.push(line.clone());
      }
    }
    argv
  }

  fn build_command(&self, spec: &ExecSpec) -> std::process::Command {
    let argv = self.invocation(spec);
    // argv[0] is the program; the rest are its arguments.
    let mut cmd = std::process::Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    cmd
  }
}

/// The kubectl binary, overridable for tests / non-standard installs.
fn kubectl_program() -> String {
  std::env::var("VORPAL_KUBECTL").unwrap_or_else(|_| "kubectl".into())
}

/// The docker binary, overridable likewise.
fn docker_program() -> String {
  std::env::var("VORPAL_DOCKER").unwrap_or_else(|_| "docker".into())
}

#[async_trait]
impl Transport for CommandTransport {
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

  #[test]
  fn kubectl_invocation_has_the_right_shape() {
    let t = CommandTransport::kubectl(Some("prod"), "web-0", Some("app"));
    let argv = t.invocation(&ExecSpec::argv(["echo", "hi"]));
    assert_eq!(
      argv,
      vec!["kubectl", "exec", "-i", "-n", "prod", "-c", "app", "web-0", "--", "echo", "hi"]
    );
    assert_eq!(t.descriptor().scheme, "k8s");
    assert_eq!(t.descriptor().address, "prod/web-0/app");
    // A shell spec is wrapped in `sh -c`.
    let argv = t.invocation(&ExecSpec::shell("cat > /tmp/x".into()));
    assert_eq!(&argv[argv.len() - 3..], &["sh", "-c", "cat > /tmp/x"]);
    // No namespace / container ⇒ they are omitted.
    let bare = CommandTransport::kubectl(None, "pod1", None);
    assert_eq!(bare.invocation(&ExecSpec::argv(["true"])), vec!["kubectl", "exec", "-i", "pod1", "--", "true"]);
  }

  #[test]
  fn docker_invocation_has_no_dash_dash_and_carries_remote_env() {
    let t = CommandTransport::docker("mycontainer");
    let argv = t.invocation(&ExecSpec::argv(["id"]).env("TOKEN", "secret123"));
    // env is applied *remotely* via `env K=V`, never on the local docker process.
    assert_eq!(argv, vec!["docker", "exec", "-i", "mycontainer", "env", "TOKEN=secret123", "id"]);
    assert_eq!(t.descriptor().scheme, "docker");
  }

  /// End-to-end exec through the generic transport, using a stub that emulates a `kubectl exec`-style
  /// wrapper (skip the CLI's own args up to `--`, then run the remainder locally). This exercises the
  /// real spawn/stdio/exit-status path the k8s/docker backends use.
  #[tokio::test]
  async fn command_transport_execs_through_a_wrapper_stub() {
    let dir = tempfile::tempdir().unwrap();
    let stub = dir.path().join("wrap.sh");
    // Skip everything up to and including `--`, then exec the rest — a faithful stand-in for how
    // `kubectl exec … -- cmd` runs `cmd` on the node.
    std::fs::write(&stub, "#!/bin/sh\nwhile [ \"$1\" != \"--\" ]; do shift; done\nshift\nexec \"$@\"\n").unwrap();
    #[cfg(unix)]
    {
      use std::os::unix::fs::PermissionsExt;
      std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    // Prefix mimics `kubectl exec -i pod --`; the stub ignores it and runs the remote command.
    let t = CommandTransport::new(
      "k8s",
      "ns/pod",
      stub.to_str().unwrap(),
      vec!["exec".into(), "-i".into(), "pod".into(), "--".into()],
    );

    let mut proc = t.exec(&ExecSpec::argv(["printf", "through-the-wrapper"])).await.unwrap();
    let mut out = String::new();
    proc.stdout.take().unwrap().read_to_string(&mut out).await.unwrap();
    assert_eq!(out, "through-the-wrapper");
    assert!(proc.wait().await.unwrap().success());

    // Exit status is forwarded from the remote command.
    let out = t.exec_capture(&ExecSpec::shell("exit 7".into())).await.unwrap();
    assert_eq!(out.status.code, Some(7));

    // push_file (the default `cat > dest`) works over the wrapped exec's piped stdin.
    let dest = dir.path().join("landed.bin");
    let mut body = &b"pushed-through-wrapper"[..];
    t.push_file(dest.to_str().unwrap(), 0o600, 22, &mut body).await.unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), b"pushed-through-wrapper");
  }
}
