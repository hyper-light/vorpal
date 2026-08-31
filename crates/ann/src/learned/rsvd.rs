//! Deterministic randomized SVD of a sparse SYMMETRIC matrix (the PPMI matrix): the
//! Halko–Martinsson–Tropp randomized range finder (SIAM Rev. 2011, arXiv:0909.4061)
//! with power iterations, a modified-Gram-Schmidt QR, and an exact cyclic-Jacobi
//! eigensolver on the small projected matrix. For a symmetric matrix the SVD is the
//! eigendecomposition up to signs: singular values are |λ|, so the top-d factors are
//! the top-d eigenpairs by |λ|.
//!
//! Determinism is structural, not hoped for (the design doc's "MKL-class pitfall made
//! structurally impossible"):
//! - the Gaussian test matrix comes from a seeded xorshift + Box–Muller — no ambient
//!   randomness;
//! - every parallel reduction is FIXED-ORDER: dot products and matrix applications sum
//!   serially within fixed 4096-element blocks and then serially across block results,
//!   so the float rounding tree is a pure function of the data at any thread count;
//! - the Jacobi sweeps visit (p, q) in row-cyclic order and terminate on a
//!   machine-epsilon criterion (no iteration-count tunable): stop when the off-diagonal
//!   mass no longer strictly decreases or falls under `ε · initial mass`.
//!
//! Errors, never panics: non-finite inputs, degenerate shapes, and non-convergence all
//! return typed errors — the caller states the lexical fallback.

use rayon::prelude::*;
use vorpal_mem::{AccessPattern, ScratchMmap};

use crate::Rng;

/// Where the range finder keeps its two n×k working blocks. Training always passes
/// `Scratch` (kernel-scale blocks are ~2.8 GB EACH; the pager carries them and
/// anonymous RSS stays bounded — small corpora's files never leave page cache and are
/// deleted on success, so one code path serves every scale). `InRam` serves tests and
/// small ad-hoc callers; the two are bit-identical, pinned by the workspace oracle.
pub enum FactorWorkspace<'a> {
  InRam,
  Scratch { dir: &'a std::path::Path },
}

/// One n×k f32 working block over either backing, with a logical length (scratch
/// files never shrink; truncation is bookkeeping).
struct FactorBlock {
  storage: FactorStorage,
  len: usize,
}

enum FactorStorage {
  Ram(Vec<f32>),
  Scratch(ScratchMmap),
}

impl FactorBlock {
  fn new(len: usize, workspace: &FactorWorkspace, index: usize) -> Result<Self, String> {
    let storage = match workspace {
      FactorWorkspace::InRam => FactorStorage::Ram(vec![0.0f32; len]),
      FactorWorkspace::Scratch { dir } => {
        let path = dir.join(format!("rsvd-block-{index}.scratch"));
        let scratch =
          ScratchMmap::create(&path, (len * std::mem::size_of::<f32>()).max(1), AccessPattern::Random)
            .map_err(|e| format!("creating factor scratch {}: {e}", path.display()))?;
        FactorStorage::Scratch(scratch)
      }
    };
    Ok(FactorBlock { storage, len })
  }

  fn as_slice(&self) -> &[f32] {
    match &self.storage {
      FactorStorage::Ram(vec) => &vec[..self.len],
      FactorStorage::Scratch(scratch) => {
        &bytemuck::cast_slice(scratch.as_bytes())[..self.len]
      }
    }
  }

  fn as_mut_slice(&mut self) -> &mut [f32] {
    let len = self.len;
    match &mut self.storage {
      FactorStorage::Ram(vec) => &mut vec[..len],
      FactorStorage::Scratch(scratch) => {
        &mut bytemuck::cast_slice_mut(scratch.as_mut_bytes())[..len]
      }
    }
  }

  fn truncate(&mut self, len: usize) {
    if len < self.len {
      self.len = len;
      if let FactorStorage::Ram(vec) = &mut self.storage {
        vec.truncate(len);
      }
    }
  }

