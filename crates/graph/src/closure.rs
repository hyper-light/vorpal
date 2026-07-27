//! Transitive closure as masked SpMV over the CSR/CSC (§11.5).
//!
//! Semi-naive Datalog reachability *is* iterated `frontier_{k+1} = neighbors(frontier_k) \ visited`
//! over the graph: the "new this round" frontier is the mask, so settled nodes are never
//! re-expanded. Traversal is **direction-optimizing** (Beamer): top-down *push* while the frontier
//! is small, bottom-up *pull* (set-intersection against reverse edges) once it is dense — the
//! latter avoids scanning huge frontiers on power-law graphs. One kernel serves `callersOf` /
//! `refsTo` / `importersOf` and their transitive versions, differing only in [`Direction`].

use bit_set::BitSet;

use crate::edge::EdgeType;
use crate::graph::Graph;

/// Which edge direction to follow: `Out` = what a seed reaches (`refsTo`/`defines`-transitive),
/// `In` = what reaches a seed (`callersOf`/container-transitive).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
  Out,
  In,
}

impl Direction {
  fn opposite(self) -> Self {
    match self {
      Direction::Out => Direction::In,
      Direction::In => Direction::Out,
    }
  }
}

/// Force a traversal mode, or let the frontier density choose (Beamer).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strategy {
  Auto,
  Push,
  Pull,
}

/// Switch to bottom-up pull once the frontier exceeds `n / ALPHA` (Beamer's heuristic threshold).
const ALPHA: usize = 8;

fn neighbors_dir(graph: &Graph, node: u32, dir: Direction) -> &[u32] {
  match dir {
    Direction::Out => graph.out_targets(node),
    Direction::In => graph.in_targets(node),
  }
}

/// The set of nodes reachable from `seeds` via ≥1 edge in `dir` (seeds excluded unless reached
/// through a cycle — and then still excluded, since the result is "reachable, not the source").
pub fn reachable(graph: &Graph, seeds: &[u32], dir: Direction) -> BitSet {
  reachable_strategy(graph, seeds, dir, Strategy::Auto)
}

/// Reachable from `seeds` following **only** edges whose base type is in `allowed`, up to
/// `max_depth` hops in `dir` (`None` = unbounded). Unlike [`reachable`], traversal is confined to
/// one relation set, so "transitive callers of X" cannot leak across containment, import, or type
/// edges. Edge-type comparison ignores the confidence byte (`EdgeType::base`). Push-only BFS:
/// a relation-filtered frontier from a single seed is small in practice, so the
/// direction-optimizing pull path is unnecessary (and pull would need per-edge reverse types).
pub fn reachable_typed(
  graph: &Graph,
  seeds: &[u32],
  dir: Direction,
  allowed: &[EdgeType],
  max_depth: Option<u32>,
) -> BitSet {
  let n = graph.node_count();
  let mut visited = BitSet::with_capacity(n);
  let mut frontier: Vec<u32> = Vec::new();
  for &s in seeds {
    if (s as usize) < n && visited.insert(s as usize) {
      frontier.push(s);
    }
  }
  let allowed_bases: Vec<u16> = allowed.iter().map(|e| e.base().0).collect();
  let mut depth = 0u32;
  while !frontier.is_empty() && max_depth.is_none_or(|md| depth < md) {
    let mut next: Vec<u32> = Vec::new();
    for &u in &frontier {
      let (targets, types) = match dir {
        Direction::Out => (graph.out_targets(u), graph.out_edge_types(u)),
        Direction::In => (graph.in_targets(u), graph.in_edge_types(u)),
      };
      for (&v, &et) in targets.iter().zip(types) {
        if !allowed_bases.contains(&EdgeType(et).base().0) {
          continue;
        }
        if (v as usize) < n && visited.insert(v as usize) {
          next.push(v);
        }
      }
    }
    frontier = next;
    depth += 1;
  }
  for &s in seeds {
    visited.remove(s as usize);
  }
  visited
}

/// Like [`reachable`], but with an explicit [`Strategy`] (used to verify push ≡ pull).
pub fn reachable_strategy(
  graph: &Graph,
  seeds: &[u32],
  dir: Direction,
  strategy: Strategy,
) -> BitSet {
  let n = graph.node_count();
  let mut visited = BitSet::with_capacity(n);
  let mut frontier = BitSet::with_capacity(n);
  for &s in seeds {
    let s = s as usize;
    if s < n {
      visited.insert(s);
      frontier.insert(s);
    }
  }

  while !frontier.is_empty() {
    let use_pull = match strategy {
      Strategy::Push => false,
      Strategy::Pull => true,
      Strategy::Auto => frontier.count().saturating_mul(ALPHA) >= n,
    };
    let mut next = BitSet::with_capacity(n);

    if use_pull {
      // Bottom-up: an unvisited `w` joins the frontier iff a reverse-`dir` neighbor is in it.
      let reverse = dir.opposite();
      for w in 0..n {
        if visited.contains(w) {
          continue;
        }
        if neighbors_dir(graph, w as u32, reverse)
          .iter()
          .any(|&x| frontier.contains(x as usize))
        {
          visited.insert(w);
          next.insert(w);
        }
      }
    } else {
      // Top-down: expand each frontier node's `dir` neighbors.
      for u in frontier.iter() {
        for &v in neighbors_dir(graph, u as u32, dir) {
          let v = v as usize;
          if !visited.contains(v) {
            visited.insert(v);
            next.insert(v);
          }
        }
      }
    }
    frontier = next;
  }

  for &s in seeds {
    visited.remove(s as usize);
  }
  visited
}
