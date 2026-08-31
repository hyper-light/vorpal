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

// ---------------------------------------------------------------------------------
// The plan's SECOND penalty form: per-relation linear maps (functional retrofitting,
// Lengerich et al. 2018), restricted to DIAGONAL A_r. Dense d×d maps cost d² per
// edge per pass — ~5.5×10¹¹ flops at kernel scale, two orders past the stage budget
// — while a diagonal map keeps the pairwise penalty ‖A_r xᵢ − xⱼ‖² component-
// decoupled: the same Jacobi skeleton, the same termination law, and the SAME descent
// proof shape. Per component with scale a, the edge Hessian off-diagonal is −w·a and
// the descent form contributes w·(a·xᵢ + xⱼ)² ≥ 0 for ANY sign of a — the signless
// argument verbatim. The fit is closed-form per relation per dimension (1-D least
// squares over the relation's directed pairs), so no ridge constant exists: a
// dimension whose normal-equation denominator sits below the relation's own
// numerical floor (max-denominator × ε₃₂ — the crate's standard dependence
// criterion) keeps a = 1, falling back to the identity form exactly where the data
// cannot determine a map.
// ---------------------------------------------------------------------------------

/// DIRECTED, relation-annotated adjacency over the semantic row space: both views of
/// every directed edge (out and in), each slot carrying the folded weight (relation
/// weight × grade × symmetric degree normalization), the base relation id, and
/// whether the slot is the OUT view (row is the edge's source). The out view of each
/// edge appears exactly once — fits and Ψ walk it to count each directed edge once.
pub struct FunctionalEdges {
  /// Row `i`'s slots live at `[offsets[i]..offsets[i+1]]` (len n+1).
  pub offsets: Vec<u64>,
  pub targets: Vec<u32>,
  pub weights: Vec<f32>,
  /// Base relation id per slot (the EdgeType low byte, opaque here).
  pub relation: Vec<u8>,
  /// True where this slot is the edge's OUT view (row = source).
  pub outgoing: Vec<bool>,
}

impl FunctionalEdges {
  fn validate(&self, n: usize) -> Result<(), String> {
    if self.offsets.len() != n + 1 {
      return Err(format!(
        "functional edges: {} offsets for {n} rows (want n+1)",
        self.offsets.len()
      ));
    }
    if self.offsets.first() != Some(&0) {
      return Err("functional edges: offsets must start at 0".to_string());
    }
    for window in self.offsets.windows(2) {
      if window[1] < window[0] {
        return Err("functional edges: offsets not monotone".to_string());
      }
    }
    let total = *self.offsets.last().unwrap_or(&0) as usize;
    if self.targets.len() != total
      || self.weights.len() != total
      || self.relation.len() != total
      || self.outgoing.len() != total
    {
      return Err("functional edges: column lengths disagree with the offsets".to_string());
    }
    if self.targets.iter().any(|&t| t as usize >= n) {
      return Err("functional edges: target row out of range".to_string());
    }
    if self.weights.iter().any(|w| !w.is_finite() || *w < 0.0) {
      return Err("functional edges: weights must be finite and non-negative".to_string());
    }
    Ok(())
  }
}

/// Per-relation diagonal scales: index by relation id; `None` (or an id past the end)
/// means the identity map. Produced by [`fit_diagonal_maps`].
pub type DiagonalMaps = Vec<Option<Vec<f32>>>;

#[inline]
fn scale_of(scales: &DiagonalMaps, relation: u8, component: usize) -> f32 {
  scales
    .get(relation as usize)
    .and_then(|map| map.as_ref())
    .and_then(|map| map.get(component))
    .copied()
    .unwrap_or(1.0)
}

