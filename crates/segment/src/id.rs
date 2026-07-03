//! Dense monotone node identity (§9.2).

/// A dense, monotone internal node id: `logical_id_base + row`.
///
/// This is a **physical locator, not a permanent identity** — valid from the epoch that assigned
/// it and forwarded (not preserved) across a locality-relabel compaction (§9.8). The permanent
/// identity is `blake3(path:entityPath)`, held in the canonical index (a different crate).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeId(pub u64);

impl NodeId {
  #[inline]
  pub const fn new(raw: u64) -> Self {
    Self(raw)
  }

  #[inline]
  pub const fn raw(self) -> u64 {
    self.0
  }

  /// The row index of this id within a segment whose ids start at `base`, if it falls in
  /// `[base, base + row_count)`.
  #[inline]
  pub fn local_row(self, base: u64, row_count: u64) -> Option<u64> {
    let row = self.0.checked_sub(base)?;
    (row < row_count).then_some(row)
  }
}

impl From<u64> for NodeId {
  fn from(raw: u64) -> Self {
    Self(raw)
  }
}
