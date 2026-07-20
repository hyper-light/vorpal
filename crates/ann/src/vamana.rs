//! Vamana graph construction + greedy beam search (§10.2's DiskANN family, in-memory form).
//!
//! Deterministic by construction: seeded insertion order, approximate-medoid start, two build
//! passes (α=1 then α=target) with robust pruning — the same inputs always yield the same graph
//! (bit-identical rebuild is part of the acceptance bar).

use std::collections::HashSet;

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
  let mut beam: Vec<Scored> = vec![(medoid, l2_sq(vec_of(medoid), query))];
  let mut expanded: HashSet<u32> = HashSet::new();
  let mut visited: Vec<Scored> = Vec::new();

  while let Some(&(next, dist)) = beam.iter().find(|(v, _)| !expanded.contains(v)) {
    expanded.insert(next);
    visited.push((next, dist));
    for &nb in &graph[next as usize] {
      if !expanded.contains(&nb) && !beam.iter().any(|&(v, _)| v == nb) {
        beam.push((nb, l2_sq(vec_of(nb), query)));
      }
    }
    beam.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    beam.truncate(l);
  }
  visited
}

impl Vamana {
  pub fn build(vectors: &[f32], dim: usize, params: &BuildParams) -> Self {
    let n = vectors.len().checked_div(dim).unwrap_or(0);
    if n == 0 {
      return Self {
        graph: Vec::new(),
        medoid: 0,
      };
    }
    let vec_of = |i: u32| &vectors[i as usize * dim..(i as usize + 1) * dim];

    // Approximate medoid: the point closest to the centroid.
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
      .min_by(|&a, &b| {
        l2_sq(vec_of(a), &centroid)
          .partial_cmp(&l2_sq(vec_of(b), &centroid))
          .unwrap_or(std::cmp::Ordering::Equal)
      })
      .unwrap_or(0);

    let mut vamana = Self {
      graph: vec![Vec::new(); n],
      medoid,
    };

    // Seeded random insertion order; two passes over it (α=1 refines reach, α>1 adds shortcuts).
    let mut order: Vec<u32> = (0..n as u32).collect();
    let mut rng = Rng::new(params.seed);
    for i in (1..n).rev() {
      order.swap(i, rng.below(i + 1));
    }
    for alpha in [1.0, params.alpha] {
      for &p in &order {
        let visited = greedy_search(
          &vamana.graph,
          vamana.medoid,
          vectors,
          dim,
          vec_of(p),
          params.l_build,
        );
        let candidates: Vec<(u32, f32)> = visited.into_iter().filter(|&(v, _)| v != p).collect();
        let pruned = robust_prune(vectors, dim, p, candidates, alpha, params.r);
        vamana.graph[p as usize] = pruned.clone();
        for b in pruned {
          if !vamana.graph[b as usize].contains(&p) {
            vamana.graph[b as usize].push(p);
            if vamana.graph[b as usize].len() > params.r {
              let neighbors: Vec<(u32, f32)> = vamana.graph[b as usize]
                .iter()
                .map(|&v| (v, l2_sq(vec_of(b), vec_of(v))))
                .collect();
              vamana.graph[b as usize] = robust_prune(vectors, dim, b, neighbors, alpha, params.r);
            }
          }
        }
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
  mut candidates: Vec<(u32, f32)>,
  alpha: f32,
  r: usize,
) -> Vec<u32> {
  let vec_of = |i: u32| &vectors[i as usize * dim..(i as usize + 1) * dim];
  candidates.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
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
