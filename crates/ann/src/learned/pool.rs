//! Post-processing and pooling, exactly per the cited papers:
//!
//! * **ABTT** — "All-but-the-Top" (Mu & Viswanath, arXiv:1702.01417): word vectors are
//!   centered and their projections on the top ⌈d/100⌉ principal components removed —
//!   those directions encode frequency, not meaning (≈ +4% STS in the paper).
//! * **uSIF** — Ethayarajh, ACL-RepL4NLP 2018 (W18-3012), Algorithm 1 verbatim: with
//!   vocabulary V, unigram probabilities p(w), and average document length n,
//!   α = |{w ∈ V : p(w) > 1 − (1 − 1/|V|)ⁿ}| / |V|, Z = |V| / 2, a = (1 − α) / (α · Z);
//!   a document embeds as (1/|s|) Σ_w a/(p(w) + ½a) · v_w over UNIT-NORMALIZED word
//!   vectors ("normalize the word vectors, take a weighted average" — the paper's own
//!   §1 description of the estimator), followed by PIECEWISE common-component removal:
//!   the top m = 5 singular vectors c'_i of the document-embedding matrix (m fixed by
//!   the paper), each subtracted with weight λ_i = σ_i² / Σ_j≤m σ_j².
//!
//! Both PC computations reduce to exact Jacobi eigendecompositions of d×d Gram
//! matrices, accumulated in f64 in fixed order — no randomized step, bit-deterministic.

use rayon::prelude::*;

use super::rsvd::jacobi_eigen;

/// uSIF's m: the number of common components removed piecewise — fixed at 5 by the
/// paper ("We fix m at 5", W18-3012 §3.3).
pub const USIF_COMPONENTS: usize = 5;

/// ABTT's component count for dimension d: ⌈d/100⌉ — the paper's rule of thumb
/// ("d/100", arXiv:1702.01417 §3).
pub fn abtt_component_count(d: usize) -> usize {
  d.div_ceil(100)
}

/// The top-`m` eigenpairs of a d×d Gram matrix (f64, row-major), by exact cyclic
/// Jacobi: returns (eigenvalues desc by |λ|, row-major m×d component rows, unit-norm).
fn top_gram_components(gram: &[f64], d: usize, m: usize) -> Result<(Vec<f64>, Vec<Vec<f32>>), String> {
  if gram.len() != d * d {
    return Err(format!("gram matrix shape {} ≠ {d}×{d}", gram.len()));
  }
  let mut work = gram.to_vec();
  let (values, vectors) = jacobi_eigen(&mut work, d)?;
  let mut order: Vec<usize> = (0..d).collect();
  order.sort_by(|&a, &b| values[b].abs().total_cmp(&values[a].abs()).then(a.cmp(&b)));
  order.truncate(m.min(d));
  let mut components = Vec::with_capacity(order.len());
  let mut eigenvalues = Vec::with_capacity(order.len());
  for &index in &order {
    let mut component: Vec<f32> = (0..d).map(|row| vectors[row * d + index] as f32).collect();
    // Deterministic sign convention: first nonzero coordinate positive (eigenvectors
    // are sign-ambiguous; projections are sign-invariant, but persisted bytes must be
    // canonical).
    if let Some(first) = component.iter().find(|v| **v != 0.0)
      && *first < 0.0
    {
      for value in component.iter_mut() {
        *value = -*value;
      }
    }
    components.push(component);
    eigenvalues.push(values[index]);
  }
  Ok((eigenvalues, components))
}

/// All-but-the-Top post-processing, fit on the word matrix (row-major n×d): stores the
/// mean and the top ⌈d/100⌉ principal components of the centered matrix.
pub struct Abtt {
  pub mean: Vec<f32>,
  /// Row-major: one unit component per row.
  pub components: Vec<Vec<f32>>,
}

impl Abtt {
  /// Fit on `rows` (row-major n×d). Gram accumulation is chunked and combined in fixed
  /// order (bit-deterministic at any thread count).
  pub fn fit(rows: &[f32], n: usize, d: usize) -> Result<Abtt, String> {
    if n == 0 || d == 0 || rows.len() != n * d {
      return Err(format!("ABTT fit shape mismatch: {} vs {n}×{d}", rows.len()));
    }
    // Mean, f64 fixed-order.
    let mut mean64 = vec![0.0f64; d];
    for row in rows.chunks_exact(d) {
      for (m, &v) in mean64.iter_mut().zip(row) {
        *m += v as f64;
      }
    }
    for m in mean64.iter_mut() {
      *m /= n as f64;
    }
    // Centered Gram: fixed 4096-row blocks summed serially, block partials combined
    // serially — the reduction-tree discipline the whole crate uses.
    let block_rows = 4096usize;
    let partials: Vec<Vec<f64>> = rows
      .par_chunks(block_rows * d)
      .map(|block| {
        let mut gram = vec![0.0f64; d * d];
        for row in block.chunks_exact(d) {
          for i in 0..d {
            let ci = row[i] as f64 - mean64[i];
            let gram_row = &mut gram[i * d..(i + 1) * d];
            for (j, slot) in gram_row.iter_mut().enumerate() {
              *slot += ci * (row[j] as f64 - mean64[j]);
            }
          }
        }
        gram
      })
      .collect();
    let mut gram = vec![0.0f64; d * d];
    for partial in &partials {
      for (g, p) in gram.iter_mut().zip(partial) {
        *g += p;
      }
    }
    let (_, components) = top_gram_components(&gram, d, abtt_component_count(d))?;
    Ok(Abtt {
      mean: mean64.iter().map(|&m| m as f32).collect(),
      components,
    })
  }

