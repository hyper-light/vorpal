//! Remote fan-out (docs/REMOTE.md): run scan/search on remote nodes without installing vorpal
//! there, streaming already-rendered result fragments back into the *same* consumer channel and
//! printers a local run uses.
//!
//! R0 scope: the wire protocol, the real agent (`vorpal-agent` / the hidden `__agent` self-exec
//! mode), a `loopback://` subprocess transport, and both execution modes — **agent** (the node
//! runs the real engine and ships rendered fragments) and **stream** (the coordinator reconstructs
//! `ignore`-crate discovery from a raw enumeration and matches wire-delivered content locally).
//! The gate is differential: `vorpal scan --remote loopback://` must equal a local `vorpal scan`.
//!
//! Authorization note: remote execution is deliberately explicit — nothing connects anywhere
//! unless the operator passes `--remote` with concrete targets. R0's only transport is
//! `loopback://` (a local child process).

pub(crate) mod agent;
pub(crate) mod discovery;
pub(crate) mod fingerprint;
pub(crate) mod producer;
pub(crate) mod remote_stream;
pub(crate) mod rules_wire;
pub(crate) mod session;
pub(crate) mod spec;
#[cfg(test)]
pub(crate) mod testserver;

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::Result;
use clap::{Args, ValueEnum};

/// Agent-side production that also reports each rendered fragment's **match count**, so the
/// coordinator can enforce a global `--max-results` across nodes without decoding opaque fragment
/// bytes (docs/REMOTE.md §3.1). The count is the number of matches rendered into that fragment;
/// local production drops it (`produce_item`), the agent ships it as `Rendered.match_count`.
pub(crate) trait CountedProduce {
  fn produce_counted<P: Printer>(
    &self,
    path: &Path,
    processor: &P::Processor,
  ) -> Result<Vec<(P::Processed, u32)>>;
}

use crate::config::ProjectConfig;
use crate::print::{
  CloudPrinter, ColoredPrinter, FileNamePrinter, JSONPrinter, Printer, WireFragment,
};
use crate::run::{RunArg, RunPrinterKind};
use crate::scan::{ScanArg, ScanPrinterKind, ScanWithConfig};
use crate::utils::ErrorContext as EC;
use crate::utils::run_producer;

/// argv[1] sentinel that turns the main `vorpal` binary into the agent (loopback default: the
/// coordinator re-executes itself, so no second binary is required).
pub const AGENT_ARG: &str = "__agent";

/// This build's exact version, for the handshake gate (I2). An unparseable version is a hard
/// error: a shared fallback sentinel would make the exact-match gate vacuously pass between
/// genuinely different builds — the precise silent divergence the gate exists to prevent.
pub(crate) fn current_version() -> Result<vorpal_wire::SemVer> {
  vorpal_wire::SemVer::parse(env!("CARGO_PKG_VERSION")).ok_or_else(|| {
    anyhow::anyhow!(
      "this build's version `{}` is not parseable for the remote handshake",
      env!("CARGO_PKG_VERSION")
    )
  })
}

/// How work executes on a node (docs/REMOTE.md D1).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub enum RemoteMode {
  /// Negotiate: push/exec an agent where possible, else stream bytes. Loopback always can exec.
  #[default]
  Auto,
  /// Require agent mode: the node runs the real engine and ships rendered fragments.
  Agent,
  /// Require streaming mode: the node only ships bytes; the coordinator reconstructs discovery
  /// (I1) and runs the match pipeline itself.
  Stream,
}

/// Remote fan-out options, flattened into `scan` and `run`.
#[derive(Args, Clone, Default)]
pub struct RemoteArgs {
  /// Execute this command on remote nodes instead of the local filesystem.
  ///
  /// May be passed multiple times. R0 supports `loopback://` (spawn a local agent subprocess —
  /// the differential-testing transport every other transport must match).
  #[clap(long = "remote", value_name = "TARGET", action = clap::ArgAction::Append)]
  pub targets: Vec<String>,

  /// How work is executed on nodes.
  #[clap(long = "remote-mode", value_name = "MODE", default_value = "auto")]
  pub mode: RemoteMode,

  /// Path to a `vorpal-agent` binary to spawn for exec-capable targets.
  ///
  /// Defaults to re-executing the current binary in agent mode.
  #[clap(long = "agent-binary", value_name = "PATH")]
  pub agent_binary: Option<PathBuf>,

