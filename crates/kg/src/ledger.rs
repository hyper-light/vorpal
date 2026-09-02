//! Exact allocation + fault + contention ledger (feature `alloc-ledger`).
//!
//! jemalloc's `stats.allocated` is LIVE bytes — a phase that allocates and frees
//! ten gigabytes shows a delta of zero. Churn is what the hyper-optimization
//! campaign hunts, so this module holds event counters the binary's wrapping
//! allocator bumps on every `alloc`/`dealloc`/`realloc`, tree-sitter's C-side
//! events on their own lines, the process's cumulative page-fault and rusage
//! readings, and counters at the pipeline's known serialization points.
//! [`snapshot`] is printed at every phase stamp; per-phase deltas are computed
//! offline from the trace.
//!
//! **The measurement must not manufacture the contention it measures**: the
//! first ledger build used four global atomics and doubled kernel-scale user
//! CPU purely on cache-line ping-pong (~10⁹ events × 14 threads on two lines).
//! Hot counters are therefore sharded across 32 cache-line-aligned slots picked
//! by pthread identity — allocator-context-safe (no TLS init, no allocation) —
//! and summed at snapshot time. Slow-path counters (lock contention, parks,
//! full channels) stay single atomics: they count events that already blocked.

use std::sync::atomic::{AtomicU64, Ordering};

const SLOT_COUNT: usize = 32;

/// One thread-affine counter slot: the full hot set colocated on isolated
/// cache lines so a thread's bumps never ping-pong with its neighbors'.
#[repr(align(128))]
struct Slot {
  allocs: AtomicU64,
  deallocs: AtomicU64,
  reallocs: AtomicU64,
  alloc_bytes: AtomicU64,
  ts_allocs: AtomicU64,
  ts_reallocs: AtomicU64,
  ts_frees: AtomicU64,
  ts_alloc_bytes: AtomicU64,
  /// Reentrancy guard for backtrace sampling — capture/format/store all
  /// allocate, and those allocations must not re-enter the sampler.
  sampling: std::sync::atomic::AtomicBool,
}

impl Slot {
  const fn new() -> Self {
    Self {
      allocs: AtomicU64::new(0),
      deallocs: AtomicU64::new(0),
      reallocs: AtomicU64::new(0),
      alloc_bytes: AtomicU64::new(0),
      ts_allocs: AtomicU64::new(0),
      ts_reallocs: AtomicU64::new(0),
      ts_frees: AtomicU64::new(0),
      ts_alloc_bytes: AtomicU64::new(0),
      sampling: std::sync::atomic::AtomicBool::new(false),
    }
  }
}

static SLOTS: [Slot; SLOT_COUNT] = [const { Slot::new() }; SLOT_COUNT];

/// The calling thread's slot. `pthread_self` is allocation-free and safe in
/// allocator context (thread-locals are NOT: their lazy init can allocate).
/// pthread_t values are pointers into per-thread control blocks — page-plus
/// aligned, so bits 12+ decorrelate threads; a collision merely shares a slot.
#[inline]
fn slot() -> &'static Slot {
  #[cfg(unix)]
  let id = unsafe { libc::pthread_self() } as usize;
  #[cfg(not(unix))]
  let id = 0usize;
  &SLOTS[(id >> 12) & (SLOT_COUNT - 1)]
}

/// Note one allocation of `bytes` — called by the binary's wrapping allocator.
#[inline]
pub fn note_alloc(bytes: usize) {
  let s = slot();
  let n = s.allocs.fetch_add(1, Ordering::Relaxed);
  s.alloc_bytes.fetch_add(bytes as u64, Ordering::Relaxed);
  let mask = SAMPLE_MASK.load(Ordering::Relaxed);
  if mask != 0 && (n & mask) == 0 {
    sample_backtrace(s, bytes);
  }
}

/// Note one deallocation.
#[inline]
pub fn note_dealloc(_bytes: usize) {
  slot().deallocs.fetch_add(1, Ordering::Relaxed);
}