  /// Apply: v ← v − μ − Σᵢ (uᵢᵀ(v−μ)) uᵢ.
  pub fn apply(&self, vector: &mut [f32]) {
    for (v, m) in vector.iter_mut().zip(&self.mean) {
      *v -= m;
    }
    for component in &self.components {
      let mut projection = 0.0f64;
      for (v, c) in vector.iter().zip(component) {
        projection += *v as f64 * *c as f64;
      }
      for (v, c) in vector.iter_mut().zip(component) {
        *v -= (projection * *c as f64) as f32;
      }
    }
  }
}

/// uSIF's closed-form weighting, computed from the frequency table alone (W18-3012
/// Algorithm 1, lines 2–7). Errors (degenerate vocabularies where α ∈ {0, 1}) are
/// typed — the caller states the lexical fallback.
pub struct UsifWeighting {
  pub a: f64,
}

impl UsifWeighting {
  /// `probabilities`: unigram p(w) over the vocabulary (must sum to ~1); `average_len`:
  /// E_s |s| over the training documents.
  pub fn from_frequencies(probabilities: &[f64], average_len: f64) -> Result<Self, String> {
    let vocab = probabilities.len();
    if vocab == 0 {
      return Err("uSIF over an empty vocabulary".to_string());
    }
    if !(average_len.is_finite() && average_len > 0.0) {
      return Err(format!("uSIF average document length degenerate: {average_len}"));
    }
    // threshold = 1 − (1 − 1/|V|)ⁿ — the probability a given word is produced at least
    // once by n steps of a uniform random walk (Algorithm 1 line 5).
    let threshold = 1.0 - (1.0 - 1.0 / vocab as f64).powf(average_len);
    let above = probabilities.iter().filter(|&&p| p > threshold).count();
    let alpha = above as f64 / vocab as f64;
    if alpha <= 0.0 || alpha >= 1.0 {
      return Err(format!(
        "uSIF α degenerate ({alpha}: {above}/{vocab} above threshold {threshold}) — corpus too \
         small or too uniform to weight"
      ));
    }
    let z = vocab as f64 / 2.0;
    let a = (1.0 - alpha) / (alpha * z);
    if !(a.is_finite() && a > 0.0) {
      return Err(format!("uSIF a degenerate: {a}"));
    }
    Ok(Self { a })
  }

  /// The per-word weight a / (p(w) + ½a) (Algorithm 1 line 8).
  pub fn weight(&self, probability: f64) -> f64 {
    self.a / (probability + 0.5 * self.a)
  }
}

/// The piecewise common components of the document-embedding matrix: the top m = 5
/// singular directions c'_i with weights λ_i = σ_i² / Σ_j≤m σ_j² (Algorithm 1 lines
/// 10–17). Fit from the d×d Gram of the document embeddings (Σ c̃ c̃ᵀ) — its
/// eigenvalues ARE σ_i² and its eigenvectors the right singular vectors, which live in
/// embedding space.
pub struct SentenceComponents {
  pub lambdas: Vec<f32>,
  pub components: Vec<Vec<f32>>,
}

impl SentenceComponents {
  pub fn from_gram(gram: &[f64], d: usize) -> Result<Self, String> {
    let (eigenvalues, components) = top_gram_components(gram, d, USIF_COMPONENTS)?;
    let kept: Vec<f64> = eigenvalues.iter().map(|v| v.max(0.0)).collect();
    let mass: f64 = kept.iter().sum();
    if !(mass.is_finite() && mass > 0.0) {
      return Err("sentence-component spectrum degenerate (zero mass)".to_string());
    }
    Ok(Self {
      lambdas: kept.iter().map(|&v| (v / mass) as f32).collect(),
      components,
    })
  }

