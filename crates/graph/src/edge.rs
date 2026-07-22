//! Edge types and the append-only edge log (write form, §9.3).

/// A code-graph edge label (§3.3). A small dense integer so it packs into a CSR payload column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EdgeType(pub u16);

impl EdgeType {
  pub const DEFINES: EdgeType = EdgeType(0);
  pub const CALLS: EdgeType = EdgeType(1);
  pub const REFERENCES: EdgeType = EdgeType(2);
  pub const IMPORTS: EdgeType = EdgeType(3);
  pub const IMPLEMENTS: EdgeType = EdgeType(4);
  pub const OF_TYPE: EdgeType = EdgeType(5);
  pub const OVERRIDES: EdgeType = EdgeType(6);
  pub const HAS_METHOD: EdgeType = EdgeType(7);
  pub const HAS_FIELD: EdgeType = EdgeType(8);
}

/// Append-only edge log: struct-of-arrays so a compaction pass streams columns (§9.3, §11.2).
/// Node ids are dense `u32` locators.
#[derive(Debug, Default, Clone)]
pub struct EdgeLog {
  srcs: Vec<u32>,
  dsts: Vec<u32>,
  etypes: Vec<u16>,
}

impl EdgeLog {
  pub fn new() -> Self {
    Self::default()
  }

  /// O(1) append — the write path never buffers the whole repo.
  pub fn push(&mut self, src: u32, dst: u32, etype: EdgeType) {
    self.srcs.push(src);
    self.dsts.push(dst);
    self.etypes.push(etype.0);
  }

  pub fn len(&self) -> usize {
    self.srcs.len()
  }

  pub fn is_empty(&self) -> bool {
    self.srcs.is_empty()
  }

  pub fn clear(&mut self) {
    self.srcs.clear();
    self.dsts.clear();
    self.etypes.clear();
  }

  /// Release growth slack (capacity beyond length) back to the allocator.
  pub fn shrink_to_fit(&mut self) {
    self.srcs.shrink_to_fit();
    self.dsts.shrink_to_fit();
    self.etypes.shrink_to_fit();
  }

  pub(crate) fn srcs(&self) -> &[u32] {
    &self.srcs
  }

  pub(crate) fn dsts(&self) -> &[u32] {
    &self.dsts
  }

  pub(crate) fn etypes(&self) -> &[u16] {
    &self.etypes
  }

  /// Iterate `(src, dst, etype)` triples.
  pub fn iter(&self) -> impl Iterator<Item = (u32, u32, EdgeType)> + '_ {
    (0..self.len()).map(move |i| (self.srcs[i], self.dsts[i], EdgeType(self.etypes[i])))
  }
}
