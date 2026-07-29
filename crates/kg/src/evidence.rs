//! The per-edge evidence sidecar (`evidence.bin`, IMPROVEMENTS §5): one fixed-width row per
//! edge *occurrence*, retaining what resolution knew when it bound the edge — the source span
//! of the referencing token, the resolver branch that chose the target, the packed confidence,
//! and the candidate count. This is what lets every persisted relation answer "why does this
//! relation exist?" instead of only "how confident was it?".
//!
//! Layout: magic + version + count, then 24-byte little-endian rows
//! `[from u32][to u32][etype u16][reason u8][confidence u8][candidates u32][span_start u32][span_end u32]`
//! sorted by `(from, to, etype, span_start, span_end)` — a canonical order, so the artifact is
//! a pure function of the edge set (deterministic, and it joins the generation's content id).
//! Lookup is a binary search on the `(from, to)` prefix. The file is optional: an index
//! without one (older generation) simply answers no evidence, never an error.

use std::fs;
use std::io::{self, BufWriter, Write};
use std::path::Path;

use vorpal_mem::{AccessPattern, CorpusProbe, Hotness, MappedStore, ResourcePolicy, StoreKind};

const MAGIC: &[u8; 4] = b"VEVD";
const VERSION: u32 = 1;
const ROW: usize = 24;

/// One edge occurrence's retained evidence. `etype` is the base edge type (confidence carried
/// separately); `reason` is a `vorpal_resolve::ResolveReason` tag, kept raw here so the store
/// stays dependency-free — consumers label it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EvidenceRow {
  pub from: u32,
  pub to: u32,
  pub etype: u16,
  pub reason: u8,
  pub confidence: u8,
  pub candidates: u32,
  pub span_start: u32,
  pub span_end: u32,
}

impl EvidenceRow {
  fn key(&self) -> (u32, u32, u16, u32, u32) {
    (self.from, self.to, self.etype, self.span_start, self.span_end)
  }
}

/// Persist rows as `evidence.bin` under `dir` (tmp + rename). Rows are sorted canonically here
/// — callers hand over arrival order.
pub fn save(dir: &Path, mut rows: Vec<EvidenceRow>) -> io::Result<()> {
  rows.sort_unstable_by_key(EvidenceRow::key);
  let tmp = dir.join("evidence.bin.tmp");
  let mut out = BufWriter::with_capacity(1 << 20, fs::File::create(&tmp)?);
  out.write_all(MAGIC)?;
  out.write_all(&VERSION.to_le_bytes())?;
  out.write_all(&(rows.len() as u64).to_le_bytes())?;
  for row in &rows {
    out.write_all(&row.from.to_le_bytes())?;
    out.write_all(&row.to.to_le_bytes())?;
    out.write_all(&row.etype.to_le_bytes())?;
    out.write_all(&[row.reason, row.confidence])?;
    out.write_all(&row.candidates.to_le_bytes())?;
    out.write_all(&row.span_start.to_le_bytes())?;
    out.write_all(&row.span_end.to_le_bytes())?;
  }
  out.flush()?;
  drop(out);
  fs::rename(&tmp, dir.join("evidence.bin"))
}

/// The mapped read side: rows stay on disk; lookups touch only the pages a binary search and
/// its matching run need.
pub struct EvidenceStore {
  store: MappedStore,
  count: usize,
}