  /// Report success even if some remote nodes fail before finishing.
  ///
  /// By default a node that dies mid-scan makes the run *incomplete* (exit code 4), so
  /// "clean, no error matches" stays provably distinct from "a node died" (docs/REMOTE.md §3.4).
  #[clap(long = "remote-allow-partial")]
  pub allow_partial: bool,

  /// SSH login user (fallback when the `ssh://user@…` URI omits it; else `$USER`).
  #[clap(long = "ssh-user", value_name = "USER")]
  pub ssh_user: Option<String>,

  /// SSH private key file (OpenSSH format). If unset, common `~/.ssh/id_*` keys are tried.
  #[clap(long = "ssh-key", value_name = "PATH")]
  pub ssh_key: Option<PathBuf>,

  /// Read the SSH password from this environment variable (never passed on the command line).
  #[clap(long = "ssh-password-env", value_name = "VAR")]
  pub ssh_password_env: Option<String>,

  /// Read the SSH key passphrase from this environment variable.
  #[clap(long = "ssh-key-passphrase-env", value_name = "VAR")]
  pub ssh_key_passphrase_env: Option<String>,

  /// Push this local agent binary to each exec-capable node and run it (agent mode), instead of
  /// assuming the agent is already installed there.
  ///
  /// The coordinator probes the node for a writable+executable landing spot, streams the binary
  /// in, runs it, and removes it afterward.
  #[clap(long = "push-agent", value_name = "PATH")]
  pub push_agent: Option<PathBuf>,

  /// Deliver `--push-agent` via this stage-0 `vorpal-loader` binary instead of a plain push: the
  /// coordinator pushes the (tiny) loader, then streams the Ed25519-signed agent to it, and the
  /// loader verifies + execs it from memory — the multi-MB agent never lands on the node's disk
  /// (docs/REMOTE.md §2, §6). Requires `--push-agent`.
  #[clap(long = "loader", value_name = "PATH", requires = "push_agent")]
  pub loader: Option<PathBuf>,
}

impl RemoteArgs {
  pub fn is_remote(&self) -> bool {
    !self.targets.is_empty()
  }
}

/// A parsed remote target. The URI grammar is shared across transports; vsock/containerd arrive in
/// later phases.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
  /// `loopback://` — a local child process (the differential-testing reference).
  Loopback,
  /// `ssh://[user@]host[:port]` — a node reached over SSH.
  Ssh(SshUri),
  /// `k8s://[ns/]pod[/container]` — a pod reached via `kubectl exec`.
  K8s(K8sTarget),
  /// `docker://container` — a container reached via `docker exec`.
  Docker(DockerTarget),
}

/// The parts of a `k8s://` target. Namespace defaults to the kubeconfig's current context when
/// omitted; container defaults to the pod's default container.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct K8sTarget {
  pub namespace: Option<String>,
  pub pod: String,
  pub container: Option<String>,
}

/// A `docker://` target — the container name or id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DockerTarget {
  pub container: String,
}

/// The address parts of an `ssh://` target. Auth and host-key policy come from `RemoteArgs`, not
/// the URI (a password never belongs in a URI/argv).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SshUri {
  pub user: Option<String>,
  pub host: String,
  pub port: u16,
}

/// Parse `ssh://[user@]host[:port]` (default port 22), including bracketed IPv6
/// (`ssh://[::1]:22`). Rejects empty host and a bad port.
fn parse_ssh_uri(rest: &str) -> Result<SshUri> {
  let (user, hostport) = match rest.split_once('@') {
    Some((u, hp)) if !u.is_empty() => (Some(u.to_string()), hp),
    _ => (None, rest),
  };
  let bad_port = |p: &str| {
    anyhow::anyhow!(EC::RemoteInvalid(format!(
      "invalid ssh port `{p}` in `ssh://{rest}`"
    )))
  };
  let (host, port) = if let Some(after) = hostport.strip_prefix('[') {
    // Bracketed IPv6: `[addr]` or `[addr]:port`.
    let (addr, tail) = after.split_once(']').ok_or_else(|| {
      anyhow::anyhow!(EC::RemoteInvalid(format!("unclosed `[` in `ssh://{rest}`")))
    })?;
    let port = match tail.strip_prefix(':') {
      Some(p) => p.parse::<u16>().map_err(|_| bad_port(p))?,
      None if tail.is_empty() => 22,
      None => return Err(bad_port(tail)),
    };
    (addr.to_string(), port)
  } else {
    match hostport.rsplit_once(':') {
      Some((h, p)) => (h.to_string(), p.parse::<u16>().map_err(|_| bad_port(p))?),
      None => (hostport.to_string(), 22),
    }
  };
  if host.is_empty() {
    return Err(anyhow::anyhow!(EC::RemoteInvalid(format!(
      "missing host in `ssh://{rest}`"
    ))));
  }
  Ok(SshUri { user, host, port })
}

