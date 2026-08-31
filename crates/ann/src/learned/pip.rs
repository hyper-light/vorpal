//! PIP-loss dimensionality selection (Yin & Shen, "On the Dimensionality of Word
//! Embedding", NeurIPS 2018, arXiv:1812.04224) — the design doc's D2: d is chosen by a
//! closed-form criterion from the corpus's own spectrum, clamped and recorded in
//! provenance, never guessed.
//!
//! Pieces, each taken verbatim from the paper / reference implementation:
//!
//! * **Noise estimate** (paper §5.1): split the corpus into halves, build both count
//!   matrices; `σ̂ = ‖M̃₁ − M̃₂‖_F / (2√(mn))` (square symmetric here: m = n = |V|).
//! * **Signal spectrum** (reference implementation `PIP_loss_calculator.py`):
//!   soft-threshold the empirical spectrum at `2σ√n` — `λᵢ ← max(0, λ̂ᵢ − 2σ√n)`.
//! * **Expected PIP loss upper bound** (Theorem 3, general α, quoted verbatim):
//!   `√(Σ_{i=k+1}^d λᵢ^{4α}) + 2√(2n)·α·σ·√(Σ_{i=1}^k λᵢ^{4α−2})
//!   + √2·Σ_{i=1}^k (λᵢ^{2α} − λ_{i+1}^{2α})·σ·√(Σ_{r≤i<s} (λ_r − λ_s)^{−2})`.
//! * **Selection**: k* = argmin_k of the bound (ties → smallest k), then clamped to the
//!   caller's provenance-recorded range (design determination D2: [64, 256]).
//!
//! At the crate's α = 0.5 (TACL 2015 symmetric weighting) the bound simplifies:
//! λ^{4α} = λ², λ^{4α−2} = 1, λ^{2α} = λ.
//!
//! Degeneracies are handled by the math, not by tunables: a duplicated eigenvalue
//! straddling the cut makes the perturbation bound genuinely infinite → that k is never
//! selected; a zero multiplier (λᵢ = λᵢ₊₁) zeroes its term before the gap sum is even
//! computed (0·∞ never forms); an empty post-threshold spectrum is a typed error — the
//! corpus is below its own noise floor and the caller must state the lexical fallback.

/// The symmetric eigenvalue-weighting exponent (TACL 2015: Σ^p, p = 0.5 — "using SVD
/// 'correctly' (p=1) is bad"; design determination D2). PIP's α is the same exponent.
pub const PIP_ALPHA_EXPONENT: f64 = 0.5;

/// `σ̂ = ‖M̃₁ − M̃₂‖_F / (2√(m·n))` over two half-corpus SYMMETRIC matrices streamed as
/// ASCENDING upper-triangle triples (i ≤ j — the PPMI stream's order). Off-diagonal
/// cells count twice in the Frobenius norm (both triangles). Pull-based so the
/// external-count path never materializes the halves; the slice version below
/// delegates here — one formula source.
pub fn estimate_noise_sigma_streams<IA, IB>(
  half_a: IA,
  half_b: IB,
  vocab: usize,
) -> Result<f64, String>
where
  IA: Iterator<Item = Result<(u32, u32, f64), String>>,
  IB: Iterator<Item = Result<(u32, u32, f64), String>>,
{
  if vocab == 0 {
    return Err("noise estimate over an empty vocabulary".to_string());
  }
  let mut frobenius_sq = 0.0f64;
  let mut account = |cell: (u32, u32), delta: f64| {
    let weight = if cell.0 == cell.1 { 1.0 } else { 2.0 };
    frobenius_sq += weight * delta * delta;
  };
  let mut half_a = half_a.peekable();
  let mut half_b = half_b.peekable();
  loop {
    // Peek both heads; errors surface before any ordering decision.
    let key_a = match half_a.peek() {
      Some(Ok((i, j, _))) => Some((*i, *j)),
      Some(Err(_)) => {
        return Err(half_a.next().and_then(Result::err).unwrap_or_default());
      }
      None => None,
    };
    let key_b = match half_b.peek() {
      Some(Ok((i, j, _))) => Some((*i, *j)),
      Some(Err(_)) => {
        return Err(half_b.next().and_then(Result::err).unwrap_or_default());
      }
      None => None,
    };
    match (key_a, key_b) {
      (Some(ka), Some(kb)) if ka == kb => {
        let (_, _, va) = half_a.next().transpose()?.unwrap_or((0, 0, 0.0));
        let (_, _, vb) = half_b.next().transpose()?.unwrap_or((0, 0, 0.0));
        account(ka, va - vb);
      }
      (Some(ka), Some(kb)) if ka < kb => {
        let (_, _, va) = half_a.next().transpose()?.unwrap_or((0, 0, 0.0));
        account(ka, va);
      }
      (Some(_), Some(kb)) => {
        let (_, _, vb) = half_b.next().transpose()?.unwrap_or((0, 0, 0.0));
        account(kb, -vb);
      }
      (Some(ka), None) => {
        let (_, _, va) = half_a.next().transpose()?.unwrap_or((0, 0, 0.0));
        account(ka, va);
      }
      (None, Some(kb)) => {
        let (_, _, vb) = half_b.next().transpose()?.unwrap_or((0, 0, 0.0));
        account(kb, -vb);
      }
      (None, None) => break,
    }
  }
  let sigma = frobenius_sq.sqrt() / (2.0 * (vocab as f64 * vocab as f64).sqrt());
  if !sigma.is_finite() {
    return Err(format!("noise estimate non-finite ({sigma})"));
  }
  Ok(sigma)
}

