//! Relation-aware convex retrofit of definition vectors over the knowledge graph
//! (semantic-tier Stage 2; docs/wip/SEMANTIC_TIER.md).
//!
//! The objective is Faruqui et al. 2015 ("Retrofitting Word Vectors to Semantic
//! Lexicons", NAACL) verbatim:
//!
//! ```text
//! Ψ(X) = Σᵢ αᵢ·‖xᵢ − qᵢ‖²  +  Σ_{(i,j)∈E} βᵢⱼ·‖xᵢ − xⱼ‖²   (each edge ONCE)
//! ```
//!
//! with the paper's own constants: αᵢ = 1 and βᵢⱼ carrying the 1/deg(i)
//! normalization — both CITED, not tuned. The CSR stores both directions of a
//! symmetric edge, so the evaluator halves the edge term to keep the once-per-edge
//! convention whose row minimizer is exactly the sweep's update.
//!
//! Jacobi descent on this objective is PROVEN, not assumed: per component the
//! Hessian is A = 2(I + L) with L the weighted graph Laplacian, the Jacobi splitting
//! has D = 2(I + Deg), B = 2W, and the descent condition 2D − A = 2(I + Deg + W) is
//! strictly positive definite because Deg + W is the signless Laplacian,
//! xᵀ(Deg+W)x = Σ_{(i,j)} wᵢⱼ(xᵢ+xⱼ)² ≥ 0. A measured Ψ increase therefore can only
//! be a defect, and is a typed error. The caller folds everything edge-specific
//! (relation weight learned from the corpus, resolution-grade factor, degree
//! normalization) into [`RetroEdges::weights`] up front, so this module is pure
//! convex algebra over plain arrays: it never sees edge types, grades, or graph
//! machinery (vorpal-ann deliberately has no kg dependency).
//!
//! Iteration is JACOBI — every row of X_{t+1} reads only X_t, so rows parallelize
//! with structural thread-count invariance (no reduction crosses a row; per-row
//! neighbor sums accumulate in f64 in CSR order). The system is strictly diagonally
//! dominant (α = 1 > 0 joins every diagonal), so Jacobi converges monotonically on
//! the convex objective; termination is machine-precision on Ψ itself — the sweep
//! loop stops when Ψ stops STRICTLY decreasing by more than Ψ₀·ε₃₂ — never an
//! iteration-count tunable. A Ψ increase between sweeps is a typed error (the
//! caller's auto-disable seam), because on this objective it can only mean a defect.

use rayon::prelude::*;

/// Typed, grade-weighted, degree-normalized adjacency over the SEMANTIC ROW space
/// (row index = position in the tier's row universe, NOT a node id): CSR with one
/// f32 weight per directed edge. Weights arrive fully folded — relation weight ×
/// grade × 1/deg — non-negative and finite; a symmetric relation appears in both
/// directions.
pub struct RetroEdges {
  /// Row `i`'s neighbors live at `targets[offsets[i]..offsets[i+1]]` (len n+1).
  pub offsets: Vec<u64>,
  pub targets: Vec<u32>,
  pub weights: Vec<f32>,
}

impl RetroEdges {
  /// An edgeless graph over `n` rows (retrofit degenerates to the identity).
  pub fn empty(n: usize) -> RetroEdges {
    RetroEdges {
      offsets: vec![0; n + 1],
      targets: Vec::new(),
      weights: Vec::new(),
    }
  }

  fn validate(&self, n: usize) -> Result<(), String> {
    if self.offsets.len() != n + 1 {
      return Err(format!(
        "retrofit edges: {} offsets for {n} rows (want n+1)",
        self.offsets.len()
      ));
    }
    if self.offsets.first() != Some(&0) {
      return Err("retrofit edges: offsets must start at 0".to_string());
    }
    for window in self.offsets.windows(2) {
      if window[1] < window[0] {
        return Err("retrofit edges: offsets not monotone".to_string());
      }
    }
    let total = *self.offsets.last().unwrap_or(&0) as usize;
    if self.targets.len() != total || self.weights.len() != total {
      return Err(format!(
        "retrofit edges: {} targets / {} weights for {total} slots",
        self.targets.len(),
        self.weights.len()
      ));
    }
    if self.targets.iter().any(|&t| t as usize >= n) {
      return Err("retrofit edges: target row out of range".to_string());
    }
    if self.weights.iter().any(|w| !w.is_finite() || *w < 0.0) {
      return Err("retrofit edges: weights must be finite and non-negative".to_string());
    }
    Ok(())
  }
}

