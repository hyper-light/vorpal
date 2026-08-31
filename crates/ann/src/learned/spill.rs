//! Bounded-memory co-occurrence counting at any scale: the classical two-phase
//! external aggregation. Events stream into a buffer; on overflow the buffer is
//! sorted, PRE-AGGREGATED (duplicates collapse before touching disk — gram fan-out
//! repeats pairs heavily), and appended to a scratch file as one sorted run; finished
//! counts stream back through a k-way merge that re-aggregates across runs. Small
//! corpora never overflow and never touch disk — the near-zero-baseline law — while
//! kernel/Meta-scale corpora hold RAM proportional to ONE buffer plus merge cursors,
//! never to the event volume (measured elsewhere: joint-gram events reach ~10 GB at
//! kernel scale and ~20× that at Meta scale if materialized).
//!
//! Every size here is derived, not tuned: the buffer follows the classical ONE-pass
//! external-merge balance `M ≥ √(N·page/pair)` (Knuth §5.4) with a floor from the
//! EXISTING policy clamp (`ResourcePolicy::arena_chunk_bytes`), and the merge fan-in
//! comes from the probed page size (one page of read-ahead per cursor).
//! Determinism: runs are sorted and pre-aggregated, the k-way merge re-aggregates
//! under a total order, and the output stream is a pure function of the aggregated
//! MULTISET — run partitioning, file layout, and feed parallelism are invisible to
//! it (`CoocCounts::from_events` byte-equality, pinned by the oracle tests). That
//! invariance is what lets [`count_ranges`] feed events in PARALLEL: each fixed
//! document range sorts and aggregates locally into one run per counter, appended
//! through a small writer pool, and the merged stream cannot tell the difference.

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// One aggregated record on disk: (min id, max id, count), little-endian, 16 bytes.
const RECORD_BYTES: usize = 16;

/// Derive the event-buffer capacity (in events) for an expected total: `√N` balanced
/// against the policy's arena clamp as the floor (small corpora stay entirely in the
/// buffer; the floor is the SAME 64 KiB–2 MiB clamp every batch structure uses).
pub fn buffer_events_for(
  expected_events: u64,
  page_bytes: usize,
  arena_chunk_bytes: usize,
) -> usize {
  // The classical ONE-merge-pass external-sort balance: a buffer of M events yields
  // ≈ N/M sorted runs and k-way-merges buffer_bytes/page_bytes of them at once, so a
  // single level suffices when N/M ≤ M·pair_bytes/page_bytes ⇒
  // M ≥ √(N · page_bytes / pair_bytes) (Knuth, TAOCP vol. 3 §5.4 merge balance).
  // The earlier plain √N dropped the page factor — measured at kernel scale as ~4,200
  // runs through TWO merge levels, 56 s of pure rewrite I/O. The policy arena chunk
  // stays as the floor so tiny corpora keep their zero-spill fast path; N is a sizing
  // ESTIMATE shaping only the buffer, never correctness.
  let pair_bytes = std::mem::size_of::<(u32, u32)>();
  let balanced = ((expected_events as f64) * (page_bytes.max(1) as f64) / pair_bytes as f64)
    .sqrt()
    .ceil() as usize;
  let floor_events = arena_chunk_bytes / pair_bytes;
  balanced.max(floor_events).max(1)
}

/// Streaming pair counter with spill-to-scratch overflow.
pub struct SpillCounter {
  scratch_path: PathBuf,
  buffer: Vec<(u32, u32)>,
  buffer_cap: usize,
  /// Per sorted run: record count (runs are appended back-to-back in file order).
  run_records: Vec<u64>,
  writer: Option<BufWriter<File>>,
  marginals: Vec<u64>,
  total_events: u64,
  /// Merge fan-in: cursors per merge pass (page-sized read-ahead per cursor).
  fan_in: usize,
  /// Probed I/O granularity — every merge cursor's read-ahead buffer.
  page_bytes: usize,
}

