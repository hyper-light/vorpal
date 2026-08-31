//! Observed calls from runtime traces — the `observed.bin` sidecar (VOBS v1).
//!
//! An additive, generation-stamped sidecar like the ANN tier and `communities.bin`, but its
//! content comes from OUTSIDE the tree (`vorpal-index ingest-traces` on folded stacks), so
//! it never joins the generation id and is never rebuilt automatically: a new generation
//! renumbers nodes, the stamp stops matching, and the sidecar reads as absent until the
//! traces are ingested again — stated by every surface, never silently dropped.
//!
//! Rows are `(from, to, count)` — an observed direct call from `from` into `to`, summed
//! over all ingested stacks — sorted by `(from, to)`, so the file is byte-deterministic
//! for a given (generation, trace set).

use std::io::{self, Write};
use std::path::Path;

const MAGIC: &[u8; 4] = b"VOBS";
const VERSION: u32 = 1;
/// magic, version, stamp, row count.
const HEADER_LEN: usize = 20;
const ROW_LEN: usize = 16;

/// One observed caller→callee pair with its sample/count weight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObservedRow {
  pub from: u32,
  pub to: u32,
  pub count: u64,
}

/// Persist observed rows for the generation stamped `stamp`. Rows are canonicalized here
/// (sorted by `(from, to)`, duplicate pairs summed) — save order never leaks.
pub fn save_observed(dir: &Path, stamp: u64, mut rows: Vec<ObservedRow>) -> io::Result<()> {
  rows.sort_unstable_by_key(|r| (r.from, r.to));
  rows.dedup_by(|b, a| {
    if a.from == b.from && a.to == b.to {
      a.count = a.count.saturating_add(b.count);
      true
    } else {
      false
    }
  });
  let mut buf = Vec::with_capacity(HEADER_LEN + rows.len() * ROW_LEN);
  buf.extend_from_slice(MAGIC);
  buf.extend_from_slice(&VERSION.to_le_bytes());
  buf.extend_from_slice(&stamp.to_le_bytes());
  buf.extend_from_slice(&(rows.len() as u32).to_le_bytes());
  for row in &rows {
    buf.extend_from_slice(&row.from.to_le_bytes());
    buf.extend_from_slice(&row.to.to_le_bytes());
    buf.extend_from_slice(&row.count.to_le_bytes());
  }
  let tmp = dir.join("observed.bin.tmp");
  {
    let mut file = std::fs::File::create(&tmp)?;
    file.write_all(&buf)?;
    file.sync_all()?;
  }
  std::fs::rename(&tmp, dir.join("observed.bin"))
}

/// The observed-call table for one generation: rows sorted by `(from, to)` plus a
/// callee-sorted permutation for the inbound direction.
pub struct ObservedStore {
  rows: Vec<ObservedRow>,
  /// Indices into `rows`, sorted by `(to, from)`.
  by_to: Vec<u32>,
}

impl ObservedStore {
  /// Load the sidecar if present AND stamped for `stamp`; a missing, stale, or foreign
  /// file is `Ok(empty)` — callers state absence, the store never invents rows. Torn or
  /// malformed bytes under a MATCHING stamp are an error (that is corruption, not age).
  pub fn load(dir: &Path, stamp: u64) -> io::Result<ObservedStore> {
    let bytes = match std::fs::read(dir.join("observed.bin")) {
      Ok(bytes) => bytes,
      Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Self::empty()),
      Err(err) => return Err(err),
    };
    if bytes.len() < HEADER_LEN || &bytes[0..4] != MAGIC {
      return Ok(Self::empty());
    }
    let version = u32::from_le_bytes(bytes[4..8].try_into().expect("4B"));
    let file_stamp = u64::from_le_bytes(bytes[8..16].try_into().expect("8B"));
    if version != VERSION || file_stamp != stamp {
      return Ok(Self::empty());
    }
    let count = u32::from_le_bytes(bytes[16..20].try_into().expect("4B")) as usize;
    if bytes.len() != HEADER_LEN + count * ROW_LEN {
      return Err(io::Error::other(format!(
        "observed.bin: {} bytes for {count} rows — torn sidecar",
        bytes.len()
      )));
    }
    let rows: Vec<ObservedRow> = bytes[HEADER_LEN..]
      .chunks_exact(ROW_LEN)
      .map(|row| ObservedRow {
        from: u32::from_le_bytes(row[0..4].try_into().expect("4B")),
        to: u32::from_le_bytes(row[4..8].try_into().expect("4B")),
        count: u64::from_le_bytes(row[8..16].try_into().expect("8B")),
      })
      .collect();
    let mut by_to: Vec<u32> = (0..rows.len() as u32).collect();
    by_to.sort_unstable_by_key(|&i| {
      let row = &rows[i as usize];
      (row.to, row.from)
    });
    Ok(ObservedStore { rows, by_to })
  }

  pub fn empty() -> Self {
    ObservedStore {
      rows: Vec::new(),
      by_to: Vec::new(),
    }
  }

  pub fn len(&self) -> usize {
    self.rows.len()
  }

  pub fn is_empty(&self) -> bool {
    self.rows.is_empty()
  }

  /// Observed calls out of `from`, `(to, count)` ascending by callee.
  pub fn observed_from(&self, from: u32) -> Vec<(u32, u64)> {
    let start = self.rows.partition_point(|r| r.from < from);
    self.rows[start..]
      .iter()
      .take_while(|r| r.from == from)
      .map(|r| (r.to, r.count))
      .collect()
  }

  /// Observed calls into `to`, `(from, count)` ascending by caller.
  pub fn observed_into(&self, to: u32) -> Vec<(u32, u64)> {
    let start = self.by_to.partition_point(|&i| self.rows[i as usize].to < to);
    self.by_to[start..]
      .iter()
      .map(|&i| &self.rows[i as usize])
      .take_while(|r| r.to == to)
      .map(|r| (r.from, r.count))
      .collect()
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn round_trips_sums_duplicates_and_ignores_stale_stamps() {
    let dir = std::env::temp_dir().join(format!("vorpal-vobs-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let rows = vec![
      ObservedRow { from: 2, to: 3, count: 5 },
      ObservedRow { from: 1, to: 3, count: 7 },
      ObservedRow { from: 2, to: 3, count: 1 },
    ];
    save_observed(&dir, 42, rows).unwrap();
    let store = ObservedStore::load(&dir, 42).unwrap();
    assert_eq!(store.len(), 2, "duplicate pair summed");
    assert_eq!(store.observed_from(2), vec![(3, 6)]);
    assert_eq!(store.observed_into(3), vec![(1, 7), (2, 6)]);
    // A different stamp reads as absent, not as an error.
    assert!(ObservedStore::load(&dir, 43).unwrap().is_empty());
    // A missing file reads as absent.
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    assert!(ObservedStore::load(&dir, 42).unwrap().is_empty());
    let _ = std::fs::remove_dir_all(&dir);
  }
}
