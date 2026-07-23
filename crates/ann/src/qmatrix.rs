//! Per-row i8 quantization of the vector matrix, with exact integer dot products.
//!
//! The Vamana tiers' cost is almost entirely distance computations over the row matrix
//! (~98% of a kernel-scale build). Quantizing each row to i8 with a per-row scale cuts
//! memory traffic 4× and turns the inner loop into an integer dot product — SDOT on
//! aarch64 — while keeping every distance a *pure deterministic function of the codes*:
//! the dot is exact integer arithmetic (identical on every platform and dispatch path),
//! and the surrounding scale algebra is a fixed sequence of f32 ops.
//!
//! Squared L2 between dequantized rows decomposes exactly:
//! `‖s_a·a − s_b·b‖² = s_a²‖a‖² + s_b²‖b‖² − 2·s_a·s_b·(a·b)` — the per-row self terms
//! (`snorm`) are precomputed once, so each pair costs one i8 dot plus four f32 ops.
//!
//! Quantization error only perturbs *candidate selection*; callers keep final ordering
//! exact by re-scoring the returned pool at full precision (the search path re-embeds its
//! top candidates — §10's "approximation never decides the ranking" bar).

use rayon::prelude::*;
use vorpal_mem::PodColumn;

/// Row-major i8 matrix with per-row dequantization scale and precomputed self term. Columns
/// are [`PodColumn`]s: owned when built, zero-copy mapped sections when loaded from disk.
pub(crate) struct QuantMatrix {
  /// Logical dimensionality (as embedded).
  dim: usize,
  /// Row stride: `dim` rounded up to a multiple of 16 so vector loops need no tail; the
  /// padding lanes are zero in every row and the query, contributing exactly nothing.
  padded: usize,
  pub(crate) scales: PodColumn<f32>,
  pub(crate) snorm: PodColumn<f32>,
  pub(crate) codes: PodColumn<i8>,
}

/// A quantized query: codes padded to the matrix stride, plus its scale algebra terms.
pub(crate) struct QuantQuery {
  pub codes: Vec<i8>,
  pub scale: f32,
  pub snorm: f32,
}

/// Quantize one row: `scale = max|x| / 127`, codes rounded to nearest. Exactly reversible
/// ordering-wise for the common case; the all-zero row gets scale 0 and a zero self term.
fn quantize_row(row: &[f32], codes: &mut [i8]) -> (f32, f32) {
  let max_abs = row.iter().fold(0.0f32, |m, &x| m.max(x.abs()));
  if max_abs == 0.0 {
    codes.fill(0);
    return (0.0, 0.0);
  }
  let inv = 127.0 / max_abs;
  let scale = max_abs / 127.0;
  let mut norm2 = 0i64;
  for (slot, &x) in codes.iter_mut().zip(row) {
    let q = (x * inv).round() as i32;
    let q = q.clamp(-127, 127);
    *slot = q as i8;
    norm2 += (q * q) as i64;
  }
  (scale, scale * scale * norm2 as f32)
}

impl QuantMatrix {
  /// Build by filling and quantizing one row at a time, in parallel — the full-precision
  /// matrix never exists (at kernel scale it was 2.9 GB of pure transient).
  pub fn from_rows<F: Fn(usize, &mut [f32]) + Sync>(n: usize, dim: usize, fill: F) -> Self {
    let padded = dim.next_multiple_of(16);
    let mut codes = vec![0i8; n * padded];
    let mut scales = vec![0.0f32; n];
    let mut snorm = vec![0.0f32; n];
    codes
      .par_chunks_mut(padded)
      .zip(scales.par_iter_mut())
      .zip(snorm.par_iter_mut())
      .enumerate()
      .for_each(|(i, ((row_codes, scale), self_term))| {
        let mut row = vec![0.0f32; dim];
        fill(i, &mut row);
        let (s, sn) = quantize_row(&row, &mut row_codes[..dim]);
        *scale = s;
        *self_term = sn;
      });
    Self {
      dim,
      padded,
      scales: PodColumn::from_vec(scales),
      snorm: PodColumn::from_vec(snorm),
      codes: PodColumn::from_vec(codes),
    }
  }

  /// Rewrap already-built columns (the load path — typically mapped sections).
  /// `codes.len()` must be `scales.len() * dim.next_multiple_of(16)`.
  pub fn from_columns(
    dim: usize,
    scales: PodColumn<f32>,
    snorm: PodColumn<f32>,
    codes: PodColumn<i8>,
  ) -> Self {
    let padded = dim.next_multiple_of(16);
    debug_assert_eq!(codes.len(), scales.len() * padded);
    debug_assert_eq!(scales.len(), snorm.len());
    Self {
      dim,
      padded,
      scales,
      snorm,
      codes,
    }
  }

