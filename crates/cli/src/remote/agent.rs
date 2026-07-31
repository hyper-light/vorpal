//! The agent: `vorpal __agent`, spawned by a coordinator (over a loopback pipe in R0). It speaks
//! the wire protocol on stdin/stdout and runs the **real** sync engine — the same
//! `WalkParallel` + `produce_item` a local scan/run uses — streaming already-rendered
//! `P::Processed` fragments back as `Result::Rendered` frames.
//!
//! Trust model (docs/REMOTE.md §6): the coordinator is authenticated but the agent still verifies
//! everything load-bearing — protocol/version, grammar fingerprint (I2), and the rule-payload
//! digest (I3) — and refuses with a `Bye` on any mismatch rather than producing divergent output.
//! Rendered fragments carry only bytes; the agent's own printers write to a sink so they never
//! corrupt the frame stream on stdout.

use std::io::{self, Stdout};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};

use vorpal_wire::{
  Assign, Caps, Done, FinalStats, FrameReader, FrameWriter, Hello, JobKind, JobSpec, Message,
  Outcome, PrinterSpec, RemoteError, ResultFrame, RunJob, ScanJob, Telemetry, Welcome,
};

use crate::print::{Printer, WireFragment};
use crate::run::{RunWithInferredLang, RunWithSpecificLang};
use crate::scan::ScanWithConfig;
use crate::utils::PathWorker;

use super::CountedProduce;
use super::fingerprint;
use super::rules_wire::{self, LangEnv};
use super::spec::{self, AgentPrinter};

/// True when this process was launched as an agent (argv[1] == [`super::AGENT_ARG`]).
pub fn is_agent_invocation<I: Iterator<Item = String>>(mut args: I) -> bool {
  let _bin = args.next();
  args.next().as_deref() == Some(super::AGENT_ARG)
}

/// Run the agent event loop to completion. Errors are reported to the coordinator as `Bye`; the
/// process still exits 0 (the agent's *job* failing is normal control flow, not a crash).
pub fn run_agent() -> Result<ExitCode> {
  // Self-limit so scanning a live node never disrupts its primary workload (docs/REMOTE.md §2).
  apply_nice();

  let stdin = io::stdin();
  let mut reader = FrameReader::new(stdin.lock(), vorpal_wire::DEFAULT_MAX_FRAME);
  let stdout = io::stdout();
  let writer = Arc::new(Mutex::new(FrameWriter::new(stdout)));

  if let Err(err) = handshake_and_run(&mut reader, &writer) {
    let remote_err = err
      .downcast::<RemoteError>()
      .unwrap_or_else(|e| RemoteError::Fatal(e.to_string()));
    let _ = writer
      .lock()
      .unwrap()
      .write_message(0, &Message::Bye(remote_err));
  }
  Ok(ExitCode::SUCCESS)
}

/// Lower the agent's scheduling priority so it yields to the node's primary workload.
#[cfg(unix)]
fn apply_nice() {
  // PRIO_PROCESS = 0; nice +10. Best-effort — a failure (e.g. no permission) is fine.
  unsafe {
    libc::setpriority(libc::PRIO_PROCESS, 0, 10);
  }
}

#[cfg(not(unix))]
fn apply_nice() {}

/// The agent's effective walk-thread cap: never exceed the node's cgroup CPU share, so a scan on a
/// throttled pod does not burn more CPU than the pod is allotted (§2). `shipped == 0` means "auto";
/// the cgroup budget (Linux) then sets it, else the local heuristic applies.
pub(crate) fn self_limited_threads(shipped: usize) -> usize {
  match (shipped, cgroup_cpu_budget()) {
    (0, Some(budget)) => budget,
    (n, Some(budget)) if n > 0 => n.min(budget),
    (n, _) => n,
  }
}