/// What one retrofit run did — sweeps taken and the Ψ descent, for provenance and
/// phase stamps. `final_psi ≤ initial_psi` always (an increase is an error instead).
pub struct RetrofitReport {
  pub sweeps: usize,
  pub initial_psi: f64,
  pub final_psi: f64,
}

/// Fixed-order Ψ evaluation: per-row contributions accumulate in f64, rows fold in
/// fixed 4096-row chunks combined in chunk order — the same bits at any thread count
/// (the crate's standard deterministic-reduction shape).
fn psi(x: &[f32], anchors: &[f32], dim: usize, edges: &RetroEdges) -> f64 {
  const CHUNK_ROWS: usize = 4096;
  let partials: Vec<f64> = x
    .par_chunks(CHUNK_ROWS * dim)
    .enumerate()
    .map(|(chunk_index, rows)| {
      let mut total = 0.0f64;
      for (local, row) in rows.chunks_exact(dim).enumerate() {
        let i = chunk_index * CHUNK_ROWS + local;
        let anchor = &anchors[i * dim..(i + 1) * dim];
        for (value, target) in row.iter().zip(anchor) {
          let diff = *value as f64 - *target as f64;
          total += diff * diff;
        }
        let (start, end) = (edges.offsets[i] as usize, edges.offsets[i + 1] as usize);
        for slot in start..end {
          let neighbor = edges.targets[slot] as usize;
          let weight = edges.weights[slot] as f64;
          let other = &x[neighbor * dim..(neighbor + 1) * dim];
          let mut distance = 0.0f64;
          for (value, other_value) in row.iter().zip(other) {
            let diff = *value as f64 - *other_value as f64;
            distance += diff * diff;
          }
          // Each undirected edge appears in BOTH CSR directions; the objective counts
          // it ONCE (the convention whose row minimizer is exactly `sweep`'s update —
          // evaluating the double-counted form against that update is inconsistent
          // and reads as a spurious "Ψ increase").
          total += 0.5 * weight * distance;
        }
      }
      total
    })
    .collect();
  partials.iter().sum()
}

/// One Jacobi sweep: X_{t+1}[i] = (qᵢ + Σⱼ wᵢⱼ·X_t[j]) / (1 + Σⱼ wᵢⱼ) — the paper's
/// closed-form row minimizer with αᵢ = 1. Rows are independent (write-disjoint,
/// read-only X_t), so parallelism cannot change a bit.
fn sweep(x: &[f32], anchors: &[f32], dim: usize, edges: &RetroEdges, next: &mut [f32]) {
  next
    .par_chunks_mut(dim)
    .enumerate()
    .for_each(|(i, row_out)| {
      let (start, end) = (edges.offsets[i] as usize, edges.offsets[i + 1] as usize);
      let anchor = &anchors[i * dim..(i + 1) * dim];
      if start == end {
        row_out.copy_from_slice(anchor);
        return;
      }
      let mut weight_sum = 0.0f64;
      for slot in start..end {
        weight_sum += edges.weights[slot] as f64;
      }
      let denominator = 1.0 + weight_sum;
      for (component, slot_out) in row_out.iter_mut().enumerate() {
        let mut accumulator = anchor[component] as f64;
        for slot in start..end {
          let neighbor = edges.targets[slot] as usize;
          accumulator += edges.weights[slot] as f64 * x[neighbor * dim + component] as f64;
        }
        *slot_out = (accumulator / denominator) as f32;
      }
    });
}

