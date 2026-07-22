//! Coordinator-side producers (docs/REMOTE.md §3.1). Both plug into the existing
//! `Produce`/`run_producer` seam, funneling fragments into the same channel the local walk uses,
//! so `consume_items`, every printer, and the exit-code logic are shared verbatim.
//!
//! * [`AgentModeProducer`] spawns an agent per target (a `vorpal __agent` subprocess for
//!   loopback), handshakes, ships the job, and decodes inbound `Rendered` fragments straight back
//!   into `P::Processed` — the large network saving (the node did the rendering).
//! * [`StreamModeProducer`] is for nodes that cannot exec our binary: the coordinator
//!   reconstructs `ignore`-crate discovery (I1, `discovery.rs`) and runs the real match pipeline
//!   on wire-delivered content itself.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Result, anyhow};

use vorpal_transport::{
  CommandTransport, ExecMode, ExecSpec, ForcedMode, LandingSpot, RemotePolicy, SubprocessTransport,
  Transport, decide, probe,
};
use vorpal_wire::{FinalStats, JobSpec};

use crate::print::{Printer, WireFragment};
use crate::run::{RunWithInferredLang, RunWithSpecificLang};
use crate::scan::ScanWithConfig;
use crate::utils::{ChannelCap, ErrorContext as EC, ItemSink, MaxItemCounter, PathWorker, Produce};

use super::session;
use super::{AGENT_ARG, Target};

/// Coordinator-side channel window: the printer's drain rate paces the network end-to-end (§3.1),
/// so no more than this many rendered fragments buffer ahead of the single-threaded consumer.
const REMOTE_WINDOW: usize = 256;

/// Fold a remote node's terminal `Done` stats (error-severity count, scanned/skipped file
/// counts) into the coordinator worker's own accounting, so exit codes and `--inspect` output
/// match a local run (§3.4).
pub type DoneSink = Box<dyn Fn(&FinalStats) + Send + Sync>;

/// Per-node completion accounting for exit-code aggregation (§3.4). A dead/failed node makes the
/// run **incomplete**: reserved exit code 4 unless `--remote-allow-partial`, and it takes
/// precedence over a diagnostic-error exit.
#[derive(Clone, Default)]
pub struct NodeOutcomes {
  inner: Arc<Mutex<OutcomeInner>>,
}

#[derive(Default)]
struct OutcomeInner {
  failures: Vec<String>,
  allow_partial: bool,
}

impl NodeOutcomes {
  pub fn set_allow_partial(&self, allow: bool) {
    self.inner.lock().unwrap().allow_partial = allow;
  }

  pub fn record_failure(&self, detail: String) {
    self.inner.lock().unwrap().failures.push(detail);
  }

  /// Combine the local exit-code decision with remote completeness. `REMOTE_INCOMPLETE` wins over
  /// `DiagnosticError` (which is what `ret` may already carry), matching the doc's precedence.
  pub fn into_final(self, ret: Result<std::process::ExitCode>) -> Result<std::process::ExitCode> {
    let inner = self.inner.lock().unwrap();
    if !inner.failures.is_empty() && !inner.allow_partial {
      let detail = inner.failures.join("; ");
      return Err(anyhow!(EC::RemoteIncomplete(detail)));
    }
    if !inner.failures.is_empty() {
      // Partial results explicitly accepted: surface the failures on stderr, keep the local code.
      eprintln!(
        "warning: {} remote node(s) did not complete; results are partial:\n  {}",
        inner.failures.len(),
        inner.failures.join("\n  ")
      );
    }
    ret
  }
}

// ---------------------------------------------------------------------------
// Agent mode
// ---------------------------------------------------------------------------

/// How to reach and authenticate to SSH nodes (from the `--ssh-*` flags). The agent binary path is
/// `AgentModeProducer::agent_binary` (interpreted as the *remote* path for SSH); `None` falls back
/// to `vorpal` on the node's `PATH`.
#[derive(Clone, Default)]
pub struct SshDialOpts {
  pub user: Option<String>,
  pub key: Option<PathBuf>,
  pub password_env: Option<String>,
  pub key_passphrase_env: Option<String>,
  /// A local agent binary to push to each node (agent mode) instead of assuming it is installed.
  pub push_agent: Option<PathBuf>,
  /// A local stage-0 `vorpal-loader` binary. When set, the agent (`push_agent`) is delivered by
  /// pushing this loader and streaming the signed agent to it, so the agent execs from memory and
  /// never lands on disk (docs/REMOTE.md §2, §6).
  pub loader: Option<PathBuf>,
}