  /// c ← c − Σᵢ λᵢ · proj_{c'ᵢ}(c) — the piecewise removal (Algorithm 1 line 19).
  pub fn remove(&self, vector: &mut [f32]) {
    for (lambda, component) in self.lambdas.iter().zip(&self.components) {
      let mut projection = 0.0f64;
      for (v, c) in vector.iter().zip(component) {
        projection += *v as f64 * *c as f64;
      }
      let scale = *lambda as f64 * projection;
      for (v, c) in vector.iter_mut().zip(component) {
        *v -= (scale * *c as f64) as f32;
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn abtt_removes_the_mean_and_top_directions() {
    // 200 rows in R^8: a large common offset plus a dominant direction e0, plus small
    // noise in e1. After ABTT (d/100 → 1 component), the mean is gone and e0's energy
    // collapses.
    let d = 8;
    let n = 200;
    let mut rows = vec![0.0f32; n * d];
    let mut state = 9u64;
    let mut next = move || {
      state ^= state << 13;
      state ^= state >> 7;
      state ^= state << 17;
      state
    };
    for row in rows.chunks_exact_mut(d) {
      row[0] = 5.0 + (next() % 1000) as f32 / 100.0; // mean-heavy dominant direction
      row[1] = (next() % 100) as f32 / 100.0;
      row[2] = 3.0; // pure offset
    }
    let abtt = Abtt::fit(&rows, n, d).unwrap();
    assert_eq!(abtt.components.len(), 1);
    let mut probe = rows[0..d].to_vec();
    abtt.apply(&mut probe);
    // Component direction is dominated by e0; post-removal projection on it ≈ 0.
    let component = &abtt.components[0];
    let projection: f64 = probe
      .iter()
      .zip(component)
      .map(|(v, c)| *v as f64 * *c as f64)
      .sum();
    assert!(projection.abs() < 1e-4, "residual projection {projection}");
  }

  #[test]
  fn abtt_is_bit_deterministic() {
    let d = 16;
    let n = 500;
    let rows: Vec<f32> = (0..n * d).map(|i| ((i * 2654435761) % 997) as f32 / 997.0).collect();
    let first = Abtt::fit(&rows, n, d).unwrap();
    let second = Abtt::fit(&rows, n, d).unwrap();
    assert_eq!(
      first.mean.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
      second.mean.iter().map(|v| v.to_bits()).collect::<Vec<_>>()
    );
    for (a, b) in first.components.iter().zip(&second.components) {
      assert_eq!(
        a.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
        b.iter().map(|v| v.to_bits()).collect::<Vec<_>>()
      );
    }
  }

  #[test]
  fn usif_a_matches_a_hand_computation() {
    // |V| = 4, p = [0.4, 0.3, 0.2, 0.1], n = 2.
    // threshold = 1 − (1 − 1/4)² = 1 − 0.5625 = 0.4375 → nothing above → α = 0 → error.
    let err = UsifWeighting::from_frequencies(&[0.4, 0.3, 0.2, 0.1], 2.0);
    assert!(err.is_err(), "α = 0 must be a typed error");
    // n = 5: threshold = 1 − 0.75⁵ = 1 − 0.2373046875 = 0.7626953125 → α still 0.
    // Use a skewed vocabulary instead: p = [0.7, 0.1, 0.1, 0.1], n = 2 → threshold
    // 0.4375, one word above → α = 0.25, Z = 2, a = 0.75 / (0.25 · 2) = 1.5.
    let weighting = UsifWeighting::from_frequencies(&[0.7, 0.1, 0.1, 0.1], 2.0).unwrap();
    assert!((weighting.a - 1.5).abs() < 1e-12, "a = {}", weighting.a);
    // Weight for p = 0.1: 1.5 / (0.1 + 0.75) = 1.7647…; monotone: rarer ⇒ heavier.
    let rare = weighting.weight(0.1);
    let frequent = weighting.weight(0.7);
    assert!((rare - 1.5 / 0.85).abs() < 1e-12);
    assert!(rare > frequent);
  }

  #[test]
  fn sentence_components_remove_weighted_energy() {
    // Documents concentrated on two orthogonal directions with 4:1 spectral mass.
    let d = 6;
    let mut gram = vec![0.0f64; d * d];
    gram[0] = 8.0; // σ₁² = 8 along e0
    gram[7] = 2.0; // σ₂² = 2 along e1
    let components = SentenceComponents::from_gram(&gram, d).unwrap();
    assert_eq!(components.lambdas.len(), USIF_COMPONENTS.min(d));
    // λ over the kept mass: e0 → 0.8, e1 → 0.2 (remaining components have zero mass).
    assert!((components.lambdas[0] - 0.8).abs() < 1e-6);
    assert!((components.lambdas[1] - 0.2).abs() < 1e-6);
    let mut vector = vec![1.0f32, 1.0, 1.0, 0.0, 0.0, 0.0];
    components.remove(&mut vector);
    // e0 keeps (1 − λ₀) = 0.2 of its coordinate, e1 keeps 0.8, e2 untouched.
    assert!((vector[0] - 0.2).abs() < 1e-5, "{vector:?}");
    assert!((vector[1] - 0.8).abs() < 1e-5, "{vector:?}");
    assert!((vector[2] - 1.0).abs() < 1e-6, "{vector:?}");
  }
}
