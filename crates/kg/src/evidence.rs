//! The per-edge evidence sidecar (`evidence.bin`, IMPROVEMENTS §5 / 07-29 §4): one fixed-width
//! row per reference *occurrence*, retaining what resolution knew — the source span of the
//! referencing token, the resolver branch, the packed confidence, the candidate count, the
//! retained tie-set alternatives, and (v2) the occurrences that produced **no** edge. This is
//! what lets every relation answer "why does this exist?", "why this target and not the
//! alternatives?", and "why is there no edge here at all?".
//!
//! Layout v2: `magic + version + row_count u64 + alt_pool_len u64`, then 36-byte little-endian
//! rows `[from u32][to u32][name_hash u32][etype u16][reason u8][confidence u8][outcome u8]
//! [alt_count u8][pad u16][candidates u32][span_start u32][span_end u32][alt_off u32]`,
//! followed by the alternatives pool (`alt_pool_len` u32 node ids; `alt_off` indexes entries).
//! `to == u32::MAX` marks a no-edge outcome (external/masked). Rows sort by
//! `(from, to, etype, span_start, span_end)` — canonical, so the artifact is a pure function
//! of the occurrence set (deterministic; it joins the generation's content id), with no-edge
//! rows sorting after a `from`'s real edges. Lookup is a binary search on the `(from, to)`
//! prefix. The file is optional and versioned: v1 or missing files simply answer "no evidence
//! recorded", never an error.

use std::fs;
use std::io::{self, Write};
use std::path::Path;

use vorpal_mem::{AccessPattern, CorpusProbe, Hotness, MappedStore, ResourcePolicy, StoreKind};

const MAGIC: &[u8; 4] = b"VEVD";
const VERSION: u32 = 2;
const ROW: usize = 36;
const HEADER: usize = 24;

/// The `to` sentinel marking an occurrence that produced no edge.
pub const NO_EDGE: u32 = u32::MAX;

/// What an occurrence produced. Stored as a `u8` tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvidenceOutcome {
  /// A graph edge (`to` is a real node).
  Edge,
  /// No definition with the name exists in the tree (std/dependency target).
  External,
  /// Definitions exist, but none was safely attributable from this site.
  Masked,
}

impl EvidenceOutcome {
  pub fn tag(self) -> u8 {
    match self {
      Self::Edge => 0,
      Self::External => 1,
      Self::Masked => 2,
    }
  }

  pub fn from_tag(tag: u8) -> Self {
    match tag {
      1 => Self::External,
      2 => Self::Masked,
      _ => Self::Edge,
    }
  }

  pub fn label(self) -> &'static str {
    match self {
      Self::Edge => "edge",
      Self::External => "external",
      Self::Masked => "masked",
    }
  }
}

/// One occurrence's retained evidence. `etype` is the base edge type (confidence carried
/// separately); `reason` is a `vorpal_resolve::ResolveReason` tag, kept raw here so the store
/// stays dependency-free — consumers label it. `alternatives` holds the retained tie-set
/// candidate ids the chosen target beat (empty for unique picks and no-edge rows).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvidenceRow {
  pub from: u32,
  /// Target node id, or [`NO_EDGE`] for external/masked occurrences.
  pub to: u32,
  /// Low 32 bits of xxh3 of the referenced name — the absence-query key.
  pub name_hash: u32,
  pub etype: u16,
  pub reason: u8,
  pub confidence: u8,
  pub outcome: EvidenceOutcome,
  pub candidates: u32,
  pub span_start: u32,
  pub span_end: u32,
  pub alternatives: Vec<u32>,
}

impl EvidenceRow {
  fn key(&self) -> (u32, u32, u16, u32, u32) {
    (self.from, self.to, self.etype, self.span_start, self.span_end)
  }
}