impl SpillCounter {
  /// `scratch_path`: where overflow runs live (created lazily on first spill; the
  /// caller owns the directory and sweeps leftovers). `buffer_cap`: from
  /// [`buffer_events_for`]. `page_bytes`: the probed I/O granularity
  /// (`HardwareProbe::base_page_bytes`) — it sizes each merge cursor's read-ahead and
  /// thereby the fan-in.
  pub fn new(scratch_path: PathBuf, buffer_cap: usize, page_bytes: usize) -> SpillCounter {
    let buffer_cap = buffer_cap.max(1);
    let page_bytes = page_bytes.max(RECORD_BYTES);
    let fan_in = ((buffer_cap * std::mem::size_of::<(u32, u32)>()) / page_bytes).max(2);
    SpillCounter {
      scratch_path,
      buffer: Vec::with_capacity(buffer_cap.min(1 << 20)),
      buffer_cap,
      run_records: Vec::new(),
      writer: None,
      marginals: Vec::new(),
      total_events: 0,
      fan_in,
      page_bytes,
    }
  }

  /// Count one symmetric event (canonicalized to (min, max); both marginals credited —
  /// identical bookkeeping to `CoocCounts::add_document`).
  pub fn push(&mut self, a: u32, b: u32) -> Result<(), String> {
    let key = (a.min(b), a.max(b));
    let max_id = key.1 as usize;
    if self.marginals.len() <= max_id {
      self.marginals.resize(max_id + 1, 0);
    }
    self.marginals[key.0 as usize] += 1;
    self.marginals[key.1 as usize] += 1;
    self.total_events += 1;
    self.buffer.push(key);
    if self.buffer.len() >= self.buffer_cap {
      self.spill()?;
    }
    Ok(())
  }

  fn spill(&mut self) -> Result<(), String> {
    if self.buffer.is_empty() {
      return Ok(());
    }
    use rayon::prelude::*;
    self.buffer.par_sort_unstable();
    if self.writer.is_none() {
      let file = File::create(&self.scratch_path)
        .map_err(|e| format!("creating spill scratch {}: {e}", self.scratch_path.display()))?;
      self.writer = Some(BufWriter::new(file));
    }
    let writer = self
      .writer
      .as_mut()
      .ok_or("spill writer vanished (invariant)")?;
    let mut records = 0u64;
    let mut index = 0usize;
    while index < self.buffer.len() {
      let key = self.buffer[index];
      let mut run = 0u64;
      while index < self.buffer.len() && self.buffer[index] == key {
        run += 1;
        index += 1;
      }
      write_record(writer, key.0, key.1, run)?;
      records += 1;
    }
    self.run_records.push(records);
    self.buffer.clear();
    Ok(())
  }

  /// Finish counting. In-buffer-only corpora yield an in-memory aggregate (no disk was
  /// ever touched); spilled corpora flush the last run and merge down to at most
  /// `fan_in` runs so the final streaming pass is a single bounded k-way merge.
  pub fn finish(mut self) -> Result<SpilledCounts, String> {
    if self.writer.is_none() {
      // Pure in-memory path: aggregate the buffer exactly like a spill would, minus
      // the file.
      use rayon::prelude::*;
      self.buffer.par_sort_unstable();
      let mut records = Vec::new();
      let mut index = 0usize;
      while index < self.buffer.len() {
        let key = self.buffer[index];
        let mut run = 0u64;
        while index < self.buffer.len() && self.buffer[index] == key {
          run += 1;
          index += 1;
        }
        records.push((key.0, key.1, run));
      }
      return Ok(SpilledCounts {
        source: CountSource::Memory(records),
        marginals: self.marginals,
        total_events: self.total_events,
        fan_in: self.fan_in,
        page_bytes: self.page_bytes,
      });
    }
    self.spill()?;
    let mut writer = self.writer.take().ok_or("spill writer vanished (invariant)")?;
    writer
      .flush()
      .map_err(|e| format!("flushing spill runs: {e}"))?;
    drop(writer);
    // Convert the back-to-back run lengths into segment runs, then merge levels until
    // one bounded k-way pass can stream everything.
    let mut runs = Vec::with_capacity(self.run_records.len());
    let mut offset_records = 0u64;
    for &records in &self.run_records {
      runs.push(SegmentRun {
        file: 0,
        offset_records,
        records,
      });
      offset_records += records;
    }
    let (files, runs) = compact_to_fan_in(
      vec![self.scratch_path.clone()],
      runs,
      self.fan_in,
      self.page_bytes,
      &self.scratch_path,
    )?;
    Ok(SpilledCounts {
      source: CountSource::Runs { files, runs },
      marginals: self.marginals,
      total_events: self.total_events,
      fan_in: self.fan_in,
      page_bytes: self.page_bytes,
    })
  }
}