/// How to bring an agent up on a node: the exec spec, any pushed binary to clean up afterwards, and
/// an optional raw stdin preamble written **before** the wire handshake (the signed agent stream a
/// stage-0 loader consumes).
pub(crate) struct LaunchPlan {
  pub launch: ExecSpec,
  pub provisioned: Option<vorpal_transport::Provisioned>,
  pub preamble: Option<Vec<u8>>,
}

/// Fans an agent job out to N nodes over async transports (docs/REMOTE.md §2, §3.1). Loopback
/// (`SubprocessTransport`) and SSH share one async session driver; results funnel through an
/// async channel into a single blocking forwarder that feeds the sync printer channel, so the
/// sync consumer/printer/exit-code path is untouched and backpressure is end-to-end.
pub struct AgentModeProducer {
  targets: Vec<Target>,
  job: JobSpec,
  agent_binary: Option<PathBuf>,
  ssh: SshDialOpts,
  outcomes: NodeOutcomes,
  on_done: Option<DoneSink>,
  /// Global `--max-results` counter, shared across every node session. Each forwarded fragment
  /// claims its `match_count`; once reached, forwarding stops fleet-wide (§3.1). `None` when no
  /// cap is set. Exact for a single node (the agent truncates locally); bounded overshoot by one
  /// fragment across multiple distinct nodes.
  global_max: Option<Arc<MaxItemCounter>>,
}

impl AgentModeProducer {
  pub fn new(
    targets: Vec<Target>,
    job: JobSpec,
    agent_binary: Option<PathBuf>,
    ssh: SshDialOpts,
    outcomes: NodeOutcomes,
    on_done: Option<DoneSink>,
    global_max: Option<Arc<MaxItemCounter>>,
  ) -> Self {
    Self { targets, job, agent_binary, ssh, outcomes, on_done, global_max }
  }
}

impl<P> Produce<P> for AgentModeProducer
where
  P: Printer + 'static,
  P::Processed: WireFragment,
{
  fn channel_cap(&self) -> ChannelCap {
    ChannelCap::Window(REMOTE_WINDOW)
  }

  fn produce(self: Box<Self>, tx: ItemSink<P::Processed>, _processor: P::Processor) -> Result<()> {
    let AgentModeProducer { targets, job, agent_binary, ssh, on_done, outcomes, global_max } = *self;
    let job = Arc::new(job);
    let on_done = on_done.map(Arc::new);
    let agent_binary = Arc::new(agent_binary);
    let ssh = Arc::new(ssh);

    // A dedicated tokio runtime on this producer thread (the lsp.rs pattern); the async fan-out
    // never blocks the sync engine.
    let rt = tokio::runtime::Builder::new_multi_thread()
      .enable_all()
      .build()
      .map_err(|e| anyhow!("failed to build remote runtime: {e}"))?;

    // Async results → blocking forwarder → sync printer channel.
    let (atx, arx) = tokio::sync::mpsc::channel::<P::Processed>(REMOTE_WINDOW);
    let forwarder = session::spawn_forwarder(arx, tx);

    rt.block_on(async {
      let mut handles = Vec::new();
      for (idx, target) in targets.into_iter().enumerate() {
        let node = match connect_node::<P>(
          &target,
          idx,
          &agent_binary,
          &ssh,
          job.clone(),
          atx.clone(),
          on_done.clone(),
          global_max.clone(),
          outcomes.clone(),
        )
        .await
        {
          Ok(node) => node,
          Err(e) => {
            outcomes.record_failure(format!("node[{idx}]: {e}"));
            continue;
          }
        };
        handles.push(tokio::spawn(node.run()));
      }
      for h in handles {
        let _ = h.await;
      }
    });

    // Drop our sender so the forwarder's `blocking_recv` returns None and the thread exits; join
    // it so `outcomes` and the printer channel are fully drained before `produce` returns.
    drop(atx);
    let _ = forwarder.join();
    Ok(())
  }
}

