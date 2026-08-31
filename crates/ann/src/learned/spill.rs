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
//! Every size here is derived, not tuned: the buffer follows the classical external-
//! sort balance `B = √N` (minimizing max(buffer, runs) — the textbook optimum) with a
//! floor from the EXISTING policy clamp (`ResourcePolicy::arena_chunk_bytes`), and the
//! merge fan-in comes from the probed page size (one page of read-ahead per cursor).
//! Determinism: runs are sorted, the merge is ordered by (pair, run index), and the
//! output stream is byte-equal to the in-memory reference (`CoocCounts::from_events`)
//! — pinned by the oracle tests.

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;

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
    // Multi-level merge until one bounded k-way pass can stream everything.
    let mut path = self.scratch_path.clone();
    let mut runs = self.run_records.clone();
    let mut generation = 0usize;
    while runs.len() > self.fan_in {
      let next_path = path.with_extension(format!("merge{generation}"));
      runs = merge_level(&path, &runs, &next_path, self.fan_in, self.page_bytes)?;
      let _ = std::fs::remove_file(&path);
      path = next_path;
      generation += 1;
    }
    Ok(SpilledCounts {
      source: CountSource::Runs { path, runs },
      marginals: self.marginals,
      total_events: self.total_events,
      fan_in: self.fan_in,
      page_bytes: self.page_bytes,
    })
  }
}

enum CountSource {
  Memory(Vec<(u32, u32, u64)>),
  Runs { path: PathBuf, runs: Vec<u64> },
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
      CountSource::Runs { path, runs } => {
        debug_assert!(runs.len() <= self.fan_in);
        let mut offset = 0u64;
        let mut cursors = Vec::with_capacity(runs.len());
        for &records in runs {
          cursors.push(RunCursor::open(path, offset, records, self.page_bytes)?);
          offset += records * RECORD_BYTES as u64;
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
    if let CountSource::Runs { path, .. } = self.source {
      std::fs::remove_file(&path).map_err(|e| format!("removing {}: {e}", path.display()))?;
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

/// One merge level: combine groups of `fan_in` runs from `input` into aggregated runs
/// in `output`; returns the new run table.
fn merge_level(
  input: &std::path::Path,
  runs: &[u64],
  output: &std::path::Path,
  fan_in: usize,
  page_bytes: usize,
) -> Result<Vec<u64>, String> {
  let file =
    File::create(output).map_err(|e| format!("creating merge level {}: {e}", output.display()))?;
  let mut writer = BufWriter::new(file);
  let mut new_runs = Vec::new();
  let mut index = 0usize;
  while index < runs.len() {
    let group = &runs[index..(index + fan_in).min(runs.len())];
    // The byte offset of this group's first run inside the concatenated input.
    let group_offset: u64 = runs[..index].iter().sum::<u64>() * RECORD_BYTES as u64;
    let mut cursors = Vec::with_capacity(group.len());
    let mut offset = group_offset;
    for &records in group {
      cursors.push(RunCursor::open(input, offset, records, page_bytes)?);
      offset += records * RECORD_BYTES as u64;
    }
    let mut records = 0u64;
    merge_cursors(cursors, &mut |a, b, count| {
      write_record(&mut writer, a, b, count)?;
      records += 1;
      Ok(())
    })?;
    new_runs.push(records);
    index += group.len();
  }
  writer
    .flush()
    .map_err(|e| format!("flushing merge level: {e}"))?;
  Ok(new_runs)
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