/// Note one reallocation to `new_bytes`.
#[inline]
pub fn note_realloc(new_bytes: usize) {
  let s = slot();
  let n = s.reallocs.fetch_add(1, Ordering::Relaxed);
  s.alloc_bytes.fetch_add(new_bytes as u64, Ordering::Relaxed);
  let mask = REALLOC_SAMPLE_MASK.load(Ordering::Relaxed);
  if mask != 0 && (n & mask) == 0 {
    sample_backtrace(s, new_bytes);
  }
}

// --- tree-sitter C-side counters: the parser's allocations route through
// `ts_set_allocator` shims, not the Rust global allocator — counting them
// separately attributes parse churn (the pipeline's dominant phase) on its own
// ledger line.

#[inline]
pub fn note_ts_alloc(bytes: usize) {
  let s = slot();
  let n = s.ts_allocs.fetch_add(1, Ordering::Relaxed);
  s.ts_alloc_bytes.fetch_add(bytes as u64, Ordering::Relaxed);
  let mask = TS_SAMPLE_MASK.load(Ordering::Relaxed);
  if mask != 0 && (n & mask) == 0 {
    sample_backtrace(s, bytes);
  }
}

#[inline]
pub fn note_ts_realloc(new_bytes: usize) {
  let s = slot();
  let n = s.ts_reallocs.fetch_add(1, Ordering::Relaxed);
  s.ts_alloc_bytes.fetch_add(new_bytes as u64, Ordering::Relaxed);
  // Reallocs sample under the same TS mask: the per-grammar sweep showed
  // realloc-storm outliers (hundreds per KB against a ~5 median) whose
  // growth chains are exactly what the backtrace names.
  let mask = TS_SAMPLE_MASK.load(Ordering::Relaxed);
  if mask != 0 && (n & mask) == 0 {
    sample_backtrace(s, new_bytes);
  }
}

#[inline]
pub fn note_ts_free() {
  slot().ts_frees.fetch_add(1, Ordering::Relaxed);
}

// --- contention counters: bumped from the pipeline's known serialization
// points under their crates' forwarded `alloc-ledger` features. Each lives on
// a slow path already (a failed try-lock, a park, a full channel), so a single
// shared atomic is honest — the event it counts costs orders of magnitude more.

/// Interner shard read-locks that could not be taken immediately (try_read
/// failed → the caller blocked behind a writer or writer-queue).
pub static INTERN_READ_CONTENDED: AtomicU64 = AtomicU64::new(0);
/// Interner shard write-lock acquisitions (≈ first-sight string inserts).
pub static INTERN_WRITES: AtomicU64 = AtomicU64::new(0);
/// ByteBudget admissions that parked on the condvar (no room in flight).
pub static BUDGET_PARKS: AtomicU64 = AtomicU64::new(0);
/// Bounded-channel sends that found the channel full and blocked.
pub static CHAN_FULL: AtomicU64 = AtomicU64::new(0);

// --- callsite attribution sampling (`VORPAL_ALLOC_SAMPLE=<shift>`): every
// 2^shift-th Rust allocation (per slot) captures a symbolized backtrace into a
// bounded site table, dumped at process end. At shift 16 a kernel-scale build
// takes ~2,400 samples for ~0.3 s of guarded capture cost — enough to rank the
// stream phase's 155 M allocations by callsite without distorting them.

/// `2^shift − 1`, or 0 when sampling is off. Written ONCE from
/// [`init_sampling_from_env`] before any allocator activity worth sampling —
/// never lazily from allocator context (env reads allocate).
static SAMPLE_MASK: AtomicU64 = AtomicU64::new(0);

/// Sampling mask for the tree-sitter C-side shims (`VORPAL_TS_SAMPLE=<shift>`)
/// — separate from the Rust mask so a per-grammar parse profile can sample
/// the C side densely without the Rust sites swamping the table (or vice
/// versa). Backtraces through the vendored runtime symbolize C frames
/// (subtree pool, lexer arrays, external scanners), which is what attributes
/// each grammar's parse churn to actual callsites.
static TS_SAMPLE_MASK: AtomicU64 = AtomicU64::new(0);