/// Parse `k8s://[namespace/]pod[/container]` (1–3 slash-separated components: `pod`, `ns/pod`, or
/// `ns/pod/container`).
fn parse_k8s_uri(rest: &str) -> Result<K8sTarget> {
  let parts: Vec<&str> = rest
    .trim_matches('/')
    .split('/')
    .filter(|p| !p.is_empty())
    .collect();
  let invalid = || {
    anyhow::anyhow!(EC::RemoteInvalid(format!(
      "invalid k8s target `k8s://{rest}` (expected `[namespace/]pod[/container]`)"
    )))
  };
  match parts.as_slice() {
    [pod] => Ok(K8sTarget {
      namespace: None,
      pod: (*pod).to_string(),
      container: None,
    }),
    [ns, pod] => Ok(K8sTarget {
      namespace: Some((*ns).to_string()),
      pod: (*pod).to_string(),
      container: None,
    }),
    [ns, pod, ctr] => Ok(K8sTarget {
      namespace: Some((*ns).to_string()),
      pod: (*pod).to_string(),
      container: Some((*ctr).to_string()),
    }),
    _ => Err(invalid()),
  }
}

/// SSH dial options gathered from the `--ssh-*`/`--push-agent` flags.
fn ssh_dial_opts(remote: &RemoteArgs) -> producer::SshDialOpts {
  producer::SshDialOpts {
    user: remote.ssh_user.clone(),
    key: remote.ssh_key.clone(),
    password_env: remote.ssh_password_env.clone(),
    key_passphrase_env: remote.ssh_key_passphrase_env.clone(),
    push_agent: remote.push_agent.clone(),
    loader: remote.loader.clone(),
  }
}

pub(crate) fn parse_targets(raw: &[String]) -> Result<Vec<Target>> {
  let mut targets: Vec<Target> = Vec::with_capacity(raw.len());
  for t in raw {
    let trimmed = t.trim_end_matches('/');
    let target = if trimmed == "loopback:" || trimmed == "loopback" {
      Target::Loopback
    } else if let Some(rest) = t.strip_prefix("ssh://") {
      Target::Ssh(parse_ssh_uri(rest)?)
    } else if let Some(rest) = t.strip_prefix("k8s://") {
      Target::K8s(parse_k8s_uri(rest)?)
    } else if let Some(rest) = t.strip_prefix("docker://") {
      let container = rest.trim_matches('/');
      if container.is_empty() {
        return Err(anyhow::anyhow!(EC::RemoteInvalid(format!(
          "missing container in `{t}`"
        ))));
      }
      Target::Docker(DockerTarget {
        container: container.to_string(),
      })
    } else {
      return Err(anyhow::anyhow!(EC::RemoteInvalid(format!(
        "unsupported target `{trimmed}` (supported: `loopback://`, `ssh://[user@]host[:port]`, `k8s://[ns/]pod[/ctr]`, `docker://container`; vsock/containerd arrive in later phases)"
      ))));
    };
    // Identical targets are one node, not N copies of its results: duplicating them would print
    // every match once per copy in agent mode (and inflate error counts feeding the exit code),
    // while stream mode would collapse them anyway — collapse consistently, and say so.
    if targets.contains(&target) {
      eprintln!("note: duplicate remote target `{t}` collapsed (identical targets are one node)");
    } else {
      targets.push(target);
    }
  }
  Ok(targets)
}

fn reject_unsupported_flags(interactive: bool, stdin: bool) -> Result<()> {
  if interactive {
    return Err(anyhow::anyhow!(EC::RemoteInvalid(
      "--interactive/--update-all edit local files and cannot run remotely; run the rewrite on the node or drop --remote".into(),
    )));
  }
  if stdin {
    return Err(anyhow::anyhow!(EC::RemoteInvalid(
      "--stdin conflicts with --remote".into()
    )));
  }
  Ok(())
}