  pub fn len(&self) -> usize {
    self.scales.len()
  }

  pub fn dim(&self) -> usize {
    self.dim
  }

  #[inline]
  pub fn row_codes(&self, i: u32) -> &[i8] {
    &self.codes[i as usize * self.padded..(i as usize + 1) * self.padded]
  }

  /// Quantize an (already normalized, `dim`-length) query with the same scheme as rows.
  pub fn quantize_query(&self, query: &[f32]) -> QuantQuery {
    let mut codes = vec![0i8; self.padded];
    let (scale, snorm) = quantize_row(&query[..self.dim], &mut codes[..self.dim]);
    QuantQuery {
      codes,
      scale,
      snorm,
    }
  }

  /// Stage row `i`'s codes into cache (the beam loop's next-neighbor prefetch).
  #[inline]
  pub fn prefetch_row(&self, i: u32) {
    vorpal_mem::prefetch_read(self.codes[i as usize * self.padded..].as_ptr());
  }

  /// Squared L2 between dequantized rows `a` and `b`.
  #[inline]
  pub fn dist_sq(&self, a: u32, b: u32) -> f32 {
    let dot = dot_i8(self.row_codes(a), self.row_codes(b));
    self.snorm[a as usize] + self.snorm[b as usize]
      - 2.0 * self.scales[a as usize] * self.scales[b as usize] * dot as f32
  }

  /// Squared L2 between dequantized row `i` and a quantized query.
  #[inline]
  pub fn dist_to_query(&self, i: u32, query: &QuantQuery) -> f32 {
    let dot = dot_i8(self.row_codes(i), &query.codes);
    self.snorm[i as usize] + query.snorm - 2.0 * self.scales[i as usize] * query.scale * dot as f32
  }
}

/// Exact i8 dot product. Both slices have equal, 16-multiple length. Every path — SDOT,
/// widening-multiply NEON, scalar — computes the identical integer, so dispatch can never
/// affect results.
#[inline]
fn dot_i8(a: &[i8], b: &[i8]) -> i32 {
  #[cfg(target_arch = "aarch64")]
  {
    // Apple Silicon builds enable `dotprod` at compile time, so this is a static branch;
    // generic aarch64 pays one cached feature probe and falls back to the multiply path.
    if cfg!(target_feature = "dotprod") || std::arch::is_aarch64_feature_detected!("dotprod") {
      // SAFETY: dotprod verified; lengths are equal multiples of 16 by construction.
      unsafe { dot_i8_sdot(a, b) }
    } else {
      // SAFETY: NEON is baseline on aarch64.
      unsafe { dot_i8_neon(a, b) }
    }
  }
  #[cfg(target_arch = "x86_64")]
  {
    if cfg!(target_feature = "avx2") || is_x86_feature_detected!("avx2") {
      // SAFETY: avx2 verified; lengths are equal multiples of 16 by construction.
      return unsafe { dot_i8_avx2(a, b) };
    }
    if is_x86_feature_detected!("sse4.1") {
      // SAFETY: sse4.1 verified; same length invariants.
      return unsafe { dot_i8_sse41(a, b) };
    }
    dot_i8_scalar(a, b)
  }
  #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
  dot_i8_scalar(a, b)
}

