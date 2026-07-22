//! Directed CSR with an aligned edge-type payload column (read form, §9.3).
//!
//! Built by GVEL-style counting-scatter (degree histogram → prefix sum → scatter, §11.2): one
//! pass to count, one to place, keeping the `etype` column in step with `targets`.

use vorpal_mem::{PodColumn, prefetch_read};

#[derive(Debug, Clone, Default)]
pub(crate) struct DirectedCsr {
  /// `row_offsets[u]..row_offsets[u + 1]` bounds node `u`'s slice; length `node_count + 1`.
  row_offsets: PodColumn<u64>,
  /// The other endpoint per edge (dst for out-CSR, src for in-CSC).
  targets: PodColumn<u32>,
  /// Edge label per edge, aligned 1:1 with `targets`.
  etypes: PodColumn<u16>,
}

impl DirectedCsr {
  /// Build from parallel `(key, other, etype)` columns, where `key` is the row (src for out,
  /// dst for in). All three slices have equal length.
  pub(crate) fn build(node_count: u32, keys: &[u32], others: &[u32], etypes: &[u16]) -> Self {
    let n = node_count as usize;
    let m = keys.len();
    let mut row_offsets = vec![0u64; n + 1];
    for &k in keys {
      row_offsets[k as usize + 1] += 1;
    }
    for i in 1..=n {
      row_offsets[i] += row_offsets[i - 1];
    }
    let mut targets = vec![0u32; m];
    let mut out_etypes = vec![0u16; m];
    let mut cursor = row_offsets[..n].to_vec();
    for i in 0..m {
      let k = keys[i] as usize;
      let slot = cursor[k] as usize;
      targets[slot] = others[i];
      out_etypes[slot] = etypes[i];
      cursor[k] += 1;
    }
    Self {
      row_offsets: PodColumn::from_vec(row_offsets),
      targets: PodColumn::from_vec(targets),
      etypes: PodColumn::from_vec(out_etypes),
    }
  }

  /// Assemble from already-built columns (the mapped load path).
  pub(crate) fn from_columns(
    row_offsets: PodColumn<u64>,
    targets: PodColumn<u32>,
    etypes: PodColumn<u16>,
  ) -> Self {
    debug_assert_eq!(targets.len(), etypes.len());
    Self {
      row_offsets,
      targets,
      etypes,
    }
  }

  pub(crate) fn row_offsets(&self) -> &[u64] {
    &self.row_offsets
  }

  pub(crate) fn raw_targets(&self) -> &[u32] {
    &self.targets
  }

  pub(crate) fn raw_etypes(&self) -> &[u16] {
    &self.etypes
  }

  pub(crate) fn total_edges(&self) -> usize {
    self.targets.len()
  }

  #[inline]
  fn range(&self, u: u32) -> (usize, usize) {
    let start = self.row_offsets[u as usize] as usize;
    let end = self.row_offsets[u as usize + 1] as usize;
    (start, end)
  }

  pub(crate) fn targets(&self, u: u32) -> &[u32] {
    let (a, b) = self.range(u);
    &self.targets[a..b]
  }

  pub(crate) fn edge_types(&self, u: u32) -> &[u16] {
    let (a, b) = self.range(u);
    &self.etypes[a..b]
  }

  pub(crate) fn degree(&self, u: u32) -> usize {
    let (a, b) = self.range(u);
    b - a
  }

  /// Visit `u`'s neighbors, prefetching the adjacency of the neighbor `distance` hops ahead
  /// (§8.3 software pipeline). `distance == 0` is the plain scan.
  pub(crate) fn for_each_prefetched<F: FnMut(u32, u16)>(&self, u: u32, distance: usize, mut f: F) {
    let (a, b) = self.range(u);
    let ts = &self.targets[a..b];
    let es = &self.etypes[a..b];
    for i in 0..ts.len() {
      if distance != 0 {
        if let Some(&ahead) = ts.get(i + distance) {
          let off = self.row_offsets[ahead as usize] as usize;
          if off < self.targets.len() {
            prefetch_read(&self.targets[off] as *const u32);
          }
        }
      }
      f(ts[i], es[i]);
    }
  }
}