/// The node's cgroup CPU budget in whole cores (ceil), or None when unthrottled/unknown. Reads
/// cgroup v2 (`cpu.max`) then v1 (`cpu.cfs_quota_us`/`cpu.cfs_period_us`).
#[cfg(target_os = "linux")]
fn cgroup_cpu_budget() -> Option<usize> {
  fn cores(quota: f64, period: f64) -> Option<usize> {
    if period > 0.0 && quota > 0.0 {
      Some((quota / period).ceil().max(1.0) as usize)
    } else {
      None
    }
  }
  if let Ok(s) = std::fs::read_to_string("/sys/fs/cgroup/cpu.max") {
    let mut it = s.split_whitespace();
    if let (Some(q), Some(p)) = (it.next(), it.next()) {
      if q != "max" {
        if let (Ok(q), Ok(p)) = (q.parse::<f64>(), p.parse::<f64>()) {
          return cores(q, p);
        }
      }
      return None; // "max" → unthrottled
    }
  }
  let q = std::fs::read_to_string("/sys/fs/cgroup/cpu/cpu.cfs_quota_us").ok()?;
  let p = std::fs::read_to_string("/sys/fs/cgroup/cpu/cpu.cfs_period_us").ok()?;
  cores(q.trim().parse().ok()?, p.trim().parse().ok()?)
}

#[cfg(not(target_os = "linux"))]
fn cgroup_cpu_budget() -> Option<usize> {
  None
}

fn recv<R: io::Read>(reader: &mut FrameReader<R>) -> Result<Message> {
  reader.read_message()?.map(|(_ch, msg)| msg).ok_or_else(|| {
    anyhow!(RemoteError::Fatal(
      "coordinator closed the stream early".into()
    ))
  })
}

fn handshake_and_run<R: io::Read>(
  reader: &mut FrameReader<R>,
  writer: &Arc<Mutex<FrameWriter<Stdout>>>,
) -> Result<()> {
  // --- Hello / Welcome ---
  let Message::Hello(hello) = recv(reader)? else {
    return Err(anyhow!(RemoteError::Fatal("expected Hello".into())));
  };
  check_versions(&hello)?;
  let welcome = Welcome {
    protocol: vorpal_wire::PROTOCOL_VERSION,
    agent_version: super::current_version()?,
    node_id: node_id(),
    caps: Caps {
      grammar_fingerprint: fingerprint::grammar_fingerprint(),
      ..Caps::default()
    },
    host: host_info(),
  };
  writer
    .lock()
    .unwrap()
    .write_message(0, &Message::Welcome(welcome))?;

  // --- Job ---
  let Message::Job(job) = recv(reader)? else {
    return Err(anyhow!(RemoteError::Fatal("expected Job".into())));
  };
  // Register the project language environment BEFORE any rule/lang parsing, then re-check the
  // grammar fingerprint against what the coordinator computed for itself (I2).
  if let Some(env_bytes) = &job.lang_env {
    LangEnv::decode(env_bytes)?.apply()?;
  }
  let got_fp = fingerprint::grammar_fingerprint();
  if got_fp != job.expected_grammar_fingerprint {
    return Err(anyhow!(RemoteError::GrammarMismatch {
      expected: job.expected_grammar_fingerprint,
      got: got_fp,
    }));
  }

  // --- Assign --- (R0: SelfEnumerate only)
  let Message::Assign(assign) = recv(reader)? else {
    return Err(anyhow!(RemoteError::Fatal("expected Assign".into())));
  };
  if !matches!(assign, Assign::SelfEnumerate) {
    return Err(anyhow!(RemoteError::UnsupportedAssign));
  }

  // Emit heartbeats for the whole job run so a legitimately-quiet-but-alive scan (walking a huge
  // ignored tree with no matches) keeps the coordinator's steady-state read deadline fed; a truly
  // wedged agent sends nothing and is reaped (§3.4). A *scoped* thread borrows the stop flag and the
  // frame-writer straight off the stack — no `Arc`, no refcount traffic — and is auto-joined when
  // the job returns; heartbeats interleave through the same writer mutex the result threads use.
  let stop = AtomicBool::new(false);
  let stats: Result<FinalStats> = std::thread::scope(|s| {
    s.spawn(|| heartbeat_loop(&stop, writer));
    // Test-only fault injection (§8 adversarial tests): stall without producing work. With
    // heartbeats on the coordinator must tolerate it; with heartbeats off it must reap the node.
    maybe_test_stall();
    let stats = match &job.kind {
      JobKind::Scan(scan) => run_scan_job(&job, scan, writer),
      JobKind::Run(run) => run_run_job(&job, run, writer),
    };
    stop.store(true, Ordering::Release);
    stats
  });
  let stats = stats?;
  writer.lock().unwrap().write_message(
    0,
    &Message::Done(Done {
      epoch: 0,
      outcome: Outcome::Complete,
      stats,
    }),
  )?;
  Ok(())
}