/// Build one node's transport + agent-launch spec and wrap it in a session (dial → launch → wrap).
#[allow(clippy::too_many_arguments)]
async fn connect_node<P>(
  target: &Target,
  idx: usize,
  agent_binary: &Option<PathBuf>,
  ssh: &SshDialOpts,
  job: Arc<JobSpec>,
  tx: tokio::sync::mpsc::Sender<P::Processed>,
  on_done: Option<Arc<DoneSink>>,
  global_max: Option<Arc<MaxItemCounter>>,
  outcomes: NodeOutcomes,
) -> Result<session::NodeSession<P>>
where
  P: Printer,
  P::Processed: WireFragment,
{
  let (transport, label) = dial_transport(target, idx, agent_binary, ssh).await?;
  // Forced-agent path: no prior probe, so `agent_launch_for` probes itself if `--push-agent`.
  let plan = agent_launch_for(target, &*transport, idx, agent_binary, ssh, None).await?;
  Ok(session::NodeSession {
    label,
    transport,
    launch: plan.launch,
    job,
    tx,
    on_done,
    global_max,
    outcomes,
    provisioned: plan.provisioned,
    preamble: plan.preamble,
  })
}

/// Dial a node's transport (no agent launch yet). Loopback re-execs this binary in agent mode; SSH
/// connects + authenticates; k8s/docker wrap `kubectl exec`/`docker exec`. Shared by the forced-agent
/// path, the stream path, and negotiation (which then probes the dialed transport before deciding
/// agent-vs-stream, reusing this one connection).
pub(crate) async fn dial_transport(
  target: &Target,
  idx: usize,
  agent_binary: &Option<PathBuf>,
  ssh: &SshDialOpts,
) -> Result<(Box<dyn Transport>, String)> {
  match target {
    Target::Loopback => {
      let program = match agent_binary {
        Some(path) => path.clone(),
        None => std::env::current_exe()
          .map_err(|e| anyhow!("cannot locate current executable for loopback agent: {e}"))?,
      };
      let transport = SubprocessTransport::new(program, vec![AGENT_ARG.to_string()]);
      Ok((Box::new(transport), format!("node[{idx}] loopback")))
    }
    Target::Ssh(uri) => {
      let transport = connect_ssh(uri, ssh).await?;
      Ok((Box::new(transport), format!("node[{idx}] ssh://{}:{}", uri.host, uri.port)))
    }
    Target::K8s(k) => {
      let transport =
        CommandTransport::kubectl(k.namespace.as_deref(), &k.pod, k.container.as_deref());
      let label = format!("node[{idx}] k8s://{}", transport.descriptor().address);
      Ok((Box::new(transport), label))
    }
    Target::Docker(d) => {
      let transport = CommandTransport::docker(&d.container);
      Ok((Box::new(transport), format!("node[{idx}] docker://{}", d.container)))
    }
  }
}