/// Retrofit over caller-provided working buffers — the scale form: at kernel size the
/// three n×dim regions (anchors, X_t, X_{t+1}) live on [`vorpal_mem::ScratchMmap`]
/// files (the OS pager carries the working set; anonymous RSS stays bounded), and this
/// function only ever sees `&[f32]`/`&mut [f32]`, so scratch-backed and in-RAM runs
/// share every instruction. Contract: `x` and `next` are anchor-sized; `x` starts as a
/// byte-copy of `anchors` (X₀ = Q). The refined matrix always ENDS in `x` (an odd
/// sweep count copies the final buffer back — one linear pass, dwarfed by the sweeps).
/// Every number is cited (α = 1, Faruqui 2015) or machine-derived (the ε₃₂ descent
/// floor); sweep count is an OUTPUT, never an input.
pub fn retrofit_into(
  anchors: &[f32],
  x: &mut [f32],
  next: &mut [f32],
  dim: usize,
  edges: &RetroEdges,
) -> Result<RetrofitReport, String> {
  if dim == 0 {
    return Err("retrofit: zero dimension".to_string());
  }
  if anchors.len() % dim != 0 {
    return Err("retrofit: anchor matrix not row-shaped".to_string());
  }
  if x.len() != anchors.len() || next.len() != anchors.len() {
    return Err("retrofit: working buffers must be anchor-sized".to_string());
  }
  if anchors.iter().any(|v| !v.is_finite()) {
    return Err("retrofit: non-finite anchor".to_string());
  }
  let n = anchors.len() / dim;
  edges.validate(n)?;

  let initial_psi = psi(x, anchors, dim, edges);
  // The descent floor: further sweeps are noise once Ψ moves by less than the
  // objective's own f32 representability — Ψ₀ scaled by ε₃₂, machine-derived.
  let floor = initial_psi * f32::EPSILON as f64;
  let mut previous_psi = initial_psi;
  let mut sweeps = 0usize;
  // Reference-swapped double buffering: `current` holds X_t, `scratch` receives
  // X_{t+1}; the bindings swap, the storage never moves.
  let (mut current, mut scratch) = (x, next);
  loop {
    sweep(current, anchors, dim, edges, scratch);
    std::mem::swap(&mut current, &mut scratch);
    sweeps += 1;
    let current_psi = psi(current, anchors, dim, edges);
    if current_psi > previous_psi {
      // Convex objective + Jacobi descent PROVEN above: an increase can only be a
      // defect. Typed error — the caller's auto-disable seam, never a panic.
      return Err(format!(
        "retrofit: Ψ increased at sweep {sweeps} ({previous_psi} → {current_psi})"
      ));
    }
    if previous_psi - current_psi <= floor {
      if sweeps % 2 == 1 {
        // Final X landed in the `next` buffer; the contract puts it in `x`.
        scratch.copy_from_slice(current);
      }
      return Ok(RetrofitReport {
        sweeps,
        initial_psi,
        final_psi: current_psi,
      });
    }
    previous_psi = current_psi;
  }
}

