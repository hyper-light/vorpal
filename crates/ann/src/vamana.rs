//! Vamana graph construction + greedy beam search (§10.2's DiskANN family, in-memory form).
//!
//! Construction is the ParlayANN-style **deterministic batch-parallel** build (§5): insertion
//! order is a seeded shuffle processed in doubling-prefix rounds — within a round every point
//! beam-searches the *frozen* graph in parallel and proposes its pruned neighborhood, then a
//! sequential merge applies proposals and back-edges in batch order. Same inputs yield the
//! same graph at any thread count (bit-identical rebuild stays part of the acceptance bar);
//! doubling prefixes keep the early graph dense, so small corpora get the same connectivity a
//! serial insertion build produced.

use rayon::prelude::*;

use crate::{Rng, l2_sq};

pub(crate) struct Vamana {
  /// Out-neighbors per node, each list ≤ `r`.
  pub graph: Vec<Vec<u32>>,
  pub medoid: u32,
}

pub(crate) struct BuildParams {
  pub r: usize,
  pub l_build: usize,
  pub alpha: f32,
  pub seed: u64,
}

/// A candidate node with its distance to the query.
pub(crate) type Scored = (u32, f32);

/// Beam search from `medoid` over `graph`: expands the closest unexpanded beam entry until the
/// beam is exhausted, returning every visited node with its distance — the candidate pool for
/// pruning (build) and exact reranking (search).
///
/// The beam is a sorted array with inline expanded flags — total order `(distance, id)`, so
/// traversal is deterministic — and membership checks scan the (≤ `l`-sized) beam plus the
/// visited list instead of hashing: at beam widths that fit cache lines, linear beats a
/// `HashSet` and allocates nothing. A node that was expanded and then evicted lives on in
/// `visited`, which is exactly what stops it re-entering the beam and looping.
pub(crate) fn greedy_search(
  graph: &[Vec<u32>],
  medoid: u32,
  vectors: &[f32],
  dim: usize,
  query: &[f32],
  l: usize,
) -> Vec<Scored> {
  if graph.is_empty() {
    return Vec::new();
  }
  let vec_of = |i: u32| &vectors[i as usize * dim..(i as usize + 1) * dim];
  // (distance, id, expanded) — kept sorted ascending by (distance, id).
  let mut beam: Vec<(f32, u32, bool)> = vec![(l2_sq(vec_of(medoid), query), medoid, false)];
  let mut visited: Vec<Scored> = Vec::new();

  loop {
    // Beam is sorted: the first unexpanded entry is the closest one.
    let Some(index) = beam.iter().position(|&(_, _, expanded)| !expanded) else {
      break;
    };
    beam[index].2 = true;
    let (dist, next, _) = beam[index];
    visited.push((next, dist));
    for &nb in &graph[next as usize] {
      if beam.iter().any(|&(_, v, _)| v == nb) || visited.iter().any(|&(v, _)| v == nb) {
        continue;
      }
      let d = l2_sq(vec_of(nb), query);
      let at = beam.partition_point(|&(bd, bv, _)| (bd, bv) < (d, nb));
      if at < l {
        beam.insert(at, (d, nb, false));
        beam.truncate(l);
      }
    }
  }
  visited
}