/// The agent's heartbeat interval, or `None` when disabled (`VORPAL_REMOTE_HEARTBEAT_MS=0`). The
/// coordinator's steady-state read deadline (`VORPAL_REMOTE_READ_TIMEOUT_MS`, default 30 s) is a
/// large multiple of this so a few dropped beats don't false-positive a live node.
fn heartbeat_interval() -> Option<Duration> {
  let ms = std::env::var("VORPAL_REMOTE_HEARTBEAT_MS")
    .ok()
    .and_then(|v| v.parse::<u64>().ok())
    .unwrap_or(5_000);
  (ms > 0).then(|| Duration::from_millis(ms))
}

/// Pulse `Telemetry::Heartbeat` frames at [`heartbeat_interval`] until `stop` is set. Borrows the
/// writer by shared reference (the walk threads write results through the same mutex). Returns
/// immediately when heartbeats are disabled.
fn heartbeat_loop(stop: &AtomicBool, writer: &Mutex<FrameWriter<Stdout>>) {
  let Some(interval) = heartbeat_interval() else {
    return;
  };
  let started = Instant::now();
  let mut seq = 0u64;
  // Wait in small slices so `stop` is observed promptly at job end (no lingering thread).
  let slice = interval.min(Duration::from_millis(100));
  let mut waited = Duration::ZERO;
  while !stop.load(Ordering::Acquire) {
    std::thread::sleep(slice);
    waited += slice;
    if waited < interval {
      continue;
    }
    waited = Duration::ZERO;
    if stop.load(Ordering::Acquire) {
      break;
    }
    seq += 1;
    let beat = Telemetry::Heartbeat {
      seq,
      monotonic_ms: started.elapsed().as_millis() as u64,
    };
    // A write error means the coordinator went away — nothing left to keep alive.
    if writer
      .lock()
      .unwrap()
      .write_message(0, &Message::Telemetry(beat))
      .is_err()
    {
      break;
    }
  }
}

/// Test-only: sleep for `VORPAL_AGENT_TEST_STALL_MS` before running the job, to exercise the
/// coordinator's heartbeat/read-deadline handling deterministically (unset in normal operation).
fn maybe_test_stall() {
  if let Some(ms) = std::env::var("VORPAL_AGENT_TEST_STALL_MS")
    .ok()
    .and_then(|v| v.parse::<u64>().ok())
  {
    std::thread::sleep(Duration::from_millis(ms));
  }
}

fn check_versions(hello: &Hello) -> Result<()> {
  if hello.protocol != vorpal_wire::PROTOCOL_VERSION {
    return Err(anyhow!(RemoteError::Fatal(format!(
      "protocol mismatch: coordinator speaks {}, agent speaks {}",
      hello.protocol,
      vorpal_wire::PROTOCOL_VERSION
    ))));
  }
  // Exact version match or refuse (mirrors the .vseg reject-on-mismatch rule, upholds I2).
  let ours = super::current_version()?;
  if hello.coordinator_version != ours {
    return Err(anyhow!(RemoteError::VersionMismatch {
      coordinator: hello.coordinator_version,
      agent: ours,
    }));
  }
  Ok(())
}

fn node_id() -> String {
  hostname().unwrap_or_else(|| "loopback".into())
}

fn hostname() -> Option<String> {
  std::env::var("HOSTNAME").ok().filter(|h| !h.is_empty())
}

