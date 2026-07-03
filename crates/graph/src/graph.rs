//! The compacted graph: CSR (out) + CSC (in), the read form (§9.3).

use crate::csr::DirectedCsr;
use crate::edge::{EdgeLog, EdgeType};

/// A sealed, compacted directed graph over dense `u32` node ids, with both traversal directions
/// materialized so `callersOf` and `refsTo` each read exactly one direction (§9.3).
#[derive(Debug, Clone, Default)]
pub struct Graph {
  node_count: u32,
  out: DirectedCsr,
  inc: DirectedCsr,
}

impl Graph {
  /// An empty graph over `node_count` nodes.
  pub fn empty(node_count: u32) -> Self {
    Self::from_parts(node_count, &[], &[], &[])
  }

  /// Compact an [`EdgeLog`] into CSR + CSC (the write→read transform, §9.3).
  pub fn compact(node_count: u32, log: &EdgeLog) -> Self {
    Self::from_parts(node_count, log.srcs(), log.dsts(), log.etypes())
  }

  /// Build both directions from parallel `(src, dst, etype)` columns.
  pub(crate) fn from_parts(node_count: u32, srcs: &[u32], dsts: &[u32], etypes: &[u16]) -> Self {
    // out-CSR keys on src; in-CSC keys on dst (so its "targets" are the sources).
    let out = DirectedCsr::build(node_count, srcs, dsts, etypes);
    let inc = DirectedCsr::build(node_count, dsts, srcs, etypes);
    Self {
      node_count,
      out,
      inc,
    }
  }

  pub fn node_count(&self) -> usize {
    self.node_count as usize
  }

  pub fn edge_count(&self) -> usize {
    self.out.total_edges()
  }

  /// Out-neighbors of `u` (`refsTo` / `calls` direction).
  pub fn out_targets(&self, u: u32) -> &[u32] {
    self.out.targets(u)
  }

  pub fn out_edge_types(&self, u: u32) -> &[u16] {
    self.out.edge_types(u)
  }

  pub fn out_degree(&self, u: u32) -> usize {
    self.out.degree(u)
  }

  /// In-neighbors of `u` (`callersOf` / who-references direction) — one CSC read, no fan-out.
  pub fn in_targets(&self, u: u32) -> &[u32] {
    self.inc.targets(u)
  }

  pub fn in_edge_types(&self, u: u32) -> &[u16] {
    self.inc.edge_types(u)
  }

  pub fn in_degree(&self, u: u32) -> usize {
    self.inc.degree(u)
  }

  /// Prefetching out-neighbor walk (§8.3): `f(dst, etype)` per edge, staging `distance` ahead.
  pub fn for_each_out_prefetched<F: FnMut(u32, EdgeType)>(
    &self,
    u: u32,
    distance: usize,
    mut f: F,
  ) {
    self
      .out
      .for_each_prefetched(u, distance, |dst, et| f(dst, EdgeType(et)));
  }

  /// All out-edges as `(src, dst, etype)` triples (for compaction / relabel rebuilds).
  pub fn out_edges(&self) -> impl Iterator<Item = (u32, u32, u16)> + '_ {
    (0..self.node_count).flat_map(move |u| {
      self
        .out
        .targets(u)
        .iter()
        .zip(self.out.edge_types(u))
        .map(move |(&dst, &et)| (u, dst, et))
    })
  }
}