/// Sampling mask for REALLOCATIONS only (`VORPAL_REALLOC_SAMPLE=<shift>`) — growth
/// chains hide inside the alloc histogram (a Vec that doubles eight times is one
/// logical site but eight reallocs), so realloc-heavy regressions get their own lens.
static REALLOC_SAMPLE_MASK: AtomicU64 = AtomicU64::new(0);

/// Read `VORPAL_ALLOC_SAMPLE` / `VORPAL_TS_SAMPLE` and arm sampling. Call
/// from `main`, never from allocator context.
pub fn init_sampling_from_env() {
  if let Some(shift) = std::env::var("VORPAL_ALLOC_SAMPLE")
    .ok()
    .and_then(|v| v.parse::<u32>().ok())
  {
    SAMPLE_MASK.store((1u64 << shift.min(40)) - 1, Ordering::Relaxed);
  }
  if let Some(shift) = std::env::var("VORPAL_TS_SAMPLE")
    .ok()
    .and_then(|v| v.parse::<u32>().ok())
  {
    TS_SAMPLE_MASK.store((1u64 << shift.min(40)) - 1, Ordering::Relaxed);
  }
  if let Some(shift) = std::env::var("VORPAL_REALLOC_SAMPLE")
    .ok()
    .and_then(|v| v.parse::<u32>().ok())
  {
    REALLOC_SAMPLE_MASK.store((1u64 << shift.min(40)) - 1, Ordering::Relaxed);
  }
}

/// Distinct callsites retained; one overflow bucket keeps the count honest
/// beyond it. Linear-scanned — sampling is rare and the table small.
const MAX_SITES: usize = 512;

/// (site hash, samples, bytes, symbolized trace)
static SAMPLES: std::sync::Mutex<Vec<(u64, u64, u64, String)>> = std::sync::Mutex::new(Vec::new());
static SAMPLE_OVERFLOW: AtomicU64 = AtomicU64::new(0);

#[cold]
fn sample_backtrace(slot: &Slot, bytes: usize) {
  use std::sync::atomic::AtomicBool;
  let _ = AtomicBool::new(false); // keep the import local to the cold path
  if slot.sampling.swap(true, Ordering::Acquire) {
    return; // a sample (or its allocations) is already in flight on this slot
  }
  let text = format!("{}", std::backtrace::Backtrace::force_capture());
  // FNV-1a over the trace text — no hasher allocation, stable within a run.
  let mut hash: u64 = 0xcbf29ce484222325;
  for byte in text.bytes() {
    hash ^= byte as u64;
    hash = hash.wrapping_mul(0x100000001b3);
  }
  if let Ok(mut samples) = SAMPLES.lock() {
    if let Some(entry) = samples.iter_mut().find(|(h, ..)| *h == hash) {
      entry.1 += 1;
      entry.2 += bytes as u64;
    } else if samples.len() < MAX_SITES {
      samples.push((hash, 1, bytes as u64, text));
    } else {
      SAMPLE_OVERFLOW.fetch_add(1, Ordering::Relaxed);
    }
  }
  slot.sampling.store(false, Ordering::Release);
}

/// Print the sampled-callsite histogram (top sites by sample count) to stderr.
/// A no-op when sampling never armed or nothing was captured.
pub fn dump_samples() {
  let Ok(mut samples) = SAMPLES.lock() else {
    return;
  };
  if samples.is_empty() {
    return;
  }
  samples.sort_unstable_by(|a, b| b.1.cmp(&a.1));
  let total: u64 = samples.iter().map(|(_, n, ..)| n).sum();
  let overflow = SAMPLE_OVERFLOW.load(Ordering::Relaxed);
  eprintln!(
    "[alloc-sample] {total} samples across {} sites (overflow {overflow}); top sites:",
    samples.len()
  );
  for (rank, (_, count, bytes, trace)) in samples.iter().take(24).enumerate() {
    eprintln!(
      "[alloc-sample] #{rank}: {count} samples ({:.1}%), ~{}MB sampled",
      *count as f64 * 100.0 / total.max(1) as f64,
      bytes / 1048576
    );
    // Trim allocator/ledger scaffolding; keep the meaningful application frames.
    for line in trace
      .lines()
      .filter(|l| {
        !l.contains("ledger::") && !l.contains("::alloc::") && !l.contains("backtrace::")
      })
      .take(16)
    {
      eprintln!("[alloc-sample]   {line}");
    }
  }
}