fn host_info() -> vorpal_wire::HostInfo {
  vorpal_wire::HostInfo {
    arch: std::env::consts::ARCH.into(),
    os: std::env::consts::OS.into(),
    nproc: std::thread::available_parallelism()
      .map(|n| n.get() as u32)
      .unwrap_or(1),
    hostname: hostname().unwrap_or_default(),
  }
}

// ---------------------------------------------------------------------------
// Scan
// ---------------------------------------------------------------------------

fn run_scan_job(
  job: &JobSpec,
  scan: &ScanJob,
  writer: &Arc<Mutex<FrameWriter<Stdout>>>,
) -> Result<FinalStats> {
  let configs = rules_wire::decode_scan_rules(&scan.rules)?;
  let unused = spec::severity_from_name(&scan.unused_suppression_severity)?;
  let no_suppress = spec::severity_from_name(&scan.no_suppress_all_severity)?;
  // The two synthesized suppression rules, rebuilt from their resolved severities (the same
  // `CombinedScan` constructors the local scan uses).
  let unused_rule =
    vorpal_config::CombinedScan::unused_config(unused, vorpal_language::SupportLang::Rust.into());
  let no_suppress_rule = vorpal_config::CombinedScan::no_suppress_all_config(
    no_suppress,
    vorpal_language::SupportLang::Rust.into(),
  );
  let mut scan_arg = spec::scan_arg_from_job(job)?;
  // Cap parallelism to the node's cgroup CPU share so we don't disrupt its primary workload.
  scan_arg.input.threads = self_limited_threads(scan_arg.input.threads);
  let proj_dir = std::path::PathBuf::from(&scan.proj_dir);
  let worker = Arc::new(ScanWithConfig::from_remote_parts(
    scan_arg,
    configs,
    unused_rule,
    no_suppress_rule,
    proj_dir,
  )?);
  let matched = stream_worker(worker.clone(), &job.printer, writer)?;
  let trace = worker.get_trace();
  Ok(FinalStats {
    scanned: trace.scanned() as u64,
    skipped: trace.skipped() as u64,
    matched,
    error_count: worker.local_error_count() as u64,
  })
}

// ---------------------------------------------------------------------------
// Run
// ---------------------------------------------------------------------------

fn run_run_job(
  job: &JobSpec,
  run: &RunJob,
  writer: &Arc<Mutex<FrameWriter<Stdout>>>,
) -> Result<FinalStats> {
  let mut arg = spec::run_arg_from_job(job, run)?;
  arg.input.threads = self_limited_threads(arg.input.threads);
  let trace = arg.output.inspect.run_trace();
  if arg.lang.is_some() {
    let worker = Arc::new(RunWithSpecificLang::new(arg, trace)?);
    let matched = stream_worker(worker.clone(), &job.printer, writer)?;
    let file_trace = worker.get_trace();
    Ok(FinalStats {
      scanned: file_trace.scanned() as u64,
      skipped: file_trace.skipped() as u64,
      matched,
      error_count: 0,
    })
  } else {
    let worker = Arc::new(RunWithInferredLang { arg, trace });
    let matched = stream_worker(worker.clone(), &job.printer, writer)?;
    let file_trace = worker.get_trace();
    Ok(FinalStats {
      scanned: file_trace.scanned() as u64,
      skipped: file_trace.skipped() as u64,
      matched,
      error_count: 0,
    })
  }
}

// ---------------------------------------------------------------------------
// The shared streaming driver
// ---------------------------------------------------------------------------

/// Reconstruct the printer from the shipped spec, then run the real walk, encoding each
/// `P::Processed` fragment into a `Rendered` frame. Returns the total **match count** (sum of
/// per-fragment counts), used for `FinalStats.matched`.
fn stream_worker<W>(
  worker: Arc<W>,
  printer_spec: &PrinterSpec,
  writer: &Arc<Mutex<FrameWriter<Stdout>>>,
) -> Result<u64>
where
  W: PathWorker + CountedProduce + 'static,
{
  match spec::printer_from_spec(printer_spec) {
    AgentPrinter::Json(p) => drive(worker, p, writer),
    AgentPrinter::Colored(p) => drive(worker, p, writer),
    AgentPrinter::FileName(p) => drive(worker, p, writer),
    AgentPrinter::Cloud(p) => drive(worker, p, writer),
  }
}

