//! Dense-row algebra for the doc-side encoder sidecar (ENCODER_RESEARCH §8.2,
//! option 2): symmetric per-row int8 quantization of L2-normalized rows, an
//! int8 candidate scan, and the f16 rescore of the bounded top — the ANN v5
//! "i8 codes + full-precision rescore" scheme applied to encoder rows. Pure
//! algebra, no I/O: the index crate owns the file format and the fusion.
//!
//! Numerics: the scan ranks by the exact integer dot `Σ q_i8·r_i8` scaled by the
//! row's own scale (the query scale is a common factor), so its order is
//! deterministic and thread-count independent (each row is scored on its own,
//! then merged by `(score desc, id asc)`). The rescore widens the stored f16 row
//! to f32 and takes the f64-accumulated dot with the f32 query in ascending
//! dimension order — the same fixed-order law as the rerank's cosine.
//! Retention datum for the oversample factor: int8 candidates rescored at ×4
//! recover 99% of full-precision top-k (HF embedding-quantization, MTEB
//! retrieval; cited in ENCODER_RESEARCH §6) — [`RESCORE_OVERSAMPLE`].

use rayon::prelude::*;

use crate::encoder::{f16_bits_to_f32, f32_to_f16_bits};

/// Candidates the int8 scan hands to the f16 rescore, as a multiple of the
/// requested pool (the ×4 retention datum above).
pub const RESCORE_OVERSAMPLE: usize = 4;

/// Symmetric int8 quantization of one row: `scale = max|x| / 127`, codes
/// round-to-nearest. A zero row keeps scale 0 and all-zero codes.
pub fn quantize_row(row: &[f32], codes: &mut [i8]) -> f32 {
  let peak = row.iter().fold(0.0f32, |m, v| m.max(v.abs()));
  if peak == 0.0 {
    codes.fill(0);
    return 0.0;
  }
  let scale = peak / 127.0;
  let inv = 127.0 / peak;
  for (code, value) in codes.iter_mut().zip(row) {
    *code = (value * inv).round().clamp(-127.0, 127.0) as i8;
  }
  scale
}

/// Narrow one row to IEEE half bits (round-to-nearest-even) for the rescore store.
pub fn row_to_f16(row: &[f32], out: &mut [u16]) {
  for (slot, value) in out.iter_mut().zip(row) {
    *slot = f32_to_f16_bits(*value);
  }
}

/// Integer dot of two int8 vectors — sixteen i32 lanes in fixed order (the
/// compiler widens these to NEON/AVX multiply-adds); exact, so the lane split
/// is only a speed choice.
fn dot_i8(a: &[i8], b: &[i8]) -> i32 {
  const LANES: usize = 16;
  let blocks = a.len() / LANES * LANES;
  let mut lanes = [0i32; LANES];
  for (x, y) in a[..blocks].chunks_exact(LANES).zip(b[..blocks].chunks_exact(LANES)) {
    for lane in 0..LANES {
      lanes[lane] += x[lane] as i32 * y[lane] as i32;
    }
  }
  let mut total: i32 = lanes.iter().sum();
  for (x, y) in a[blocks..].iter().zip(&b[blocks..]) {
    total += *x as i32 * *y as i32;
  }
  total
}

/// The int8 candidate scan: score every row against the quantized query, keep
/// the `take` best by `(score desc, row asc)`. `admit(row)` filters BEFORE
/// ranking so a filtered pool stays honest. Rows are `[n][dim]` codes with a
/// per-row scale; returns `(row index, approximate cosine)`.
pub fn scan_i8(
  codes: &[i8],
  scales: &[f32],
  dim: usize,
  query: &[i8],
  query_scale: f32,
  take: usize,
  admit: impl Fn(usize) -> bool + Sync,
) -> Vec<(usize, f32)> {
  if dim == 0 || take == 0 || query.len() != dim {
    return Vec::new();
  }
  let n = scales.len().min(codes.len() / dim);
  // Per-thread partial top lists over row chunks, merged once: each row's score
  // is a pure function of its own bytes, so the merge order never changes a
  // result. Chunk width balances rayon scheduling against the merge cost.
  let chunk = (n / (rayon::current_num_threads() * 4)).max(1024);
  let mut partials: Vec<Vec<(f32, usize)>> = (0..n)
    .into_par_iter()
    .step_by(chunk)
    .map(|start| {
      let end = (start + chunk).min(n);
      let mut best: Vec<(f32, usize)> = Vec::with_capacity(take + 1);
      for row in start..end {
        if !admit(row) {
          continue;
        }
        let dot = dot_i8(&codes[row * dim..(row + 1) * dim], query);
        let score = dot as f32 * scales[row] * query_scale;
        push_top(&mut best, (score, row), take);
      }
      best
    })
    .collect();
  let mut merged: Vec<(f32, usize)> = partials.iter_mut().flat_map(std::mem::take).collect();
  merged.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
  merged.truncate(take);
  merged.into_iter().map(|(score, row)| (row, score)).collect()
}