/// One cumulative reading of every ledger dimension.
#[derive(Debug, Clone, Copy, Default)]
pub struct Snapshot {
  pub allocs: u64,
  pub deallocs: u64,
  pub reallocs: u64,
  pub alloc_bytes: u64,
  pub ts_allocs: u64,
  pub ts_reallocs: u64,
  pub ts_frees: u64,
  pub ts_alloc_bytes: u64,
  /// Total page faults (macOS: mach `TASK_EVENTS_INFO` faults; Linux: minflt+majflt).
  pub faults: u64,
  /// Copy-on-write faults (macOS only; 0 elsewhere).
  pub cow_faults: u64,
  /// Pages read in from backing store (macOS pageins / Linux majflt).
  pub pageins: u64,
  /// Cumulative user CPU microseconds (getrusage) — per-phase deltas over wall
  /// time give the phase's parallel efficiency.
  pub user_us: u64,
  /// Cumulative system CPU microseconds.
  pub sys_us: u64,
  /// Voluntary context switches — blocking waits (locks, channels, parks).
  pub vcsw: u64,
  /// Involuntary context switches — preemption.
  pub ivcsw: u64,
  pub intern_read_contended: u64,
  pub intern_writes: u64,
  pub budget_parks: u64,
  pub chan_full: u64,
}

pub fn snapshot() -> Snapshot {
  let (faults, cow_faults, pageins) = os_faults();
  let (user_us, sys_us, vcsw, ivcsw) = rusage_self();
  let mut snap = Snapshot {
    faults,
    cow_faults,
    pageins,
    user_us,
    sys_us,
    vcsw,
    ivcsw,
    intern_read_contended: INTERN_READ_CONTENDED.load(Ordering::Relaxed),
    intern_writes: INTERN_WRITES.load(Ordering::Relaxed),
    budget_parks: BUDGET_PARKS.load(Ordering::Relaxed),
    chan_full: CHAN_FULL.load(Ordering::Relaxed),
    ..Snapshot::default()
  };
  for slot in &SLOTS {
    snap.allocs += slot.allocs.load(Ordering::Relaxed);
    snap.deallocs += slot.deallocs.load(Ordering::Relaxed);
    snap.reallocs += slot.reallocs.load(Ordering::Relaxed);
    snap.alloc_bytes += slot.alloc_bytes.load(Ordering::Relaxed);
    snap.ts_allocs += slot.ts_allocs.load(Ordering::Relaxed);
    snap.ts_reallocs += slot.ts_reallocs.load(Ordering::Relaxed);
    snap.ts_frees += slot.ts_frees.load(Ordering::Relaxed);
    snap.ts_alloc_bytes += slot.ts_alloc_bytes.load(Ordering::Relaxed);
  }
  snap
}

/// Cumulative (user µs, sys µs, voluntary csw, involuntary csw) for this
/// process via `getrusage(RUSAGE_SELF)` — best-effort zeros on failure.
#[cfg(unix)]
fn rusage_self() -> (u64, u64, u64, u64) {
  let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
  if unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) } != 0 {
    return (0, 0, 0, 0);
  }
  let tv_us = |tv: libc::timeval| tv.tv_sec.max(0) as u64 * 1_000_000 + tv.tv_usec.max(0) as u64;
  (
    tv_us(usage.ru_utime),
    tv_us(usage.ru_stime),
    usage.ru_nvcsw.max(0) as u64,
    usage.ru_nivcsw.max(0) as u64,
  )
}

#[cfg(not(unix))]
fn rusage_self() -> (u64, u64, u64, u64) {
  (0, 0, 0, 0)
}