  /// Success-path cleanup: scratch files are removed; RAM just drops. (Error paths
  /// leave scratch files to the owner's start-up sweep — the scratch lifecycle law.)
  fn delete(self) -> Result<(), String> {
    match self.storage {
      FactorStorage::Ram(_) => Ok(()),
      FactorStorage::Scratch(scratch) => scratch
        .delete()
        .map_err(|e| format!("removing factor scratch: {e}")),
    }
  }
}

/// Fixed reduction block: partial sums are computed inside blocks of this many elements
/// and combined serially across blocks. The VALUE only shapes the rounding tree (any
/// fixed value is equally deterministic); 4096 keeps blocks cache-resident.
const REDUCTION_BLOCK: usize = 4096;

/// HMT oversampling: the range finder samples `d + OVERSAMPLE` directions. Cited
/// constant — "an oversampling parameter of 5 or 10 is sufficient" (HMT 2011, §1.4).
pub const OVERSAMPLE: usize = 10;

/// HMT power iterations: q = 2 subspace iterations sharpen the spectrum enough for
/// slowly-decaying spectra (HMT 2011, Algorithm 4.4 discussion; q ∈ {1, 2} standard).
pub const POWER_ITERATIONS: usize = 2;

/// A sparse symmetric matrix in CSR form, materialized with BOTH triangles so row
/// application is one contiguous pass. Built from upper-triangle triples.
pub struct SymmetricCsr {
  n: usize,
  row_starts: Vec<usize>,
  columns: Vec<u32>,
  values: Vec<f32>,
}

impl SymmetricCsr {
  /// Build from a RE-STREAMABLE ascending upper-triangle source `(i, j, value)` with
  /// `i ≤ j`: pass 1 sizes every row, pass 2 fills — nothing materializes beyond the
  /// CSR itself, which is what lets kernel/Meta-scale PPMI streams (external counts →
  /// [`crate::learned::PpmiStream`]) build without a triples vector. The closure is
  /// invoked twice and must yield the identical sequence (the spill merge is
  /// deterministic; the equality oracle pins it). Duplicates and non-finite values are
  /// rejected.
  pub fn from_pair_stream<F, I>(n: usize, stream: F) -> Result<Self, String>
  where
    F: Fn() -> Result<I, String>,
    I: Iterator<Item = Result<(u32, u32, f64), String>>,
  {
    let mut counts = vec![0usize; n];
    for item in stream()? {
      let (i, j, value) = item?;
      if (i as usize) >= n || (j as usize) >= n || i > j {
        return Err(format!("triple ({i}, {j}) out of range for n = {n}"));
      }
      if !value.is_finite() {
        return Err(format!("non-finite matrix value at ({i}, {j})"));
      }
      counts[i as usize] += 1;
      if i != j {
        counts[j as usize] += 1;
      }
    }
    let mut row_starts = vec![0usize; n + 1];
    for row in 0..n {
      row_starts[row + 1] = row_starts[row] + counts[row];
    }
    let nnz = row_starts[n];
    let mut columns = vec![0u32; nnz];
    let mut values = vec![0.0f32; nnz];
    let mut cursor = row_starts.clone();
    // Pairs arrive sorted by (i, j); inserting the mirrored entries through per-row
    // cursors keeps every row's column list sorted (verified below).
    for item in stream()? {
      let (i, j, value) = item?;
      let slot = *cursor
        .get(i as usize)
        .ok_or_else(|| format!("row {i} out of range on the second pass"))?;
      if slot >= nnz && nnz > 0 {
        return Err("second stream pass yielded more entries than the first".to_string());
      }
      columns[slot] = j;
      values[slot] = value as f32;
      cursor[i as usize] += 1;
      if i != j {
        let slot = cursor[j as usize];
        columns[slot] = i;
        values[slot] = value as f32;
        cursor[j as usize] += 1;
      }
    }
    for row in 0..n {
      if cursor[row] != row_starts[row + 1] {
        return Err(format!(
          "second stream pass disagreed with the first at row {row} (re-stream not identical)"
        ));
      }
      let slice = &columns[row_starts[row]..cursor[row]];
      if slice.windows(2).any(|w| w[0] >= w[1]) {
        return Err(format!(
          "row {row} columns not strictly increasing — duplicate or unsorted triples"
        ));
      }
    }
    Ok(Self {
      n,
      row_starts,
      columns,
      values,
    })
  }

