//! The async agent-session driver (docs/REMOTE.md §3.1) shared by **every** transport. A node is
//! a [`vorpal_transport::Transport`]; the coordinator execs the agent over it, then speaks the
//! wire protocol on the process's async stdio. Loopback (`SubprocessTransport`) and SSH run the
//! exact same session code — that is what makes the differential gate (`loopback ≡ ssh ≡ local`)
//! meaningful.
//!
//! Results funnel through an async mpsc into a single blocking forwarder thread that does the
//! `SyncSender::send` into the existing sync printer channel — so `consume_items`, every printer,
//! and the exit-code logic stay untouched, and backpressure is end-to-end (printer drain rate →
//! async channel → transport reads).

use std::sync::Arc;

use anyhow::{Result, anyhow};
use tokio::sync::mpsc::Sender as AsyncSender;
use tokio::time::timeout;

use vorpal_transport::{ExecSpec, Transport};
use vorpal_wire::{
  Assign, AsyncFrameReader, AsyncFrameWriter, Caps, Hello, JobSpec, Message, Outcome, ResultFrame,
  SemVer,
};

use crate::print::{Printer, WireFragment};
use crate::utils::MaxItemCounter;

use super::fingerprint;
use super::producer::{DoneSink, NodeOutcomes};

/// How long to wait for Hello→Welcome before failing a node — a wedged agent binary must not hang
/// the CLI.
fn handshake_timeout() -> std::time::Duration {
  let ms = std::env::var("VORPAL_REMOTE_HANDSHAKE_TIMEOUT_MS")
    .ok()
    .and_then(|v| v.parse::<u64>().ok())
    .unwrap_or(30_000);
  std::time::Duration::from_millis(ms)
}

/// Steady-state read deadline: the longest the coordinator waits for *any* frame — a result, a
/// `Done`, or a `Telemetry::Heartbeat` — before presuming the node wedged and reaping it. A live
/// but quiet agent pulses heartbeats every `VORPAL_REMOTE_HEARTBEAT_MS` (default 5 s), so the 30 s
/// default tolerates several dropped beats; a genuinely hung agent trips it instead of hanging the
/// CLI forever. `0` disables the deadline (unbounded wait).
fn read_timeout() -> Option<std::time::Duration> {
  let ms = std::env::var("VORPAL_REMOTE_READ_TIMEOUT_MS")
    .ok()
    .and_then(|v| v.parse::<u64>().ok())
    .unwrap_or(30_000);
  (ms > 0).then(|| std::time::Duration::from_millis(ms))
}

/// Everything one node session needs, transport-agnostic.
pub struct NodeSession<P: Printer> {
  pub label: String,
  pub transport: Box<dyn Transport>,
  /// How to launch the agent on this node (empty argv for loopback; the pushed path for SSH).
  pub launch: ExecSpec,
  pub job: Arc<JobSpec>,
  pub tx: AsyncSender<P::Processed>,
  pub on_done: Option<Arc<DoneSink>>,
  pub global_max: Option<Arc<MaxItemCounter>>,
  pub outcomes: NodeOutcomes,
  /// A pushed binary to remove when the session ends (`--push-agent`/`--loader`); `None` otherwise.
  pub provisioned: Option<vorpal_transport::Provisioned>,
  /// Raw bytes to write to the agent process's stdin **before** the handshake — the Ed25519-signed
  /// agent stream a stage-0 loader consumes (`--loader`). `None` for a directly-launched agent.
  pub preamble: Option<Vec<u8>>,
}

impl<P> NodeSession<P>
where
  P: Printer,
  P::Processed: WireFragment,
{
  /// Run the whole session; a failure is recorded as a node outcome (never panics the fan-out).
  pub async fn run(self) {
    let label = self.label.clone();
    let outcomes = self.outcomes.clone();
    if let Err(e) = self.run_inner().await {
      outcomes.record_failure(format!("{label}: {e}"));
    }
  }

  async fn run_inner(self) -> Result<()> {
    let NodeSession {
      label,
      transport,
      launch,
      job,
      tx,
      on_done,
      global_max,
      outcomes: _,
      provisioned,
      preamble,
    } = self;
    let result = drive_agent_session::<P>(
      &*transport,
      launch,
      preamble.as_deref(),
      &job,
      &tx,
      on_done.as_deref(),
      global_max.as_deref(),
      &label,
    )
    .await;
    // Always remove a pushed agent binary and close the transport, whatever the outcome.
    if let Some(prov) = &provisioned {
      prov.cleanup(&*transport).await;
    }
    let _ = transport.shutdown().await;
    result
  }
}