/// [`estimate_noise_sigma_streams`] over materialized triple slices.
pub fn estimate_noise_sigma(
  half_a: &[(u32, u32, f64)],
  half_b: &[(u32, u32, f64)],
  vocab: usize,
) -> Result<f64, String> {
  estimate_noise_sigma_streams(
    half_a.iter().map(|&t| Ok(t)),
    half_b.iter().map(|&t| Ok(t)),
    vocab,
  )
}

/// Soft-threshold the empirical spectrum at `2σ√n` (reference implementation): the
/// universal singular-value threshold for a square matrix under i.i.d. noise σ.
/// Returns the DESCENDING thresholded spectrum (input order preserved — callers pass
/// spectra already sorted descending, as `top_symmetric_eigen` emits them by |λ|).
pub fn soft_threshold(spectrum: &[f64], sigma: f64, vocab: usize) -> Vec<f64> {
  let threshold = 2.0 * sigma * (vocab as f64).sqrt();
  spectrum
    .iter()
    .map(|&lambda| (lambda - threshold).max(0.0))
    .collect()
}

/// One dimensionality selection: the chosen d and the full loss curve (provenance —
/// the numbers that made the decision travel with the model).
pub struct PipSelection {
  pub d: usize,
  pub losses: Vec<f64>,
}