/// (total faults, cow faults, pageins) for this process, best-effort — a read
/// failure reports zeros rather than disturbing the run.
#[cfg(target_os = "macos")]
fn os_faults() -> (u64, u64, u64) {
  // mach TASK_EVENTS_INFO: cumulative event counts for the calling task. Fields
  // are `integer_t` (i32) and can wrap on extremely long runs; the ledger reads
  // them as saturating-nonnegative.
  #[repr(C)]
  struct TaskEventsInfo {
    faults: i32,
    pageins: i32,
    cow_faults: i32,
    messages_sent: i32,
    messages_received: i32,
    syscalls_mach: i32,
    syscalls_unix: i32,
    csw: i32,
  }
  const TASK_EVENTS_INFO: u32 = 2;
  const COUNT: u32 = (std::mem::size_of::<TaskEventsInfo>() / std::mem::size_of::<i32>()) as u32;
  unsafe extern "C" {
    static mach_task_self_: u32;
    fn task_info(target: u32, flavor: u32, info: *mut i32, count: *mut u32) -> i32;
  }
  let mut info = TaskEventsInfo {
    faults: 0,
    pageins: 0,
    cow_faults: 0,
    messages_sent: 0,
    messages_received: 0,
    syscalls_mach: 0,
    syscalls_unix: 0,
    csw: 0,
  };
  let mut count = COUNT;
  let kr = unsafe {
    task_info(
      mach_task_self_,
      TASK_EVENTS_INFO,
      &mut info as *mut TaskEventsInfo as *mut i32,
      &mut count,
    )
  };
  if kr != 0 {
    return (0, 0, 0);
  }
  (
    info.faults.max(0) as u64,
    info.cow_faults.max(0) as u64,
    info.pageins.max(0) as u64,
  )
}

#[cfg(target_os = "linux")]
fn os_faults() -> (u64, u64, u64) {
  // /proc/self/stat: minflt is field 10, majflt field 12 (1-based), counted
  // after the parenthesised comm (which may contain spaces).
  use std::io::Read;
  let mut buf = [0u8; 512];
  let read = std::fs::File::open("/proc/self/stat")
    .and_then(|mut f| f.read(&mut buf))
    .unwrap_or(0);
  let Some(text) = std::str::from_utf8(&buf[..read]).ok() else {
    return (0, 0, 0);
  };
  let Some(after_comm) = text.rsplit_once(')').map(|(_, rest)| rest) else {
    return (0, 0, 0);
  };
  let mut fields = after_comm.split_ascii_whitespace();
  // after_comm starts at field 3 (state); minflt = field 10, majflt = field 12.
  let minflt: u64 = fields.nth(7).and_then(|v| v.parse().ok()).unwrap_or(0);
  let majflt: u64 = fields.nth(1).and_then(|v| v.parse().ok()).unwrap_or(0);
  (minflt + majflt, 0, majflt)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn os_faults() -> (u64, u64, u64) {
  (0, 0, 0)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn counters_accumulate_and_snapshot_reads_them() {
    let before = snapshot();
    note_alloc(128);
    note_realloc(256);
    note_dealloc(128);
    note_ts_alloc(64);
    let after = snapshot();
    assert!(after.allocs >= before.allocs + 1);
    assert!(after.reallocs >= before.reallocs + 1);
    assert!(after.deallocs >= before.deallocs + 1);
    assert!(after.alloc_bytes >= before.alloc_bytes + 128 + 256);
    assert!(after.ts_allocs >= before.ts_allocs + 1);
    assert!(after.ts_alloc_bytes >= before.ts_alloc_bytes + 64);
  }

  #[cfg(any(target_os = "macos", target_os = "linux"))]
  #[test]
  fn fault_and_rusage_readings_are_nonzero_for_a_live_process() {
    // Any process that got this far has faulted pages in and burned CPU; zeros
    // would mean the OS read paths are broken (silent under-reporting).
    let snap = snapshot();
    assert!(snap.faults > 0, "process fault count should be observable");
    assert!(snap.user_us > 0, "user CPU should be observable");
  }
}
