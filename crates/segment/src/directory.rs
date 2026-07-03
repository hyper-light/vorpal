//! The resident segment directory (§9.2): dense-`NodeId` → `(segment, row)` by binary search.
//!
//! Because ids are dense and contiguous per segment (`logical_id_base + row`), the whole "offset
//! index" collapses to a tiny sorted `id_base → segment` table — a few bytes per segment, fully
//! resident — instead of an 8-byte-per-id array. Locating a node is a binary search plus a range
//! check, O(log #segments), no per-id structure.

use crate::id::NodeId;

/// Opaque segment handle (index into the store's segment list).
pub type SegmentId = u32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DirEntry {
  id_base: u64,
  row_count: u64,
  segment_id: SegmentId,
}

/// Sorted `id_base → segment` directory for one store.
#[derive(Debug, Default, Clone)]
pub struct SegmentDirectory {
  entries: Vec<DirEntry>,
}

impl SegmentDirectory {
  pub fn new() -> Self {
    Self {
      entries: Vec::new(),
    }
  }

  pub fn len(&self) -> usize {
    self.entries.len()
  }

  pub fn is_empty(&self) -> bool {
    self.entries.is_empty()
  }

  /// The next id a new segment should start at (one past the current max id), so appends stay
  /// dense and monotone.
  pub fn next_id_base(&self) -> u64 {
    self
      .entries
      .last()
      .map(|e| e.id_base + e.row_count)
      .unwrap_or(0)
  }

  /// Register a sealed segment covering `[id_base, id_base + row_count)`.
  ///
  /// Panics if the segment overlaps or is not appended in id order — the dense-monotone-id
  /// invariant (§9.2) is a bug if violated, not a runtime condition.
  pub fn insert(&mut self, id_base: u64, row_count: u64, segment_id: SegmentId) {
    if let Some(last) = self.entries.last() {
      assert!(
        id_base >= last.id_base + last.row_count,
        "segment id range [{id_base}, +{row_count}) overlaps or precedes existing segments"
      );
    }
    self.entries.push(DirEntry {
      id_base,
      row_count,
      segment_id,
    });
  }

  /// Resolve a `NodeId` to `(segment_id, row)`, or `None` if no segment covers it.
  pub fn locate(&self, id: NodeId) -> Option<(SegmentId, u64)> {
    let raw = id.raw();
    // Rightmost entry whose id_base <= raw.
    let idx = match self.entries.binary_search_by(|e| e.id_base.cmp(&raw)) {
      Ok(i) => i,
      Err(0) => return None,
      Err(i) => i - 1,
    };
    let entry = &self.entries[idx];
    id.local_row(entry.id_base, entry.row_count)
      .map(|row| (entry.segment_id, row))
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn locates_ids_across_segment_boundaries() {
    let mut dir = SegmentDirectory::new();
    assert_eq!(dir.next_id_base(), 0);
    dir.insert(0, 100, 0);
    dir.insert(100, 50, 1);
    dir.insert(150, 200, 2);
    assert_eq!(dir.len(), 3);
    assert_eq!(dir.next_id_base(), 350);

    assert_eq!(dir.locate(NodeId(0)), Some((0, 0)));
    assert_eq!(dir.locate(NodeId(99)), Some((0, 99)));
    assert_eq!(dir.locate(NodeId(100)), Some((1, 0)));
    assert_eq!(dir.locate(NodeId(149)), Some((1, 49)));
    assert_eq!(dir.locate(NodeId(150)), Some((2, 0)));
    assert_eq!(dir.locate(NodeId(349)), Some((2, 199)));
    assert_eq!(dir.locate(NodeId(350)), None);
  }

  #[test]
  fn handles_gaps_and_empty() {
    let mut dir = SegmentDirectory::new();
    assert_eq!(dir.locate(NodeId(5)), None);
    // A gap (e.g. a fully-tombstoned segment skipped): [0,10) then [20,30).
    dir.insert(0, 10, 0);
    dir.insert(20, 10, 7);
    assert_eq!(dir.locate(NodeId(9)), Some((0, 9)));
    assert_eq!(dir.locate(NodeId(10)), None, "in the gap");
    assert_eq!(dir.locate(NodeId(19)), None, "in the gap");
    assert_eq!(dir.locate(NodeId(20)), Some((7, 0)));
  }

  #[test]
  #[should_panic(expected = "overlaps")]
  fn rejects_overlapping_segments() {
    let mut dir = SegmentDirectory::new();
    dir.insert(0, 100, 0);
    dir.insert(50, 100, 1);
  }
}
