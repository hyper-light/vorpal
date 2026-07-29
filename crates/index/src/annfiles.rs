//! `ann.files` — the vector tier's per-file identity map, and the machinery that diffs it
//! against a live KG.
//!
//! The base `ann.bin` embeds one KG generation, but node ids are dense row indices
//! reassigned by every re-link. What *is* stable across generations is structure: every
//! file's nodes occupy one contiguous id range, ranges appear in manifest (path-sorted)
//! order, and an unchanged file reproduces byte-identical rows. `ann.files` records, for
//! the generation the base embedded, each file's `(path, id range, row digest)`; diffing it
//! against the current KG's ranges yields exactly the overlay the search path needs —
//! unchanged files remap by offset, changed/new files re-embed exactly, vanished files
//! tombstone.
//!
//! The digest is `xxh3` over the file's rows' fixed-width `(kind_tag u8, content_hash u64)`
//! pairs: no string reads (~1ms for millions of rows), no concatenation boundary ambiguity,
//! and `kind` participation means an Import↔definition flip (which changes vector-tier row
//! membership without changing any string) changes the digest. Range-length inequality is
//! additionally treated as "changed" by the differ — a free second factor against digest
//! collision.

use std::fs;
use std::io::{self, BufWriter, Write};
use std::path::Path;

use vorpal_kg::{Kg, NodeId};

const FILES_MAGIC: u32 = 0x5641_4631; // "VAF1"

/// One file's slice of a KG generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRun {
  pub path: String,
  /// First node id of the file's contiguous range.
  pub start: u64,
  /// Number of nodes in the range (File node + items + members, Imports included).
  pub len: u32,
  /// xxh3 over the range's `(kind_tag, content_hash)` pairs.
  pub digest: u64,
}

/// Scan a sealed KG into its per-file runs, in id order. Grouping is by contiguous equal
/// `path` — sound because commit assigns every file's nodes consecutively in manifest
/// order (pinned by the sharded/streamed byte-identity oracles).
pub fn file_runs_of(kg: &Kg) -> Vec<FileRun> {
  use rayon::prelude::*;
  let n = kg.node_count() as u64;
  if n == 0 {
    return Vec::new();
  }
  // Parallel boundary scan (a row starts a run iff its path differs from its predecessor's),
  // then parallel per-run hashing. Both passes are pure per-row reads; the ordered collects
  // make the output identical to the serial walk.
  let starts: Vec<u64> = (0..n)
    .into_par_iter()
    .filter(|&i| {
      i == 0
        || match (kg.node(NodeId::new(i - 1)), kg.node(NodeId::new(i))) {
          (Some(prev), Some(here)) => prev.path != here.path,
          _ => true,
        }
    })
    .collect();
  starts
    .par_iter()
    .enumerate()
    .map(|(run_index, &start)| {
      let end = starts.get(run_index + 1).copied().unwrap_or(n);
      let mut hasher = xxhash_rust::xxh3::Xxh3::new();
      for at in start..end {
        if let Some(view) = kg.node(NodeId::new(at)) {
          hasher.update(&[kind_tag(view.kind)]);
          hasher.update(&view.content_hash.to_le_bytes());
        }
      }
      FileRun {
        path: kg
          .node(NodeId::new(start))
          .map(|view| view.path.to_string())
          .unwrap_or_default(),
        start,
        len: (end - start) as u32,
        digest: hasher.digest(),
      }
    })
    .collect()
}

/// A dense tag per symbol kind for digest input. Only stability within one process pair of
/// writes/reads matters less than *any change changing the digest* — `SymbolKind` is
/// `#[repr]`-stable in the KG's own serialized tag space, which we reuse.
fn kind_tag(kind: vorpal_kg::SymbolKind) -> u8 {
  kind.tag()
}