impl EvidenceStore {
  /// Map `evidence.bin` under `dir`, if present and well-formed. `None` is "no evidence
  /// recorded" (older generation, torn file) — a degraded answer, never an error.
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
    if bytes.len() < 16 || &bytes[0..4] != MAGIC {
      return None;
    }
    if u32::from_le_bytes(bytes[4..8].try_into().ok()?) != VERSION {
      return None;
    }
    let count = u64::from_le_bytes(bytes[8..16].try_into().ok()?) as usize;
    if bytes.len() != 16 + count * ROW {
      return None; // torn write or foreign generation
    }
    Some(EvidenceStore { store, count })
  }

  fn row(&self, i: usize) -> EvidenceRow {
    let at = 16 + i * ROW;
    let b = &self.store.as_bytes()[at..at + ROW];
    EvidenceRow {
      from: u32::from_le_bytes(b[0..4].try_into().unwrap()),
      to: u32::from_le_bytes(b[4..8].try_into().unwrap()),
      etype: u16::from_le_bytes(b[8..10].try_into().unwrap()),
      reason: b[10],
      confidence: b[11],
      candidates: u32::from_le_bytes(b[12..16].try_into().unwrap()),
      span_start: u32::from_le_bytes(b[16..20].try_into().unwrap()),
      span_end: u32::from_le_bytes(b[20..24].try_into().unwrap()),
    }
  }

  /// Every retained occurrence of edges `from → to`, across all edge types, in canonical
  /// order. Binary search on the sorted `(from, to)` prefix, then the contiguous run.
  pub fn edges_between(&self, from: u32, to: u32) -> Vec<EvidenceRow> {
    let lo = self.partition(|r| (r.from, r.to) < (from, to));
    let hi = self.partition(|r| (r.from, r.to) <= (from, to));
    (lo..hi).map(|i| self.row(i)).collect()
  }

  /// Every retained occurrence originating at `from`, in canonical order — "why does this
  /// node relate to anything?" for one-sided queries.
  pub fn edges_from(&self, from: u32) -> Vec<EvidenceRow> {
    let lo = self.partition(|r| r.from < from);
    let hi = self.partition(|r| r.from <= from);
    (lo..hi).map(|i| self.row(i)).collect()
  }

  /// Every retained row, in canonical order — the complete emitted-resolution-edge list,
  /// which is exactly the denominator a precision measurement needs.
  pub fn rows(&self) -> impl Iterator<Item = EvidenceRow> + '_ {
    (0..self.count).map(|i| self.row(i))
  }

  pub fn len(&self) -> usize {
    self.count
  }

  pub fn is_empty(&self) -> bool {
    self.count == 0
  }

  /// `partition_point` over the mapped rows.
  fn partition(&self, pred: impl Fn(&EvidenceRow) -> bool) -> usize {
    let (mut lo, mut hi) = (0usize, self.count);
    while lo < hi {
      let mid = (lo + hi) / 2;
      if pred(&self.row(mid)) {
        lo = mid + 1;
      } else {
        hi = mid;
      }
    }
    lo
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn roundtrips_sorts_and_looks_up() {
    let dir = std::env::temp_dir().join(format!("vorpal-evidence-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let row = |from, to, span: u32| EvidenceRow {
      from,
      to,
      etype: 1,
      reason: 6,
      confidence: 90,
      candidates: 1,
      span_start: span,
      span_end: span + 4,
    };
    // Arrival order is scrambled; save canonicalizes.
    save(&dir, vec![row(2, 7, 40), row(1, 3, 10), row(2, 7, 20), row(1, 9, 5)]).unwrap();
    let store = EvidenceStore::open(&dir).unwrap();
    assert_eq!(store.len(), 4);
    let hits = store.edges_between(2, 7);
    assert_eq!(hits.len(), 2);
    assert!(hits[0].span_start < hits[1].span_start, "canonical span order");
    assert_eq!(store.edges_between(9, 9), Vec::new());
    assert_eq!(store.edges_from(1).len(), 2);

    // Determinism: same rows, any arrival order → identical bytes.
    let a = fs::read(dir.join("evidence.bin")).unwrap();
    save(&dir, vec![row(1, 9, 5), row(2, 7, 20), row(1, 3, 10), row(2, 7, 40)]).unwrap();
    assert_eq!(a, fs::read(dir.join("evidence.bin")).unwrap());

    // Torn file loads as None, not an error.
    let mut bytes = fs::read(dir.join("evidence.bin")).unwrap();
    bytes.truncate(bytes.len() - 5);
    fs::write(dir.join("evidence.bin"), &bytes).unwrap();
    assert!(EvidenceStore::open(&dir).is_none());
    let _ = fs::remove_dir_all(&dir);
  }
}
