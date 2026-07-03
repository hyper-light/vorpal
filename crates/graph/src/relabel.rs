//! Compaction-time `NodeId` remap for locality (§9.8).
//!
//! A deterministic BFS/RCM-style order relabels nodes so neighbors get numerically near ids
//! (better cache-line + TLB locality on multi-hop walks). The resulting [`ForwardingTable`] maps
//! `old_id → new_id`; successive relabels **compose** so a stale cross-unit reference resolves in
//! a single lookup, never a chain. The order is a pure function of the graph → bit-identical
//! rebuild.

use std::collections::VecDeque;

use crate::graph::Graph;

/// Dense `old_id → new_id` map produced by a relabel (§9.8 "dense array over the old id range").
#[derive(Debug, Clone)]
pub struct ForwardingTable {
  new_of: Vec<u32>,
}

impl ForwardingTable {
  /// Build from an order where `order[new_id] = old_id` (e.g. [`bfs_locality_order`]).
  pub fn from_order(order: &[u32]) -> Self {
    let mut new_of = vec![0u32; order.len()];
    for (new_id, &old) in order.iter().enumerate() {
      new_of[old as usize] = new_id as u32;
    }
    Self { new_of }
  }

  /// The no-op relabel over `n` nodes.
  pub fn identity(n: u32) -> Self {
    Self {
      new_of: (0..n).collect(),
    }
  }

  pub fn len(&self) -> usize {
    self.new_of.len()
  }

  pub fn is_empty(&self) -> bool {
    self.new_of.is_empty()
  }

  /// Map an old id to its current id.
  #[inline]
  pub fn translate(&self, old: u32) -> u32 {
    self.new_of[old as usize]
  }

  /// Compose so this table applies *after* `older`: the result maps `older`'s input space
  /// straight to this table's output — one lookup, not a chain (§9.8).
  pub fn compose(&self, older: &ForwardingTable) -> ForwardingTable {
    let new_of = older
      .new_of
      .iter()
      .map(|&mid| self.new_of[mid as usize])
      .collect();
    ForwardingTable { new_of }
  }
}

/// Deterministic BFS locality order (undirected view; neighbors visited in ascending id).
/// Returns `order` where `order[new_id] = old_id`. Disconnected components follow in id order.
pub fn bfs_locality_order(g: &Graph) -> Vec<u32> {
  let n = g.node_count();
  let mut visited = vec![false; n];
  let mut order = Vec::with_capacity(n);
  let mut queue = VecDeque::new();

  for start in 0..n as u32 {
    if visited[start as usize] {
      continue;
    }
    visited[start as usize] = true;
    queue.push_back(start);
    while let Some(u) = queue.pop_front() {
      order.push(u);
      let mut nbrs: Vec<u32> = g
        .out_targets(u)
        .iter()
        .chain(g.in_targets(u))
        .copied()
        .collect();
      nbrs.sort_unstable();
      nbrs.dedup();
      for v in nbrs {
        if !visited[v as usize] {
          visited[v as usize] = true;
          queue.push_back(v);
        }
      }
    }
  }
  order
}

/// Mean `|src - dst|` over all edges — the locality metric §9.8 uses to decide whether a relabel
/// pays (lower is more local). This is the `avg edge id-span` referenced by the §8.4 loop.
pub fn avg_edge_id_span(g: &Graph) -> f64 {
  let (mut sum, mut count) = (0u64, 0u64);
  for (u, v, _) in g.out_edges() {
    sum += (u as i64 - v as i64).unsigned_abs();
    count += 1;
  }
  if count == 0 {
    0.0
  } else {
    sum as f64 / count as f64
  }
}

impl Graph {
  /// Rebuild this graph with every endpoint remapped through `fwd` (the relabel's read form).
  pub fn relabel(&self, fwd: &ForwardingTable) -> Graph {
    let m = self.edge_count();
    let mut srcs = Vec::with_capacity(m);
    let mut dsts = Vec::with_capacity(m);
    let mut etypes = Vec::with_capacity(m);
    for (u, v, et) in self.out_edges() {
      srcs.push(fwd.translate(u));
      dsts.push(fwd.translate(v));
      etypes.push(et);
    }
    Graph::from_parts(self.node_count() as u32, &srcs, &dsts, &etypes)
  }
}