/// Decide how to launch the agent on an already-dialed transport, provisioning it if `--push-agent`
/// (and delivering it via the stage-0 loader if `--loader`). `known_landing` lets negotiation pass
/// the landing spot it already found during the probe so provisioning does not probe a second time;
/// the forced-agent path passes `None` and re-probes.
async fn agent_launch_for(
  target: &Target,
  transport: &dyn Transport,
  idx: usize,
  agent_binary: &Option<PathBuf>,
  ssh: &SshDialOpts,
  known_landing: Option<&vorpal_transport::LandingSpot>,
) -> Result<LaunchPlan> {
  match target {
    // The loopback agent's behavior is driven entirely by the wire protocol on its stdio, and the
    // `AGENT_ARG` is already baked into the subprocess argv, so the exec spec is empty.
    Target::Loopback => Ok(LaunchPlan { launch: ExecSpec::argv(Vec::<String>::new()), provisioned: None, preamble: None }),
    // Every exec-capable remote (SSH, k8s, docker) launches the agent identically: push it if
    // `--push-agent` (optionally via the zero-residue `--loader`), else assume it is installed.
    Target::Ssh(_) | Target::K8s(_) | Target::Docker(_) => {
      let Some(local_bin) = &ssh.push_agent else {
        // The agent is assumed present: `--agent-binary` gives its remote path, else `vorpal` on PATH.
        let remote_agent = agent_binary
          .as_ref()
          .map(|p| p.to_string_lossy().into_owned())
          .unwrap_or_else(|| "vorpal".to_string());
        return Ok(LaunchPlan {
          launch: ExecSpec::argv([remote_agent, AGENT_ARG.to_string()]),
          provisioned: None,
          preamble: None,
        });
      };
      // A writable+executable landing spot (reuse the probe's finding if negotiation already has it).
      let landing = match known_landing {
        Some(l) => l.clone(),
        None => vorpal_transport::probe(transport)
          .await
          .map_err(|e| anyhow!("node probe failed: {e}"))?
          .landing
          .ok_or_else(|| {
            anyhow!("no writable+executable landing spot on the node (noexec?); use --remote-mode stream")
          })?,
      };
      let agent_bytes = std::fs::read(local_bin)
        .map_err(|e| anyhow!("cannot read --push-agent binary {}: {e}", local_bin.display()))?;
      // A per-node unique suffix without `rand`: pid ⊕ index.
      let unique = (std::process::id() as u64) << 16 ^ idx as u64;

      if let Some(loader_bin) = &ssh.loader {
        // Zero-residue delivery: push the tiny loader, then stream the signed agent to it. The agent
        // execs from memory and never touches the node's disk (§2, §6).
        let loader_bytes = std::fs::read(loader_bin)
          .map_err(|e| anyhow!("cannot read --loader binary {}: {e}", loader_bin.display()))?;
        let (signing_key, verifying_key) = vorpal_loader::generate_keypair();
        let pubkey_hex = vorpal_loader::verifying_key_to_hex(&verifying_key);
        let prov = vorpal_transport::push_agent(transport, &landing, "vorpal-loader", unique, &loader_bytes)
          .await
          .map_err(|e| anyhow!("pushing the loader failed: {e}"))?;
        // `loader --pubkey <hex> -- vorpal-agent __agent`; the loader verifies then execs the agent
        // with this argv. The signed agent stream is written to stdin before the handshake.
        let launch = ExecSpec::argv([
          prov.remote_path.clone(),
          "--pubkey".to_string(),
          pubkey_hex,
          "--".to_string(),
          "vorpal-agent".to_string(),
          AGENT_ARG.to_string(),
        ]);
        let preamble = vorpal_loader::sign_payload(&agent_bytes, &signing_key, false);
        Ok(LaunchPlan { launch, provisioned: Some(prov), preamble: Some(preamble) })
      } else {
        // Plain push: land the agent, chmod +x, exec it directly (residue until cleanup).
        let prov = vorpal_transport::push_agent(transport, &landing, "vorpal", unique, &agent_bytes)
          .await
          .map_err(|e| anyhow!("pushing the agent failed: {e}"))?;
        let launch = ExecSpec::argv([prov.remote_path.clone(), AGENT_ARG.to_string()]);
        Ok(LaunchPlan { launch, provisioned: Some(prov), preamble: None })
      }
    }
  }
}

/// Connect + authenticate an SSH transport from the URI and `--ssh-*` flags.
pub async fn connect_ssh(
  uri: &super::SshUri,
  ssh: &SshDialOpts,
) -> Result<vorpal_transport::ssh::SshTransport> {
  use vorpal_transport::Redacted;
  use vorpal_transport::ssh::{HostKeyVerifier, SshAuth, SshConfig, SshTransport};

  let user = uri
    .user
    .clone()
    .or_else(|| ssh.user.clone())
    .or_else(|| std::env::var("USER").ok())
    .ok_or_else(|| anyhow!("no ssh user (set ssh://user@… or --ssh-user or $USER)"))?;

  let auth = if let Some(var) = &ssh.password_env {
    let password = std::env::var(var)
      .map_err(|_| anyhow!("--ssh-password-env {var} is not set in the environment"))?;
    SshAuth::Password { user, password: Redacted(password) }
  } else {
    let path = ssh.key.clone().or_else(default_ssh_key).ok_or_else(|| {
      anyhow!("no ssh credentials: pass --ssh-key <path> or --ssh-password-env <VAR>")
    })?;
    let passphrase = ssh
      .key_passphrase_env
      .as_ref()
      .and_then(|v| std::env::var(v).ok())
      .map(Redacted);
    SshAuth::KeyFile { user, path, passphrase }
  };

  // Dev-tool default: trust-on-first-use (a warning is logged). Operators can pin keys later.
  let verifier = HostKeyVerifier::AcceptAny;
  SshTransport::connect(SshConfig { host: uri.host.clone(), port: uri.port, auth, verifier })
    .await
    .map_err(|e| anyhow!("{e}"))
}

