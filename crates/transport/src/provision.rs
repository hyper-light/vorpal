//! Agent provisioning (docs/REMOTE.md §2): land the agent binary on a node and hand back the path
//! to exec. R1 does a plain push (`cat > landing/agent && chmod 0700`); the signed stage-0
//! loader (Ed25519+blake3 → memfd/fexecve, zero-residue) layers on top in a later step.
//!
//! Residue: the pushed file is removed by [`Provisioned::cleanup`] after the session; where the
//! session runs the binary and then unlinks it, the running inode keeps it alive with nothing left
//! on disk. The doc's true zero-*persistent*-residue guarantee (memfd) is a hardening follow-up.

use crate::negotiate::LandingSpot;
use crate::process::ExecSpec;
use crate::{Transport, TransportError};

/// A binary landed on a node: the remote path to exec, plus the transport-agnostic cleanup.
pub struct Provisioned {
  pub remote_path: String,
}

impl Provisioned {
  /// Best-effort removal of the pushed file.
  pub async fn cleanup(&self, transport: &dyn Transport) {
    let _ = transport
      .exec_capture(&ExecSpec::shell(format!("rm -f {}", crate::shell_quote(&self.remote_path))))
      .await;
  }
}

/// Push `bytes` to a fresh path under `landing` and mark it executable. `name_hint` seeds the
/// filename (the caller supplies a unique suffix — no `Date`/`rand` in this crate).
pub async fn push_agent(
  transport: &dyn Transport,
  landing: &LandingSpot,
  name_hint: &str,
  unique: u64,
  bytes: &[u8],
) -> Result<Provisioned, TransportError> {
  let remote_path = format!("{}/.vorpal-agent-{name_hint}-{unique}", landing.dir());
  let mut body = bytes;
  transport.push_file(&remote_path, 0o700, bytes.len() as u64, &mut body).await?;
  Ok(Provisioned { remote_path })
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::SubprocessTransport;

  #[tokio::test]
  async fn push_agent_lands_an_executable_and_runs_it() {
    let dir = tempfile::tempdir().unwrap();
    let landing = LandingSpot::Tmp(dir.path().to_str().unwrap().to_string());
    let t = SubprocessTransport::new("sh", vec![]);
    // A tiny "agent" script that just echoes.
    let script = b"#!/bin/sh\necho provisioned-ok\n";
    let p = push_agent(&t, &landing, "test", 42, script).await.unwrap();
    // It exists and is executable — run it.
    let out = t
      .exec_capture(&ExecSpec::shell(crate::shell_quote(&p.remote_path)))
      .await
      .unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "provisioned-ok");
    p.cleanup(&t).await;
    assert!(!std::path::Path::new(&p.remote_path).exists(), "cleanup removes the pushed file");
  }
}