/// Persist rows as `evidence.bin` under `dir` (tmp + rename). Rows are sorted canonically here
/// — callers hand over arrival order — and the alternatives pool is laid out in sorted-row
/// order, so the bytes are a pure function of the occurrence set.
pub fn save(dir: &Path, mut rows: Vec<EvidenceRow>) -> io::Result<()> {
  use rayon::prelude::*;
  crate::phase_stamp("evidence: sort start");
  // Sort under a TOTAL order (canonical key, then every remaining field): the sorted
  // sequence is unique, so the output is a pure function of the row set — deterministic
  // under any sort algorithm and any thread count, which is what licenses the parallel
  // unstable sort. (The previous serial sort keyed only on `key()`; ties, if any ever
  // exist, were ordered by pdqsort's whims — the total order is strictly more canonical.)
  rows.par_sort_unstable_by(|a, b| {
    a.key()
      .cmp(&b.key())
      .then_with(|| a.name_hash.cmp(&b.name_hash))
      .then_with(|| a.reason.cmp(&b.reason))
      .then_with(|| a.confidence.cmp(&b.confidence))
      .then_with(|| a.outcome.tag().cmp(&b.outcome.tag()))
      .then_with(|| a.candidates.cmp(&b.candidates))
      .then_with(|| a.alternatives.cmp(&b.alternatives))
  });
  crate::phase_stamp("evidence: encode start");

  // Fixed-width rows encode at exact offsets: chunk the row set, prefix-sum each chunk's
  // alternative count (the only cross-row dependency), then encode rows and pool in
  // parallel straight into their final positions. The serial form issued ~13 tiny
  // `write_all`s per row — ~88M calls at kernel scale, single-threaded.
  const CHUNK: usize = 64 * 1024;
  let chunk_alt_counts: Vec<u32> = rows
    .par_chunks(CHUNK)
    .map(|chunk| chunk.iter().map(|r| r.alternatives.len() as u32).sum())
    .collect();
  let mut chunk_alt_offsets = Vec::with_capacity(chunk_alt_counts.len());
  let mut running = 0u32;
  for &count in &chunk_alt_counts {
    chunk_alt_offsets.push(running);
    running += count;
  }
  let pool_len = running as usize;

  let mut rows_buf = vec![0u8; rows.len() * ROW];
  let mut pool_buf = vec![0u8; pool_len * 4];
  {
    // Pool slices per chunk are disjoint by construction (prefix offsets); split_at_mut
    // walks them off the front in order.
    let mut pool_rest: &mut [u8] = &mut pool_buf;
    let mut pool_slices = Vec::with_capacity(chunk_alt_counts.len());
    for &count in &chunk_alt_counts {
      let (head, tail) = pool_rest.split_at_mut(count as usize * 4);
      pool_slices.push(head);
      pool_rest = tail;
    }
    rows
      .par_chunks(CHUNK)
      .zip(rows_buf.par_chunks_mut(CHUNK * ROW))
      .zip(pool_slices)
      .zip(chunk_alt_offsets)
      .for_each(|(((chunk, out_rows), out_pool), mut alt_off)| {
        let mut pool_at = 0usize;
        for (row, out) in chunk.iter().zip(out_rows.chunks_exact_mut(ROW)) {
          out[0..4].copy_from_slice(&row.from.to_le_bytes());
          out[4..8].copy_from_slice(&row.to.to_le_bytes());
          out[8..12].copy_from_slice(&row.name_hash.to_le_bytes());
          out[12..14].copy_from_slice(&row.etype.to_le_bytes());
          out[14] = row.reason;
          out[15] = row.confidence;
          out[16] = row.outcome.tag();
          out[17] = row.alternatives.len().min(255) as u8;
          out[18] = 0;
          out[19] = 0;
          out[20..24].copy_from_slice(&row.candidates.to_le_bytes());
          out[24..28].copy_from_slice(&row.span_start.to_le_bytes());
          out[28..32].copy_from_slice(&row.span_end.to_le_bytes());
          out[32..36].copy_from_slice(&alt_off.to_le_bytes());
          for &alt in &row.alternatives {
            out_pool[pool_at..pool_at + 4].copy_from_slice(&alt.to_le_bytes());
            pool_at += 4;
          }
          alt_off += row.alternatives.len() as u32;
        }
      });
  }
  crate::phase_stamp("evidence: write start");

  let tmp = dir.join("evidence.bin.tmp");
  let mut out = fs::File::create(&tmp)?;
  let mut header = [0u8; 24];
  header[0..4].copy_from_slice(MAGIC);
  header[4..8].copy_from_slice(&VERSION.to_le_bytes());
  header[8..16].copy_from_slice(&(rows.len() as u64).to_le_bytes());
  header[16..24].copy_from_slice(&(pool_len as u64).to_le_bytes());
  out.write_all(&header)?;
  out.write_all(&rows_buf)?;
  out.write_all(&pool_buf)?;
  drop(out);
  crate::phase_stamp("evidence: save done");
  fs::rename(&tmp, dir.join("evidence.bin"))
}

/// The mapped read side: rows stay on disk; lookups touch only the pages a binary search and
/// its matching run need.
pub struct EvidenceStore {
  store: MappedStore,
  count: usize,
  pool_at: usize,
  pool_len: usize,
}