/// Persist `runs` as `ann.files` under `dir` (tmp + rename), stamped with the generation of
/// the KG they describe.
pub fn save(dir: &Path, base_stamp: u64, runs: &[FileRun]) -> io::Result<()> {
  let tmp = dir.join("ann.files.tmp");
  let mut out = BufWriter::with_capacity(1 << 20, fs::File::create(&tmp)?);
  out.write_all(&FILES_MAGIC.to_le_bytes())?;
  out.write_all(&base_stamp.to_le_bytes())?;
  out.write_all(&(runs.len() as u64).to_le_bytes())?;
  for run in runs {
    out.write_all(&(run.path.len() as u32).to_le_bytes())?;
    out.write_all(run.path.as_bytes())?;
    out.write_all(&run.start.to_le_bytes())?;
    out.write_all(&run.len.to_le_bytes())?;
    out.write_all(&run.digest.to_le_bytes())?;
  }
  out.flush()?;
  drop(out);
  let dest = dir.join("ann.files");
  #[cfg(windows)]
  let _ = fs::remove_file(&dest);
  fs::rename(&tmp, &dest)
}

/// Load `ann.files` from `dir`: `(base_stamp, runs)`. `None` for missing/foreign/torn files
/// — callers treat that exactly like a stale tier (fallback).
pub fn load(dir: &Path) -> Option<(u64, Vec<FileRun>)> {
  // Generation-aware: an index root resolves to its live generation; a resolved dir is a no-op.
  let dir = &vorpal_kg::resolve_index_dir(dir);
  let bytes = fs::read(dir.join("ann.files")).ok()?;
  let u32_at = |at: usize| -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
  };
  let u64_at = |at: usize| -> Option<u64> {
    Some(u64::from_le_bytes(bytes.get(at..at + 8)?.try_into().ok()?))
  };
  if u32_at(0)? != FILES_MAGIC {
    return None;
  }
  let base_stamp = u64_at(4)?;
  let count = u64_at(12)? as usize;
  let mut runs = Vec::with_capacity(count);
  let mut at = 20usize;
  for _ in 0..count {
    let path_len = u32_at(at)? as usize;
    let path = std::str::from_utf8(bytes.get(at + 4..at + 4 + path_len)?).ok()?;
    let fixed = at + 4 + path_len;
    runs.push(FileRun {
      path: path.to_string(),
      start: u64_at(fixed)?,
      len: u32_at(fixed + 8)?,
      digest: u64_at(fixed + 12)?,
    });
    at = fixed + 20;
  }
  Some((base_stamp, runs))
}

/// The reconciliation of a base ANN generation with the current KG: which base rows still
/// stand (and where their ids moved), which are dead, and which current rows must be
/// scored exactly because the base never embedded them.
pub struct OverlayView {
  /// Base-generation ranges in id order: `(base_start, len, remap_target)` where the target
  /// is the file's current start id (unchanged file) or `None` (changed/deleted — dead).
  ranges: Vec<(u64, u32, Option<u64>)>,
  /// Current-generation node ids the base does not cover (changed + new files), Imports
  /// excluded — exactly the vector tier's row policy.
  pub overlay_ids: Vec<u64>,
  /// Base nodes whose rows are dead — the beam-search overfetch bump.
  pub tombstoned_nodes: u64,
}

/// Overlay refusal ceiling: past this fraction of changed nodes, exhaustive scan of
/// everything is both simpler and about as fast as base + a huge overlay.
const MAX_OVERLAY_PERCENT: u64 = 15;

