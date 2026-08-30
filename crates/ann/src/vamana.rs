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

  /// Stage `v`'s mark slot into cache ahead of the `seen` probe (a random 2-byte load into
  /// a multi-MB array — the expansion loop's other unhidden memory dependency).
  #[inline]
  pub fn prefetch(&self, v: u32) {
    if let Some(slot) = self.mark.get(v as usize) {
      vorpal_mem::prefetch::prefetch_read(slot as *const u16 as *const u8);
    }
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
/// Deterministic build instrumentation (VORPAL_PHASE_TRACE-gated): distance evaluations and
/// beam expansions are pure functions of (input, binary) under the deterministic build, so
/// two builds' counters are exactly comparable even when wall time drowns in machine noise.
pub(crate) static BUILD_DIST_EVALS: std::sync::atomic::AtomicU64 =
  std::sync::atomic::AtomicU64::new(0);
pub(crate) static BUILD_EXPANSIONS: std::sync::atomic::AtomicU64 =
  std::sync::atomic::AtomicU64::new(0);

#[inline]
pub(crate) fn counters_enabled() -> bool {
  static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
  *ON.get_or_init(|| std::env::var_os("VORPAL_PHASE_TRACE").is_some())
}

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
  let mut beam: Vec<(f32, u32, bool)> = Vec::with_capacity(l + 1);
  beam.push((dist(medoid), medoid, false));
  let mut visited: Vec<Scored> = Vec::with_capacity(l * 3);
  // Per-expansion admission buffer (see below) — one allocation per search, reused.
  let mut cand: Vec<(f32, u32)> = Vec::with_capacity(64);
  // Survivor ids staged between the filter pass and the distance pass, reused.
  let mut fresh: Vec<u32> = Vec::with_capacity(64);

  // Frontier cursor: entries before `frontier` are all expanded, so the first unexpanded
  // entry (the (distance, id) minimum among unexpanded — the array is sorted) is found in
  // O(1) instead of the previous rescan-from-zero (O(beam) per expansion, ~1.7×10¹⁰ flag
  // reads per kernel-scale build).
  let mut frontier = 0usize;
  while frontier < beam.len() {
    if beam[frontier].2 {
      frontier += 1;
      continue;
    }
    beam[frontier].2 = true;
    let (dist_next, next, _) = beam[frontier];
    frontier += 1;
    visited.push((next, dist_next));
    // The expansion is memory-latency-bound (one random ~256 B row per candidate). Two
    // passes convert a 1-deep prefetch pipeline into full-fan memory-level parallelism:
    // the FILTER pass runs the visited checks (staging each upcoming mark slot) and issues
    // the row prefetch for every survivor — putting all of them in flight at once — and
    // the DISTANCE pass then evaluates rows the memory system has already been fetching.
    // Same survivors, same distances, same admission set: bit-identical output (sha-pinned).
    let row = graph.row(next);
    cand.clear();
    fresh.clear();
    for (i, &nb) in row.iter().enumerate() {
      if let Some(&ahead) = row.get(i + 1) {
        stamps.prefetch(ahead);
      }
      if stamps.seen(nb) {
        continue;
      }
      prefetch(nb);
      fresh.push(nb);
    }
    for &nb in &fresh {
      cand.push((dist(nb), nb));
    }
    if counters_enabled() {
      use std::sync::atomic::Ordering;
      BUILD_EXPANSIONS.fetch_add(1, Ordering::Relaxed);
      BUILD_DIST_EVALS.fetch_add(cand.len() as u64, Ordering::Relaxed);
    }
    if cand.is_empty() {
      continue;
    }
    // Batch admission: sort this expansion's candidates once and set_union-merge them into
    // the sorted beam in one pass — replacing up to R separate binary-search + O(beam)
    // memmove inserts. The result is the l smallest (distance, id) entries of
    // old-beam ∪ candidates, exactly what the sequential inserts produced (each sequential
    // insert kept the running l-minimum; admission or eviction order cannot change the
    // final set, and ties cannot exist — ids are unique).
    cand.sort_unstable_by(|a, b| (a.0, a.1).partial_cmp(&(b.0, b.1)).unwrap());
    let old = std::mem::replace(&mut beam, Vec::with_capacity(l + 1));
    let mut oi = 0usize;
    let mut ci = 0usize;
    let mut first_new: Option<usize> = None;
    while beam.len() < l && (oi < old.len() || ci < cand.len()) {
      let take_new = match (old.get(oi), cand.get(ci)) {
        (Some(&(od, ov, _)), Some(&(cd, cv))) => (cd, cv) < (od, ov),
        (None, Some(_)) => true,
        (Some(_), None) => false,
        (None, None) => unreachable!(),
      };
      if take_new {
        if first_new.is_none() {
          first_new = Some(beam.len());
        }
        let (cd, cv) = cand[ci];
        beam.push((cd, cv, false));
        ci += 1;
      } else {
        beam.push(old[oi]);
        oi += 1;
      }
    }
    // A new entry landing before the cursor reopens the expanded prefix at its position.
    if let Some(at) = first_new
      && at < frontier
    {
      frontier = at;
    }
  }
  visited
}