/// Surface user-input errors exactly as a local run would, **before** any fan-out. Without this,
/// a bad `--globs` pattern (which locally fails synchronously with `BuildGlobs`) would reach the
/// node, fail there, and be misdiagnosed as a node death (`RemoteIncomplete`, exit 4).
fn validate_walk_inputs(input: &crate::utils::InputArgs) -> Result<()> {
  use anyhow::Context as _;
  input.build_globs().map(|_| ()).context(EC::BuildGlobs)
}

/// Remote `scan` entry: owns printer dispatch because remote requires `Processed: WireFragment`
/// (every non-interactive printer qualifies; interactive is rejected above the ladder).
pub fn scan_remote_dispatch(arg: ScanArg, project: Result<ProjectConfig>) -> Result<ExitCode> {
  reject_unsupported_flags(arg.output.needs_interactive(), arg.input.stdin)?;
  validate_walk_inputs(&arg.input)?;
  let targets = parse_targets(&arg.remote.targets)?;
  let context = arg.context.get();
  // Same selection as the local ladder (`ScanArg::printer_kind`) — every non-interactive printer's
  // output is a relocatable `WireFragment`; interactive is rejected above.
  match arg.printer_kind() {
    ScanPrinterKind::FilesWithMatches => {
      let printer = FileNamePrinter::stdout(arg.output.color);
      scan_remote(arg, printer, project, targets)
    }
    ScanPrinterKind::Cloud(format) => {
      let printer = CloudPrinter::stdout(format);
      scan_remote(arg, printer, project, targets)
    }
    ScanPrinterKind::Json(json) => {
      let printer = JSONPrinter::stdout(json).include_metadata(arg.include_metadata);
      scan_remote(arg, printer, project, targets)
    }
    ScanPrinterKind::Colored => {
      let printer = ColoredPrinter::stdout(arg.output.color)
        .style(arg.report_style)
        .context(context);
      scan_remote(arg, printer, project, targets)
    }
  }
}

fn scan_remote<P>(
  arg: ScanArg,
  printer: P,
  project: Result<ProjectConfig>,
  targets: Vec<Target>,
) -> Result<ExitCode>
where
  P: Printer + 'static,
  P::Processed: WireFragment,
{
  // Capture what the job must ship *before* the worker consumes `arg`/`project`.
  let lang_env = rules_wire::LangEnv::from_project(project.as_ref().ok());
  // Project utils feed the rule set only when the rules themselves come from the project:
  // `--rule`/`--inline-rules` compile locally against empty globals (`ScanWithConfig::try_new`),
  // and shipping project utils anyway would make the remote run read — and possibly fail on —
  // project state the equivalent local run never touches.
  let globals_yaml = match project.as_ref().ok() {
    Some(p) if arg.uses_project_rules() => crate::config::collect_util_yaml(p)?,
    _ => vec![],
  };
  let mode = arg.remote.mode;
  let agent_binary = arg.remote.agent_binary.clone();
  let ssh = ssh_dial_opts(&arg.remote);
  let allow_partial = arg.remote.allow_partial;
  // Global `--max-results` counter, claimed across every node's fragments (agent mode; §3.1).
  let global_max = arg
    .max_results
    .map(|n| Arc::new(crate::utils::MaxItemCounter::new(n)));
  let worker = Arc::new(ScanWithConfig::try_new(arg, project)?);
  let job = spec::build_scan_job(&worker, &lang_env, globals_yaml)?;

  let outcome = producer::NodeOutcomes::default();
  outcome.set_allow_partial(allow_partial);
  // Fold each node's terminal stats into the same counters a local scan feeds: error-severity
  // matches drive the exit code, scanned/skipped drive `--inspect` (§3.4). Used by the agent-
  // bearing producers (auto/agent); stream nodes update the shared worker trace directly.
  let on_done = {
    let w = worker.clone();
    Box::new(move |stats: &vorpal_wire::FinalStats| {
      w.add_remote_error_count(stats.error_count as usize);
      crate::utils::PathWorker::get_trace(&*w)
        .add_remote(stats.scanned as usize, stats.skipped as usize);
    }) as producer::DoneSink
  };
  let ret = match mode {
    RemoteMode::Auto => {
      // Negotiate per node: agent where it can exec, stream where it can't (§2, D1).
      let producer = producer::NegotiatingProducer::<P>::new(
        targets,
        job,
        producer::StreamWorker::Scan(worker.clone()),
        agent_binary,
        ssh,
        outcome.clone(),
        Some(on_done),
        global_max,
      );
      run_producer(worker.clone(), Box::new(producer), printer)
    }
    RemoteMode::Agent => {
      let producer = producer::AgentModeProducer::new(
        targets,
        job,
        agent_binary,
        ssh,
        outcome.clone(),
        Some(on_done),
        global_max,
      );
      run_producer(worker.clone(), Box::new(producer), printer)
    }
    RemoteMode::Stream => {
      let producer = producer::StreamModeProducer::new(
        targets,
        producer::StreamWorker::Scan(worker.clone()),
        ssh,
        agent_binary,
        outcome.clone(),
      );
      run_producer(worker.clone(), Box::new(producer), printer)
    }
  };
  outcome.into_final(ret)
}