/// The first existing default private key in `~/.ssh`.
fn default_ssh_key() -> Option<PathBuf> {
  let home = std::env::var_os("HOME")?;
  let ssh_dir = PathBuf::from(home).join(".ssh");
  for name in ["id_ed25519", "id_ecdsa", "id_rsa"] {
    let p = ssh_dir.join(name);
    if p.exists() {
      return Some(p);
    }
  }
  None
}

// ---------------------------------------------------------------------------
// Negotiating (auto) mode
// ---------------------------------------------------------------------------

/// `--remote-mode auto` (docs/REMOTE.md D1): per node, probe and **decide** — agent mode where the
/// node can exec a pushed binary, byte-stream where it cannot (distroless/noexec). Loopback always
/// execs, so it never probes. One fleet can therefore mix agent nodes and stream nodes, all feeding
/// the same consumer: agent results bridge async→sync through the forwarder; stream results (matched
/// on the coordinator) go straight into the sync channel — exactly as the two forced producers do.
///
/// The node is dialed once; the same connection is probed and then reused for the chosen mode (no
/// re-dial, no second authentication).
pub struct NegotiatingProducer<P: Printer> {
  targets: Vec<Target>,
  job: JobSpec,
  stream_worker: StreamWorker,
  agent_binary: Option<PathBuf>,
  ssh: SshDialOpts,
  outcomes: NodeOutcomes,
  on_done: Option<DoneSink>,
  global_max: Option<Arc<MaxItemCounter>>,
  /// The decision policy. Permissive by default (the operator's `--remote` opt-in is the real gate,
  /// §6); an eventual `--remote-policy` will populate it. `allow_push_exec = false` forces stream.
  policy: RemotePolicy,
  _printer: std::marker::PhantomData<fn() -> P>,
}

impl<P: Printer> NegotiatingProducer<P> {
  #[allow(clippy::too_many_arguments)]
  pub fn new(
    targets: Vec<Target>,
    job: JobSpec,
    stream_worker: StreamWorker,
    agent_binary: Option<PathBuf>,
    ssh: SshDialOpts,
    outcomes: NodeOutcomes,
    on_done: Option<DoneSink>,
    global_max: Option<Arc<MaxItemCounter>>,
  ) -> Self {
    Self {
      targets,
      job,
      stream_worker,
      agent_binary,
      ssh,
      outcomes,
      on_done,
      global_max,
      policy: RemotePolicy::default(),
      _printer: std::marker::PhantomData,
    }
  }

  /// Override the negotiation policy (tests force stream demotion via `allow_push_exec = false`).
  #[cfg(test)]
  pub fn with_policy(mut self, policy: RemotePolicy) -> Self {
    self.policy = policy;
    self
  }
}

/// The per-node routing decision, carrying the landing spot the probe already found so the agent
/// push does not probe a second time.
enum Route {
  Agent(Option<LandingSpot>),
  Stream,
}