  /// [`SymmetricCsr::from_pair_stream`] over a materialized triple slice — the
  /// in-memory reference path, same construction code.
  pub fn from_upper_triples(n: usize, triples: &[(u32, u32, f64)]) -> Result<Self, String> {
    Self::from_pair_stream(n, || Ok(triples.iter().map(|&t| Ok(t))))
  }

  pub fn n(&self) -> usize {
    self.n
  }

  /// Stored entries (both triangles).
  pub fn nnz(&self) -> usize {
    self.row_starts.last().copied().unwrap_or(0)
  }

  /// `out = M · x_block` for `k` interleaved column vectors stored row-major
  /// (`x[row * k + column]`). Parallel over output rows; each output element is a
  /// serial sum over the row's nonzeros in CSR order — fixed-order by construction.
  fn apply(&self, x: &[f32], k: usize, out: &mut [f32]) {
    debug_assert_eq!(x.len(), self.n * k);
    debug_assert_eq!(out.len(), self.n * k);
    out
      .par_chunks_mut(k)
      .enumerate()
      .for_each(|(row, out_row)| {
        let mut acc = vec![0.0f64; k];
        let start = self.row_starts[row];
        let end = self.row_starts[row + 1];
        for slot in start..end {
          let column = self.columns[slot] as usize;
          let value = self.values[slot] as f64;
          let x_row = &x[column * k..column * k + k];
          for (a, &xv) in acc.iter_mut().zip(x_row) {
            *a += value * xv as f64;
          }
        }
        for (o, a) in out_row.iter_mut().zip(&acc) {
          *o = *a as f32;
        }
      });
  }
}

/// Fixed-order parallel dot product: the explicit-SIMD wide-dot kernel inside fixed
/// blocks (one rounding tree on every architecture — see `crate::kernels`), block
/// partials combined serially — the same bits at any thread count on any machine.
fn det_dot(a: &[f32], b: &[f32]) -> f64 {
  let partials: Vec<f64> = a
    .par_chunks(REDUCTION_BLOCK)
    .zip(b.par_chunks(REDUCTION_BLOCK))
    .map(|(xa, xb)| crate::kernels::dot_wide(xa, xb))
    .collect();
  partials.iter().sum()
}

/// Column view helpers over a row-major n×k block.
fn column_copy(block: &[f32], k: usize, column: usize, out: &mut [f32]) {
  for (row, slot) in out.iter_mut().enumerate() {
    *slot = block[row * k + column];
  }
}

fn column_store(block: &mut [f32], k: usize, column: usize, data: &[f32]) {
  for (row, &value) in data.iter().enumerate() {
    block[row * k + column] = value;
  }
}

/// Modified Gram–Schmidt orthonormalization of the k columns of a row-major n×k block,
/// in place, RANK-REVEALING: a dependent column (residual below f32 representability
/// relative to that column's own pre-projection norm — the storage precision defines
/// what counts as signal) is dropped and survivors compact left, still at stride k.
/// Returns the independent-column count. A rank-deficient matrix thereby yields its
/// ENTIRE numerical range exactly — HMT's range-finder semantics; small corpora are
/// routinely rank-deficient and must factor, never error. Zero surviving columns (the
/// zero matrix) is the only failure. Deterministic: columns in order, `det_dot` trees.
fn orthonormalize(block: &mut [f32], n: usize, k: usize) -> Result<usize, String> {
  let mut current = vec![0.0f32; n];
  let mut earlier = vec![0.0f32; n];
  let mut kept = 0usize;
  for column in 0..k {
    column_copy(block, k, column, &mut current);
    let original = det_dot(&current, &current).sqrt();
    for prior in 0..kept {
      column_copy(block, k, prior, &mut earlier);
      let projection = det_dot(&current, &earlier);
      current
        .par_chunks_mut(REDUCTION_BLOCK)
        .zip(earlier.par_chunks(REDUCTION_BLOCK))
        .for_each(|(cur, ear)| {
          for (c, e) in cur.iter_mut().zip(ear) {
            *c -= (projection * *e as f64) as f32;
          }
        });
    }
    let norm = det_dot(&current, &current).sqrt();
    if !norm.is_finite() || !original.is_finite() {
      return Err(format!("non-finite column {column} during orthonormalization"));
    }
    if norm == 0.0 || norm <= original * f32::EPSILON as f64 {
      continue; // dependent direction — drop it, keep scanning the rest
    }
    let inverse = (1.0 / norm) as f32;
    for value in current.iter_mut() {
      *value *= inverse;
    }
    column_store(block, k, kept, &current);
    kept += 1;
  }
  if kept == 0 {
    return Err("orthonormalization found no independent directions (zero matrix)".to_string());
  }
  Ok(kept)
}