/// The agent wire session over an already-connected transport: exec the agent, handshake (under a
/// watchdog), ship the job, and drain results into `tx`.
#[allow(clippy::too_many_arguments)]
async fn drive_agent_session<P>(
  transport: &dyn Transport,
  launch: ExecSpec,
  preamble: Option<&[u8]>,
  job: &Arc<JobSpec>,
  tx: &AsyncSender<P::Processed>,
  on_done: Option<&DoneSink>,
  global_max: Option<&MaxItemCounter>,
  label: &str,
) -> Result<()>
where
  P: Printer,
  P::Processed: WireFragment,
{
  {
    let mut proc = transport
      .exec(&launch)
      .await
      .map_err(|e| anyhow!("exec agent failed: {e}"))?;
    let killer = proc.killer();
    let mut stdin = proc
      .stdin
      .take()
      .ok_or_else(|| anyhow!("agent stdin unavailable"))?;
    let stdout = proc
      .stdout
      .take()
      .ok_or_else(|| anyhow!("agent stdout unavailable"))?;
    // A stage-0 loader consumes the signed-agent preamble from stdin, verifies it, and execs the
    // agent — which then reads the wire frames the `AsyncFrameWriter` writes next from the same pipe.
    if let Some(bytes) = preamble {
      use tokio::io::AsyncWriteExt;
      stdin
        .write_all(bytes)
        .await
        .map_err(|e| anyhow!("{label}: writing loader preamble failed: {e}"))?;
      stdin
        .flush()
        .await
        .map_err(|e| anyhow!("{label}: flushing loader preamble failed: {e}"))?;
    }
    let mut writer = AsyncFrameWriter::new(stdin);
    let mut reader = AsyncFrameReader::new(stdout, vorpal_wire::DEFAULT_MAX_FRAME);

    let ours = super::current_version()?;

    // --- Handshake, under a watchdog: a wedged agent must fail this node, not hang the CLI. ---
    let handshake = async {
      writer
        .write_message(
          0,
          &Message::Hello(Hello {
            protocol: vorpal_wire::PROTOCOL_VERSION,
            coordinator_version: ours,
            session: job.job_id as u128,
            wanted: Caps::default(),
          }),
        )
        .await?;
      let welcome = match reader.read_message().await? {
        Some((_, Message::Welcome(w))) => w,
        Some((_, Message::Bye(e))) => return Err(anyhow!("agent refused at handshake: {e}")),
        Some((_, other)) => return Err(anyhow!("expected Welcome, got {}", describe(&other))),
        None => return Err(anyhow!("agent closed the stream before Welcome")),
      };
      verify_welcome(&welcome, ours)?;
      Ok::<_, anyhow::Error>(())
    };
    match timeout(handshake_timeout(), handshake).await {
      Ok(Ok(())) => {}
      Ok(Err(e)) => {
        killer.kill().await;
        return Err(e).map_err(|e| anyhow!("{label}: {e}"));
      }
      Err(_) => {
        killer.kill().await;
        return Err(anyhow!(
          "{label}: agent did not complete handshake within the deadline"
        ));
      }
    }

    // --- Job + Assign ---
    writer
      .write_message(0, &Message::Job((**job).clone()))
      .await?;
    writer
      .write_message(0, &Message::Assign(Assign::SelfEnumerate))
      .await?;

    // --- Drain results until Done/Bye ---
    // Every steady-state read is bounded by the deadline: a live-but-quiet agent pulses heartbeats
    // that reset it, so only a genuinely wedged node trips it — and is reaped rather than hanging
    // the CLI forever (§3.4).
    let deadline = read_timeout();
    let mut failed = false;
    loop {
      let next = reader.read_message();
      let read = match deadline {
        Some(dur) => match timeout(dur, next).await {
          Ok(r) => r,
          Err(_) => {
            killer.kill().await;
            return Err(anyhow!(
              "{label}: no frame within the {dur:?} read deadline (no result, heartbeat, or Done) — node presumed wedged"
            ));
          }
        },
        None => next.await,
      };
      let msg = match read? {
        Some((_, m)) => m,
        None => return Err(anyhow!("{label}: agent closed the stream unexpectedly")),
      };
      match msg {
        Message::Result(ResultFrame::Rendered {
          bytes, match_count, ..
        }) => {
          // Global `--max-results` (§3.1): once the cross-node cap is reached, stop forwarding.
          if global_max.as_ref().is_some_and(|c| c.reached_max()) {
            break;
          }
          let processed = P::Processed::decode(&bytes)
            .map_err(|e| anyhow!("cannot decode rendered fragment: {e}"))?;
          if tx.send(processed).await.is_err() {
            // The consumer hung up (printer error / channel closed): stop pulling.
            break;
          }
          if let Some(c) = &global_max {
            c.claim(match_count as usize);
          }
        }
        Message::Result(_) | Message::Telemetry(_) => {}
        Message::Done(done) => {
          if matches!(done.outcome, Outcome::Failed) {
            failed = true;
          }
          if let Some(sink) = &on_done {
            sink(&done.stats);
          }
          break;
        }
        Message::Bye(e) => return Err(anyhow!("{label}: agent aborted: {e}")),
        other => return Err(anyhow!("{label}: unexpected frame {}", describe(&other))),
      }
    }

    let status = proc
      .wait()
      .await
      .map_err(|e| anyhow!("{label}: wait failed: {e}"))?;
    if failed {
      return Err(anyhow!("{label}: agent reported job failure"));
    }
    if !status.success() {
      return Err(anyhow!("{label}: agent exited with {status:?}"));
    }
    Ok(())
  }
}