/// Bounded insertion keeping the `take` best `(score desc, row asc)` entries.
fn push_top(best: &mut Vec<(f32, usize)>, entry: (f32, usize), take: usize) {
  let worse = |a: &(f32, usize), b: &(f32, usize)| a.0 < b.0 || (a.0 == b.0 && a.1 > b.1);
  if best.len() == take && worse(&entry, &best[take - 1]) {
    return;
  }
  let at = best.partition_point(|kept| !worse(kept, &entry));
  best.insert(at, entry);
  best.truncate(take);
}

/// Rescore int8 candidates on their stored f16 rows: f64-accumulated dot with the
/// f32 query, ascending dims. Returns `(row, cosine)` by `(cosine desc, row asc)`.
pub fn rescore_f16(
  halves: &[u16],
  dim: usize,
  query: &[f32],
  candidates: &[(usize, f32)],
) -> Vec<(usize, f32)> {
  let mut scored: Vec<(usize, f32)> = candidates
    .iter()
    .filter_map(|&(row, _)| {
      let half = halves.get(row * dim..(row + 1) * dim)?;
      let dot: f64 = half
        .iter()
        .zip(query)
        .map(|(h, q)| f16_bits_to_f32(*h) as f64 * *q as f64)
        .sum();
      Some((row, dot as f32))
    })
    .collect();
  scored.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
  scored
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Hash-mixed pseudo-random coordinate in [-0.5, 0.5) — no period in `i`, so no
  /// two synthetic rows coincide (a modular or sinusoidal generator did).
  fn pseudo(i: usize, d: usize) -> f32 {
    let h = (i as u64)
      .wrapping_mul(0x9E37_79B9_7F4A_7C15)
      .wrapping_add((d as u64).wrapping_mul(0xBF58_476D_1CE4_E5B9));
    let h = (h ^ (h >> 31)).wrapping_mul(0x94D0_49BB_1331_11EB);
    (h >> 40) as f32 / (1u64 << 24) as f32 - 0.5
  }

  fn unit(v: &[f32]) -> Vec<f32> {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    v.iter().map(|x| x / norm).collect()
  }

  #[test]
  fn quantization_round_trips_within_a_half_step() {
    let row = unit(&[0.3, -0.9, 0.05, 0.0, 0.7, -0.2, 0.1, 0.4]);
    let mut codes = [0i8; 8];
    let scale = quantize_row(&row, &mut codes);
    for (code, value) in codes.iter().zip(&row) {
      assert!((*code as f32 * scale - value).abs() <= scale / 2.0 + 1e-7);
    }
    assert_eq!(quantize_row(&[0.0; 4], &mut [0i8; 4]), 0.0);
  }

  #[test]
  fn scan_and_rescore_rank_the_nearest_row_first() {
    let dim = 32;
    let rows: Vec<Vec<f32>> = (0..200)
      .map(|i| unit(&(0..dim).map(|d| pseudo(i, d)).collect::<Vec<_>>()))
      .collect();
    let mut codes = vec![0i8; 200 * dim];
    let mut scales = vec![0.0f32; 200];
    let mut halves = vec![0u16; 200 * dim];
    for (i, row) in rows.iter().enumerate() {
      scales[i] = quantize_row(row, &mut codes[i * dim..(i + 1) * dim]);
      row_to_f16(row, &mut halves[i * dim..(i + 1) * dim]);
    }
    let query = rows[57].clone();
    let mut q_codes = vec![0i8; dim];
    let q_scale = quantize_row(&query, &mut q_codes);
    let candidates = scan_i8(&codes, &scales, dim, &q_codes, q_scale, 8, |_| true);
    assert_eq!(candidates[0].0, 57, "the identical row must scan first");
    let rescored = rescore_f16(&halves, dim, &query, &candidates);
    assert_eq!(rescored[0].0, 57);
    assert!((rescored[0].1 - 1.0).abs() < 1e-3, "self-cosine at f16 precision");
    // Filtered scan: the excluded identical row never appears.
    let filtered = scan_i8(&codes, &scales, dim, &q_codes, q_scale, 8, |row| row != 57);
    assert!(filtered.iter().all(|(row, _)| *row != 57));
    assert_eq!(filtered.len(), 8);
  }

  #[test]
  fn scan_is_deterministic_across_thread_counts() {
    let dim = 16;
    let n = 5000;
    let mut codes = vec![0i8; n * dim];
    let mut scales = vec![0.0f32; n];
    for i in 0..n {
      let row = unit(&(0..dim).map(|d| ((i * 17 + d * 5) % 11) as f32 - 5.0).collect::<Vec<_>>());
      scales[i] = quantize_row(&row, &mut codes[i * dim..(i + 1) * dim]);
    }
    let query = unit(&(0..dim).map(|d| (d % 3) as f32 - 1.0).collect::<Vec<_>>());
    let mut q_codes = vec![0i8; dim];
    let q_scale = quantize_row(&query, &mut q_codes);
    let wide = scan_i8(&codes, &scales, dim, &q_codes, q_scale, 50, |_| true);
    let narrow = rayon::ThreadPoolBuilder::new()
      .num_threads(1)
      .build()
      .unwrap()
      .install(|| scan_i8(&codes, &scales, dim, &q_codes, q_scale, 50, |_| true));
    assert_eq!(wide, narrow);
  }
}
