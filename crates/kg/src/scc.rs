//! Seal-time strongly-connected-component sizes over the `calls` subgraph (B6 v1.5 /
//! plan G): `scc_size` — 1 for acyclic nodes, the cycle's node count for recursion knots —
//! persisted as a name-addressed node column, so mutual-recursion clusters are queryable
//! without any runtime graph analysis.
//!
//! Deterministic by construction: nodes root in ascending id order, adjacency lists are
//! built by a stable counting pass over the edge log, and Tarjan's assignment is a pure
//! function of both. Iterative (explicit frame stack) — a recursive walk would overflow on
//! a multi-million-node graph's deep call chains.

use vorpal_graph::{EdgeLog, EdgeType};

const UNVISITED: u32 = u32::MAX;

/// Component size per node over the CALLS edges in `edges`. `n` bounds the id space;
/// out-of-range endpoints (impossible in a sealed writer, tolerated totally) are skipped.
pub(crate) fn scc_sizes(n: usize, edges: &EdgeLog) -> Vec<u32> {
  // Counting-sort adjacency over calls edges only.
  let mut degree = vec![0u32; n + 1];
  let mut call_edges = 0usize;
  for (src, dst, etype) in edges.iter() {
    if etype.base() == EdgeType::CALLS && (src as usize) < n && (dst as usize) < n {
      degree[src as usize] += 1;
      call_edges += 1;
    }
  }
  let mut offsets = vec![0u32; n + 1];
  for i in 0..n {
    offsets[i + 1] = offsets[i] + degree[i];
  }
  let mut adj = vec![0u32; call_edges];
  let mut cursor = offsets.clone();
  for (src, dst, etype) in edges.iter() {
    if etype.base() == EdgeType::CALLS && (src as usize) < n && (dst as usize) < n {
      adj[cursor[src as usize] as usize] = dst;
      cursor[src as usize] += 1;
    }
  }
  drop(cursor);
  drop(degree);

  let mut index = vec![UNVISITED; n];
  let mut lowlink = vec![0u32; n];
  let mut on_stack = vec![false; n];
  let mut stack: Vec<u32> = Vec::new();
  let mut sizes = vec![1u32; n];
  let mut next_index = 0u32;
  // Explicit DFS frames: (node, next adjacency position within its list).
  let mut frames: Vec<(u32, u32)> = Vec::new();

  for root in 0..n as u32 {
    if index[root as usize] != UNVISITED {
      continue;
    }
    frames.push((root, 0));
    while let Some(frame) = frames.last_mut() {
      let v = frame.0;
      let vi = v as usize;
      if frame.1 == 0 && index[vi] == UNVISITED {
        index[vi] = next_index;
        lowlink[vi] = next_index;
        next_index += 1;
        stack.push(v);
        on_stack[vi] = true;
      }
      let start = offsets[vi] as usize;
      let end = offsets[vi + 1] as usize;
      let mut descended = false;
      while (frame.1 as usize) < end - start {
        let w = adj[start + frame.1 as usize];
        frame.1 += 1;
        let wi = w as usize;
        if index[wi] == UNVISITED {
          frames.push((w, 0));
          descended = true;
          break;
        } else if on_stack[wi] {
          lowlink[vi] = lowlink[vi].min(index[wi]);
        }
      }
      if descended {
        continue;
      }
      frames.pop();
      if let Some(parent) = frames.last() {
        let pi = parent.0 as usize;
        lowlink[pi] = lowlink[pi].min(lowlink[vi]);
      }
      if lowlink[vi] == index[vi] {
        // v roots a component: its members are exactly the stack's tail from v
        // (each node is on the stack at most once). Assign sizes in place and
        // truncate — the per-component collection Vec this replaces allocated
        // ONCE PER NODE on acyclic-majority graphs (8.8M allocations at kernel
        // scale for mostly `size == 1` answers), in every language's corpus.
        if let Some(start) = stack.iter().rposition(|&w| w == v) {
          let size = (stack.len() - start) as u32;
          for &w in &stack[start..] {
            on_stack[w as usize] = false;
            sizes[w as usize] = size;
          }
          stack.truncate(start);
        }
      }
    }
  }
  sizes
}

#[cfg(test)]
mod tests {
  use super::*;

  fn log(edges: &[(u32, u32)]) -> EdgeLog {
    let mut out = EdgeLog::new();
    for &(s, d) in edges {
      out.push(s, d, EdgeType::CALLS.with_confidence(100));
    }
    out
  }

  #[test]
  fn cycles_knots_and_singletons() {
    // 0→1→2→0 (triangle), 3→4 (chain), 5 isolated, 6⇄6 (self-loop is a 1-node SCC).
    let edges = log(&[(0, 1), (1, 2), (2, 0), (3, 4), (6, 6)]);
    let sizes = scc_sizes(7, &edges);
    assert_eq!(sizes, vec![3, 3, 3, 1, 1, 1, 1]);

    // Two interlocking cycles form one component: 0→1→2→0 plus 2→3→1.
    let edges = log(&[(0, 1), (1, 2), (2, 0), (2, 3), (3, 1)]);
    assert_eq!(scc_sizes(4, &edges), vec![4, 4, 4, 4]);

    // Non-calls edges never bind components.
    let mut edges = EdgeLog::new();
    edges.push(0, 1, EdgeType::CALLS.with_confidence(100));
    edges.push(1, 0, EdgeType::REFERENCES.with_confidence(100));
    assert_eq!(scc_sizes(2, &edges), vec![1, 1]);
  }

  #[test]
  fn deep_chain_does_not_overflow() {
    // A 200k-deep call chain — the recursive formulation would blow the thread stack.
    let n = 200_000u32;
    let mut edges = EdgeLog::new();
    for i in 0..n - 1 {
      edges.push(i, i + 1, EdgeType::CALLS.with_confidence(100));
    }
    // Close the loop: the whole chain is one giant SCC.
    edges.push(n - 1, 0, EdgeType::CALLS.with_confidence(100));
    let sizes = scc_sizes(n as usize, &edges);
    assert!(sizes.iter().all(|&s| s == n), "one {n}-node component");
  }
}