/// [`orthonormalize`], then repack the surviving columns from stride `width` down to a
/// dense row-major n×kept block (truncating the buffer). The forward row walk never
/// clobbers an unread source (`kept ≤ width` ⇒ write index < read index for every
/// later row).
fn shrink_to_rank(block: &mut FactorBlock, n: usize, width: usize) -> Result<usize, String> {
  let kept = orthonormalize(block.as_mut_slice(), n, width)?;
  if kept < width {
    let slice = block.as_mut_slice();
    for row in 1..n {
      for column in 0..kept {
        slice[row * kept + column] = slice[row * width + column];
      }
    }
    block.truncate(n * kept);
  }
  Ok(kept)
}

/// Exact eigendecomposition of a small dense symmetric k×k matrix by cyclic Jacobi
/// rotations (row-cyclic (p, q) order — deterministic). Returns (eigenvalues, row-major
/// k×k eigenvector matrix V with columns as eigenvectors), unsorted. Terminates when
/// the off-diagonal mass stops strictly decreasing or falls below ε · initial mass —
/// a machine-precision criterion, not an iteration-count tunable.
pub(crate) fn jacobi_eigen(matrix: &mut [f64], k: usize) -> Result<(Vec<f64>, Vec<f64>), String> {
  let mut vectors = vec![0.0f64; k * k];
  for i in 0..k {
    vectors[i * k + i] = 1.0;
  }
  let off = |m: &[f64]| -> f64 {
    let mut sum = 0.0;
    for p in 0..k {
      for q in (p + 1)..k {
        sum += m[p * k + q] * m[p * k + q];
      }
    }
    sum
  };
  let initial = off(matrix);
  if !initial.is_finite() {
    return Err("non-finite projected matrix".to_string());
  }
  let mut previous = f64::INFINITY;
  loop {
    let current = off(matrix);
    if current <= initial * f64::EPSILON || current >= previous {
      break; // converged to machine precision, or no further progress is possible
    }
    previous = current;
    for p in 0..k {
      for q in (p + 1)..k {
        let apq = matrix[p * k + q];
        if apq == 0.0 {
          continue;
        }
        let app = matrix[p * k + p];
        let aqq = matrix[q * k + q];
        let theta = (aqq - app) / (2.0 * apq);
        let t = if theta >= 0.0 {
          1.0 / (theta + (1.0 + theta * theta).sqrt())
        } else {
          1.0 / (theta - (1.0 + theta * theta).sqrt())
        };
        let c = 1.0 / (1.0 + t * t).sqrt();
        let s = t * c;
        for i in 0..k {
          let aip = matrix[i * k + p];
          let aiq = matrix[i * k + q];
          matrix[i * k + p] = c * aip - s * aiq;
          matrix[i * k + q] = s * aip + c * aiq;
        }
        for j in 0..k {
          let apj = matrix[p * k + j];
          let aqj = matrix[q * k + j];
          matrix[p * k + j] = c * apj - s * aqj;
          matrix[q * k + j] = s * apj + c * aqj;
        }
        for i in 0..k {
          let vip = vectors[i * k + p];
          let viq = vectors[i * k + q];
          vectors[i * k + p] = c * vip - s * viq;
          vectors[i * k + q] = s * vip + c * viq;
        }
      }
    }
  }
  let eigenvalues: Vec<f64> = (0..k).map(|i| matrix[i * k + i]).collect();
  if eigenvalues.iter().any(|v| !v.is_finite()) {
    return Err("non-finite eigenvalues".to_string());
  }
  Ok((eigenvalues, vectors))
}