/// Closed-form diagonal fit: for each relation r and dimension c, the 1-D weighted
/// least squares aᵣc = Σ w·xᵢc·xⱼc / Σ w·xᵢc² over r's directed pairs (i → j, the
/// OUT view — each edge once). Deterministic (fixed 4096-row chunks folded in chunk
/// order). Dimensions the data cannot determine — denominator at or below the
/// relation's max-denominator × ε₃₂, the crate's dependence criterion — keep a = 1;
/// relations with no pairs (or no determined dimension) map to `None` = identity.
pub fn fit_diagonal_maps(
  anchors: &[f32],
  dim: usize,
  edges: &FunctionalEdges,
) -> Result<DiagonalMaps, String> {
  if dim == 0 {
    return Err("diagonal fit: zero dimension".to_string());
  }
  if anchors.len() % dim != 0 {
    return Err("diagonal fit: anchor matrix not row-shaped".to_string());
  }
  let n = anchors.len() / dim;
  edges.validate(n)?;
  const CHUNK_ROWS: usize = 4096;
  let chunk_count = n.div_ceil(CHUNK_ROWS).max(1);
  type Partial = Vec<Option<(Vec<f64>, Vec<f64>)>>; // per relation: (Σ w·xi·xj, Σ w·xi²)
  let partials: Vec<Partial> = (0..chunk_count)
    .into_par_iter()
    .map(|chunk| {
      let start = chunk * CHUNK_ROWS;
      let end = ((chunk + 1) * CHUNK_ROWS).min(n);
      let mut local: Partial = vec![None; 256];
      for row in start..end {
        let own = &anchors[row * dim..(row + 1) * dim];
        let (slot_start, slot_end) = (edges.offsets[row] as usize, edges.offsets[row + 1] as usize);
        for slot in slot_start..slot_end {
          if !edges.outgoing[slot] {
            continue;
          }
          let weight = edges.weights[slot] as f64;
          let neighbor = edges.targets[slot] as usize;
          let other = &anchors[neighbor * dim..(neighbor + 1) * dim];
          let entry = local[edges.relation[slot] as usize]
            .get_or_insert_with(|| (vec![0.0f64; dim], vec![0.0f64; dim]));
          for component in 0..dim {
            let source = own[component] as f64;
            entry.0[component] += weight * source * other[component] as f64;
            entry.1[component] += weight * source * source;
          }
        }
      }
      local
    })
    .collect();
  let mut numerator: Partial = vec![None; 256];
  for partial in partials {
    for (slot, local) in partial.into_iter().enumerate() {
      if let Some((num, den)) = local {
        let entry = numerator[slot].get_or_insert_with(|| (vec![0.0f64; dim], vec![0.0f64; dim]));
        for component in 0..dim {
          entry.0[component] += num[component];
          entry.1[component] += den[component];
        }
      }
    }
  }
  let mut maps: DiagonalMaps = vec![None; 256];
  for (slot, sums) in numerator.into_iter().enumerate() {
    let Some((num, den)) = sums else { continue };
    let den_max = den.iter().cloned().fold(0.0f64, f64::max);
    if den_max <= 0.0 {
      continue; // relation carried no determinable signal at all
    }
    let floor = den_max * f32::EPSILON as f64;
    let mut map = vec![1.0f32; dim];
    let mut determined = false;
    for component in 0..dim {
      if den[component] > floor {
        let scale = (num[component] / den[component]) as f32;
        if scale.is_finite() {
          map[component] = scale;
          determined = true;
        }
      }
    }
    if determined {
      maps[slot] = Some(map);
    }
  }
  Ok(maps)
}

/// Fixed-order Ψ for the functional objective, evaluated ONCE per directed edge (the
/// out view): Σᵢ‖xᵢ−qᵢ‖² + Σ_{i→j} w·‖a∘xᵢ − xⱼ‖². Same deterministic chunk shape as
/// the identity evaluator.
fn psi_functional(
  x: &[f32],
  anchors: &[f32],
  dim: usize,
  edges: &FunctionalEdges,
  scales: &DiagonalMaps,
) -> f64 {
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
          if !edges.outgoing[slot] {
            continue;
          }
          let weight = edges.weights[slot] as f64;
          let relation = edges.relation[slot];
          let neighbor = edges.targets[slot] as usize;
          let other = &x[neighbor * dim..(neighbor + 1) * dim];
          let mut distance = 0.0f64;
          for component in 0..dim {
            let mapped = scale_of(scales, relation, component) as f64 * row[component] as f64;
            let diff = mapped - other[component] as f64;
            distance += diff * diff;
          }
          total += weight * distance;
        }
      }
      total
    })
    .collect();
  partials.iter().sum()
}

/// One functional Jacobi sweep. Per component c the row minimizer is
/// xᵢc = (qᵢc + Σ_out w·a·xⱼc + Σ_in w·a·xⱼc) / (1 + Σ_out w·a² + Σ_in w) — the
/// numerator coefficient is w·a on BOTH views, the denominator addend is w·a² on the
/// out view (the map applies to the source) and w on the in view.
fn sweep_functional(
  x: &[f32],
  anchors: &[f32],
  dim: usize,
  edges: &FunctionalEdges,
  scales: &DiagonalMaps,
  next: &mut [f32],
) {
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
      for (component, slot_out) in row_out.iter_mut().enumerate() {
        let mut numerator = anchor[component] as f64;
        let mut denominator = 1.0f64;
        for slot in start..end {
          let weight = edges.weights[slot] as f64;
          let scale = scale_of(scales, edges.relation[slot], component) as f64;
          let neighbor = edges.targets[slot] as usize;
          numerator += weight * scale * x[neighbor * dim + component] as f64;
          denominator += if edges.outgoing[slot] {
            weight * scale * scale
          } else {
            weight
          };
        }
        *slot_out = (numerator / denominator) as f32;
      }
    });
}