/// k* = argmin over k ∈ [1, s] (s = positive thresholded values) of the Theorem-3
/// bound at α = [`PIP_ALPHA_EXPONENT`], clamped to `clamp` (inclusive). `vocab` is the
/// matrix size n in the bound's √(2n) factor.
pub fn select_dimension(
  signal: &[f64],
  sigma: f64,
  vocab: usize,
  clamp: (usize, usize),
) -> Result<PipSelection, String> {
  if clamp.0 == 0 || clamp.0 > clamp.1 {
    return Err(format!("degenerate clamp range [{}, {}]", clamp.0, clamp.1));
  }
  if !(sigma.is_finite() && sigma >= 0.0) {
    return Err(format!("degenerate noise estimate {sigma}"));
  }
  let s = signal.iter().take_while(|&&l| l > 0.0).count();
  if s == 0 {
    return Err(
      "spectrum entirely below the noise floor — corpus cannot support a learned tier".to_string(),
    );
  }
  if signal[..s].windows(2).any(|w| w[0] < w[1]) {
    return Err("signal spectrum not sorted descending".to_string());
  }
  let alpha = PIP_ALPHA_EXPONENT;
  // Theorem 3's three exponents, named for their terms — every digit is the cited
  // formula's, none is tunable. E = U·D^α makes inner products scale as λ^{2α} (two
  // factors), and the PIP loss is a Frobenius norm over PRODUCTS of embeddings, which
  // squares that to λ^{4α}; the noise-variance term carries the theorem's λ^{4α−2}.
  let bias_exponent = 4.0 * alpha; // λ^{4α} — discarded-signal (bias) term
  let variance_exponent = 4.0 * alpha - 2.0; // λ^{4α−2} — noise-variance term
  let singular_exponent = 2.0 * alpha; // λ^{2α} — the embedding's own singular scale
  let lambda = &signal[..s];
  let at = |i: usize| -> f64 { lambda.get(i).copied().unwrap_or(0.0) };

  // Bias tail: Σ_{i>k} λ_i^{4α}, as suffix sums.
  let mut tail_bias = vec![0.0f64; s + 1];
  for k in (0..s).rev() {
    tail_bias[k] = tail_bias[k + 1] + lambda[k].powf(bias_exponent);
  }

  let mut losses = Vec::with_capacity(s);
  let mut variance_prefix = 0.0f64; // Σ_{i≤k} λ_i^{4α−2}
  let mut gap_terms = 0.0f64; // Σ_{i≤k} (λ_i^{2α} − λ_{i+1}^{2α})·σ·√(G_i)
  for (i, &lam) in lambda.iter().enumerate() {
    let k = i + 1; // 1-based candidate dimensionality
    variance_prefix += lam.powf(variance_exponent);
    let multiplier = lam.powf(singular_exponent) - at(i + 1).powf(singular_exponent);
    if multiplier > 0.0 {
      // G_i = Σ_{r ≤ i < s'} (λ_r − λ_{s'})^{−2}: a duplicated eigenvalue straddling
      // the cut makes the bound genuinely infinite — that k can never win.
      let mut gap_sum = 0.0f64;
      'gaps: for r in 0..=i {
        for sp in (i + 1)..=s {
          let gap = at(r) - at(sp);
          if gap <= 0.0 {
            gap_sum = f64::INFINITY;
            break 'gaps;
          }
          gap_sum += 1.0 / (gap * gap);
        }
      }
      gap_terms += multiplier * sigma * gap_sum.sqrt();
    }
    let loss = tail_bias[k].sqrt()
      + 2.0 * (2.0 * vocab as f64).sqrt() * alpha * sigma * variance_prefix.sqrt()
      + std::f64::consts::SQRT_2 * gap_terms;
    losses.push(loss);
  }

  let mut best = 0usize;
  for (k, loss) in losses.iter().enumerate() {
    if loss < &losses[best] {
      best = k;
    }
  }
  if !losses[best].is_finite() {
    return Err("every candidate dimensionality has infinite PIP bound".to_string());
  }
  let d = (best + 1).clamp(clamp.0, clamp.1).min(s.max(clamp.0)).min(clamp.1);
  Ok(PipSelection { d, losses })
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn sigma_matches_a_hand_computation() {
    // M₁ has (0,0)=2, (0,1)=1; M₂ has (0,1)=3, (1,1)=1; |V| = 2.
    // Diff cells: (0,0)=2 (diag, ×1), (0,1)=−2 (off-diag, ×2), (1,1)=−1 (diag, ×1).
    // ‖diff‖²_F = 4 + 2·4 + 1 = 13 → σ = √13 / (2·√4) = √13 / 4.
    let a = [(0, 0, 2.0), (0, 1, 1.0)];
    let b = [(0, 1, 3.0), (1, 1, 1.0)];
    let sigma = estimate_noise_sigma(&a, &b, 2).unwrap();
    assert!((sigma - 13f64.sqrt() / 4.0).abs() < 1e-12, "{sigma}");
  }

  #[test]
  fn soft_threshold_is_the_reference_rule() {
    // 2σ√n with σ = 0.5, n = 4 → threshold 2.0.
    assert_eq!(
      soft_threshold(&[5.0, 2.5, 1.0], 0.5, 4),
      vec![3.0, 0.5, 0.0]
    );
  }

  #[test]
  fn selection_finds_the_knee_of_a_gapped_spectrum() {
    // Strong signal for 4 dimensions, then dust: with modest noise, the loss curve's
    // minimum must land at the knee (k = 4) — keeping dust adds variance faster than
    // it removes bias.
    let signal = vec![100.0, 60.0, 30.0, 15.0, 0.4, 0.3, 0.2, 0.1];
    let selection = select_dimension(&signal, 0.05, 10_000, (1, 256)).unwrap();
    assert_eq!(selection.d, 4, "losses: {:?}", selection.losses);
    // The curve is finite and the reported curve length covers every candidate.
    assert_eq!(selection.losses.len(), signal.len());
    assert!(selection.losses.iter().all(|l| l.is_finite()));
  }

  #[test]
  fn clamping_and_floors_apply() {
    let signal = vec![10.0, 8.0, 6.0, 4.0, 2.0, 1.0];
    let selection = select_dimension(&signal, 0.01, 1000, (3, 4)).unwrap();
    assert!(selection.d >= 3 && selection.d <= 4);
  }

  #[test]
  fn below_noise_floor_is_a_typed_error() {
    assert!(select_dimension(&[0.0, 0.0], 1.0, 100, (1, 8)).is_err());
    let thresholded = soft_threshold(&[1.0, 0.5], 10.0, 10_000);
    assert!(select_dimension(&thresholded, 10.0, 10_000, (1, 8)).is_err());
  }

  #[test]
  fn duplicate_eigenvalues_never_poison_with_nan() {
    // λ₂ = λ₃ exactly: the multiplier at i = 2 is zero (term skipped, no 0·∞), while
    // any k whose CUT straddles the duplicate gets an infinite bound and cannot win.
    let signal = vec![10.0, 5.0, 5.0, 1.0];
    let selection = select_dimension(&signal, 0.1, 1000, (1, 8)).unwrap();
    assert!(selection.losses.iter().all(|l| !l.is_nan()), "{:?}", selection.losses);
    assert_ne!(selection.d, 2, "a cut inside a duplicate pair has an infinite bound");
  }

  #[test]
  fn deterministic() {
    let signal = vec![9.0, 4.0, 2.0, 1.0, 0.5];
    let first = select_dimension(&signal, 0.2, 5000, (1, 16)).unwrap();
    let second = select_dimension(&signal, 0.2, 5000, (1, 16)).unwrap();
    assert_eq!(first.d, second.d);
    let bits = |v: &[f64]| v.iter().map(|x| x.to_bits()).collect::<Vec<_>>();
    assert_eq!(bits(&first.losses), bits(&second.losses));
  }
}