/// The real producer loop for one concrete printer `P`. Mirrors `LocalWalkProducer::produce`
/// (same discovery, same production, same error handling — a production error is a skip, exactly
/// as locally), but each rendered item is framed and streamed instead of channel-sent, tagged
/// with its true match count so the coordinator can enforce a global `--max-results` (§3.1).
fn drive<W, P>(worker: Arc<W>, printer: P, writer: &Arc<Mutex<FrameWriter<Stdout>>>) -> Result<u64>
where
  W: PathWorker + CountedProduce + 'static,
  P: Printer,
  P::Processed: WireFragment,
{
  let walker = worker.build_walk()?;
  let processor = printer.get_processor();
  // Total matches across all fragments (accurate stats), and a monotonic frame sequence.
  let matched = Arc::new(AtomicU64::new(0));
  let seq = Arc::new(AtomicU64::new(0));
  let write_err: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

  walker.run(|| {
    let worker = worker.clone();
    let processor = &processor;
    let writer = writer.clone();
    let matched = matched.clone();
    let seq = seq.clone();
    let write_err = write_err.clone();
    Box::new(move |result| {
      use ignore::WalkState;
      let Some(path) = crate::utils::filter_result(result) else {
        return WalkState::Continue;
      };
      let stats = worker.get_trace();
      stats.add_scanned();
      let Ok(items) = worker.produce_counted::<P>(&path, processor) else {
        stats.add_skipped();
        return WalkState::Continue;
      };
      for (item, count) in items {
        let mut bytes = Vec::new();
        item.encode(&mut bytes);
        // An empty fragment (e.g. an empty JSON buffer for a no-match file) carries nothing and
        // must not be framed — the local printer skips it too.
        if bytes.is_empty() {
          continue;
        }
        matched.fetch_add(u64::from(count), Ordering::AcqRel);
        let n = seq.fetch_add(1, Ordering::AcqRel);
        let frame = ResultFrame::Rendered {
          seq: n,
          epoch: 0,
          match_count: count,
          bytes,
        };
        let mut w = writer.lock().unwrap();
        if let Err(e) = w.write_message(1, &Message::Result(frame)) {
          *write_err.lock().unwrap() = Some(e.to_string());
          return WalkState::Quit;
        }
      }
      if worker.should_stop() {
        return WalkState::Quit;
      }
      WalkState::Continue
    })
  });

  if let Some(e) = write_err.lock().unwrap().take() {
    return Err(anyhow!(RemoteError::Io { path: None, msg: e }));
  }
  Ok(matched.load(Ordering::Acquire))
}

#[cfg(test)]
mod tests {
  use super::self_limited_threads;

  /// The cap never *raises* the shipped thread count — a coordinator asking for N threads gets at
  /// most N, whatever the node's budget. When a cgroup budget exists it can only lower N (or fill
  /// in for the `0` = auto sentinel). On a machine with no cgroup limit the value passes through.
  #[test]
  fn self_limit_never_exceeds_shipped_request() {
    // No cgroup on the test host (macOS/CI-without-throttle) → identity for explicit requests.
    if super::cgroup_cpu_budget().is_none() {
      assert_eq!(self_limited_threads(4), 4);
      assert_eq!(self_limited_threads(1), 1);
      assert_eq!(self_limited_threads(0), 0); // auto stays auto when nothing constrains it
    } else {
      // On a throttled host the result is bounded by the budget and never raised past the request.
      let budget = super::cgroup_cpu_budget().unwrap();
      assert!(self_limited_threads(1024) <= budget);
      assert!(self_limited_threads(1) <= 1);
      assert_eq!(self_limited_threads(0), budget);
    }
  }
}