/// Remote `run` entry (same shape as scan; pattern jobs ship a `RunJob`).
pub fn run_remote_dispatch(arg: RunArg, project: Result<ProjectConfig>) -> Result<ExitCode> {
  reject_unsupported_flags(arg.output.needs_interactive(), arg.input.stdin)?;
  validate_walk_inputs(&arg.input)?;
  let targets = parse_targets(&arg.remote.targets)?;
  let context = arg.context.get();
  match arg.printer_kind() {
    RunPrinterKind::FilesWithMatches => {
      let printer = FileNamePrinter::stdout(arg.output.color);
      run_remote(arg, printer, project, targets)
    }
    RunPrinterKind::Json(json) => {
      let printer = JSONPrinter::stdout(json).context(context);
      run_remote(arg, printer, project, targets)
    }
    RunPrinterKind::Colored => {
      let printer = ColoredPrinter::stdout(arg.output.color)
        .heading(arg.heading)
        .context(context);
      run_remote(arg, printer, project, targets)
    }
  }
}

fn run_remote<P>(
  arg: RunArg,
  printer: P,
  project: Result<ProjectConfig>,
  targets: Vec<Target>,
) -> Result<ExitCode>
where
  P: Printer + 'static,
  P::Processed: WireFragment,
{
  let lang_env = rules_wire::LangEnv::from_project(project.as_ref().ok());
  let mode = arg.remote.mode;
  let agent_binary = arg.remote.agent_binary.clone();
  let job = spec::build_run_job(&arg, &lang_env)?;
  let trace = arg.output.inspect.run_trace();
  let outcome = producer::NodeOutcomes::default();
  outcome.set_allow_partial(arg.remote.allow_partial);
  // The coordinator constructs the same worker a local run would: its `consume_items` owns the
  // has-matches exit code and the pattern-has-error diagnosis, both of which must behave
  // identically whether items were produced locally or remotely.
  let ssh = ssh_dial_opts(&arg.remote);
  if arg.lang.is_some() {
    let worker = Arc::new(crate::run::RunWithSpecificLang::new(arg, trace)?);
    let stream_worker = producer::StreamWorker::RunSpecific(worker.clone());
    drive_remote(
      worker,
      stream_worker,
      printer,
      mode,
      targets,
      job,
      agent_binary,
      ssh,
      outcome,
    )
  } else {
    let worker = Arc::new(crate::run::RunWithInferredLang { arg, trace });
    let stream_worker = producer::StreamWorker::RunInferred(worker.clone());
    drive_remote(
      worker,
      stream_worker,
      printer,
      mode,
      targets,
      job,
      agent_binary,
      ssh,
      outcome,
    )
  }
}