/// One bulk-synchronous insertion round: every point in `batch` searches the FROZEN graph
/// and proposes its pruned out-neighborhood; forward edges swap in; back-edges transpose by
/// counting scatter; each touched target merge-prunes once. Proposals are a pure function
/// of the graph state at round start, so thread count cannot change the outcome. With
/// `include_existing`, a point's current out-edges join its candidate pool — the refinement
/// pass's guarantee that re-pruning never silently drops an edge the fresh search missed.
#[allow(clippy::too_many_arguments)]
fn run_round(
  matrix: &QuantMatrix,
  params: &BuildParams,
  alpha: f32,
  medoid: u32,
  flat: &mut Vec<u32>,
  lens: &mut Vec<u8>,
  counts: &mut [u32],
  offsets: &mut [u32],
  cursors: &mut [u32],
  stamp_pool: &std::sync::Mutex<Vec<VisitStamps>>,
  batch: &[u32],
  include_existing: bool,
) {
  let n = lens.len();
  let threads = rayon::current_num_threads().max(1);
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
            flat,
            lens,
            cap: params.r,
          },
          medoid,
          params.l_build,
          &mut stamps,
          |x| matrix.dist_sq(x, p),
          |x| matrix.prefetch_row(x),
        );
        let mut candidates: Vec<Scored> = visited.into_iter().filter(|&(v, _)| v != p).collect();
        if include_existing {
          let at = p as usize * params.r;
          for &v in &flat[at..at + lens[p as usize] as usize] {
            candidates.push((v, matrix.dist_sq(v, p)));
          }
        }
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
  for (p, pruned) in &proposals {
    let at = *p as usize * params.r;
    flat[at..at + pruned.len()].copy_from_slice(pruned);
    lens[*p as usize] = pruned.len() as u8;
  }
  counts[..].fill(0);
  for (_, pruned) in &proposals {
    for &b in pruned {
      counts[b as usize] += 1;
    }
  }
  let mut total = 0u32;
  for b in 0..n {
    offsets[b] = total;
    total += counts[b];
  }
  offsets[n] = total;
  let mut incoming_flat: Vec<u32> = vec![0; total as usize];
  cursors[..n].copy_from_slice(&offsets[..n]);
  for (p, pruned) in &proposals {
    for &b in pruned {
      let slot = cursors[b as usize];
      incoming_flat[slot as usize] = *p;
      cursors[b as usize] = slot + 1;
    }
  }
  let touched: Vec<u32> = (0..n as u32).filter(|&b| counts[b as usize] > 0).collect();
  let merged: Vec<(u32, Vec<u32>)> = touched
    .into_par_iter()
    .map(|b| {
      let incoming = &incoming_flat[offsets[b as usize] as usize..offsets[b as usize + 1] as usize];
      let at = b as usize * params.r;
      let mut neighbors: Vec<u32> = flat[at..at + lens[b as usize] as usize].to_vec();
      for &p in incoming {
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
    if counters_enabled() {
      use std::sync::atomic::Ordering;
      BUILD_DIST_EVALS.store(0, Ordering::Relaxed);
      BUILD_EXPANSIONS.store(0, Ordering::Relaxed);
    }
    // Reused stamp arrays, bounded by in-flight task count; dropped with the build. A fresh
    // zero-filled ~9 MB array per task per round re-faulted its pages on every allocation
    // under immediate-decay jemalloc — ~700k minor faults per kernel-scale build.
    let stamp_pool: std::sync::Mutex<Vec<VisitStamps>> = std::sync::Mutex::new(Vec::new());
    {
      // Round-reused transpose scratch (n-sized; zeroed per round — a memset, not an alloc).
      let mut counts: Vec<u32> = vec![0; n];
      let mut offsets: Vec<u32> = vec![0; n + 1];
      let mut cursors: Vec<u32> = vec![0; n];
      let mut start = 0usize;
      let mut round = 1usize;
      while start < n {
        let batch: Vec<u32> = order[start..(start + round).min(n)].to_vec();
        run_round(
          matrix,
          params,
          params.alpha,
          medoid,
          &mut flat,
          &mut lens,
          &mut counts,
          &mut offsets,
          &mut cursors,
          &stamp_pool,
          &batch,
          false,
        );
        start += round;
        round *= 2;
      }
      // Refinement round (two-pass Vamana; FastKCNA's k-CNA result): every node re-acquires
      // its candidate pool by searching the FINISHED graph — early-round nodes chose their
      // edges against a graph that barely existed, and this is the pass that repairs them.
      // One bulk-synchronous round over ascending ids (deterministic), existing out-edges
      // included in each pool so re-pruning can only refine, never regress reach.
      let refine: Vec<u32> = (0..n as u32).collect();
      run_round(
        matrix,
        params,
        params.alpha,
        medoid,
        &mut flat,
        &mut lens,
        &mut counts,
        &mut offsets,
        &mut cursors,
        &stamp_pool,
        &refine,
        true,
      );
    }
    // In-coverage repair: a node no other node points at is unreachable by graph traversal
    // (only the medoid is legitimately entered from outside), so its whole neighborhood can
    // never surface in any beam's visited pool. For each such node v (ascending id —
    // deterministic), splice v into the out-list of its nearest own out-neighbor u via the
    // same α-prune every other edge decision uses: u's list re-prunes over {existing ∪ v},
    // so v displaces an edge only when α-domination says it should. In-degree 0 is a
    // structural condition, not a tuned threshold.
    {
      let mut indegree: Vec<u32> = vec![0; n];
      for u in 0..n {
        let at = u * params.r;
        for &nb in &flat[at..at + lens[u] as usize] {
          indegree[nb as usize] += 1;
        }
      }
      let starved: Vec<u32> = (0..n as u32)
        .filter(|&v| indegree[v as usize] == 0 && v != medoid && lens[v as usize] > 0)
        .collect();
      if counters_enabled() {
        crate::trace(&format!("vamana: {} unreachable nodes repaired", starved.len()));
      }
      for v in starved {
        let vat = v as usize * params.r;
        // Nearest own out-neighbor: v's list is (re)built by robust_prune, whose first
        // entry is its closest survivor — but recompute to be explicit and order-proof.
        let mut best: Option<(f32, u32)> = None;
        for &u in &flat[vat..vat + lens[v as usize] as usize] {
          let key = (matrix.dist_sq(u, v), u);
          if best.is_none() || key < best.expect("checked") {
            best = Some(key);
          }
        }
        let Some((_, u)) = best else { continue };
        let uat = u as usize * params.r;
        let mut pool: Vec<Scored> = flat[uat..uat + lens[u as usize] as usize]
          .iter()
          .map(|&w| (w, matrix.dist_sq(u, w)))
          .collect();
        pool.push((v, matrix.dist_sq(u, v)));
        let pruned = robust_prune(matrix, u, pool, params.alpha, params.r, params.l_build);
        flat[uat..uat + pruned.len()].copy_from_slice(&pruned);
        lens[u as usize] = pruned.len() as u8;
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
///
/// Lazily memoized (the DiskANN-Rust `prune.rs` structure): instead of the eager form —
/// each selected neighbor rescanning every remaining candidate — each candidate, taken in
/// (distance, id) order, checks the already-selected list from where it last stopped and
/// halts at its FIRST occluder. A candidate is selected iff no earlier-selected neighbor
/// α-dominates it, which is exactly the eager loop's condition, in the same order — the
/// selected set is bit-identical (pinned by the unchanged ann.bin sha) — but a rejected
/// candidate now costs ~1 pairwise distance instead of being rescanned by every later
/// selection.
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
  // The farthest tail of a big visited pool never survives domination anyway; capping at
  // the search beam width (DiskANN prunes from an L-bounded pool) bounds the cost without
  // changing what the loop would have kept in practice.
  candidates.truncate(pool_cap);
  let mut result: Vec<u32> = Vec::with_capacity(r.min(candidates.len()));
  let mut prune_evals = 0u64;
  'candidates: for &(v, dist_p) in &candidates {
    // Check against selected neighbors in selection order; first occluder rejects. The
    // eager form removed `v` the moment its first occluder was SELECTED — same pairs, same
    // outcome, no rescans. (`result` only grows, so no memo index is even needed: every
    // candidate is visited once and scans the selected prefix exactly once.)
    for &s_id in &result {
      if v == s_id {
        continue 'candidates;
      }
      prune_evals += 1;
      if alpha * matrix.dist_sq(s_id, v) <= dist_p {
        continue 'candidates;
      }
    }
    result.push(v);
    if result.len() >= r {
      break;
    }
  }
  if counters_enabled() && prune_evals > 0 {
    BUILD_DIST_EVALS.fetch_add(prune_evals, std::sync::atomic::Ordering::Relaxed);
  }
  let _ = p;
  result
}