/// SDOT inner loop (the `vdotq_s32` intrinsic is still unstable, so this is `asm!`): each
/// `sdot` folds 16 exact i8×i8 products into i32 lanes — 4× the throughput of the widening
/// path on the instruction that dominates graph construction. Main loop handles 64 bytes per
/// iteration on four independent accumulators; a 16-byte loop covers the remainder.
#[cfg(target_arch = "aarch64")]
unsafe fn dot_i8_sdot(a: &[i8], b: &[i8]) -> i32 {
  debug_assert_eq!(a.len(), b.len());
  debug_assert_eq!(a.len() % 16, 0);
  let mut pa = a.as_ptr();
  let mut pb = b.as_ptr();
  let mut blocks = a.len() / 64;
  let mut tail = (a.len() % 64) / 16;
  let acc: i32;
  // SAFETY: pointers stay within the equal-length slices (blocks·64 + tail·16 == len);
  // loads only, no stack use; flags and the listed vector registers are clobbered.
  unsafe {
    std::arch::asm!(
      ".arch_extension dotprod",
      "movi v0.4s, #0",
      "movi v1.4s, #0",
      "movi v2.4s, #0",
      "movi v3.4s, #0",
      "cbz {blocks}, 3f",
      "2:",
      "ldp q4, q5, [{pa}], #32",
      "ldp q6, q7, [{pb}], #32",
      "ldp q16, q17, [{pa}], #32",
      "ldp q18, q19, [{pb}], #32",
      "sdot v0.4s, v4.16b, v6.16b",
      "sdot v1.4s, v5.16b, v7.16b",
      "sdot v2.4s, v16.16b, v18.16b",
      "sdot v3.4s, v17.16b, v19.16b",
      "subs {blocks}, {blocks}, #1",
      "b.ne 2b",
      "3:",
      "cbz {tail}, 5f",
      "4:",
      "ldr q4, [{pa}], #16",
      "ldr q6, [{pb}], #16",
      "sdot v0.4s, v4.16b, v6.16b",
      "subs {tail}, {tail}, #1",
      "b.ne 4b",
      "5:",
      "add v0.4s, v0.4s, v1.4s",
      "add v2.4s, v2.4s, v3.4s",
      "add v0.4s, v0.4s, v2.4s",
      "addv s0, v0.4s",
      "fmov {acc:w}, s0",
      pa = inout(reg) pa,
      pb = inout(reg) pb,
      blocks = inout(reg) blocks,
      tail = inout(reg) tail,
      acc = out(reg) acc,
      out("v0") _, out("v1") _, out("v2") _, out("v3") _,
      out("v4") _, out("v5") _, out("v6") _, out("v7") _,
      out("v16") _, out("v17") _, out("v18") _, out("v19") _,
      options(nostack, readonly),
    );
  }
  let _ = (pa, pb, blocks, tail);
  acc
}

#[cfg_attr(target_arch = "aarch64", allow(dead_code))]
fn dot_i8_scalar(a: &[i8], b: &[i8]) -> i32 {
  a.iter().zip(b).map(|(&x, &y)| x as i32 * y as i32).sum()
}

/// AVX2 inner loop: sign-extend 16 i8 lanes to i16, `madd_epi16` multiplies and pair-sums
/// into i32 lanes — every step exact (i16×i16 products are i32 arithmetic by definition;
/// per-lane accumulation stays far below i32 range at these magnitudes), so this returns
/// the same integer as the scalar path on any input.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn dot_i8_avx2(a: &[i8], b: &[i8]) -> i32 {
  use core::arch::x86_64::*;
  debug_assert_eq!(a.len(), b.len());
  debug_assert_eq!(a.len() % 16, 0);
  unsafe {
    let mut acc = _mm256_setzero_si256();
    let mut pa = a.as_ptr();
    let mut pb = b.as_ptr();
    for _ in 0..a.len() / 16 {
      let va = _mm256_cvtepi8_epi16(_mm_loadu_si128(pa as *const __m128i));
      let vb = _mm256_cvtepi8_epi16(_mm_loadu_si128(pb as *const __m128i));
      acc = _mm256_add_epi32(acc, _mm256_madd_epi16(va, vb));
      pa = pa.add(16);
      pb = pb.add(16);
    }
    let hi = _mm256_extracti128_si256(acc, 1);
    let lo = _mm256_castsi256_si128(acc);
    let sum4 = _mm_add_epi32(hi, lo);
    let sum2 = _mm_add_epi32(sum4, _mm_shuffle_epi32(sum4, 0b00_00_11_10));
    let sum1 = _mm_add_epi32(sum2, _mm_shuffle_epi32(sum2, 0b00_00_00_01));
    _mm_cvtsi128_si32(sum1)
  }
}

/// SSE4.1 variant of the same exact arithmetic, for the pre-AVX2 x86 tail (and for
/// Rosetta, which exposes SSE4.1 but not AVX2 — this path is what x86 test runs on Apple
/// Silicon actually execute).
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse4.1")]
unsafe fn dot_i8_sse41(a: &[i8], b: &[i8]) -> i32 {
  use core::arch::x86_64::*;
  debug_assert_eq!(a.len(), b.len());
  debug_assert_eq!(a.len() % 16, 0);
  unsafe {
    let mut acc = _mm_setzero_si128();
    let mut pa = a.as_ptr();
    let mut pb = b.as_ptr();
    for _ in 0..a.len() / 8 {
      let va = _mm_cvtepi8_epi16(_mm_loadl_epi64(pa as *const __m128i));
      let vb = _mm_cvtepi8_epi16(_mm_loadl_epi64(pb as *const __m128i));
      acc = _mm_add_epi32(acc, _mm_madd_epi16(va, vb));
      pa = pa.add(8);
      pb = pb.add(8);
    }
    let sum2 = _mm_add_epi32(acc, _mm_shuffle_epi32(acc, 0b00_00_11_10));
    let sum1 = _mm_add_epi32(sum2, _mm_shuffle_epi32(sum2, 0b00_00_00_01));
    _mm_cvtsi128_si32(sum1)
  }
}