/// The functional-form solver: identical contract, termination law, and Ψ-increase
/// error to [`retrofit_into`] — only the penalty differs (‖a_r∘xᵢ − xⱼ‖² per directed
/// edge). With every map at identity this reaches the same fixed point as the
/// identity form (the update algebra coincides at a ≡ 1).
pub fn retrofit_functional_into(
  anchors: &[f32],
  x: &mut [f32],
  next: &mut [f32],
  dim: usize,
  edges: &FunctionalEdges,
  scales: &DiagonalMaps,
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
  if scales
    .iter()
    .flatten()
    .any(|map| map.len() != dim || map.iter().any(|a| !a.is_finite()))
  {
    return Err("retrofit: diagonal maps must be dim-shaped and finite".to_string());
  }

  let initial_psi = psi_functional(x, anchors, dim, edges, scales);
  let floor = initial_psi * f32::EPSILON as f64;
  let mut previous_psi = initial_psi;
  let mut sweeps = 0usize;
  let (mut current, mut scratch) = (x, next);
  loop {
    sweep_functional(current, anchors, dim, edges, scales, scratch);
    std::mem::swap(&mut current, &mut scratch);
    sweeps += 1;
    let current_psi = psi_functional(current, anchors, dim, edges, scales);
    if current_psi > previous_psi {
      return Err(format!(
        "retrofit(functional): Ψ increased at sweep {sweeps} ({previous_psi} → {current_psi})"
      ));
    }
    if previous_psi - current_psi <= floor {
      if sweeps % 2 == 1 {
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

  /// Both views of each directed edge (src slot outgoing, dst slot incoming).
  fn directed(n: usize, pairs: &[(u32, u32, f32, u8)]) -> FunctionalEdges {
    let mut rows: Vec<Vec<(u32, f32, u8, bool)>> = vec![Vec::new(); n];
    for &(source, target, weight, relation) in pairs {
      rows[source as usize].push((target, weight, relation, true));
      rows[target as usize].push((source, weight, relation, false));
    }
    let mut edges = FunctionalEdges {
      offsets: vec![0],
      targets: Vec::new(),
      weights: Vec::new(),
      relation: Vec::new(),
      outgoing: Vec::new(),
    };
    for row in rows {
      for (target, weight, relation, out) in row {
        edges.targets.push(target);
        edges.weights.push(weight);
        edges.relation.push(relation);
        edges.outgoing.push(out);
      }
      edges.offsets.push(edges.targets.len() as u64);
    }
    edges
  }

  #[test]
  fn diagonal_fit_recovers_an_exact_map_and_falls_back_when_undetermined() {
    // Relation 3: xⱼ = (2·xᵢ₀, 0.5·xᵢ₁) exactly, power-of-two values so the 1-D
    // normal equations divide exactly. Relation 7: component 1 carries NO source
    // signal (xᵢ₁ = 0) — undetermined, must stay at the identity scale 1.
    let dim = 2;
    let anchors = [
      0.5f32, 1.0, /* 0 -> */ 1.0, 0.5, // 1 = (2·0.5, 0.5·1.0)
      1.0, 0.25, /* 2 -> */ 2.0, 0.125, // 3
      0.25, 0.5, /* 4 -> */ 0.5, 0.25, // 5
      1.0, 0.0, /* 6 -> */ 4.0, 0.75, // 7 (relation 7; component 1 source = 0)
    ];
    let edges = directed(
      8,
      &[(0, 1, 1.0, 3), (2, 3, 1.0, 3), (4, 5, 1.0, 3), (6, 7, 1.0, 7)],
    );
    let maps = fit_diagonal_maps(&anchors, dim, &edges).unwrap();
    let relation3 = maps[3].as_ref().expect("relation 3 fitted");
    assert_eq!(relation3[0], 2.0);
    assert_eq!(relation3[1], 0.5);
    let relation7 = maps[7].as_ref().expect("relation 7 fitted on component 0");
    assert_eq!(relation7[0], 4.0);
    assert_eq!(relation7[1], 1.0, "undetermined dimension keeps identity");
    assert!(maps[5].is_none(), "relation with no pairs = identity");
  }

  #[test]
  fn functional_two_node_system_reaches_the_closed_form() {
    // d=1, q=(1,0), one directed edge 0→1 with w=1 and a=2:
    // Ψ = (x₀−1)² + x₁² + (2x₀−x₁)². Stationarity: 5x₀−2x₁=1 and x₁=x₀ ⇒ x=(⅓,⅓).
    let edges = directed(2, &[(0, 1, 1.0, 0)]);
    let mut maps: DiagonalMaps = vec![None; 256];
    maps[0] = Some(vec![2.0]);
    let anchors = [1.0f32, 0.0];
    let mut x = anchors.to_vec();
    let mut next = vec![0.0f32; 2];
    let report = retrofit_functional_into(&anchors, &mut x, &mut next, 1, &edges, &maps).unwrap();
    // Same tolerance derivation as the identity test: gap ≤ ΔΨ/(1−ρ²) with ρ² = 0.4
    // here, curvature ≥ 1 ⇒ ‖x−x*‖ ≤ √(2·ε₃₂·Ψ₀).
    let tolerance = (2.0 * f32::EPSILON as f64 * report.initial_psi).sqrt();
    assert!((x[0] as f64 - 1.0 / 3.0).abs() < tolerance, "{x:?} tol {tolerance}");
    assert!((x[1] as f64 - 1.0 / 3.0).abs() < tolerance, "{x:?} tol {tolerance}");
    assert!(report.final_psi <= report.initial_psi);
  }

  #[test]
  fn unit_scales_reach_the_identity_fixed_point() {
    // With every map at identity the two forms share a fixed point; each solver stops
    // within its own ε-floor of it (their Ψ evaluators round differently, so sweep
    // counts may differ by one) — compare within the SUM of both derived bounds.
    let (anchors, edges) = random_case(120, 8, 41);
    let (identity_x, identity_report) = retrofit_in_ram(&anchors, 8, &edges).unwrap();
    // Rebuild the same topology as functional edges: each undirected slot pair was
    // symmetric, so mark the lower→higher direction as the out view once.
    let mut pairs = Vec::new();
    for row in 0..120u32 {
      let (start, end) = (edges.offsets[row as usize] as usize, edges.offsets[row as usize + 1] as usize);
      for slot in start..end {
        let target = edges.targets[slot];
        if row < target {
          pairs.push((row, target, edges.weights[slot], 0u8));
        }
      }
    }
    let functional = directed(120, &pairs);
    let maps: DiagonalMaps = vec![None; 256];
    let mut x = anchors.to_vec();
    let mut next = vec![0.0f32; anchors.len()];
    let functional_report =
      retrofit_functional_into(&anchors, &mut x, &mut next, 8, &functional, &maps).unwrap();
    let tolerance = (2.0 * f32::EPSILON as f64 * identity_report.initial_psi).sqrt()
      + (2.0 * f32::EPSILON as f64 * functional_report.initial_psi).sqrt();
    for (a, b) in identity_x.iter().zip(&x) {
      assert!(
        (*a as f64 - *b as f64).abs() <= tolerance,
        "{a} vs {b} (tol {tolerance})"
      );
    }
  }

  #[test]
  fn functional_descent_terminates_and_is_bit_deterministic() {
    let (anchors, _) = random_case(200, 8, 77);
    let mut state = 77u64;
    let mut next_random = move || {
      state ^= state << 13;
      state ^= state >> 7;
      state ^= state << 17;
      state
    };
    let mut pairs = Vec::new();
    for _ in 0..400 {
      let a = (next_random() % 200) as u32;
      let b = (next_random() % 200) as u32;
      if a != b {
        let w = ((next_random() % 1000) + 1) as f32 / 1000.0;
        pairs.push((a, b, w, (next_random() % 4) as u8));
      }
    }
    let edges = directed(200, &pairs);
    let maps = fit_diagonal_maps(&anchors, 8, &edges).unwrap();
    let run = || {
      let mut x = anchors.to_vec();
      let mut next = vec![0.0f32; anchors.len()];
      let report =
        retrofit_functional_into(&anchors, &mut x, &mut next, 8, &edges, &maps).unwrap();
      (x, report)
    };
    let (x1, r1) = run();
    let (x2, r2) = run();
    assert!(r1.final_psi <= r1.initial_psi);
    assert_eq!(r1.sweeps, r2.sweeps);
    assert_eq!(
      x1.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
      x2.iter().map(|v| v.to_bits()).collect::<Vec<_>>()
    );
  }
}
