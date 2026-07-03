//! Index-over-pointer CSR adjacency with a prefetching frontier walk (§8.3, §9.3, §11.2).
//!
//! Compressed Sparse Row is the read form of the graph: contiguous neighbor lists (cache-line
//! and prefetch friendly) keyed by dense `u32` node ids — never pointers. Built with the
//! GVEL-style counting-scatter (degree histogram → prefix sum → scatter), cheap enough to
//! rebuild from an edge log at compaction.

use crate::prefetch::prefetch_read;

/// Read-only CSR adjacency over dense `u32` node ids.
#[derive(Debug, Clone)]
pub struct Csr {
  /// `row_offsets[u]..row_offsets[u+1]` bounds node `u`'s neighbor slice; length `n + 1`.
  row_offsets: Vec<u64>,
  /// Concatenated neighbor ids in node order; length = edge count.
  col_indices: Vec<u32>,
}

impl Csr {
  /// Build CSR for `node_count` nodes from `(src, dst)` edges via counting-scatter.
  ///
  /// Panics if any endpoint is `>= node_count` (dense-id invariant, checked in debug via index).
  pub fn from_edges(node_count: u32, edges: &[(u32, u32)]) -> Self {
    let n = node_count as usize;
    let mut row_offsets = vec![0u64; n + 1];
    for &(src, _) in edges {
      row_offsets[src as usize + 1] += 1;
    }
    for i in 0..n {
      row_offsets[i + 1] += row_offsets[i];
    }
    let mut col_indices = vec![0u32; edges.len()];
    let mut cursor = row_offsets[..n].to_vec();
    for &(src, dst) in edges {
      let slot = cursor[src as usize];
      col_indices[slot as usize] = dst;
      cursor[src as usize] += 1;
    }
    Self {
      row_offsets,
      col_indices,
    }
  }

  pub fn node_count(&self) -> usize {
    self.row_offsets.len() - 1
  }

  pub fn edge_count(&self) -> usize {
    self.col_indices.len()
  }

  /// Node `u`'s neighbor slice (contiguous).
  pub fn neighbors(&self, u: u32) -> &[u32] {
    let start = self.row_offsets[u as usize] as usize;
    let end = self.row_offsets[u as usize + 1] as usize;
    &self.col_indices[start..end]
  }

  /// Visit `u`'s neighbors, prefetching the adjacency of the neighbor `distance` hops ahead —
  /// the one-hop-lookahead software pipeline for multi-hop traversal (§8.3). `distance == 0`
  /// is the plain scan (baseline). Yields the same order as [`Csr::neighbors`].
  pub fn for_each_neighbor_prefetched<F: FnMut(u32)>(&self, u: u32, distance: usize, mut f: F) {
    let ns = self.neighbors(u);
    for i in 0..ns.len() {
      if distance != 0 {
        if let Some(&ahead) = ns.get(i + distance) {
          let off = self.row_offsets[ahead as usize] as usize;
          if off < self.col_indices.len() {
            prefetch_read(&self.col_indices[off] as *const u32);
          }
        }
      }
      f(ns[i]);
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn sample() -> Csr {
    // 0 -> {1,2}, 1 -> {2}, 2 -> {0}, 3 -> {}
    Csr::from_edges(4, &[(0, 1), (0, 2), (1, 2), (2, 0)])
  }

  #[test]
  fn builds_correct_adjacency() {
    let g = sample();
    assert_eq!(g.node_count(), 4);
    assert_eq!(g.edge_count(), 4);
    assert_eq!(g.neighbors(0), &[1, 2]);
    assert_eq!(g.neighbors(1), &[2]);
    assert_eq!(g.neighbors(2), &[0]);
    assert_eq!(g.neighbors(3), &[] as &[u32]);
  }

  #[test]
  fn prefetched_walk_matches_plain_walk() {
    let g = sample();
    for u in 0..g.node_count() as u32 {
      for &distance in &[0usize, 1, 2, 8] {
        let mut got = Vec::new();
        g.for_each_neighbor_prefetched(u, distance, |v| got.push(v));
        assert_eq!(got, g.neighbors(u), "node {u} distance {distance}");
      }
    }
  }
}