impl<P> Produce<P> for NegotiatingProducer<P>
where
  P: Printer + 'static,
  P::Processed: WireFragment,
{
  fn channel_cap(&self) -> ChannelCap {
    ChannelCap::Window(REMOTE_WINDOW)
  }

  fn produce(self: Box<Self>, tx: ItemSink<P::Processed>, processor: P::Processor) -> Result<()> {
    let NegotiatingProducer {
      targets, job, stream_worker, agent_binary, ssh, outcomes, on_done, global_max, policy, ..
    } = *self;
    let job = Arc::new(job);
    let on_done = on_done.map(Arc::new);

    let rt = tokio::runtime::Builder::new_multi_thread()
      .enable_all()
      .build()
      .map_err(|e| anyhow!("failed to build remote runtime: {e}"))?;

    // Agent nodes stream rendered fragments through an async channel → blocking forwarder → the
    // sync printer channel; stream nodes match on the coordinator and feed that same sync channel
    // directly. Backpressure is end-to-end through both paths.
    let (atx, arx) = tokio::sync::mpsc::channel::<P::Processed>(REMOTE_WINDOW);
    let forwarder = session::spawn_forwarder(arx, tx.clone());

    rt.block_on(async {
      let mut handles = Vec::new();
      for (idx, target) in targets.into_iter().enumerate() {
        // Dial once; the same connection is probed and then reused for whichever mode we pick.
        let (transport, label) = match dial_transport(&target, idx, &agent_binary, &ssh).await {
          Ok(t) => t,
          Err(e) => {
            outcomes.record_failure(format!("node[{idx}]: {e}"));
            continue;
          }
        };

        // Classify. Loopback always execs (no probe); a remote node is probed and `decide`d.
        let route = match &target {
          Target::Loopback => Route::Agent(None),
          Target::Ssh(_) | Target::K8s(_) | Target::Docker(_) => match probe(&*transport).await {
            Ok(p) => match decide(&p, &policy, ForcedMode::Auto) {
              Ok(ExecMode::Agent { landing }) => Route::Agent(Some(landing)),
              Ok(ExecMode::Stream) => Route::Stream,
              Err(e) => {
                outcomes.record_failure(format!("{label}: {e}"));
                let _ = transport.shutdown().await;
                continue;
              }
            },
            Err(e) => {
              outcomes.record_failure(format!("{label}: probe failed: {e}"));
              let _ = transport.shutdown().await;
              continue;
            }
          },
        };

        match route {
          Route::Agent(landing) => {
            match agent_launch_for(&target, &*transport, idx, &agent_binary, &ssh, landing.as_ref())
              .await
            {
              Ok(plan) => {
                let node: session::NodeSession<P> = session::NodeSession {
                  label,
                  transport,
                  launch: plan.launch,
                  job: job.clone(),
                  tx: atx.clone(),
                  on_done: on_done.clone(),
                  global_max: global_max.clone(),
                  outcomes: outcomes.clone(),
                  provisioned: plan.provisioned,
                  preamble: plan.preamble,
                };
                handles.push(tokio::spawn(node.run()));
              }
              Err(e) => {
                outcomes.record_failure(format!("{label}: {e}"));
                let _ = transport.shutdown().await;
              }
            }
          }
          Route::Stream => {
            // Reuse the probed connection: match the node's files on the coordinator (§3.3).
            if let Err(e) = super::remote_stream::stream_over_transport::<P>(
              &*transport,
              &stream_worker,
              &tx,
              &processor,
            )
            .await
            {
              outcomes.record_failure(format!("stream {label}: {e}"));
            }
            let _ = transport.shutdown().await;
          }
        }
      }
      for h in handles {
        let _ = h.await;
      }
    });

    // Close our async sender so the forwarder drains and exits; join it so the printer channel and
    // `outcomes` are fully settled before `produce` returns.
    drop(atx);
    let _ = forwarder.join();
    Ok(())
  }
}

// ---------------------------------------------------------------------------
// Stream mode
// ---------------------------------------------------------------------------

/// The coordinator-side worker whose in-memory match pipeline stream mode drives.
pub enum StreamWorker {
  Scan(Arc<ScanWithConfig>),
  RunSpecific(Arc<RunWithSpecificLang>),
  RunInferred(Arc<RunWithInferredLang>),
}

pub struct StreamModeProducer {
  targets: Vec<Target>,
  worker: StreamWorker,
  ssh: SshDialOpts,
  agent_binary: Option<PathBuf>,
  outcomes: NodeOutcomes,
}

impl StreamModeProducer {
  pub fn new(
    targets: Vec<Target>,
    worker: StreamWorker,
    ssh: SshDialOpts,
    agent_binary: Option<PathBuf>,
    outcomes: NodeOutcomes,
  ) -> Self {
    Self { targets, worker, ssh, agent_binary, outcomes }
  }
}

impl<P> Produce<P> for StreamModeProducer
where
  P: Printer + 'static,
  P::Processed: WireFragment,
{
  fn channel_cap(&self) -> ChannelCap {
    ChannelCap::Window(REMOTE_WINDOW)
  }

  fn produce(self: Box<Self>, tx: ItemSink<P::Processed>, processor: P::Processor) -> Result<()> {
    let StreamModeProducer { targets, worker, ssh, agent_binary, outcomes } = *self;
    if targets.is_empty() {
      return Ok(());
    }
    let _ = &agent_binary; // (reserved: a remote helper path could accelerate enumeration)

    // A loopback node's filesystem is the local one, so its stream reconstruction reads the local
    // FS directly (fast path). All loopback targets are identical views → enumerate once.
    let has_loopback = targets.iter().any(|t| matches!(t, Target::Loopback));
    if has_loopback {
      if let Err(e) = super::producer::stream_local::<P>(&worker, &tx, &processor) {
        outcomes.record_failure(format!("stream loopback: {e}"));
      }
    }

    // A non-loopback node's filesystem is remote: reconstruct discovery from the node's tree over
    // the transport and fetch surviving files' content — the coordinator runs the same match
    // pipeline on it (§3.3). Driven on a dedicated tokio runtime (transports are async).
    let remote: Vec<Target> = targets.into_iter().filter(|t| !matches!(t, Target::Loopback)).collect();
    if !remote.is_empty() {
      let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| anyhow!("failed to build remote runtime: {e}"))?;
      rt.block_on(async {
        for (idx, target) in remote.iter().enumerate() {
          if let Err(e) =
            super::remote_stream::stream_remote::<P>(target, idx, &ssh, &worker, &tx, &processor).await
          {
            outcomes.record_failure(format!("stream node[{idx}]: {e}"));
          }
        }
      });
    }
    Ok(())
  }
}