/// [`retrofit_into`] with owned buffers — the test/reference form. Returns the refined
/// matrix.
pub fn retrofit_in_ram(
  anchors: &[f32],
  dim: usize,
  edges: &RetroEdges,
) -> Result<(Vec<f32>, RetrofitReport), String> {
  let mut x = anchors.to_vec();
  let mut next = vec![0.0f32; anchors.len()];
  let report = retrofit_into(anchors, &mut x, &mut next, dim, edges)?;
  Ok((x, report))
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Symmetric single edge with weight w in both directions.
  fn pair_edges(w: f32) -> RetroEdges {
    RetroEdges {
      offsets: vec![0, 1, 2],
      targets: vec![1, 0],
      weights: vec![w, w],
    }
  }

  #[test]
  fn two_node_system_reaches_the_closed_form() {
    // d=1, anchors q₀=0, q₁=1, w=1 both ways. Fixed point: x₀=(0+x₁)/2, x₁=(1+x₀)/2
    // ⇒ x₀ = 1/3, x₁ = 2/3 (hand-solved).
    let (x, report) = retrofit_in_ram(&[0.0, 1.0], 1, &pair_edges(1.0)).unwrap();
    // Tolerance DERIVED from the termination floor: sweeps stop once ΔΨ ≤ ε₃₂·Ψ₀, and
    // the geometric decrease sequence bounds the remaining gap Ψ−Ψ* ≤ ΔΨ/(1−ρ) with
    // ρ = (w/(1+w))² = 1/4 here; per-component curvature of Ψ is ≥ 1 (the anchor term
    // alone), so ‖x−x*‖² ≤ Ψ−Ψ* ≤ (4/3)·ε₃₂·Ψ₀ < 2·ε₃₂·Ψ₀.
    let tolerance = (2.0 * f32::EPSILON as f64 * report.initial_psi).sqrt();
    assert!((x[0] as f64 - 1.0 / 3.0).abs() < tolerance, "{x:?} tol {tolerance}");
    assert!((x[1] as f64 - 2.0 / 3.0).abs() < tolerance, "{x:?} tol {tolerance}");
    assert!(report.final_psi <= report.initial_psi);
    assert!(report.sweeps > 1);
  }

  #[test]
  fn no_edges_is_the_identity_bitwise() {
    let anchors: Vec<f32> = (0..40).map(|i| (i as f32).sin()).collect();
    let (x, report) = retrofit_in_ram(&anchors, 8, &RetroEdges::empty(5)).unwrap();
    assert_eq!(
      x.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
      anchors.iter().map(|v| v.to_bits()).collect::<Vec<_>>()
    );
    assert_eq!(report.final_psi, 0.0);
  }

  fn random_case(n: usize, dim: usize, seed: u64) -> (Vec<f32>, RetroEdges) {
    let mut state = seed.max(1);
    let mut next = move || {
      state ^= state << 13;
      state ^= state >> 7;
      state ^= state << 17;
      state
    };
    let anchors: Vec<f32> = (0..n * dim)
      .map(|_| (next() % 2000) as f32 / 1000.0 - 1.0)
      .collect();
    // Ring + random chords, symmetric, weight in (0, 1].
    let mut adjacency: Vec<Vec<(u32, f32)>> = vec![Vec::new(); n];
    let mut connect = |a: usize, b: usize, w: f32, adjacency: &mut Vec<Vec<(u32, f32)>>| {
      adjacency[a].push((b as u32, w));
      adjacency[b].push((a as u32, w));
    };
    for i in 0..n {
      let w = ((next() % 1000) + 1) as f32 / 1000.0;
      connect(i, (i + 1) % n, w, &mut adjacency);
    }
    for _ in 0..n {
      let a = (next() as usize) % n;
      let b = (next() as usize) % n;
      if a != b {
        let w = ((next() % 1000) + 1) as f32 / 1000.0;
        connect(a, b, w, &mut adjacency);
      }
    }
    let mut offsets = vec![0u64];
    let mut targets = Vec::new();
    let mut weights = Vec::new();
    for row in &adjacency {
      for &(t, w) in row {
        targets.push(t);
        weights.push(w);
      }
      offsets.push(targets.len() as u64);
    }
    (
      anchors,
      RetroEdges {
        offsets,
        targets,
        weights,
      },
    )
  }

  #[test]
  fn descent_terminates_and_is_bit_deterministic() {
    let (anchors, edges) = random_case(300, 16, 99);
    let (x1, r1) = retrofit_in_ram(&anchors, 16, &edges).unwrap();
    let (x2, r2) = retrofit_in_ram(&anchors, 16, &edges).unwrap();
    assert!(r1.final_psi < r1.initial_psi);
    assert_eq!(r1.sweeps, r2.sweeps);
    assert_eq!(
      x1.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
      x2.iter().map(|v| v.to_bits()).collect::<Vec<_>>()
    );
  }

  #[test]
  fn malformed_edges_are_typed_errors() {
    let anchors = vec![0.0f32; 8];
    let mut edges = RetroEdges::empty(2);
    edges.offsets = vec![0, 1]; // wrong length
    assert!(retrofit_in_ram(&anchors, 4, &edges).is_err());
    let mut bad_weight = pair_edges(1.0);
    bad_weight.weights[0] = f32::NAN;
    assert!(retrofit_in_ram(&[0.0, 1.0], 1, &bad_weight).is_err());
    let mut bad_target = pair_edges(1.0);
    bad_target.targets[0] = 7;
    assert!(retrofit_in_ram(&[0.0, 1.0], 1, &bad_target).is_err());
  }
}