fn verify_welcome(welcome: &vorpal_wire::Welcome, ours: SemVer) -> Result<()> {
  // The coordinator enforces its side of the I2 gate — the doc requires it to "refuse or demote"
  // on mismatch, and an `--agent-binary` pointing at a stale build cannot gate itself. Welcome
  // carries the agent's *builtin* grammar fingerprint (customs register per job); the post-
  // `LangEnv` fingerprint is re-verified agent-side against the job.
  if welcome.protocol != vorpal_wire::PROTOCOL_VERSION {
    return Err(anyhow!(
      "agent speaks protocol {}, coordinator speaks {}",
      welcome.protocol,
      vorpal_wire::PROTOCOL_VERSION
    ));
  }
  if welcome.agent_version != ours {
    return Err(anyhow!(
      "agent version {} does not match coordinator version {ours} (exact match required)",
      welcome.agent_version
    ));
  }
  if welcome.caps.grammar_fingerprint != fingerprint::builtin_fingerprint() {
    return Err(anyhow!(
      "agent grammar fingerprint differs from coordinator (I2) — refusing agent mode"
    ));
  }
  Ok(())
}

fn describe(msg: &Message) -> &'static str {
  match msg {
    Message::Hello(_) => "Hello",
    Message::Welcome(_) => "Welcome",
    Message::Job(_) => "Job",
    Message::Assign(_) => "Assign",
    Message::Result(_) => "Result",
    Message::Control(_) => "Control",
    Message::Telemetry(_) => "Telemetry",
    Message::Done(_) => "Done",
    Message::Bye(_) => "Bye",
  }
}

/// A blocking forwarder thread: drains the async result channel into the sync printer channel.
/// `SyncSender::send` blocking never touches the tokio reactor because it runs here, off-runtime.
pub fn spawn_forwarder<T: Send + 'static>(
  mut arx: tokio::sync::mpsc::Receiver<T>,
  sink: crate::utils::ItemSink<T>,
) -> std::thread::JoinHandle<()> {
  std::thread::spawn(move || {
    while let Some(item) = arx.blocking_recv() {
      if sink.send(item).is_err() {
        break;
      }
    }
  })
}