/// NEON inner loop: widening i8×i8→i16 multiplies, pairwise-accumulated into i32 lanes —
/// every step exact (products ≤ 127² fit i16; accumulation is i32), so this returns the
/// same integer as the scalar path on any input.
#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
unsafe fn dot_i8_neon(a: &[i8], b: &[i8]) -> i32 {
  use std::arch::aarch64::*;
  debug_assert_eq!(a.len(), b.len());
  debug_assert_eq!(a.len() % 16, 0);
  unsafe {
    let mut acc = vdupq_n_s32(0);
    let mut pa = a.as_ptr();
    let mut pb = b.as_ptr();
    for _ in 0..a.len() / 16 {
      let va = vld1q_s8(pa);
      let vb = vld1q_s8(pb);
      let lo = vmull_s8(vget_low_s8(va), vget_low_s8(vb));
      let hi = vmull_high_s8(va, vb);
      acc = vpadalq_s16(acc, lo);
      acc = vpadalq_s16(acc, hi);
      pa = pa.add(16);
      pb = pb.add(16);
    }
    vaddvq_s32(acc)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn simd_and_scalar_agree_exactly() {
    // Sweep lengths that exercise the 64-byte main loop, the 16-byte tail loop, and both:
    // every dispatch path must return the same integer as the scalar reference.
    for len in [16usize, 48, 64, 256, 272, 4096] {
      let mut a = Vec::new();
      let mut b = Vec::new();
      let mut state = 0x1234_5678u64 ^ len as u64;
      for _ in 0..len {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
        a.push((state >> 40) as i8);
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
        b.push((state >> 40) as i8);
      }
      let reference = dot_i8_scalar(&a, &b);
      assert_eq!(
        dot_i8(&a, &b),
        reference,
        "dispatch path diverged at len {len}"
      );
      #[cfg(target_arch = "aarch64")]
      {
        assert_eq!(
          unsafe { dot_i8_neon(&a, &b) },
          reference,
          "neon path diverged at len {len}"
        );
        if std::arch::is_aarch64_feature_detected!("dotprod") {
          assert_eq!(
            unsafe { dot_i8_sdot(&a, &b) },
            reference,
            "sdot path diverged at len {len}"
          );
        }
      }
      #[cfg(target_arch = "x86_64")]
      {
        if is_x86_feature_detected!("avx2") {
          assert_eq!(
            unsafe { dot_i8_avx2(&a, &b) },
            reference,
            "avx2 path diverged at len {len}"
          );
        }
        if is_x86_feature_detected!("sse4.1") {
          assert_eq!(
            unsafe { dot_i8_sse41(&a, &b) },
            reference,
            "sse4.1 path diverged at len {len}"
          );
        }
      }
    }
  }

  #[test]
  fn quantized_distance_tracks_exact_distance() {
    // Sparse-ish unit vectors: quantized L2 must order pairs like exact L2 does.
    let dim = 64;
    let mk = |seed: u64| {
      let mut v = vec![0.0f32; dim];
      let mut s = seed;
      for _ in 0..8 {
        s = s.wrapping_mul(6364136223846793005).wrapping_add(97);
        let slot = (s >> 33) as usize % dim;
        v[slot] = ((s >> 16) & 0xFF) as f32 / 255.0 + 0.1;
      }
      let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
      v.iter_mut().for_each(|x| *x /= norm);
      v
    };
    let rows: Vec<Vec<f32>> = (0..32).map(|i| mk(i as u64 + 1)).collect();
    let m = QuantMatrix::from_rows(rows.len(), dim, |i, out| out.copy_from_slice(&rows[i]));
    let exact = |a: &[f32], b: &[f32]| -> f32 {
      a.iter().zip(b).map(|(x, y)| (x - y) * (x - y)).sum::<f32>()
    };
    for a in 0..rows.len() as u32 {
      for b in 0..rows.len() as u32 {
        let err = (m.dist_sq(a, b) - exact(&rows[a as usize], &rows[b as usize])).abs();
        assert!(err < 0.02, "quantized distance drifted: {err}");
      }
    }
  }
}