/// One sorted, pre-aggregated run: which scratch file holds it, its first record's
/// offset (in RECORDS), and its record count. Runs may spread across several files
/// (the parallel feed writes through a writer pool); only the SET of runs determines
/// the merged stream — file layout never affects a bit.
#[derive(Clone, Copy)]
struct SegmentRun {
  file: u32,
  offset_records: u64,
  records: u64,
}

enum CountSource {
  Memory(Vec<(u32, u32, u64)>),
  Runs {
    files: Vec<PathBuf>,
    runs: Vec<SegmentRun>,
  },
}

/// Compact to at most `fan_in` runs in ONE parallel level: the runs split into
/// `min(threads, fan_in)` contiguous groups, each k-way merged to its own segment
/// file CONCURRENTLY (groups are independent; the immutable inputs tolerate
/// concurrent readers). Output runs ≤ groups ≤ fan_in by construction, so a single
/// level always suffices; group count shapes wall time and file layout only — the
/// merged stream is partition-invariant. Shared by the serial and parallel feeds.
fn compact_to_fan_in(
  files: Vec<PathBuf>,
  runs: Vec<SegmentRun>,
  fan_in: usize,
  page_bytes: usize,
  name_base: &Path,
) -> Result<(Vec<PathBuf>, Vec<SegmentRun>), String> {
  if runs.len() <= fan_in {
    return Ok((files, runs));
  }
  use rayon::prelude::*;
  let group_count = rayon::current_num_threads().max(1).min(fan_in).max(1);
  let per_group = runs.len().div_ceil(group_count).max(1);
  let merged: Vec<Result<(PathBuf, u64), String>> = runs
    .par_chunks(per_group)
    .enumerate()
    .map(|(group_index, group)| {
      let path = name_base.with_extension(format!("merge{group_index}"));
      let file = File::create(&path)
        .map_err(|e| format!("creating merge level {}: {e}", path.display()))?;
      let mut writer = BufWriter::new(file);
      let mut cursors = Vec::with_capacity(group.len());
      for run in group {
        let source = files
          .get(run.file as usize)
          .ok_or("run references a missing scratch file (invariant)")?;
        cursors.push(RunCursor::open(
          source,
          run.offset_records * RECORD_BYTES as u64,
          run.records,
          page_bytes,
        )?);
      }
      let mut records = 0u64;
      merge_cursors(cursors, &mut |a, b, count| {
        write_record(&mut writer, a, b, count)?;
        records += 1;
        Ok(())
      })?;
      writer
        .flush()
        .map_err(|e| format!("flushing merge level: {e}"))?;
      Ok((path, records))
    })
    .collect();
  let mut new_files = Vec::with_capacity(merged.len());
  let mut new_runs = Vec::with_capacity(merged.len());
  for outcome in merged {
    let (path, records) = outcome?;
    new_runs.push(SegmentRun {
      file: new_files.len() as u32,
      offset_records: 0,
      records,
    });
    new_files.push(path);
  }
  for file in &files {
    let _ = std::fs::remove_file(file);
  }
  Ok((new_files, new_runs))
}

/// Finished counts: marginals + a re-streamable, ascending, fully aggregated pair
/// iterator — the exact information `CoocCounts` holds, without the memory.
pub struct SpilledCounts {
  source: CountSource,
  marginals: Vec<u64>,
  total_events: u64,
  fan_in: usize,
  page_bytes: usize,
}

impl SpilledCounts {
  pub fn total_events(&self) -> u64 {
    self.total_events
  }

  pub fn marginal(&self, id: u32) -> u64 {
    self.marginals.get(id as usize).copied().unwrap_or(0)
  }

  pub fn marginals(&self) -> &[u64] {
    &self.marginals
  }