impl Vamana {
  pub fn build(vectors: &[f32], dim: usize, params: &BuildParams) -> Self {
    let n = if dim == 0 { 0 } else { vectors.len() / dim };
    if n == 0 {
      return Self {
        graph: Vec::new(),
        medoid: 0,
      };
    }
    let vec_of = |i: u32| &vectors[i as usize * dim..(i as usize + 1) * dim];

    // Approximate medoid: the point closest to the centroid (distances computed in parallel).
    let mut centroid = vec![0.0f32; dim];
    for i in 0..n {
      for (c, x) in centroid.iter_mut().zip(vec_of(i as u32)) {
        *c += x;
      }
    }
    for c in centroid.iter_mut() {
      *c /= n as f32;
    }
    let medoid = (0..n as u32)
      .into_par_iter()
      .map(|i| (l2_sq(vec_of(i), &centroid), i))
      .reduce_with(|a, b| if a <= b { a } else { b })
      .map(|(_, i)| i)
      .unwrap_or(0);

    let mut vamana = Self {
      graph: vec![Vec::new(); n],
      medoid,
    };

    // Seeded random insertion order; two passes over it (α=1 refines reach, α>1 adds
    // shortcuts), each processed in doubling-prefix rounds.
    let mut order: Vec<u32> = (0..n as u32).collect();
    let mut rng = Rng::new(params.seed);
    for i in (1..n).rev() {
      order.swap(i, rng.below(i + 1));
    }
    for alpha in [1.0, params.alpha] {
      let mut start = 0usize;
      let mut round = 1usize;
      while start < n {
        let batch = &order[start..(start + round).min(n)];
        // Parallel phase: every point in the round searches the frozen graph and proposes its
        // pruned out-neighborhood. Nothing mutates, so the proposals are a pure function of
        // the graph state at round start — thread count cannot change them.
        let proposals: Vec<(u32, Vec<u32>)> = batch
          .par_iter()
          .map(|&p| {
            let visited = greedy_search(
              &vamana.graph,
              vamana.medoid,
              vectors,
              dim,
              vec_of(p),
              params.l_build,
            );
            let candidates: Vec<Scored> = visited.into_iter().filter(|&(v, _)| v != p).collect();
            (p, robust_prune(vectors, dim, p, candidates, alpha, params.r))
          })
          .collect();
        // Merge, still in deterministic batch order but no longer serial: forward edges swap
        // in first; back-edge additions are grouped per target (preserving batch order within
        // each target), and then every target runs the exact per-addition push-and-prune loop
        // the serial build ran — independently, in parallel — before its final list swaps in.
        for (p, pruned) in &proposals {
          vamana.graph[*p as usize] = pruned.clone();
        }
        let mut additions: std::collections::HashMap<u32, Vec<u32>> = std::collections::HashMap::new();
        for (p, pruned) in &proposals {
          for &b in pruned {
            additions.entry(b).or_default().push(*p);
          }
        }
        let mut targets: Vec<(u32, Vec<u32>)> = additions.into_iter().collect();
        targets.sort_unstable_by_key(|(b, _)| *b);
        let merged: Vec<(u32, Vec<u32>)> = targets
          .into_par_iter()
          .map(|(b, incoming)| {
            let mut neighbors = vamana.graph[b as usize].clone();
            for p in incoming {
              if !neighbors.contains(&p) {
                neighbors.push(p);
                if neighbors.len() > params.r {
                  let scored: Vec<Scored> = neighbors
                    .iter()
                    .map(|&v| (v, l2_sq(vec_of(b), vec_of(v))))
                    .collect();
                  neighbors = robust_prune(vectors, dim, b, scored, alpha, params.r);
                }
              }
            }
            (b, neighbors)
          })
          .collect();
        for (b, neighbors) in merged {
          vamana.graph[b as usize] = neighbors;
        }
        start += round;
        round *= 2;
      }
    }
    vamana
  }
}

/// DiskANN's robust prune: repeatedly keep the closest candidate and discard candidates it
/// α-dominates, bounding degree at `r` while preserving reach in all directions.
fn robust_prune(
  vectors: &[f32],
  dim: usize,
  p: u32,
  mut candidates: Vec<Scored>,
  alpha: f32,
  r: usize,
) -> Vec<u32> {
  let vec_of = |i: u32| &vectors[i as usize * dim..(i as usize + 1) * dim];
  candidates.sort_by(|a, b| {
    (a.1, a.0)
      .partial_cmp(&(b.1, b.0))
      .unwrap_or(std::cmp::Ordering::Equal)
  });
  candidates.dedup_by_key(|c| c.0);
  let mut result: Vec<u32> = Vec::new();
  while let Some(&(closest, _)) = candidates.first() {
    result.push(closest);
    if result.len() >= r {
      break;
    }
    candidates
      .retain(|&(v, dist_p)| v != closest && alpha * l2_sq(vec_of(closest), vec_of(v)) > dist_p);
  }
  let _ = p;
  result
}