/// The top-`d` symmetric factorization result: `factors` is row-major n×d, the rows
/// already weighted by |λ|^p for the caller's chosen symmetric eigenvalue exponent —
/// weighting is the CALLER's step (it owns the TACL p knob); here we return raw
/// eigenpairs.
pub struct TopEigen {
  /// Top eigenvalues by |λ|, descending — `min(d, numerical rank)` of them: a
  /// rank-deficient matrix returns its whole range, exactly.
  pub eigenvalues: Vec<f64>,
  /// Row-major n×`eigenvalues.len()`: column j is the eigenvector of `eigenvalues[j]`.
  /// The STRIDE is `eigenvalues.len()`, never the requested d.
  pub vectors: Vec<f32>,
}

/// Randomized top-`d` eigenpairs of a sparse symmetric matrix (HMT 2011: range finder
/// with `OVERSAMPLE` extra directions and `POWER_ITERATIONS` subspace iterations, then
/// an exact Jacobi solve of the projected matrix). Deterministic for a fixed `seed` at
/// any thread count.
pub fn top_symmetric_eigen(
  matrix: &SymmetricCsr,
  d: usize,
  seed: u64,
  workspace: FactorWorkspace,
) -> Result<TopEigen, String> {
  let n = matrix.n();
  if d == 0 {
    return Err("requested 0 factors".to_string());
  }
  let k = (d + OVERSAMPLE).min(n);
  if k == 0 {
    return Err("empty matrix".to_string());
  }

  // Seeded Gaussian test block via Box–Muller over xorshift uniforms.
  let mut rng = Rng::new(seed);
  let mut gaussian = || -> f64 {
    // Uniforms in (0, 1]: (next() >> 11) spans [0, 2^53); +1 avoids ln(0).
    let u1 = ((rng.next() >> 11) + 1) as f64 / (1u64 << 53) as f64;
    let u2 = (rng.next() >> 11) as f64 / (1u64 << 53) as f64;
    (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()
  };
  let mut block = FactorBlock::new(n * k, &workspace, 0)?;
  for value in block.as_mut_slice().iter_mut() {
    *value = gaussian() as f32;
  }

  // Range finder: Y = M·Ω, with q power iterations (orthonormalize between products —
  // HMT's numerically stable variant). Each orthonormalization is rank-revealing: a
  // rank-deficient matrix shrinks the working width to its numerical rank and the
  // factorization proceeds over exactly the range that exists.
  let mut width = k;
  let mut scratch = FactorBlock::new(n * width, &workspace, 1)?;
  matrix.apply(block.as_slice(), width, scratch.as_mut_slice());
  std::mem::swap(&mut block, &mut scratch);
  for _ in 0..POWER_ITERATIONS {
    width = shrink_to_rank(&mut block, n, width)?;
    scratch.truncate(n * width);
    matrix.apply(block.as_slice(), width, scratch.as_mut_slice());
    std::mem::swap(&mut block, &mut scratch);
  }
  width = shrink_to_rank(&mut block, n, width)?;
  let k = width; // Q: row-major n×k, orthonormal columns (k = numerical rank if less)
  scratch.truncate(n * k);

  // Projected small matrix B = Qᵀ (M Q): k×k symmetric (up to rounding — symmetrize).
  matrix.apply(block.as_slice(), k, scratch.as_mut_slice()); // scratch = M·Q
  let mut projected = vec![0.0f64; k * k];
  let mut q_column = vec![0.0f32; n];
  let mut mq_column = vec![0.0f32; n];
  for i in 0..k {
    column_copy(block.as_slice(), k, i, &mut q_column);
    for j in 0..k {
      column_copy(scratch.as_slice(), k, j, &mut mq_column);
      projected[i * k + j] = det_dot(&q_column, &mq_column);
    }
  }
  for i in 0..k {
    for j in (i + 1)..k {
      let mean = 0.5 * (projected[i * k + j] + projected[j * k + i]);
      projected[i * k + j] = mean;
      projected[j * k + i] = mean;
    }
  }

  let (eigenvalues, small_vectors) = jacobi_eigen(&mut projected, k)?;

  // Order by |λ| descending (singular order for a symmetric matrix), ties by index for
  // determinism; keep d.
  let mut order: Vec<usize> = (0..k).collect();
  order.sort_by(|&a, &b| {
    eigenvalues[b]
      .abs()
      .total_cmp(&eigenvalues[a].abs())
      .then(a.cmp(&b))
  });
  order.truncate(d.min(k));

  // Lift: U = Q · V_d (row-major n×d), fixed-order accumulation per output element.
  let d_kept = order.len();
  let mut vectors = vec![0.0f32; n * d_kept];
  {
    let block_slice = block.as_slice();
    vectors
      .par_chunks_mut(d_kept)
      .enumerate()
      .for_each(|(row, out_row)| {
        let q_row = &block_slice[row * k..row * k + k];
        for (slot, &source) in out_row.iter_mut().zip(&order) {
          let mut acc = 0.0f64;
          for (i, &q) in q_row.iter().enumerate() {
            acc += q as f64 * small_vectors[i * k + source];
          }
          *slot = acc as f32;
        }
      });
  }
  block.delete()?;
  scratch.delete()?;
  let eigenvalues: Vec<f64> = order.iter().map(|&i| eigenvalues[i]).collect();
  if vectors.iter().any(|v| !v.is_finite()) {
    return Err("non-finite factor entries".to_string());
  }
  Ok(TopEigen {
    eigenvalues,
    vectors,
  })
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Dense reference: full Jacobi on the dense symmetric matrix.
  fn dense_reference(dense: &[f64], n: usize) -> (Vec<f64>, Vec<f64>) {
    let mut m = dense.to_vec();
    let (values, vectors) = jacobi_eigen(&mut m, n).unwrap();
    (values, vectors)
  }

  fn toy_matrix(n: usize, seed: u64) -> (SymmetricCsr, Vec<f64>) {
    let mut rng = Rng::new(seed);
    let mut dense = vec![0.0f64; n * n];
    let mut triples = Vec::new();
    for i in 0..n {
      for j in i..n {
        // Sparse-ish PPMI shape: nonnegative, ~40% dense.
        if rng.below(5) < 2 {
          let value = (rng.below(1000) as f64) / 250.0;
          dense[i * n + j] = value;
          dense[j * n + i] = value;
          triples.push((i as u32, j as u32, value));
        }
      }
    }
    (
      SymmetricCsr::from_upper_triples(n, &triples).unwrap(),
      dense,
    )
  }

  /// A symmetric matrix with a PRESCRIBED decaying spectrum: M = Σ λᵢ vᵢvᵢᵀ over a
  /// seeded random orthonormal basis. The honest RSVD oracle fixture — HMT's guarantees
  /// are gap-dependent, so the test must control the gap (a flat random-matrix spectrum
  /// tests nothing HMT promises).
  fn prescribed_spectrum(n: usize, spectrum: &[f64], seed: u64) -> (SymmetricCsr, Vec<f64>) {
    let mut rng = Rng::new(seed);
    let mut gaussian = || -> f64 {
      let u1 = ((rng.next() >> 11) + 1) as f64 / (1u64 << 53) as f64;
      let u2 = (rng.next() >> 11) as f64 / (1u64 << 53) as f64;
      (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()
    };
    // Random n×n, orthonormalized column-by-column in f64 (test-local MGS).
    let k = spectrum.len();
    let mut basis = vec![vec![0.0f64; n]; k];
    for column in basis.iter_mut() {
      for value in column.iter_mut() {
        *value = gaussian();
      }
    }
    for c in 0..k {
      for p in 0..c {
        let dot: f64 = basis[c].iter().zip(&basis[p]).map(|(a, b)| a * b).sum();
        let prior = basis[p].clone();
        for (v, q) in basis[c].iter_mut().zip(&prior) {
          *v -= dot * q;
        }
      }
      let norm: f64 = basis[c].iter().map(|v| v * v).sum::<f64>().sqrt();
      for v in basis[c].iter_mut() {
        *v /= norm;
      }
    }
    let mut dense = vec![0.0f64; n * n];
    for (lambda, column) in spectrum.iter().zip(&basis) {
      for i in 0..n {
        for j in 0..n {
          dense[i * n + j] += lambda * column[i] * column[j];
        }
      }
    }
    let mut triples = Vec::new();
    for i in 0..n {
      for j in i..n {
        triples.push((i as u32, j as u32, dense[i * n + j]));
      }
    }
    (
      SymmetricCsr::from_upper_triples(n, &triples).unwrap(),
      dense,
    )
  }

  #[test]
  fn recovers_the_dominant_subspace_of_a_gapped_matrix() {
    // Six dominant factors, then a tail 125× below the last kept one — a real spectral
    // gap, the regime HMT quantifies. λ₁ = 40.
    let n = 40;
    let d = 6;
    let mut spectrum = vec![40.0, 20.0, 10.0, 5.0, 2.5, 1.25];
    for i in 0..30 {
      spectrum.push(0.01 * 0.9f64.powi(i));
    }
    let (sparse, dense) = prescribed_spectrum(n, &spectrum, 7);
    let got = top_symmetric_eigen(&sparse, d, 42, FactorWorkspace::InRam).unwrap();

    let (ref_values, ref_vectors) = dense_reference(&dense, n);
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by(|&a, &b| ref_values[b].abs().total_cmp(&ref_values[a].abs()));

    // Eigenvalue tolerance DERIVED from f32 intermediate storage: each stored block
    // entry carries ≤ ε₃₂ relative noise; a Rayleigh quotient over n×k stored entries
    // perturbs eigenvalues by ≲ λ₁ · ε₃₂ · √(n·k) ≈ 40 · 1.19e-7 · 25 ≈ 1.2e-4.
    let k = d + OVERSAMPLE;
    let tolerance = spectrum[0] * f32::EPSILON as f64 * ((n * k) as f64).sqrt();
    for (slot, &reference_index) in order.iter().take(d).enumerate() {
      let want = ref_values[reference_index];
      let have = got.eigenvalues[slot];
      assert!(
        (want - have).abs() <= tolerance,
        "eigenvalue {slot}: want {want}, have {have} (tolerance {tolerance})"
      );
    }

    // Subspace angle: every recovered column must lie in the reference span — the
    // residual after projecting onto the top-d reference vectors is ≈ 0 (the design
    // doc's ≤ 1e-5 bound, with slack only for f32 storage).
    for column in 0..d {
      let recovered: Vec<f64> = (0..n)
        .map(|row| got.vectors[row * d + column] as f64)
        .collect();
      let mut residual: f64 = recovered.iter().map(|v| v * v).sum();
      for &reference_index in order.iter().take(d) {
        let mut dot = 0.0f64;
        for row in 0..n {
          dot += recovered[row] * ref_vectors[row * n + reference_index];
        }
        residual -= dot * dot;
      }
      assert!(
        residual.abs() <= 1e-5,
        "column {column} leaks {residual} outside the reference subspace"
      );
    }
  }

  #[test]
  fn rank_deficient_matrices_factor_to_their_rank() {
    // Rank 5 by construction at n = 60, d = 20 requested: the factorization must
    // return the (numerical) range — top-5 matching the dense reference — never an
    // error. f32 storage of the CSR reintroduces noise-scale tail directions; the
    // power iterations crush them below f32 representability, and anything that
    // survives must sit at the storage-noise scale.
    let n = 60;
    let spectrum = vec![30.0, 14.0, 7.0, 3.0, 1.5];
    let (sparse, dense) = prescribed_spectrum(n, &spectrum, 21);
    let got = top_symmetric_eigen(&sparse, 20, 5, FactorWorkspace::InRam).unwrap();
    assert!(
      got.eigenvalues.len() >= spectrum.len(),
      "lost real rank: {:?}",
      got.eigenvalues
    );
    let (ref_values, _) = dense_reference(&dense, n);
    let mut sorted: Vec<f64> = ref_values.iter().map(|v| v.abs()).collect();
    sorted.sort_by(|a, b| b.total_cmp(a));
    let tolerance = spectrum[0] * f32::EPSILON as f64 * ((n * 20) as f64).sqrt();
    for (i, lambda) in spectrum.iter().enumerate() {
      assert!(
        (got.eigenvalues[i].abs() - lambda).abs() <= tolerance,
        "factor {i}: want {lambda}, have {} (tolerance {tolerance})",
        got.eigenvalues[i]
      );
    }
    for extra in got.eigenvalues.iter().skip(spectrum.len()) {
      assert!(
        extra.abs() <= tolerance,
        "phantom factor above storage noise: {extra} (tolerance {tolerance})"
      );
    }
  }

  #[test]
  fn factors_are_orthonormal() {
    let (sparse, _) = toy_matrix(60, 11);
    let d = 8;
    let got = top_symmetric_eigen(&sparse, d, 9, FactorWorkspace::InRam).unwrap();
    for a in 0..d {
      for b in a..d {
        let mut dot = 0.0f64;
        for row in 0..60 {
          dot += got.vectors[row * d + a] as f64 * got.vectors[row * d + b] as f64;
        }
        let want = if a == b { 1.0 } else { 0.0 };
        assert!(
          (dot - want).abs() < 1e-5,
          "QᵀQ[{a},{b}] = {dot}, want {want}"
        );
      }
    }
  }

  #[test]
  fn bit_identical_across_runs() {
    let (sparse, _) = toy_matrix(50, 3);
    let first = top_symmetric_eigen(&sparse, 5, 1234, FactorWorkspace::InRam).unwrap();
    let second = top_symmetric_eigen(&sparse, 5, 1234, FactorWorkspace::InRam).unwrap();
    assert_eq!(
      first.vectors.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
      second.vectors.iter().map(|v| v.to_bits()).collect::<Vec<_>>()
    );
    let third = top_symmetric_eigen(&sparse, 5, 99, FactorWorkspace::InRam).unwrap();
    assert_ne!(
      first.vectors.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
      third.vectors.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
      "different seeds must explore different test spaces"
    );
  }

  #[test]
  fn scratch_workspace_is_bit_identical_to_ram_and_cleans_up() {
    let (sparse, _) = toy_matrix(50, 3);
    let ram = top_symmetric_eigen(&sparse, 5, 1234, FactorWorkspace::InRam).unwrap();
    let dir = std::env::temp_dir().join(format!("vorpal-rsvd-scratch-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let scratch =
      top_symmetric_eigen(&sparse, 5, 1234, FactorWorkspace::Scratch { dir: &dir }).unwrap();
    assert_eq!(
      ram.eigenvalues.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
      scratch.eigenvalues.iter().map(|v| v.to_bits()).collect::<Vec<_>>()
    );
    assert_eq!(
      ram.vectors.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
      scratch.vectors.iter().map(|v| v.to_bits()).collect::<Vec<_>>()
    );
    // The success path removes both block files.
    assert!(
      std::fs::read_dir(&dir).unwrap().next().is_none(),
      "factor scratch must be deleted on success"
    );
    let _ = std::fs::remove_dir_all(&dir);
  }

  #[test]
  fn degenerate_inputs_are_typed_errors() {
    assert!(SymmetricCsr::from_upper_triples(4, &[(3, 1, 1.0)]).is_err()); // lower triple
    assert!(SymmetricCsr::from_upper_triples(2, &[(0, 1, f64::NAN)]).is_err());
    let empty = SymmetricCsr::from_upper_triples(0, &[]).unwrap();
    assert!(top_symmetric_eigen(&empty, 4, 1, FactorWorkspace::InRam).is_err());
  }
}