  /// Pull-based stream over every aggregated pair, ascending (min, max). Re-streamable:
  /// each call opens fresh cursors — callers make as many passes as they need
  /// (marginal-dependent transforms, CSR sizing, CSR fill), and pull semantics let two
  /// streams be ZIPPED (the σ half-split difference walks two of these in lockstep).
  pub fn iter(&self) -> Result<PairIter<'_>, String> {
    match &self.source {
      CountSource::Memory(records) => Ok(PairIter {
        inner: PairIterInner::Memory(records.iter()),
      }),
      CountSource::Runs { files, runs } => {
        debug_assert!(runs.len() <= self.fan_in);
        let mut cursors = Vec::with_capacity(runs.len());
        for run in runs {
          let path = files
            .get(run.file as usize)
            .ok_or("run references a missing scratch file (invariant)")?;
          cursors.push(RunCursor::open(
            path,
            run.offset_records * RECORD_BYTES as u64,
            run.records,
            self.page_bytes,
          )?);
        }
        Ok(PairIter {
          inner: PairIterInner::Runs {
            merge: RunMerge::new(cursors),
          },
        })
      }
    }
  }

  /// Callback convenience over [`SpilledCounts::iter`].
  pub fn for_each_pair(&self, mut consumer: impl FnMut(u32, u32, u64)) -> Result<(), String> {
    for item in self.iter()? {
      let (a, b, count) = item?;
      consumer(a, b, count);
    }
    Ok(())
  }

  /// Remove any scratch backing (the success path; absent for in-memory counts).
  pub fn delete(self) -> Result<(), String> {
    if let CountSource::Runs { files, .. } = self.source {
      for path in files {
        std::fs::remove_file(&path).map_err(|e| format!("removing {}: {e}", path.display()))?;
      }
    }
    Ok(())
  }
}

fn write_record(writer: &mut BufWriter<File>, a: u32, b: u32, count: u64) -> Result<(), String> {
  let mut record = [0u8; RECORD_BYTES];
  record[0..4].copy_from_slice(&a.to_le_bytes());
  record[4..8].copy_from_slice(&b.to_le_bytes());
  record[8..16].copy_from_slice(&count.to_le_bytes());
  writer
    .write_all(&record)
    .map_err(|e| format!("writing spill record: {e}"))
}

struct RunCursor {
  reader: BufReader<File>,
  remaining: u64,
  head: Option<(u32, u32, u64)>,
}

impl RunCursor {
  fn open(path: &std::path::Path, offset: u64, records: u64, page_bytes: usize) -> Result<Self, String> {
    let mut file = File::open(path).map_err(|e| format!("opening run: {e}"))?;
    file
      .seek(SeekFrom::Start(offset))
      .map_err(|e| format!("seeking run: {e}"))?;
    let mut cursor = RunCursor {
      reader: BufReader::with_capacity(page_bytes.max(RECORD_BYTES), file),
      remaining: records,
      head: None,
    };
    cursor.advance()?;
    Ok(cursor)
  }

  fn advance(&mut self) -> Result<(), String> {
    if self.remaining == 0 {
      self.head = None;
      return Ok(());
    }
    let mut record = [0u8; RECORD_BYTES];
    self
      .reader
      .read_exact(&mut record)
      .map_err(|e| format!("reading spill record: {e}"))?;
    self.remaining -= 1;
    let a = u32::from_le_bytes(record[0..4].try_into().map_err(|_| "record slice")?);
    let b = u32::from_le_bytes(record[4..8].try_into().map_err(|_| "record slice")?);
    let count = u64::from_le_bytes(record[8..16].try_into().map_err(|_| "record slice")?);
    self.head = Some((a, b, count));
    Ok(())
  }
}

/// K-way merge of the sorted runs in `path` (concatenated, `runs[i]` records each),
/// re-aggregating equal pairs across runs, streaming ascending output.
/// The streaming pair iterator behind [`SpilledCounts::iter`].
pub struct PairIter<'a> {
  inner: PairIterInner<'a>,
}

enum PairIterInner<'a> {
  Memory(std::slice::Iter<'a, (u32, u32, u64)>),
  Runs { merge: RunMerge },
}

impl Iterator for PairIter<'_> {
  type Item = Result<(u32, u32, u64), String>;

  fn next(&mut self) -> Option<Self::Item> {
    match &mut self.inner {
      PairIterInner::Memory(iter) => iter.next().map(|&record| Ok(record)),
      PairIterInner::Runs { merge } => merge.next_pair().transpose(),
    }
  }
}

/// K-way aggregating merge over run cursors: a min-heap of `(pair, cursor)` heads
/// costs O(log k) per record instead of two O(k) head scans — measured at kernel
/// scale, the linear form put ~176 s of pure head-scanning into the σ and CSR
/// streaming passes once the one-level merge balance left ~700 live runs. Every run
/// holds at most one record per key (runs are aggregated at birth), so draining a
/// key pops each contributing cursor exactly once; counts are integers, so the
/// summation order cannot change a single bit.
struct RunMerge {
  cursors: Vec<RunCursor>,
  heap: std::collections::BinaryHeap<std::cmp::Reverse<((u32, u32), usize)>>,
}

