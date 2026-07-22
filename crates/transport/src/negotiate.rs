//! Node negotiation (docs/REMOTE.md §2): a one-round-trip probe reports what a node can do
//! (arch/OS, an exec-capable landing spot, primitive inventory), and [`decide`] fuses that with
//! the requested mode + policy into an [`ExecMode`]. Agent mode where a node can exec a pushed
//! binary; byte-stream where it can't.

use crate::process::ExecSpec;
use crate::{RemotePolicy, Transport, TransportError};

/// Where a pushed binary can be written **and executed** on a node, best first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LandingSpot {
  /// A tmpfs that is exec-capable — leaves zero persistent residue.
  DevShm,
  XdgRuntime(String),
  /// A writable+exec directory (usually `/tmp`).
  Tmp(String),
}

impl LandingSpot {
  pub fn dir(&self) -> &str {
    match self {
      LandingSpot::DevShm => "/dev/shm",
      LandingSpot::XdgRuntime(d) | LandingSpot::Tmp(d) => d,
    }
  }
}

/// The result of probing a node.
#[derive(Clone, Debug)]
pub struct NodeProbe {
  pub os: String,   // `uname -s`, lowercased (linux/darwin/…)
  pub arch: String, // `uname -m` (x86_64/aarch64/…)
  /// First writable **and executable** landing spot found, if any (None ⇒ cannot exec a push).
  pub landing: Option<LandingSpot>,
  pub has_base64: bool,
  pub has_tar: bool,
}

/// Decided execution model for one node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExecMode {
  /// Push+run the agent, landing it here.
  Agent { landing: LandingSpot },
  /// The node ships bytes; the coordinator reconstructs and matches.
  Stream,
}

/// The user's `--remote-mode`, mapped for the decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ForcedMode {
  Auto,
  Agent,
  Stream,
}

/// The one-round-trip probe script. Reports OS/arch, the first writable+executable dir among a
/// preference list (writing a tiny script and running it — this catches `noexec` mounts), and a
/// primitive inventory. Portable POSIX sh.
const PROBE_SCRIPT: &str = r#"
printf 'OS %s\n' "$(uname -s 2>/dev/null | tr 'A-Z' 'a-z')"
printf 'ARCH %s\n' "$(uname -m 2>/dev/null)"
command -v base64 >/dev/null 2>&1 && printf 'HAS base64\n'
command -v tar >/dev/null 2>&1 && printf 'HAS tar\n'
for d in /dev/shm "$XDG_RUNTIME_DIR" /tmp; do
  [ -n "$d" ] && [ -d "$d" ] && [ -w "$d" ] || continue
  t="$d/.vorpal-probe.$$"
  printf '#!/bin/sh\necho ok\n' > "$t" 2>/dev/null || continue
  chmod 0700 "$t" 2>/dev/null
  if [ "$("$t" 2>/dev/null)" = ok ]; then rm -f "$t"; printf 'LAND %s\n' "$d"; break; fi
  rm -f "$t" 2>/dev/null
done
printf 'END\n'
"#;

/// Probe a node in one round trip.
pub async fn probe(transport: &dyn Transport) -> Result<NodeProbe, TransportError> {
  let out = transport
    .exec_capture(&ExecSpec::shell(format!("sh -c {}", crate::shell_quote(PROBE_SCRIPT))))
    .await?;
  let text = String::from_utf8_lossy(&out.stdout);
  let mut probe =
    NodeProbe { os: String::new(), arch: String::new(), landing: None, has_base64: false, has_tar: false };
  for line in text.lines() {
    let Some((tag, rest)) = line.split_once(' ') else { continue };
    match tag {
      "OS" => probe.os = rest.to_string(),
      "ARCH" => probe.arch = rest.to_string(),
      "HAS" => match rest {
        "base64" => probe.has_base64 = true,
        "tar" => probe.has_tar = true,
        _ => {}
      },
      "LAND" if probe.landing.is_none() => {
        probe.landing = Some(match rest {
          "/dev/shm" => LandingSpot::DevShm,
          d => LandingSpot::Tmp(d.to_string()),
        });
      }
      _ => {}
    }
  }
  Ok(probe)
}

/// Fuse the probe, the requested mode, and policy into a per-node execution model.
///
/// `--remote-mode agent` forces agent mode (erroring if the node cannot exec); `stream` forces
/// stream; `auto` picks agent when the node can exec a pushed binary and policy permits it, else
/// demotes to stream.
pub fn decide(
  probe: &NodeProbe,
  policy: &RemotePolicy,
  forced: ForcedMode,
) -> Result<ExecMode, TransportError> {
  let can_exec = policy.allow_push_exec && probe.landing.is_some();
  match forced {
    ForcedMode::Stream => Ok(ExecMode::Stream),
    ForcedMode::Agent => match &probe.landing {
      Some(landing) if policy.allow_push_exec => Ok(ExecMode::Agent { landing: landing.clone() }),
      Some(_) => Err(TransportError::policy("agent mode requires push+exec, which policy forbids")),
      None => Err(TransportError::unsupported(
        "agent mode requested but the node has no writable+executable landing spot (noexec?); use --remote-mode stream",
      )),
    },
    ForcedMode::Auto => {
      if can_exec {
        Ok(ExecMode::Agent { landing: probe.landing.clone().unwrap() })
      } else {
        Ok(ExecMode::Stream)
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::SubprocessTransport;

  #[tokio::test]
  async fn probe_reports_this_machine() {
    // A generic subprocess transport runs the probe as a local shell.
    let t = SubprocessTransport::new("sh", vec![]);
    let p = probe(&t).await.unwrap();
    assert!(!p.os.is_empty(), "uname -s should report something");
    assert!(!p.arch.is_empty());
    // Every dev machine has an exec-capable /tmp.
    assert!(p.landing.is_some(), "expected a writable+exec landing spot");
    assert!(p.has_base64, "base64 is expected on a dev machine");
  }

  #[test]
  fn decide_respects_mode_and_capability() {
    let policy = RemotePolicy::default();
    let can = NodeProbe {
      os: "linux".into(),
      arch: "x86_64".into(),
      landing: Some(LandingSpot::DevShm),
      has_base64: true,
      has_tar: true,
    };
    let cannot = NodeProbe { landing: None, ..can.clone() };

    assert!(matches!(decide(&can, &policy, ForcedMode::Auto).unwrap(), ExecMode::Agent { .. }));
    assert_eq!(decide(&cannot, &policy, ForcedMode::Auto).unwrap(), ExecMode::Stream);
    assert_eq!(decide(&can, &policy, ForcedMode::Stream).unwrap(), ExecMode::Stream);
    assert!(matches!(decide(&can, &policy, ForcedMode::Agent).unwrap(), ExecMode::Agent { .. }));
    assert!(decide(&cannot, &policy, ForcedMode::Agent).is_err(), "agent forced but noexec → error");
  }
}
