//! Least-privilege remote policy (docs/REMOTE.md §6), consulted before every connect/provision
//! step. Any out-of-policy node is refused before a byte moves.

use std::collections::BTreeSet;
use std::fmt;

/// The transport backends a run may use.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Backend {
  Subprocess,
  Ssh,
  K8s,
  Docker,
  Containerd,
  Vsock,
  Command,
}

impl Backend {
  pub fn scheme(self) -> &'static str {
    match self {
      Backend::Subprocess => "loopback",
      Backend::Ssh => "ssh",
      Backend::K8s => "k8s",
      Backend::Docker => "docker",
      Backend::Containerd => "containerd",
      Backend::Vsock => "vsock",
      Backend::Command => "cmd",
    }
  }
}

/// A least-privilege grant. Separate `allow_push_exec` (run a pushed binary) from byte-read
/// (streaming only needs read), plus backend/host allowlists and fan-out caps.
#[derive(Clone, Debug)]
pub struct RemotePolicy {
  /// Backends permitted this run (empty = all — the permissive default for a local dev tool).
  pub allowed_backends: BTreeSet<Backend>,
  /// May the coordinator push and execute a binary on a node? (Agent mode needs this; streaming
  /// mode does not.) `false` forces stream mode everywhere.
  pub allow_push_exec: bool,
  /// Host allow/deny lists (SSH). Empty allow = any; deny always wins.
  pub host_allow: BTreeSet<String>,
  pub host_deny: BTreeSet<String>,
  /// Max nodes and per-node in-flight streams.
  pub max_nodes: usize,
  pub max_streams_per_node: usize,
}

impl Default for RemotePolicy {
  fn default() -> Self {
    // Permissive by default — this is an operator-run dev tool on a laptop-local cluster. The gate
    // that actually matters is the explicit `--remote <target>` opt-in (nothing connects without
    // it); the policy is here so a deployment can tighten it.
    Self {
      allowed_backends: BTreeSet::new(),
      allow_push_exec: true,
      host_allow: BTreeSet::new(),
      host_deny: BTreeSet::new(),
      max_nodes: 4096,
      max_streams_per_node: 64,
    }
  }
}

impl RemotePolicy {
  /// Refuse a backend that is not permitted.
  pub fn check_backend(&self, backend: Backend) -> Result<(), String> {
    if self.allowed_backends.is_empty() || self.allowed_backends.contains(&backend) {
      Ok(())
    } else {
      Err(format!("backend {} is not permitted by policy", backend.scheme()))
    }
  }

  /// Refuse a host that is denied, or not allowed when an allowlist is set.
  pub fn check_host(&self, host: &str) -> Result<(), String> {
    if self.host_deny.contains(host) {
      return Err(format!("host {host} is on the deny list"));
    }
    if !self.host_allow.is_empty() && !self.host_allow.contains(host) {
      return Err(format!("host {host} is not on the allow list"));
    }
    Ok(())
  }
}

/// A newtype whose `Debug`/`Display` never reveal the wrapped secret — for tokens, keys, passwords
/// in logs/errors.
#[derive(Clone)]
pub struct Redacted<T>(pub T);

impl<T> Redacted<T> {
  pub fn expose(&self) -> &T {
    &self.0
  }
}

impl<T> fmt::Debug for Redacted<T> {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str("<redacted>")
  }
}

impl<T> fmt::Display for Redacted<T> {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str("<redacted>")
  }
}