impl RunMerge {
  fn new(cursors: Vec<RunCursor>) -> Self {
    let mut heap = std::collections::BinaryHeap::with_capacity(cursors.len());
    for (index, cursor) in cursors.iter().enumerate() {
      if let Some((a, b, _)) = cursor.head {
        heap.push(std::cmp::Reverse(((a, b), index)));
      }
    }
    RunMerge { cursors, heap }
  }

  /// The smallest head across cursors, ALL equal heads drained and summed
  /// (aggregation makes cursor order irrelevant to the output). `Ok(None)` = every
  /// cursor exhausted.
  fn next_pair(&mut self) -> Result<Option<(u32, u32, u64)>, String> {
    let Some(&std::cmp::Reverse((key, _))) = self.heap.peek() else {
      return Ok(None);
    };
    let mut total = 0u64;
    while let Some(&std::cmp::Reverse((head, index))) = self.heap.peek() {
      if head != key {
        break;
      }
      self.heap.pop();
      let cursor = &mut self.cursors[index];
      let Some((_, _, count)) = cursor.head else {
        return Err("merge heap referenced an exhausted cursor (invariant)".to_string());
      };
      total += count;
      cursor.advance()?;
      if let Some((a, b, _)) = cursor.head {
        self.heap.push(std::cmp::Reverse(((a, b), index)));
      }
    }
    Ok(Some((key.0, key.1, total)))
  }
}

/// Callback loop over [`RunMerge`] (the merge-level writer path).
fn merge_cursors(
  cursors: Vec<RunCursor>,
  consumer: &mut impl FnMut(u32, u32, u64) -> Result<(), String>,
) -> Result<(), String> {
  let mut merge = RunMerge::new(cursors);
  while let Some((a, b, count)) = merge.next_pair()? {
    consumer(a, b, count)?;
  }
  Ok(())
}

/// The k-way merge fan-in the spill law affords: how many run cursors (one page
/// each) fit inside the balanced buffer. The Knuth balance makes
/// ceil(events/buffer) land AT this fan-in, so a parallel feed that caps its range
/// count here produces one run per range per counter and consumers merge them in a
/// single level — no compaction pass (measured NET-NEGATIVE at kernel scale: ~26 s
/// of read+rewrite to save ~17 s of consumer heap depth).
pub fn merge_fan_in(buffer_events: usize, page_bytes: usize) -> usize {
  let page_bytes = page_bytes.max(RECORD_BYTES);
  ((buffer_events.max(1) * std::mem::size_of::<(u32, u32)>()) / page_bytes).max(2)
}

