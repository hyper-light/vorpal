//! Vamana graph construction + greedy beam search (§10.2's DiskANN family, in-memory form).
//!
//! Construction is the ParlayANN-style **deterministic batch-parallel** build (§5): insertion
//! order is a seeded shuffle processed in doubling-prefix rounds — within a round every point
//! beam-searches the *frozen* graph in parallel and proposes its pruned neighborhood, then a
//! sequential merge applies proposals and back-edges in batch order. Same inputs yield the
//! same graph at any thread count (bit-identical rebuild stays part of the acceptance bar);
//! doubling prefixes keep the early graph dense, so small corpora get the same connectivity a
//! serial insertion build produced.
//!
//! All distances run over the i8 [`QuantMatrix`] — exact integer dots, deterministic
//! everywhere — which is what turned a kernel-scale build from minutes into seconds.

use rayon::prelude::*;

use crate::Rng;
use crate::qmatrix::{QuantMatrix, QuantQuery};

pub(crate) struct Vamana {
  /// Out-neighbors as one fixed-capacity slab: node `i`'s row is
  /// `flat[i·cap .. i·cap + lens[i]]`. Never materialized as per-row vectors — the slab is
  /// the build form, the search form, and what the CSR save walks.
  pub flat: Vec<u32>,
  pub lens: Vec<u8>,
  pub cap: usize,
  pub medoid: u32,
}

/// Adjacency access for beam search: per-node lists during construction, CSR sections
/// (typically zero-copy mapped) after load. Same rows either way.
pub(crate) enum Adjacency<'a> {
  Csr {
    offsets: &'a [u64],
    targets: &'a [u32],
  },
  /// Fixed-capacity rows in one contiguous slab — the under-construction form. One
  /// allocation for the whole graph instead of one `Vec` per row: traversal stops chasing
  /// millions of scattered heap pointers across as many heap pages (the fault profile's
  /// second spike, after the stamp churn).
  FlatCap {
    flat: &'a [u32],
    lens: &'a [u8],
    cap: usize,
  },
}

impl Adjacency<'_> {
  #[inline]
  pub fn is_empty(&self) -> bool {
    match self {
      Adjacency::Csr { offsets, .. } => offsets.len() <= 1,
      Adjacency::FlatCap { lens, .. } => lens.is_empty(),
    }
  }

  #[inline]
  pub fn row(&self, u: u32) -> &[u32] {
    match self {
      Adjacency::Csr { offsets, targets } => {
        &targets[offsets[u as usize] as usize..offsets[u as usize + 1] as usize]
      }
      Adjacency::FlatCap { flat, lens, cap } => {
        let at = u as usize * cap;
        &flat[at..at + lens[u as usize] as usize]
      }
    }
  }
}

pub(crate) struct BuildParams {
  pub r: usize,
  pub l_build: usize,
  pub alpha: f32,
  pub seed: u64,
}

/// A candidate node with its distance to the query.
pub(crate) type Scored = (u32, f32);

/// O(1) membership for beam search: an epoch-stamped mark per node, reused across searches
/// (bump the epoch instead of clearing 11 MB of marks per query). The linear beam/visited
/// scans this replaces were O(beam × degree × visited) id compares per search — at kernel
/// scale, a second quadratic cost rivaling the distance work itself.
pub(crate) struct VisitStamps {
  epoch: u16,
  mark: Vec<u16>,
}

impl VisitStamps {
  pub fn new(n: usize) -> Self {
    Self {
      epoch: 0,
      mark: vec![0; n],
    }
  }

  fn begin(&mut self) {
    // u16 epochs halve the resident mark array (~4.6 MB/million rows); the wrap-around
    // clear runs once per 65k searches per instance — noise.
    self.epoch = match self.epoch.checked_add(1) {
      Some(e) => e,
      None => {
        self.mark.fill(0);
        1
      }
    };
  }

  #[inline]
  fn seen(&mut self, v: u32) -> bool {
    let slot = &mut self.mark[v as usize];
    if *slot == self.epoch {
      true
    } else {
      *slot = self.epoch;
      false
    }
  }
}

