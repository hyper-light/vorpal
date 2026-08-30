//! FastScan/1-bit viability measurement (ANN_FRONTIER Tier-2, de-risk step for the
//! "full package" conclusion): BEFORE building NEON TBL blocks + packed adjacency, answer
//! the two questions the failed flat-scalar cut left open, on real corpus data:
//!
//!   M1 — estimator fidelity: with proper RaBitQ shape (rotation + per-row correction,
//!        codes PRECOMPUTED once, asymmetric f32-query × 1-bit-code), what overfetch T
//!        captures the exact quantized top-10? (The failed cut paid per-insert
//!        dequant+rotate and a scalar 4-plane estimator; this isolates pure fidelity.)
//!   M2 — steering cost: greedy traversal ordered by the estimator (expansion set
//!        unchanged), pool recall of the visited set vs beam width l. The failed cut
//!        measured −3.4pt at l=200; the package bets that cheaper steps buy l=300+ at
//!        iso-latency. This measures the actual l-compensation curve.
//!
//! Run explicitly against a committed kernel-scale generation:
//!   VORPAL_FASTSCAN_GEN=/tmp/coldbase/gen/<id> \
//!     cargo test --release -p vorpal-ann fastscan_viability -- --ignored --nocapture

#[cfg(test)]
mod tests {
  use crate::index::{AnnIndex, BUILD_SEED, CALIBRATION_K, CALIBRATION_SEARCH_L};
  use crate::qmatrix::rotate_row;
  use crate::vamana::{VisitStamps, greedy_search};
  use rayon::prelude::*;

  struct BitCodes {
    /// D/64 words per row, bit d set ⇔ rotated coordinate d ≥ 0.
    words: Vec<u64>,
    words_per_row: usize,
    /// Per-row correction o_r = Σ|v_d| / (√D · ‖v‖) — the RaBitQ estimator scale.
    correction: Vec<f32>,
    dim: usize,
  }

  fn rotated_row(quant: &crate::qmatrix::QuantMatrix, row: u32, out: &mut [f32]) {
    let codes = quant.row_codes(row);
    let scale = quant.scales[row as usize];
    for (dst, &code) in out.iter_mut().zip(codes.iter()) {
      *dst = code as f32 * scale;
    }
    rotate_row(out);
  }

  fn build_bit_codes(quant: &crate::qmatrix::QuantMatrix, n: usize, dim: usize) -> BitCodes {
    let words_per_row = dim / 64;
    let mut words = vec![0u64; n * words_per_row];
    let mut correction = vec![0f32; n];
    words
      .par_chunks_mut(words_per_row)
      .zip(correction.par_iter_mut())
      .enumerate()
      .for_each(|(row, (word_chunk, corr))| {
        let mut v = vec![0f32; dim];
        rotated_row(quant, row as u32, &mut v);
        let mut abs_sum = 0f64;
        let mut sq_sum = 0f64;
        for (d, &x) in v.iter().enumerate() {
          if x >= 0.0 {
            word_chunk[d / 64] |= 1u64 << (d % 64);
          }
          abs_sum += x.abs() as f64;
          sq_sum += (x as f64) * (x as f64);
        }
        let norm = sq_sum.sqrt().max(1e-30);
        *corr = (abs_sum / ((dim as f64).sqrt() * norm)) as f32;
      });
    BitCodes {
      words,
      words_per_row,
      correction,
      dim,
    }
  }

  /// Asymmetric estimate of <q, c> from q's rotated f32 coordinates and c's sign bits:
  /// <q, sign_c> = 2·Σ_{set bits} q_d − Σ_d q_d, then RaBitQ's rescale by ‖c‖ / (√D·o_c).
  fn est_dot_asym(bits: &BitCodes, q_rot: &[f32], q_sum: f32, row: u32, c_norm: f32) -> f32 {
    let at = row as usize * bits.words_per_row;
    let mut set_sum = 0f32;
    for (w, &word) in bits.words[at..at + bits.words_per_row].iter().enumerate() {
      let mut m = word;
      while m != 0 {
        let d = w * 64 + m.trailing_zeros() as usize;
        set_sum += q_rot[d];
        m &= m - 1;
      }
    }
    let signed = 2.0 * set_sum - q_sum;
    c_norm * signed / ((bits.dim as f32).sqrt() * bits.correction[row as usize].max(1e-9))
  }

