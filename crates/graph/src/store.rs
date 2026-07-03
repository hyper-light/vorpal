//! The graph LSM tying write + read forms together (§9.3): append to a delta, read the union,
//! compact on flush, relabel for locality on demand (§9.8).

use crate::edge::{EdgeLog, EdgeType};
use crate::graph::Graph;
use crate::relabel::{ForwardingTable, bfs_locality_order};

/// A single graph shard: a compacted [`Graph`] fronted by an append-only delta.
pub struct GraphStore {
  node_count: u32,
  compacted: Graph,
  delta: EdgeLog,
}

impl GraphStore {
  pub fn new(node_count: u32) -> Self {
    Self {
      node_count,
      compacted: Graph::empty(node_count),
      delta: EdgeLog::new(),
    }
  }

  pub fn node_count(&self) -> u32 {
    self.node_count
  }

  pub fn compacted(&self) -> &Graph {
    &self.compacted
  }

  /// Number of not-yet-compacted edges in the delta.
  pub fn pending(&self) -> usize {
    self.delta.len()
  }

  /// Write path: O(1) append to the delta (§9.3). No whole-repo buffer.
  pub fn append(&mut self, src: u32, dst: u32, etype: EdgeType) {
    self.delta.push(src, dst, etype);
  }

  /// Out-neighbors of `u` reading `compacted ∪ delta` (§9.3 read-merge).
  pub fn out_neighbors(&self, u: u32) -> Vec<(u32, EdgeType)> {
    let mut out: Vec<(u32, EdgeType)> = self
      .compacted
      .out_targets(u)
      .iter()
      .zip(self.compacted.out_edge_types(u))
      .map(|(&d, &e)| (d, EdgeType(e)))
      .collect();
    for (s, d, et) in self.delta.iter() {
      if s == u {
        out.push((d, et));
      }
    }
    out
  }

  /// In-neighbors of `u` (`callersOf`) reading `compacted ∪ delta`.
  pub fn in_neighbors(&self, u: u32) -> Vec<(u32, EdgeType)> {
    let mut inc: Vec<(u32, EdgeType)> = self
      .compacted
      .in_targets(u)
      .iter()
      .zip(self.compacted.in_edge_types(u))
      .map(|(&s, &e)| (s, EdgeType(e)))
      .collect();
    for (s, d, et) in self.delta.iter() {
      if d == u {
        inc.push((s, et));
      }
    }
    inc
  }

  /// Compaction: fold the delta into the compacted CSR/CSC (the write→read transform, §9.3).
  pub fn flush(&mut self) {
    if self.delta.is_empty() {
      return;
    }
    let cap = self.compacted.edge_count() + self.delta.len();
    let mut srcs = Vec::with_capacity(cap);
    let mut dsts = Vec::with_capacity(cap);
    let mut etypes = Vec::with_capacity(cap);
    for (u, v, et) in self.compacted.out_edges() {
      srcs.push(u);
      dsts.push(v);
      etypes.push(et);
    }
    for (u, v, et) in self.delta.iter() {
      srcs.push(u);
      dsts.push(v);
      etypes.push(et.0);
    }
    self.compacted = Graph::from_parts(self.node_count, &srcs, &dsts, &etypes);
    self.delta.clear();
  }

  /// Flush, then relabel the compacted graph in BFS/RCM locality order (§9.8). Returns the
  /// forwarding table (`old_id → new_id`); callers translate stale ids through it. After this the
  /// store's ids are in the new space.
  pub fn relabel_for_locality(&mut self) -> ForwardingTable {
    self.flush();
    let order = bfs_locality_order(&self.compacted);
    let fwd = ForwardingTable::from_order(&order);
    self.compacted = self.compacted.relabel(&fwd);
    fwd
  }
}