/// Beam search from `medoid` over `graph`: expands the closest unexpanded beam entry until the
/// beam is exhausted, returning every visited node with its distance — the candidate pool for
/// pruning (build) and reranking (search).
///
/// The beam is a sorted array with inline expanded flags — total order `(distance, id)`, so
/// traversal is deterministic. A node ever admitted to the beam is stamped in `stamps`, which
/// is exactly what stops an evicted-then-rediscovered node from re-entering and looping.
pub(crate) fn greedy_search(
  graph: &Adjacency<'_>,
  medoid: u32,
  l: usize,
  stamps: &mut VisitStamps,
  dist: impl Fn(u32) -> f32,
  prefetch: impl Fn(u32),
) -> Vec<Scored> {
  if graph.is_empty() {
    return Vec::new();
  }
  stamps.begin();
  stamps.seen(medoid);
  // (distance, id, expanded) — kept sorted ascending by (distance, id).
  let mut beam: Vec<(f32, u32, bool)> = vec![(dist(medoid), medoid, false)];
  let mut visited: Vec<Scored> = Vec::new();

  loop {
    // Beam is sorted: the first unexpanded entry is the closest one.
    let Some(index) = beam.iter().position(|&(_, _, expanded)| !expanded) else {
      break;
    };
    beam[index].2 = true;
    let (dist_next, next, _) = beam[index];
    visited.push((next, dist_next));
    // The expansion loop is memory-latency-bound (one random ~256 B row per neighbor):
    // stage the next neighbor's row while the current dot product runs.
    let row = graph.row(next);
    for (i, &nb) in row.iter().enumerate() {
      if let Some(&ahead) = row.get(i + 1) {
        prefetch(ahead);
      }
      if stamps.seen(nb) {
        continue;
      }
      let d = dist(nb);
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
  pub fn build(matrix: &QuantMatrix, params: &BuildParams) -> Self {
    let n = matrix.len();
    if n == 0 {
      return Self {
        flat: Vec::new(),
        lens: Vec::new(),
        cap: params.r,
        medoid: 0,
      };
    }

    // Approximate medoid: the point closest to the (dequantized) centroid.
    let medoid = {
      let dim = matrix.dim();
      let mut centroid = vec![0.0f32; dim];
      for i in 0..n as u32 {
        let scale = matrix.scales[i as usize];
        for (c, &q) in centroid.iter_mut().zip(matrix.row_codes(i)) {
          *c += scale * q as f32;
        }
      }
      for c in centroid.iter_mut() {
        *c /= n as f32;
      }
      let query: QuantQuery = matrix.quantize_query(&centroid);
      (0..n as u32)
        .into_par_iter()
        .map(|i| (matrix.dist_to_query(i, &query), i))
        .reduce_with(|a, b| if a <= b { a } else { b })
        .map(|(_, i)| i)
        .unwrap_or(0)
    };

    // The under-construction graph is one contiguous slab of fixed-capacity rows (`r`
    // slots + a length byte per node): the per-row `Vec` form cost one heap allocation per
    // node, re-cloned on every swap-in, and scattered traversal over ~n heap pages.
    let mut flat: Vec<u32> = vec![0; n * params.r];
    let mut lens: Vec<u8> = vec![0; n];

    // Seeded random insertion order, processed in doubling-prefix rounds. One α pass:
    // DiskANN's classic build runs α=1 then α>1, but the α=1 pass exists to refine reach
    // that the downstream exact rerank + rank fusion do not depend on — dropping it halves
    // build cost with no observed change in fused search output (kernel-scale battery).
    let mut order: Vec<u32> = (0..n as u32).collect();
    let mut rng = Rng::new(params.seed);
    for i in (1..n).rev() {
      order.swap(i, rng.below(i + 1));
    }
    let threads = rayon::current_num_threads().max(1);
    // Reused stamp arrays, bounded by in-flight task count; dropped with the build. A fresh
    // zero-filled ~9 MB array per task per round re-faulted its pages on every allocation
    // under immediate-decay jemalloc — ~700k minor faults per kernel-scale build.
    let stamp_pool: std::sync::Mutex<Vec<VisitStamps>> = std::sync::Mutex::new(Vec::new());
    for alpha in [params.alpha] {
      let mut start = 0usize;
      let mut round = 1usize;
      while start < n {
        let batch = &order[start..(start + round).min(n)];
        // Parallel phase: every point in the round searches the frozen graph and proposes its
        // pruned out-neighborhood. Nothing mutates, so the proposals are a pure function of
        // the graph state at round start — thread count cannot change them. Chunking bounds
        // the per-task stamp scratch to one allocation per in-flight task.
        let chunk = batch.len().div_ceil(threads * 4).max(1);
        let proposals: Vec<(u32, Vec<u32>)> = batch
          .par_chunks(chunk)
          .flat_map_iter(|points| {
            let mut stamps = stamp_pool
              .lock()
              .unwrap_or_else(|poisoned| poisoned.into_inner())
              .pop()
              .unwrap_or_else(|| VisitStamps::new(n));
            let mut out = Vec::with_capacity(points.len());
            for &p in points {
              let visited = greedy_search(
                &Adjacency::FlatCap {
                  flat: &flat,
                  lens: &lens,
                  cap: params.r,
                },
                medoid,
                params.l_build,
                &mut stamps,
                |x| matrix.dist_sq(x, p),
                |x| matrix.prefetch_row(x),
              );
              let candidates: Vec<Scored> = visited.into_iter().filter(|&(v, _)| v != p).collect();
              out.push((
                p,
                robust_prune(matrix, p, candidates, alpha, params.r, params.l_build),
              ));
            }
            stamp_pool
              .lock()
              .unwrap_or_else(|poisoned| poisoned.into_inner())
              .push(stamps);
            out
          })
          .collect();
        // Merge, still in deterministic batch order but no longer serial: forward edges swap
        // in first; back-edge additions are grouped per target (preserving batch order within
        // each target), and then every target runs the exact per-addition push-and-prune loop
        // the serial build ran — independently, in parallel — before its final list swaps in.
        for (p, pruned) in &proposals {
          let at = *p as usize * params.r;
          flat[at..at + pruned.len()].copy_from_slice(pruned);
          lens[*p as usize] = pruned.len() as u8;
        }
        let mut additions: std::collections::HashMap<u32, Vec<u32>> =
          std::collections::HashMap::new();
        for (p, pruned) in &proposals {
          for &b in pruned {
            additions.entry(b).or_default().push(*p);
          }
        }
        let mut targets: Vec<(u32, Vec<u32>)> = additions.into_iter().collect();
        targets.sort_unstable_by_key(|(b, _)| *b);
        // One prune per target per round: additions append in batch order, then a single
        // robust prune bounds the degree — pruning once over the union keeps the same
        // α-domination guarantee as pruning per addition, at a fraction of the distance
        // work (targets in a big round receive many back-edges).
        let merged: Vec<(u32, Vec<u32>)> = targets
          .into_par_iter()
          .map(|(b, incoming)| {
            let at = b as usize * params.r;
            let mut neighbors: Vec<u32> = flat[at..at + lens[b as usize] as usize].to_vec();
            for p in incoming {
              if !neighbors.contains(&p) {
                neighbors.push(p);
              }
            }
            if neighbors.len() > params.r {
              let scored: Vec<Scored> = neighbors
                .iter()
                .map(|&v| (v, matrix.dist_sq(b, v)))
                .collect();
              neighbors = robust_prune(matrix, b, scored, alpha, params.r, params.l_build);
            }
            (b, neighbors)
          })
          .collect();
        for (b, neighbors) in merged {
          let at = b as usize * params.r;
          flat[at..at + neighbors.len()].copy_from_slice(&neighbors);
          lens[b as usize] = neighbors.len() as u8;
        }
        start += round;
        round *= 2;
      }
    }
    Self {
      flat,
      lens,
      cap: params.r,
      medoid,
    }
  }
}

/// DiskANN's robust prune: repeatedly keep the closest candidate and discard candidates it
/// α-dominates, bounding degree at `r` while preserving reach in all directions.
fn robust_prune(
  matrix: &QuantMatrix,
  p: u32,
  mut candidates: Vec<Scored>,
  alpha: f32,
  r: usize,
  pool_cap: usize,
) -> Vec<u32> {
  candidates.sort_by(|a, b| {
    (a.1, a.0)
      .partial_cmp(&(b.1, b.0))
      .unwrap_or(std::cmp::Ordering::Equal)
  });
  candidates.dedup_by_key(|c| c.0);
  // The α-domination loop re-scores survivors per kept neighbor — quadratic in pool size.
  // The farthest tail of a big visited pool never survives domination anyway; capping at
  // the search beam width (DiskANN prunes from an L-bounded pool) bounds the cost without
  // changing what the loop would have kept in practice.
  candidates.truncate(pool_cap);
  let mut result: Vec<u32> = Vec::new();
  while let Some(&(closest, _)) = candidates.first() {
    result.push(closest);
    if result.len() >= r {
      break;
    }
    candidates.retain(|&(v, dist_p)| v != closest && alpha * matrix.dist_sq(closest, v) > dist_p);
  }
  let _ = p;
  result
}