/// Reconstruct discovery for each walk root and match wire-delivered content locally — **in
/// parallel** across the reconstructed file set, so stream mode runs the match pipeline at the
/// same multi-core throughput a local `WalkParallel` scan does rather than single-threaded.
pub fn stream_local<P>(
  worker: &StreamWorker,
  tx: &ItemSink<P::Processed>,
  processor: &P::Processor,
) -> Result<()>
where
  P: Printer,
  P::Processed: WireFragment,
{
  use rayon::prelude::*;
  use super::discovery::{GlobalIgnore, WalkConfig, reconstruct_local};

  let (input, types, langs_types_all) = stream_walk_inputs(worker)?;
  // The same glob builder the local walk uses (`InputArgs::build_globs`) — the override matcher
  // must be identical or stream-mode discovery diverges (I1).
  let overrides = input.input.build_globs()?;
  // Derive the effective ignore settings from the real CLI flags via the single shared source.
  let walk_ignore = crate::utils::NoIgnore::disregard(&input.input.no_ignore).effective();
  let cfg = WalkConfig::from_walk_ignore(
    overrides,
    if langs_types_all { None } else { Some(types) },
    walk_ignore,
  );

  // Reconstruction is I/O-bound directory walking; do it per root (fast, prunes ignored dirs),
  // then fan the surviving files out across the rayon pool for parse+match.
  let mut files = Vec::new();
  for root in &input.input.paths {
    files.extend(reconstruct_local(root, input.input.follow, &cfg, &GlobalIgnore::LocalMachine)?);
  }

  // `try_for_each_init` clones the sink once per worker thread and short-circuits the whole
  // parallel iterator when a task returns `Err` — used here purely as a stop signal (consumer
  // hung up, or `--max-results` reached), never a real error.
  let outcome = files.into_par_iter().try_for_each_init(
    || tx.clone(),
    |tx, display_path| {
      if worker_should_stop(worker) {
        return Err(());
      }
      match emit_stream_file::<P>(worker, &display_path, processor, tx) {
        StreamStep::Continue => Ok(()),
        StreamStep::Stop => Err(()),
      }
    },
  );
  let _ = outcome; // Err just means "stopped early"; not a failure.
  Ok(())
}

pub enum StreamStep {
  Continue,
  Stop,
}

/// True once the worker's `--max-results` cap has been reached (scan only; run has no cap).
pub fn worker_should_stop(worker: &StreamWorker) -> bool {
  match worker {
    StreamWorker::Scan(scan) => scan.should_stop(),
    _ => false,
  }
}

/// Read, render, and forward one file's matches on the coordinator. Reads the file's content
/// (the loopback stand-in for "shipped bytes") and moves it straight into the parser — no copy.
fn emit_stream_file<P>(
  worker: &StreamWorker,
  display_path: &std::path::Path,
  processor: &P::Processor,
  tx: &ItemSink<P::Processed>,
) -> StreamStep
where
  P: Printer,
  P::Processed: WireFragment,
{
  // Loopback: the "shipped" content is the local file. Read failures are skips, exactly as the
  // local walk skips an unreadable file; size/emptiness gating happens in the pipeline below.
  let content = match std::fs::read_to_string(display_path) {
    Ok(c) => c,
    Err(_) => return StreamStep::Continue,
  };
  match_stream_content::<P>(worker, display_path, content, /* warm */ true, processor, tx)
}