impl EvidenceStore {
  /// Map `evidence.bin` under `dir`, if present and current-format. `None` is "no evidence
  /// recorded" (older/foreign generation, torn file) — a degraded answer, never an error.
  pub fn open(dir: &Path) -> Option<EvidenceStore> {
    let store = MappedStore::map_file(
      &dir.join("evidence.bin"),
      StoreKind::Canonical,
      AccessPattern::Random,
      Hotness::Cold,
      &ResourcePolicy::probe(CorpusProbe::new(0, 0)),
    )
    .ok()?;
    let bytes = store.as_bytes();
    if bytes.len() < HEADER || &bytes[0..4] != MAGIC {
      return None;
    }
    if u32::from_le_bytes(bytes[4..8].try_into().ok()?) != VERSION {
      return None;
    }
    let count = u64::from_le_bytes(bytes[8..16].try_into().ok()?) as usize;
    let pool_len = u64::from_le_bytes(bytes[16..24].try_into().ok()?) as usize;
    let pool_at = HEADER + count * ROW;
    if bytes.len() != pool_at + pool_len * 4 {
      return None; // torn write or foreign generation
    }
    Some(EvidenceStore {
      store,
      count,
      pool_at,
      pool_len,
    })
  }

  /// Visit every retained occurrence's referenced-name hash (all outcomes — edges, external,
  /// masked), decoding nothing else: one strided u32 read per row. This is the population
  /// for "was this name referenced anywhere?" suppression (dead-code precision), where
  /// materializing full rows would cost hundreds of MB at kernel scale.
  pub fn for_each_name_hash(&self, mut f: impl FnMut(u32)) {
    let bytes = self.store.as_bytes();
    for i in 0..self.count {
      let at = HEADER + i * ROW + 8;
      f(u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()));
    }
  }

  fn row(&self, i: usize) -> EvidenceRow {
    let at = HEADER + i * ROW;
    let b = &self.store.as_bytes()[at..at + ROW];
    let alt_count = b[17] as usize;
    let alt_off = u32::from_le_bytes(b[32..36].try_into().unwrap()) as usize;
    let alternatives = (alt_off..(alt_off + alt_count).min(self.pool_len))
      .map(|slot| {
        let p = self.pool_at + slot * 4;
        u32::from_le_bytes(self.store.as_bytes()[p..p + 4].try_into().unwrap())
      })
      .collect();
    EvidenceRow {
      from: u32::from_le_bytes(b[0..4].try_into().unwrap()),
      to: u32::from_le_bytes(b[4..8].try_into().unwrap()),
      name_hash: u32::from_le_bytes(b[8..12].try_into().unwrap()),
      etype: u16::from_le_bytes(b[12..14].try_into().unwrap()),
      reason: b[14],
      confidence: b[15],
      outcome: EvidenceOutcome::from_tag(b[16]),
      candidates: u32::from_le_bytes(b[20..24].try_into().unwrap()),
      span_start: u32::from_le_bytes(b[24..28].try_into().unwrap()),
      span_end: u32::from_le_bytes(b[28..32].try_into().unwrap()),
      alternatives,
    }
  }

  /// Every retained occurrence of edges `from → to`, across all edge types, in canonical
  /// order. Binary search on the sorted `(from, to)` prefix, then the contiguous run.
  pub fn edges_between(&self, from: u32, to: u32) -> Vec<EvidenceRow> {
    let lo = self.partition(|r| (r.from, r.to) < (from, to));
    let hi = self.partition(|r| (r.from, r.to) <= (from, to));
    (lo..hi).map(|i| self.row(i)).collect()
  }

  /// Every retained occurrence originating at `from` — real edges first, then any no-edge
  /// outcomes (their `to` sentinel sorts last).
  pub fn edges_from(&self, from: u32) -> Vec<EvidenceRow> {
    let lo = self.partition(|r| r.from < from);
    let hi = self.partition(|r| r.from <= from);
    (lo..hi).map(|i| self.row(i)).collect()
  }

  /// The no-edge occurrences at `from` whose referenced-name hash matches — "why is there no
  /// edge from here to anything named X?".
  pub fn absences_from(&self, from: u32, name_hash: u32) -> Vec<EvidenceRow> {
    let lo = self.partition(|r| (r.from, r.to) < (from, NO_EDGE));
    let hi = self.partition(|r| r.from <= from);
    (lo..hi)
      .map(|i| self.row(i))
      .filter(|r| r.name_hash == name_hash)
      .collect()
  }

  /// Every retained row, in canonical order — the complete occurrence population (edges and
  /// no-edge outcomes), which is exactly what a precision/recall measurement needs.
  pub fn rows(&self) -> impl Iterator<Item = EvidenceRow> + '_ {
    (0..self.count).map(|i| self.row(i))
  }

  pub fn len(&self) -> usize {
    self.count
  }

  pub fn is_empty(&self) -> bool {
    self.count == 0
  }

  /// `partition_point` over the mapped rows (key fields only — no pool reads).
  fn partition(&self, pred: impl Fn(&EvidenceKey) -> bool) -> usize {
    let (mut lo, mut hi) = (0usize, self.count);
    while lo < hi {
      let mid = (lo + hi) / 2;
      if pred(&self.key(mid)) {
        lo = mid + 1;
      } else {
        hi = mid;
      }
    }
    lo
  }

  fn key(&self, i: usize) -> EvidenceKey {
    let at = HEADER + i * ROW;
    let b = &self.store.as_bytes()[at..at + 8];
    EvidenceKey {
      from: u32::from_le_bytes(b[0..4].try_into().unwrap()),
      to: u32::from_le_bytes(b[4..8].try_into().unwrap()),
    }
  }
}