impl OverlayView {
  /// Reconcile the persisted base (ann.bin header + ann.files) against the live `kg`.
  /// `None` whenever the artifacts cannot be trusted or the overlay is too large — the
  /// caller falls back to the exhaustive scan, which is always correct.
  pub fn assemble(dir: &Path, kg: &Kg, dim: usize) -> Option<OverlayView> {
    // Generation-aware: resolve an index root to its live generation (no-op when resolved).
    let dir = &vorpal_kg::resolve_index_dir(dir);
    let (bin_dim, bin_stamp) = vorpal_ann::AnnIndex::peek_header(&dir.join("ann.bin"))?;
    if bin_dim != dim {
      return None;
    }
    let (files_stamp, base_runs) = load(dir)?;
    if files_stamp != bin_stamp {
      return None; // torn: bin and sidecar are from different generations
    }
    let current_runs = file_runs_of(kg);

    // Both run lists are path-sorted (manifest order) — one merge pass.
    let mut ranges: Vec<(u64, u32, Option<u64>)> = Vec::with_capacity(base_runs.len());
    let mut overlay_ranges: Vec<(u64, u32)> = Vec::new();
    let mut tombstoned_nodes = 0u64;
    let (mut b, mut c) = (0usize, 0usize);
    while b < base_runs.len() || c < current_runs.len() {
      match (base_runs.get(b), current_runs.get(c)) {
        (Some(base), Some(current)) if base.path == current.path => {
          if base.len == current.len && base.digest == current.digest {
            ranges.push((base.start, base.len, Some(current.start)));
          } else {
            ranges.push((base.start, base.len, None));
            tombstoned_nodes += base.len as u64;
            overlay_ranges.push((current.start, current.len));
          }
          b += 1;
          c += 1;
        }
        (Some(base), Some(current)) if base.path < current.path => {
          // Base file vanished.
          ranges.push((base.start, base.len, None));
          tombstoned_nodes += base.len as u64;
          b += 1;
        }
        (Some(_), Some(current)) => {
          // New file.
          overlay_ranges.push((current.start, current.len));
          c += 1;
        }
        (Some(base), None) => {
          ranges.push((base.start, base.len, None));
          tombstoned_nodes += base.len as u64;
          b += 1;
        }
        (None, Some(current)) => {
          overlay_ranges.push((current.start, current.len));
          c += 1;
        }
        (None, None) => unreachable!("loop condition"),
      }
    }
    // Merge-pass output is path-ordered == base-id-ordered (manifest order), so the range
    // table is binary-searchable as built.
    debug_assert!(ranges.windows(2).all(|w| w[0].0 <= w[1].0));

    let overlay_node_total: u64 = overlay_ranges.iter().map(|&(_, len)| len as u64).sum();
    let node_count = kg.node_count() as u64;
    if node_count > 0 && overlay_node_total * 100 > node_count * MAX_OVERLAY_PERCENT {
      return None;
    }
    let overlay_ids: Vec<u64> = overlay_ranges
      .iter()
      .flat_map(|&(start, len)| start..start + len as u64)
      .filter(|&id| {
        kg.node(NodeId::new(id))
          .is_some_and(|view| view.kind != vorpal_kg::SymbolKind::Import)
      })
      .collect();
    Some(OverlayView {
      ranges,
      overlay_ids,
      tombstoned_nodes,
    })
  }

  /// Map a base-generation node id to its current-generation id — `None` for dead rows
  /// (changed/deleted files) and for ids no range covers (corrupt input; treated as dead).
  pub fn remap(&self, base_id: u64) -> Option<u64> {
    let at = self
      .ranges
      .partition_point(|&(start, _, _)| start <= base_id);
    let &(start, len, target) = self.ranges.get(at.checked_sub(1)?)?;
    if base_id >= start + len as u64 {
      return None;
    }
    Some(target? + (base_id - start))
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn codec_roundtrips_runs_exactly() {
    let dir = std::env::temp_dir().join(format!("vorpal-annfiles-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let runs = vec![
      FileRun {
        path: "src/a.rs".into(),
        start: 0,
        len: 7,
        digest: 0xDEAD_BEEF_u64,
      },
      FileRun {
        path: "src/b/c.rs".into(),
        start: 7,
        len: 3,
        digest: 42,
      },
    ];
    save(&dir, 0x1234_5678_9ABC_DEF0, &runs).unwrap();
    let (stamp, back) = load(&dir).unwrap();
    assert_eq!(stamp, 0x1234_5678_9ABC_DEF0);
    assert_eq!(back, runs);
    let _ = fs::remove_dir_all(&dir);
  }

  #[test]
  fn torn_or_foreign_files_load_as_none() {
    let dir = std::env::temp_dir().join(format!("vorpal-annfiles-torn-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    assert!(load(&dir).is_none(), "missing file");
    fs::write(dir.join("ann.files"), b"garbage").unwrap();
    assert!(load(&dir).is_none(), "foreign bytes");
    save(
      &dir,
      7,
      &[FileRun {
        path: "x".into(),
        start: 0,
        len: 1,
        digest: 9,
      }],
    )
    .unwrap();
    let full = fs::read(dir.join("ann.files")).unwrap();
    fs::write(dir.join("ann.files"), &full[..full.len() - 3]).unwrap();
    assert!(load(&dir).is_none(), "truncated record");
    let _ = fs::remove_dir_all(&dir);
  }
}