/// PARALLEL event feed for the three training counters (full + the two σ halves):
/// `emit(range, push)` generates document range `range`'s events — `push(a, b, half)`
/// counts the pair in the FULL counter and in half `half` — with ranges processed in
/// parallel. Each range sorts + pre-aggregates its events locally (exactly the
/// serial spill's per-buffer step) into ONE run per counter, appended through a
/// small writer pool; marginals and totals are integer sums (order-free). The merged
/// streams are bit-equal to the serial [`SpillCounter`] feed over the same events —
/// aggregation makes run partitioning, file layout, and scheduling invisible — and
/// the equality oracle pins it. `id_bound` is an exclusive upper bound on every
/// pushed id: marginals pre-size ONCE per worker split (per-range exact growth was
/// measured as the dominant parallel-feed cost at kernel scale) and an out-of-bound
/// id is a typed error. Returns `[full, half0, half1]`.
pub fn count_ranges<F>(
  scratch_dir: &Path,
  range_count: usize,
  id_bound: usize,
  buffer_events: usize,
  page_bytes: usize,
  emit: F,
) -> Result<[SpilledCounts; 3], String>
where
  F: Fn(usize, &mut dyn FnMut(u32, u32, u8)) + Sync,
{
  use rayon::prelude::*;
  let page_bytes = page_bytes.max(RECORD_BYTES);
  let fan_in = merge_fan_in(buffer_events, page_bytes);
  let pool_size = rayon::current_num_threads().max(1);
  // Per counter: a fixed pool of scratch files with lock-guarded writers. Which file
  // a range's run lands in is scheduling-shaped only; the run SET is deterministic.
  struct CounterPool {
    files: Vec<PathBuf>,
    writers: Vec<Mutex<(BufWriter<File>, u64)>>,
  }
  let make_pool = |name: &str| -> Result<CounterPool, String> {
    let mut files = Vec::with_capacity(pool_size);
    let mut writers = Vec::with_capacity(pool_size);
    for slot in 0..pool_size {
      let path = scratch_dir.join(format!("train-cooc-{name}-{slot}.spill"));
      let file = File::create(&path)
        .map_err(|e| format!("creating spill scratch {}: {e}", path.display()))?;
      files.push(path);
      writers.push(Mutex::new((BufWriter::new(file), 0u64)));
    }
    Ok(CounterPool { files, writers })
  };
  let pools = [make_pool("full")?, make_pool("half0")?, make_pool("half1")?];

  /// Per rayon SPLIT (bounded ≈ 2×threads by `with_min_len`), reused across every
  /// range that split processes: pre-sized marginals (the per-range exact-growth
  /// resize was measured as the parallel feed's dominant cost — quadratic-ish memcpy
  /// 700× over), cleared-and-reused event buffers, and the split's accumulated runs.
  struct WorkerState {
    buffers: [Vec<(u32, u32)>; 3],
    marginals: [Vec<u64>; 3],
    totals: [u64; 3],
    runs: [Vec<SegmentRun>; 3],
    error: Option<String>,
  }
  let flush_run = |pool: &CounterPool, range: usize, events: &mut Vec<(u32, u32)>| -> Result<Option<SegmentRun>, String> {
    if events.is_empty() {
      return Ok(None);
    }
    events.sort_unstable();
    // Serialize the aggregated records locally, then append under the writer lock in
    // one bounded write (the lock never covers sorting or aggregation).
    let mut bytes = Vec::with_capacity(events.len().min(1 << 20) * RECORD_BYTES);
    let mut records = 0u64;
    let mut index = 0usize;
    while index < events.len() {
      let key = events[index];
      let mut run = 0u64;
      while index < events.len() && events[index] == key {
        run += 1;
        index += 1;
      }
      bytes.extend_from_slice(&key.0.to_le_bytes());
      bytes.extend_from_slice(&key.1.to_le_bytes());
      bytes.extend_from_slice(&run.to_le_bytes());
      records += 1;
    }
    let slot = range % pool.writers.len();
    let mut guard = pool.writers[slot]
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner());
    let offset_records = guard.1;
    guard
      .0
      .write_all(&bytes)
      .map_err(|e| format!("writing spill run: {e}"))?;
    guard.1 += records;
    Ok(Some(SegmentRun {
      file: slot as u32,
      offset_records,
      records,
    }))
  };

  let min_ranges_per_split = range_count
    .div_ceil(pool_size.saturating_mul(2).max(1))
    .max(1);
  let workers: Vec<WorkerState> = (0..range_count)
    .into_par_iter()
    .with_min_len(min_ranges_per_split)
    .fold(
      || WorkerState {
        buffers: std::array::from_fn(|_| Vec::new()),
        marginals: std::array::from_fn(|_| vec![0u64; id_bound]),
        totals: [0u64; 3],
        runs: std::array::from_fn(|_| Vec::new()),
        error: None,
      },
      |mut state, range| {
        if state.error.is_some() {
          return state;
        }
        for buffer in &mut state.buffers {
          buffer.clear();
        }
        let mut out_of_bound = false;
        {
          let buffers = &mut state.buffers;
          let marginals = &mut state.marginals;
          let totals = &mut state.totals;
          emit(range, &mut |a: u32, b: u32, half: u8| {
            let key = (a.min(b), a.max(b));
            if key.1 as usize >= id_bound {
              out_of_bound = true;
              return;
            }
            let half_counter = 1 + (half as usize & 1);
            for counter in [0usize, half_counter] {
              marginals[counter][key.0 as usize] += 1;
              marginals[counter][key.1 as usize] += 1;
              totals[counter] += 1;
              buffers[counter].push(key);
            }
          });
        }
        if out_of_bound {
          state.error = Some(format!("event id outside bound {id_bound} (caller invariant)"));
          return state;
        }
        let mut flush_error: Option<String> = None;
        for ((pool, buffer), runs) in pools
          .iter()
          .zip(&mut state.buffers)
          .zip(&mut state.runs)
        {
          match flush_run(pool, range, buffer) {
            Ok(Some(run)) => runs.push(run),
            Ok(None) => {}
            Err(reason) => {
              flush_error = Some(reason);
              break;
            }
          }
        }
        if let Some(reason) = flush_error {
          state.error = Some(reason);
        }
        state
      },
    )
    .collect();

  let mut counters: Vec<SpilledCounts> = Vec::with_capacity(3);
  for (counter, pool) in pools.into_iter().enumerate() {
    for writer in &pool.writers {
      writer
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .0
        .flush()
        .map_err(|e| format!("flushing spill runs: {e}"))?;
    }
    let mut marginals: Vec<u64> = vec![0; id_bound];
    let mut total_events = 0u64;
    let mut runs = Vec::new();
    for worker in &workers {
      if let Some(reason) = &worker.error {
        return Err(reason.clone());
      }
      runs.extend_from_slice(&worker.runs[counter]);
      total_events += worker.totals[counter];
      for (total, part) in marginals.iter_mut().zip(&worker.marginals[counter]) {
        *total += part;
      }
    }
    // Bit parity with the serial marginal length: SpillCounter's vec ends at the
    // highest seen id, whose marginal is necessarily nonzero — trailing zeros trim
    // to exactly that length.
    while marginals.last() == Some(&0) {
      marginals.pop();
    }
    let name_base = pool.files.first().cloned().unwrap_or_else(|| {
      scratch_dir.join(format!("train-cooc-{counter}.spill"))
    });
    let (files, runs) =
      compact_to_fan_in(pool.files, runs, fan_in, page_bytes, &name_base)?;
    counters.push(SpilledCounts {
      source: CountSource::Runs { files, runs },
      marginals,
      total_events,
      fan_in,
      page_bytes,
    });
  }
  let [full, half0, half1]: [SpilledCounts; 3] = counters
    .try_into()
    .map_err(|_| "counter assembly (invariant)".to_string())?;
  Ok([full, half0, half1])
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::learned::cooc::CoocCounts;

  fn scratch(name: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!("vorpal-spill-{}-{name}.bin", std::process::id()));
    let _ = std::fs::remove_file(&path);
    path
  }

  fn random_events(n: usize, id_space: u32, seed: u64) -> Vec<(u32, u32)> {
    let mut state = seed.max(1);
    let mut next = move || {
      state ^= state << 13;
      state ^= state >> 7;
      state ^= state << 17;
      state
    };
    (0..n)
      .map(|_| {
        let a = (next() % id_space as u64) as u32;
        let b = (next() % id_space as u64) as u32;
        (a.min(b), a.max(b))
      })
      .collect()
  }

  fn reference(events: &[(u32, u32)]) -> Vec<(u32, u32, u64)> {
    let counts = CoocCounts::from_events(events.to_vec());
    counts.pairs().map(|(&(a, b), &c)| (a, b, c)).collect()
  }

  fn collect(counts: &SpilledCounts) -> Vec<(u32, u32, u64)> {
    let mut out = Vec::new();
    counts
      .for_each_pair(|a, b, c| out.push((a, b, c)))
      .unwrap();
    out
  }

  #[test]
  fn tiny_corpora_never_touch_disk_and_match_reference() {
    let path = scratch("tiny");
    let events = random_events(500, 40, 3);
    let mut counter = SpillCounter::new(path.clone(), 10_000, 4096);
    for &(a, b) in &events {
      counter.push(a, b).unwrap();
    }
    let counts = counter.finish().unwrap();
    assert!(!path.exists(), "in-buffer corpora must not create scratch");
    assert_eq!(collect(&counts), reference(&events));
    counts.delete().unwrap();
  }

  #[test]
  fn spilled_multi_run_counts_match_reference_exactly() {
    let path = scratch("runs");
    let events = random_events(20_000, 300, 9);
    // Tiny buffer forces many runs; page 64 forces a small fan-in and thus at least
    // one intermediate merge level.
    let mut counter = SpillCounter::new(path.clone(), 128, 64);
    for &(a, b) in &events {
      counter.push(a, b).unwrap();
    }
    let reference_counts = reference(&events);
    let ref_counts_struct = CoocCounts::from_events(events.clone());
    let counts = counter.finish().unwrap();
    assert_eq!(collect(&counts), reference_counts);
    assert_eq!(counts.total_events(), ref_counts_struct.total_events());
    for id in 0..300u32 {
      assert_eq!(counts.marginal(id), ref_counts_struct.marginal(id), "marginal {id}");
    }
    // Re-streamable: a second pass yields the identical stream.
    assert_eq!(collect(&counts), reference_counts);
    counts.delete().unwrap();
  }

  #[test]
  fn buffer_derivation_is_merge_balance_with_policy_floor() {
    // Exact at a perfect square: M = √(N·page/pair) = √(2048·16384/8) = 2048.
    assert_eq!(buffer_events_for(2048, 16384, 8), 2048);
    // The ONE-merge-pass property the derivation exists for: runs = ⌈N/M⌉ never
    // exceeds the k-way fan-in M·pair_bytes/page_bytes (± the final resident run).
    for &(events, page) in &[(1_000_000_000_000u64, 16384usize), (1_100_000_000, 16384)] {
      let m = buffer_events_for(events, page, 8) as u64;
      let runs = events.div_ceil(m);
      let fan_in = (m as usize * std::mem::size_of::<(u32, u32)>() / page) as u64;
      assert!(
        runs <= fan_in + 1,
        "{events} events, page {page}: {runs} runs > fan-in {fan_in}"
      );
    }
    // …and the arena-clamp floor below it (64 KiB / 8 B per event = 8192 events).
    assert_eq!(buffer_events_for(100, 16384, 64 * 1024), 8192);
  }

  #[test]
  fn parallel_ranges_match_the_serial_feed_bitwise() {
    // The invariance the parallel feed rests on, asserted end to end: the SAME
    // (a, b, half) event stream through the serial SpillCounter trio (tiny buffer —
    // real runs, real merges) and through `count_ranges` (parallel ranges, writer
    // pool, per-range runs) yields BIT-EQUAL aggregated streams, marginals, and
    // totals for all three counters.
    let events = random_events(50_000, 700, 33);
    let docs: Vec<&[(u32, u32)]> = events.chunks(5).collect();
    let dir = std::env::temp_dir().join(format!("vorpal-spill-par-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let mut full = SpillCounter::new(dir.join("s-full.spill"), 64, 64);
    let mut half0 = SpillCounter::new(dir.join("s-half0.spill"), 64, 64);
    let mut half1 = SpillCounter::new(dir.join("s-half1.spill"), 64, 64);
    for (doc_index, doc) in docs.iter().enumerate() {
      for &(a, b) in *doc {
        full.push(a, b).unwrap();
        if doc_index % 2 == 0 {
          half0.push(a, b).unwrap();
        } else {
          half1.push(a, b).unwrap();
        }
      }
    }
    let serial = [
      full.finish().unwrap(),
      half0.finish().unwrap(),
      half1.finish().unwrap(),
    ];

    // Uneven ranges (13 docs — deliberately misaligned with the doc-parity period).
    let ranges_dir = dir.join("ranges");
    std::fs::create_dir_all(&ranges_dir).unwrap();
    let docs_per_range = 13usize;
    let range_count = docs.len().div_ceil(docs_per_range);
    let parallel = count_ranges(&ranges_dir, range_count, 700, 64, 64, |range, push| {
      let start = range * docs_per_range;
      let end = ((range + 1) * docs_per_range).min(docs.len());
      for (doc_index, doc) in docs.iter().enumerate().take(end).skip(start) {
        for &(a, b) in *doc {
          push(a, b, (doc_index % 2) as u8);
        }
      }
    })
    .unwrap();

    for (serial_counts, parallel_counts) in serial.iter().zip(&parallel) {
      assert_eq!(serial_counts.total_events(), parallel_counts.total_events());
      assert_eq!(serial_counts.marginals(), parallel_counts.marginals());
      assert_eq!(collect(serial_counts), collect(parallel_counts));
    }
    let _ = std::fs::remove_dir_all(&dir);
  }

  #[test]
  fn deterministic_across_reruns() {
    let events = random_events(5_000, 100, 21);
    let run = |name: &str| {
      let path = scratch(name);
      let mut counter = SpillCounter::new(path, 64, 64);
      for &(a, b) in &events {
        counter.push(a, b).unwrap();
      }
      let counts = counter.finish().unwrap();
      let out = collect(&counts);
      counts.delete().unwrap();
      out
    };
    assert_eq!(run("det-a"), run("det-b"));
  }
}