/// Shared remote-run driver: wires the chosen worker into either producer and folds each node's
/// terminal stats into the worker's trace (`--inspect` parity, §3.4).
#[allow(clippy::too_many_arguments)]
fn drive_remote<W, P>(
  worker: Arc<W>,
  stream_worker: producer::StreamWorker,
  printer: P,
  mode: RemoteMode,
  targets: Vec<Target>,
  job: vorpal_wire::JobSpec,
  agent_binary: Option<PathBuf>,
  ssh: producer::SshDialOpts,
  outcome: producer::NodeOutcomes,
) -> Result<ExitCode>
where
  W: crate::utils::PathWorker + 'static,
  P: Printer + 'static,
  P::Processed: WireFragment,
{
  let on_done = {
    let w = worker.clone();
    Box::new(move |stats: &vorpal_wire::FinalStats| {
      crate::utils::PathWorker::get_trace(&*w)
        .add_remote(stats.scanned as usize, stats.skipped as usize);
    }) as producer::DoneSink
  };
  let ret = match mode {
    RemoteMode::Auto => {
      // Negotiate per node: agent where it can exec, stream where it can't (§2, D1). `run` has no
      // `--max-results`, so there is no global cap to enforce.
      let producer = producer::NegotiatingProducer::<P>::new(
        targets,
        job,
        stream_worker,
        agent_binary,
        ssh,
        outcome.clone(),
        Some(on_done),
        None,
      );
      run_producer(worker, Box::new(producer), printer)
    }
    RemoteMode::Agent => {
      let producer = producer::AgentModeProducer::new(
        targets,
        job,
        agent_binary,
        ssh,
        outcome.clone(),
        Some(on_done),
        // `run` has no `--max-results`, so there is no global cap to enforce.
        None,
      );
      run_producer(worker, Box::new(producer), printer)
    }
    RemoteMode::Stream => {
      let producer = producer::StreamModeProducer::new(
        targets,
        stream_worker,
        ssh,
        agent_binary,
        outcome.clone(),
      );
      run_producer(worker, Box::new(producer), printer)
    }
  };
  outcome.into_final(ret)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn loopback_targets_parse_and_duplicates_collapse() {
    assert_eq!(
      parse_targets(&["loopback://".into()]).unwrap(),
      vec![Target::Loopback]
    );
    // Identical targets are one node: duplicating them would duplicate every result in agent
    // mode while stream mode collapses them — collapse consistently at parse time.
    assert_eq!(
      parse_targets(&["loopback:".into(), "loopback://".into()]).unwrap(),
      vec![Target::Loopback]
    );
    assert!(
      parse_targets(&["vsock://3:9000".into()]).is_err(),
      "vsock arrives in a later phase"
    );
  }

  #[test]
  fn ssh_uris_parse() {
    let one = |s: &str| parse_targets(&[s.to_string()]).unwrap().pop().unwrap();
    assert_eq!(
      one("ssh://host"),
      Target::Ssh(SshUri {
        user: None,
        host: "host".into(),
        port: 22
      })
    );
    assert_eq!(
      one("ssh://alice@host:2222"),
      Target::Ssh(SshUri {
        user: Some("alice".into()),
        host: "host".into(),
        port: 2222
      })
    );
    // Bracketed IPv6, with and without a port.
    assert_eq!(
      one("ssh://[::1]:22"),
      Target::Ssh(SshUri {
        user: None,
        host: "::1".into(),
        port: 22
      })
    );
    assert_eq!(
      one("ssh://user@[fe80::1]"),
      Target::Ssh(SshUri {
        user: Some("user".into()),
        host: "fe80::1".into(),
        port: 22
      })
    );
    assert!(parse_targets(&["ssh://host:notaport".into()]).is_err());
    assert!(parse_targets(&["ssh://".into()]).is_err(), "missing host");
  }

  #[test]
  fn k8s_and_docker_uris_parse() {
    let one = |s: &str| parse_targets(&[s.to_string()]).unwrap().pop().unwrap();
    // k8s: pod / ns/pod / ns/pod/container.
    assert_eq!(
      one("k8s://web-0"),
      Target::K8s(K8sTarget {
        namespace: None,
        pod: "web-0".into(),
        container: None
      })
    );
    assert_eq!(
      one("k8s://prod/web-0"),
      Target::K8s(K8sTarget {
        namespace: Some("prod".into()),
        pod: "web-0".into(),
        container: None
      })
    );
    assert_eq!(
      one("k8s://prod/web-0/app"),
      Target::K8s(K8sTarget {
        namespace: Some("prod".into()),
        pod: "web-0".into(),
        container: Some("app".into()),
      })
    );
    assert_eq!(
      one("docker://mybox"),
      Target::Docker(DockerTarget {
        container: "mybox".into()
      })
    );
    assert!(
      parse_targets(&["k8s://a/b/c/d".into()]).is_err(),
      "too many k8s components"
    );
    assert!(
      parse_targets(&["k8s://".into()]).is_err(),
      "empty k8s target"
    );
    assert!(
      parse_targets(&["docker://".into()]).is_err(),
      "empty docker container"
    );
  }
}