/// The `(from, to)` prefix a partition probe needs — avoids materializing alternatives per probe.
struct EvidenceKey {
  from: u32,
  to: u32,
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn roundtrips_sorts_and_looks_up() {
    let dir = std::env::temp_dir().join(format!("vorpal-evidence-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let row = |from, to, span: u32, alts: Vec<u32>| EvidenceRow {
      from,
      to,
      name_hash: 0xBEEF,
      etype: 1,
      reason: 6,
      confidence: 90,
      outcome: EvidenceOutcome::Edge,
      candidates: 1 + alts.len() as u32,
      span_start: span,
      span_end: span + 4,
      alternatives: alts,
    };
    let absent = |from, span: u32, external: bool| EvidenceRow {
      from,
      to: NO_EDGE,
      name_hash: 0xD00D,
      etype: 1,
      reason: 0,
      confidence: 0,
      outcome: if external {
        EvidenceOutcome::External
      } else {
        EvidenceOutcome::Masked
      },
      candidates: if external { 0 } else { 3 },
      span_start: span,
      span_end: span + 4,
      alternatives: Vec::new(),
    };
    // Arrival order scrambled; save canonicalizes rows AND the alternatives pool.
    save(
      &dir,
      vec![
        row(2, 7, 40, vec![9, 11]),
        absent(1, 90, true),
        row(1, 3, 10, vec![]),
        row(2, 7, 20, vec![5]),
        absent(2, 80, false),
      ],
    )
    .unwrap();
    let store = EvidenceStore::open(&dir).unwrap();
    assert_eq!(store.len(), 5);
    let hits = store.edges_between(2, 7);
    assert_eq!(hits.len(), 2);
    assert!(hits[0].span_start < hits[1].span_start, "canonical span order");
    assert_eq!(hits[0].alternatives, vec![5], "pool follows sorted rows");
    assert_eq!(hits[1].alternatives, vec![9, 11]);
    // Absence lookups by (from, name_hash); no-edge rows sort after real edges.
    assert_eq!(store.absences_from(1, 0xD00D).len(), 1);
    assert_eq!(store.absences_from(1, 0xD00D)[0].outcome, EvidenceOutcome::External);
    assert_eq!(store.absences_from(2, 0xD00D)[0].outcome, EvidenceOutcome::Masked);
    assert_eq!(store.absences_from(2, 0xBEEF), Vec::new());
    assert_eq!(store.edges_from(2).len(), 3, "edges then absences");

    // Determinism: same rows, any arrival order → identical bytes.
    let a = fs::read(dir.join("evidence.bin")).unwrap();
    save(
      &dir,
      vec![
        absent(2, 80, false),
        row(2, 7, 20, vec![5]),
        absent(1, 90, true),
        row(1, 3, 10, vec![]),
        row(2, 7, 40, vec![9, 11]),
      ],
    )
    .unwrap();
    assert_eq!(a, fs::read(dir.join("evidence.bin")).unwrap());

    // Torn file loads as None, not an error.
    let mut bytes = fs::read(dir.join("evidence.bin")).unwrap();
    bytes.truncate(bytes.len() - 3);
    fs::write(dir.join("evidence.bin"), &bytes).unwrap();
    assert!(EvidenceStore::open(&dir).is_none());
    let _ = fs::remove_dir_all(&dir);
  }
}