/// Match already-acquired content and forward its items — shared by the loopback path (content
/// read from the local FS) and the remote path (content fetched over the transport).
pub fn match_stream_content<P>(
  worker: &StreamWorker,
  display_path: &std::path::Path,
  content: String,
  warm_cache: bool,
  processor: &P::Processor,
  tx: &ItemSink<P::Processed>,
) -> StreamStep
where
  P: Printer,
  P::Processed: WireFragment,
{
  let items = match worker {
    StreamWorker::Scan(scan) => {
      let normalized = normalize_for_scan(display_path, scan.project_dir());
      scan.get_trace().add_scanned();
      match scan.produce_item_from_content::<P>(display_path, &normalized, content, processor) {
        Ok(items) => items,
        Err(_) => {
          scan.get_trace().add_skipped();
          vec![]
        }
      }
    }
    StreamWorker::RunSpecific(run) => {
      run.get_trace().add_scanned();
      match run.produce_item_from_content::<P>(display_path, content, processor) {
        Ok(items) => items,
        Err(_) => {
          run.get_trace().add_skipped();
          vec![]
        }
      }
    }
    StreamWorker::RunInferred(run) => {
      run.get_trace().add_scanned();
      match run.produce_item_from_content::<P>(display_path, content, processor) {
        Ok(items) => items,
        Err(_) => {
          run.get_trace().add_skipped();
          vec![]
        }
      }
    }
  };
  if warm_cache && !items.is_empty() {
    // §3.4 parity: bank the file's extraction product like local/agent scans (loopback only — the
    // file exists locally). For a truly remote node the path is not local, so callers pass false.
    let _ = vorpal_index::warm_product_cache(display_path);
  }
  for item in items {
    if tx.send(item).is_err() {
      return StreamStep::Stop;
    }
  }
  if worker_should_stop(worker) {
    StreamStep::Stop
  } else {
    StreamStep::Continue
  }
}

/// Normalize a stream-mode path the way local scan's `produce_item` does: canonicalize then strip
/// the project prefix. On loopback the file exists, so this equals the local computation exactly.
fn normalize_for_scan(display_path: &std::path::Path, proj_dir: &std::path::Path) -> PathBuf {
  match display_path.canonicalize() {
    Ok(abs) => abs.strip_prefix(proj_dir).map(Path::to_path_buf).unwrap_or_else(|_| display_path.to_path_buf()),
    Err(_) => display_path.to_path_buf(),
  }
}

use std::path::Path;

/// Extract the walk inputs (roots, language types, no-ignore flags) from whichever worker drives
/// this stream, so discovery reconstruction sees the same configuration the real walk would. The
/// bool is "no type restriction" (inferred-lang run walks every file).
pub fn stream_walk_inputs(worker: &StreamWorker) -> Result<(StreamInput, ignore::types::Types, bool)> {
  use crate::lang::SgLang;
  use std::collections::HashSet;
  match worker {
    StreamWorker::Scan(scan) => {
      // Mirror `ScanWithConfig::build_walk`: types for the union of rule languages.
      let input = StreamInput::from_input(scan.scan_arg().input.clone());
      let mut langs = HashSet::new();
      scan.rule_collection().for_each_rule(|rule| {
        langs.insert(rule.language);
      });
      let types = SgLang::file_types_for_langs(langs.into_iter());
      Ok((input, types, false))
    }
    StreamWorker::RunSpecific(run) => {
      // Mirror `RunWithSpecificLang::build_walk` (`walk_lang` → augmented file type).
      let input = StreamInput::from_input(run.run_arg().input.clone());
      let lang = run.run_arg().lang.expect("specific lang worker has a lang");
      Ok((input, lang.augmented_file_type(), false))
    }
    StreamWorker::RunInferred(run) => {
      // Inferred-lang run walks all files (`InputArgs::walk`), no type restriction.
      let input = StreamInput::from_input(run.run_arg().input.clone());
      Ok((input, ignore::types::TypesBuilder::new().build().unwrap(), true))
    }
  }
}

/// A cloned view of the CLI walk inputs for stream reconstruction — kept as the real `InputArgs`
/// so glob building and any future walk-input logic stay the single implementation the local
/// walk uses.
pub struct StreamInput {
  pub input: crate::utils::InputArgs,
}

impl StreamInput {
  fn from_input(input: crate::utils::InputArgs) -> Self {
    Self { input }
  }
}