  /// Symmetric estimate from two sign-bit rows: <q̄, c̄> = (D − 2·hamming)/D, rescaled by
  /// both rows' norms and corrections.
  fn est_dot_sym(bits: &BitCodes, a: u32, b: u32, a_norm: f32, b_norm: f32) -> f32 {
    let (wa, wb) = (
      a as usize * bits.words_per_row,
      b as usize * bits.words_per_row,
    );
    let mut ham = 0u32;
    for w in 0..bits.words_per_row {
      ham += (bits.words[wa + w] ^ bits.words[wb + w]).count_ones();
    }
    let cos_bar = (bits.dim as f32 - 2.0 * ham as f32) / bits.dim as f32;
    let denom = (bits.correction[a as usize] * bits.correction[b as usize]).max(1e-9);
    a_norm * b_norm * cos_bar / denom
  }

  #[test]
  #[ignore = "corpus-scale measurement; run with VORPAL_FASTSCAN_GEN and --nocapture"]
  fn fastscan_viability() {
    let Some(generation) = std::env::var_os("VORPAL_FASTSCAN_GEN") else {
      eprintln!("VORPAL_FASTSCAN_GEN not set — skipping");
      return;
    };
    let index =
      AnnIndex::load(&std::path::Path::new(&generation).join("ann.bin")).expect("ann.bin");
    let quant = index.quant.as_ref().expect("vamana tier");
    let n = quant.len();
    let dim = quant.padded();
    assert_eq!(
      dim % 64,
      0,
      "sign-code path assumes 64-aligned padded dim (got {dim})"
    );

    let t0 = std::time::Instant::now();
    let bits = build_bit_codes(quant, n, dim);
    eprintln!("bit codes + corrections for {n} rows: {:?}", t0.elapsed());

    // Sanity: snorm is the dequantized squared norm and rotation is an isometry.
    {
      let mut v = vec![0f32; dim];
      rotated_row(quant, 0, &mut v);
      let rot_sq: f32 = v.iter().map(|x| x * x).sum();
      let stored = quant.snorm[0];
      assert!(
        (rot_sq - stored).abs() <= 1e-2 * stored.max(1.0),
        "rotation/norm mismatch: rotated {rot_sq} vs snorm {stored}"
      );
    }

    // Build-salt probe rows — the standing calibration draw, comparable numbers.
    let mut rng = crate::Rng::new(BUILD_SEED ^ 0x5EED_CA11_0B5E_7B01);
    let mut probes: Vec<u32> = Vec::new();
    let mut chosen = std::collections::HashSet::new();
    while probes.len() < 32.min(n) {
      let candidate = rng.below(n) as u32;
      if chosen.insert(candidate) {
        probes.push(candidate);
      }
    }

    // Exact quantized oracle per probe (the pool-recall metric's truth).
    let oracle: Vec<Vec<u32>> = probes
      .par_iter()
      .map(|&probe| {
        let mut best: Vec<(f32, u32)> = Vec::with_capacity(CALIBRATION_K + 1);
        for row in 0..n as u32 {
          if row == probe {
            continue;
          }
          let key = (quant.dist_sq(probe, row), row);
          if best.len() < CALIBRATION_K {
            let at = best.partition_point(|e| *e < key);
            best.insert(at, key);
          } else if key < *best.last().expect("k>0") {
            let at = best.partition_point(|e| *e < key);
            best.insert(at, key);
            best.pop();
          }
        }
        best.into_iter().map(|(_, r)| r).collect()
      })
      .collect();

    // ---- M1: full-scan estimator rankings vs the oracle, recall@T. ----
    let fetches = [10usize, 25, 50, 100, 200, 400];
    let mut asym_hits = vec![0usize; fetches.len()];
    let mut sym_hits = vec![0usize; fetches.len()];
    let mut want_total = 0usize;
    let t1 = std::time::Instant::now();
    for (probe_at, &probe) in probes.iter().enumerate() {
      let mut q_rot = vec![0f32; dim];
      rotated_row(quant, probe, &mut q_rot);
      let q_sum: f32 = q_rot.iter().sum();
      let q_norm_sq = quant.snorm[probe as usize];

      let scored: Vec<(f32, f32)> = (0..n as u32)
        .into_par_iter()
        .map(|row| {
          if row == probe {
            return (f32::MAX, f32::MAX);
          }
          let c_norm_sq = quant.snorm[row as usize];
          let c_norm = c_norm_sq.sqrt();
          let asym =
            q_norm_sq + c_norm_sq - 2.0 * est_dot_asym(&bits, &q_rot, q_sum, row, c_norm);
          let sym = q_norm_sq + c_norm_sq
            - 2.0
              * est_dot_sym(
                &bits,
                probe,
                row,
                q_norm_sq.sqrt(),
                c_norm,
              );
          (asym, sym)
        })
        .collect();

      let truth: std::collections::HashSet<u32> = oracle[probe_at].iter().copied().collect();
      want_total += truth.len();
      for (which, hits) in [(0usize, &mut asym_hits), (1usize, &mut sym_hits)] {
        let mut ranked: Vec<(f32, u32)> = scored
          .iter()
          .enumerate()
          .map(|(row, &(a, s))| (if which == 0 { a } else { s }, row as u32))
          .collect();
        for (fetch_at, &fetch) in fetches.iter().enumerate() {
          let take = fetch.min(ranked.len() - 1);
          ranked.select_nth_unstable_by(take, |x, y| {
            x.partial_cmp(y).unwrap_or(std::cmp::Ordering::Equal)
          });
          hits[fetch_at] += ranked[..=take].iter().filter(|(_, r)| truth.contains(r)).count();
        }
      }
    }
    eprintln!("M1 full-scan rankings: {:?}", t1.elapsed());
    eprintln!("M1 estimator recall@T against exact top-10 ({} probes):", probes.len());
    eprintln!("      T | asym (f32 x 1-bit) | sym (1-bit x 1-bit)");
    for (fetch_at, &fetch) in fetches.iter().enumerate() {
      eprintln!(
        "  {fetch:>5} | {:>18.4} | {:>19.4}",
        asym_hits[fetch_at] as f64 / want_total as f64,
        sym_hits[fetch_at] as f64 / want_total as f64,
      );
    }

    // ---- M2: estimator-STEERED traversal, pool recall of the visited set vs l. ----
    let adjacency = index.graph.adjacency();
    let mut stamps = VisitStamps::new(n);
    eprintln!("M2 steered-beam pool recall (visited-set membership of exact top-10):");
    for &(label, l) in &[
      ("exact l=200 (baseline)", CALIBRATION_SEARCH_L),
      ("asym  l=200", CALIBRATION_SEARCH_L),
      ("asym  l=300", 300),
      ("asym  l=400", 400),
      ("asym  l=600", 600),
    ] {
      let exact = label.starts_with("exact");
      let (mut hits, mut want) = (0usize, 0usize);
      let mut steps = 0usize;
      for (probe_at, &probe) in probes.iter().enumerate() {
        let mut q_rot = vec![0f32; dim];
        rotated_row(quant, probe, &mut q_rot);
        let q_sum: f32 = q_rot.iter().sum();
        let q_norm_sq = quant.snorm[probe as usize];
        let dist = |x: u32| -> f32 {
          if exact {
            quant.dist_sq(x, probe)
          } else {
            let c_norm_sq = quant.snorm[x as usize];
            q_norm_sq + c_norm_sq
              - 2.0 * est_dot_asym(&bits, &q_rot, q_sum, x, c_norm_sq.sqrt())
          }
        };
        let visited = greedy_search(
          &adjacency,
          index.medoid,
          l.min(n),
          &mut stamps,
          dist,
          |xs| [dist(xs[0]), dist(xs[1]), dist(xs[2]), dist(xs[3])],
          |x| quant.prefetch_row(x),
        );
        steps += visited.len();
        let pool: std::collections::HashSet<u32> = visited.iter().map(|&(v, _)| v).collect();
        hits += oracle[probe_at].iter().filter(|r| pool.contains(r)).count();
        want += oracle[probe_at].len();
      }
      eprintln!(
        "  {label}: pool recall {:.4} (avg expansions {:.0})",
        hits as f64 / want as f64,
        steps as f64 / probes.len() as f64
      );
    }
  }
}
