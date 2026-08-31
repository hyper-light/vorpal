//! Explicit SIMD kernels for the crate's hot float reductions, on the STABLE
//! toolchain: `core::arch` intrinsics (std::simd / `portable_simd` is nightly-only;
//! the workspace pins stable), with ONE fixed rounding tree across every path:
//!
//! * element i feeds lane (i mod [`LANES`]);
//! * per lane: multiply, THEN add — never FMA (fusion rounds differently and would
//!   fork results across CPUs);
//! * the LANES accumulators are combined serially in lane order;
//! * the scalar remainder (len % LANES) is appended last, serially.
//!
//! The scalar lane loop is the EXECUTABLE SPECIFICATION; the NEON and AVX2 paths are
//! bit-identical accelerations of it, pinned by parity tests across sizes including
//! remainders. Runtime CPU dispatch therefore cannot fork results — every path
//! computes the same bits — which is what makes explicit SIMD compatible with the
//! crate's bit-reproducibility contract. Reductions get explicit kernels because a
//! written serial FP chain forbids vectorization (non-associativity); loops without
//! cross-iteration dependencies (AXPY, Gram-row updates) auto-vectorize reliably and
//! stay in plain Rust.

/// Fixed lane count of the reduction tree — a code constant, never CPU-detected (the
/// tree must be identical on every machine). 8 f32 lanes = two NEON registers or one
/// AVX2 register; 8 f64 lanes = four NEON or two AVX2 registers.
pub(crate) const LANES: usize = 8;

/// Squared-L2 reference — the specification all SIMD paths must match bit-for-bit.
/// On aarch64 the NEON path always dispatches, so the specification is exercised only
/// by the parity tests there; on x86_64 it is the non-AVX2 fallback, elsewhere the
/// only path.
#[cfg_attr(target_arch = "aarch64", allow(dead_code))]
pub(crate) fn l2_sq_scalar(a: &[f32], b: &[f32]) -> f32 {
  let mut lanes = [0.0f32; LANES];
  let mut blocks_a = a.chunks_exact(LANES);
  let mut blocks_b = b.chunks_exact(LANES);
  for (ca, cb) in (&mut blocks_a).zip(&mut blocks_b) {
    for lane in 0..LANES {
      let d = ca[lane] - cb[lane];
      lanes[lane] += d * d;
    }
  }
  let mut sum = 0.0f32;
  for lane in lanes {
    sum += lane;
  }
  for (x, y) in blocks_a.remainder().iter().zip(blocks_b.remainder()) {
    let d = x - y;
    sum += d * d;
  }
  sum
}

/// Sum of squares reference (the normalize kernel). Same dispatch note as
/// [`l2_sq_scalar`].
#[cfg_attr(target_arch = "aarch64", allow(dead_code))]
pub(crate) fn sum_sq_scalar(v: &[f32]) -> f32 {
  let mut lanes = [0.0f32; LANES];
  let mut blocks = v.chunks_exact(LANES);
  for chunk in &mut blocks {
    for lane in 0..LANES {
      lanes[lane] += chunk[lane] * chunk[lane];
    }
  }
  let mut sum = 0.0f32;
  for lane in lanes {
    sum += lane;
  }
  for x in blocks.remainder() {
    sum += x * x;
  }
  sum
}

/// f32×f32 → f64-accumulated dot reference (the factorization kernel). Same dispatch
/// note as [`l2_sq_scalar`].
#[cfg_attr(target_arch = "aarch64", allow(dead_code))]
pub(crate) fn dot_wide_scalar(a: &[f32], b: &[f32]) -> f64 {
  let mut lanes = [0.0f64; LANES];
  let mut blocks_a = a.chunks_exact(LANES);
  let mut blocks_b = b.chunks_exact(LANES);
  for (ca, cb) in (&mut blocks_a).zip(&mut blocks_b) {
    for lane in 0..LANES {
      lanes[lane] += ca[lane] as f64 * cb[lane] as f64;
    }
  }
  let mut sum = 0.0f64;
  for lane in lanes {
    sum += lane;
  }
  for (x, y) in blocks_a.remainder().iter().zip(blocks_b.remainder()) {
    sum += *x as f64 * *y as f64;
  }
  sum
}

#[cfg(target_arch = "aarch64")]
mod neon {
  use core::arch::aarch64::*;

  use super::LANES;

  /// SAFETY: NEON is architecturally baseline on aarch64 — always present.
  pub(super) fn l2_sq(a: &[f32], b: &[f32]) -> f32 {
    debug_assert_eq!(a.len(), b.len());
    let blocks = a.len() / LANES;
    unsafe {
      let mut acc0 = vdupq_n_f32(0.0); // lanes 0..4 of the tree
      let mut acc1 = vdupq_n_f32(0.0); // lanes 4..8
      for i in 0..blocks {
        let pa = a.as_ptr().add(i * LANES);
        let pb = b.as_ptr().add(i * LANES);
        let d0 = vsubq_f32(vld1q_f32(pa), vld1q_f32(pb));
        let d1 = vsubq_f32(vld1q_f32(pa.add(4)), vld1q_f32(pb.add(4)));
        // multiply THEN add — never vfmaq (FMA would change the rounding tree).
        acc0 = vaddq_f32(acc0, vmulq_f32(d0, d0));
        acc1 = vaddq_f32(acc1, vmulq_f32(d1, d1));
      }
      // Serial combine in lane order 0..8 — NOT vaddvq (horizontal adds pair up and
      // change the tree).
      let mut sum = vgetq_lane_f32::<0>(acc0);
      sum += vgetq_lane_f32::<1>(acc0);
      sum += vgetq_lane_f32::<2>(acc0);
      sum += vgetq_lane_f32::<3>(acc0);
      sum += vgetq_lane_f32::<0>(acc1);
      sum += vgetq_lane_f32::<1>(acc1);
      sum += vgetq_lane_f32::<2>(acc1);
      sum += vgetq_lane_f32::<3>(acc1);
      for i in blocks * LANES..a.len() {
        let d = a[i] - b[i];
        sum += d * d;
      }
      sum
    }
  }

  pub(super) fn sum_sq(v: &[f32]) -> f32 {
    let blocks = v.len() / LANES;
    unsafe {
      let mut acc0 = vdupq_n_f32(0.0);
      let mut acc1 = vdupq_n_f32(0.0);
      for i in 0..blocks {
        let p = v.as_ptr().add(i * LANES);
        let x0 = vld1q_f32(p);
        let x1 = vld1q_f32(p.add(4));
        acc0 = vaddq_f32(acc0, vmulq_f32(x0, x0));
        acc1 = vaddq_f32(acc1, vmulq_f32(x1, x1));
      }
      let mut sum = vgetq_lane_f32::<0>(acc0);
      sum += vgetq_lane_f32::<1>(acc0);
      sum += vgetq_lane_f32::<2>(acc0);
      sum += vgetq_lane_f32::<3>(acc0);
      sum += vgetq_lane_f32::<0>(acc1);
      sum += vgetq_lane_f32::<1>(acc1);
      sum += vgetq_lane_f32::<2>(acc1);
      sum += vgetq_lane_f32::<3>(acc1);
      for x in &v[blocks * LANES..] {
        sum += x * x;
      }
      sum
    }
  }

  pub(super) fn dot_wide(a: &[f32], b: &[f32]) -> f64 {
    debug_assert_eq!(a.len(), b.len());
    let blocks = a.len() / LANES;
    unsafe {
      // Four f64x2 accumulators = the 8 lanes, in element order:
      // c0 = lanes {0,1}, c1 = {2,3}, c2 = {4,5}, c3 = {6,7}.
      let mut c0 = vdupq_n_f64(0.0);
      let mut c1 = vdupq_n_f64(0.0);
      let mut c2 = vdupq_n_f64(0.0);
      let mut c3 = vdupq_n_f64(0.0);
      for i in 0..blocks {
        let pa = a.as_ptr().add(i * LANES);
        let pb = b.as_ptr().add(i * LANES);
        let qa0 = vld1q_f32(pa);
        let qb0 = vld1q_f32(pb);
        let qa1 = vld1q_f32(pa.add(4));
        let qb1 = vld1q_f32(pb.add(4));
        let a01 = vcvt_f64_f32(vget_low_f32(qa0));
        let b01 = vcvt_f64_f32(vget_low_f32(qb0));
        let a23 = vcvt_high_f64_f32(qa0);
        let b23 = vcvt_high_f64_f32(qb0);
        let a45 = vcvt_f64_f32(vget_low_f32(qa1));
        let b45 = vcvt_f64_f32(vget_low_f32(qb1));
        let a67 = vcvt_high_f64_f32(qa1);
        let b67 = vcvt_high_f64_f32(qb1);
        c0 = vaddq_f64(c0, vmulq_f64(a01, b01));
        c1 = vaddq_f64(c1, vmulq_f64(a23, b23));
        c2 = vaddq_f64(c2, vmulq_f64(a45, b45));
        c3 = vaddq_f64(c3, vmulq_f64(a67, b67));
      }
      let mut sum = vgetq_lane_f64::<0>(c0);
      sum += vgetq_lane_f64::<1>(c0);
      sum += vgetq_lane_f64::<0>(c1);
      sum += vgetq_lane_f64::<1>(c1);
      sum += vgetq_lane_f64::<0>(c2);
      sum += vgetq_lane_f64::<1>(c2);
      sum += vgetq_lane_f64::<0>(c3);
      sum += vgetq_lane_f64::<1>(c3);
      for i in blocks * LANES..a.len() {
        sum += a[i] as f64 * b[i] as f64;
      }
      sum
    }
  }
}

#[cfg(target_arch = "x86_64")]
mod avx2 {
  use core::arch::x86_64::*;

  use super::LANES;

  /// SAFETY: caller must have verified AVX2 via `is_x86_feature_detected!`.
  #[target_feature(enable = "avx2")]
  pub(super) unsafe fn l2_sq(a: &[f32], b: &[f32]) -> f32 {
    let blocks = a.len() / LANES;
    let mut acc = _mm256_setzero_ps(); // one register = the 8 lanes, in element order
    for i in 0..blocks {
      let va = _mm256_loadu_ps(a.as_ptr().add(i * LANES));
      let vb = _mm256_loadu_ps(b.as_ptr().add(i * LANES));
      let d = _mm256_sub_ps(va, vb);
      // multiply THEN add — never _mm256_fmadd_ps (FMA changes the tree).
      acc = _mm256_add_ps(acc, _mm256_mul_ps(d, d));
    }
    let mut lanes = [0.0f32; LANES];
    _mm256_storeu_ps(lanes.as_mut_ptr(), acc);
    let mut sum = 0.0f32;
    for lane in lanes {
      sum += lane;
    }
    for i in blocks * LANES..a.len() {
      let d = a[i] - b[i];
      sum += d * d;
    }
    sum
  }

  #[target_feature(enable = "avx2")]
  pub(super) unsafe fn sum_sq(v: &[f32]) -> f32 {
    let blocks = v.len() / LANES;
    let mut acc = _mm256_setzero_ps();
    for i in 0..blocks {
      let x = _mm256_loadu_ps(v.as_ptr().add(i * LANES));
      acc = _mm256_add_ps(acc, _mm256_mul_ps(x, x));
    }
    let mut lanes = [0.0f32; LANES];
    _mm256_storeu_ps(lanes.as_mut_ptr(), acc);
    let mut sum = 0.0f32;
    for lane in lanes {
      sum += lane;
    }
    for x in &v[blocks * LANES..] {
      sum += x * x;
    }
    sum
  }

  #[target_feature(enable = "avx2")]
  pub(super) unsafe fn dot_wide(a: &[f32], b: &[f32]) -> f64 {
    let blocks = a.len() / LANES;
    // Two f64x4 accumulators = the 8 lanes in element order: lo = {0..4}, hi = {4..8}.
    let mut lo = _mm256_setzero_pd();
    let mut hi = _mm256_setzero_pd();
    for i in 0..blocks {
      let va = _mm256_loadu_ps(a.as_ptr().add(i * LANES));
      let vb = _mm256_loadu_ps(b.as_ptr().add(i * LANES));
      let a_lo = _mm256_cvtps_pd(_mm256_castps256_ps128(va));
      let b_lo = _mm256_cvtps_pd(_mm256_castps256_ps128(vb));
      let a_hi = _mm256_cvtps_pd(_mm256_extractf128_ps::<1>(va));
      let b_hi = _mm256_cvtps_pd(_mm256_extractf128_ps::<1>(vb));
      lo = _mm256_add_pd(lo, _mm256_mul_pd(a_lo, b_lo));
      hi = _mm256_add_pd(hi, _mm256_mul_pd(a_hi, b_hi));
    }
    let mut lanes_lo = [0.0f64; 4];
    let mut lanes_hi = [0.0f64; 4];
    _mm256_storeu_pd(lanes_lo.as_mut_ptr(), lo);
    _mm256_storeu_pd(lanes_hi.as_mut_ptr(), hi);
    let mut sum = 0.0f64;
    for lane in lanes_lo {
      sum += lane;
    }
    for lane in lanes_hi {
      sum += lane;
    }
    for i in blocks * LANES..a.len() {
      sum += a[i] as f64 * b[i] as f64;
    }
    sum
  }
}

#[inline]
pub(crate) fn l2_sq(a: &[f32], b: &[f32]) -> f32 {
  #[cfg(target_arch = "aarch64")]
  {
    neon::l2_sq(a, b)
  }
  #[cfg(target_arch = "x86_64")]
  {
    if std::arch::is_x86_feature_detected!("avx2") {
      // SAFETY: AVX2 presence just verified.
      unsafe { avx2::l2_sq(a, b) }
    } else {
      l2_sq_scalar(a, b)
    }
  }
  #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
  {
    l2_sq_scalar(a, b)
  }
}

#[inline]
pub(crate) fn sum_sq(v: &[f32]) -> f32 {
  #[cfg(target_arch = "aarch64")]
  {
    neon::sum_sq(v)
  }
  #[cfg(target_arch = "x86_64")]
  {
    if std::arch::is_x86_feature_detected!("avx2") {
      // SAFETY: AVX2 presence just verified.
      unsafe { avx2::sum_sq(v) }
    } else {
      sum_sq_scalar(v)
    }
  }
  #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
  {
    sum_sq_scalar(v)
  }
}

#[inline]
pub(crate) fn dot_wide(a: &[f32], b: &[f32]) -> f64 {
  #[cfg(target_arch = "aarch64")]
  {
    neon::dot_wide(a, b)
  }
  #[cfg(target_arch = "x86_64")]
  {
    if std::arch::is_x86_feature_detected!("avx2") {
      // SAFETY: AVX2 presence just verified.
      unsafe { avx2::dot_wide(a, b) }
    } else {
      dot_wide_scalar(a, b)
    }
  }
  #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
  {
    dot_wide_scalar(a, b)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn vectors(len: usize, seed: u64) -> (Vec<f32>, Vec<f32>) {
    let mut state = seed.max(1);
    let mut next = move || {
      state ^= state << 13;
      state ^= state >> 7;
      state ^= state << 17;
      // Mixed magnitudes exercise rounding: [-4, 4) with varying exponents.
      ((state >> 40) as f32 / (1u32 << 21) as f32) - 4.0
    };
    let a: Vec<f32> = (0..len).map(|_| next()).collect();
    let b: Vec<f32> = (0..len).map(|_| next()).collect();
    (a, b)
  }

  /// The dispatched paths must equal the scalar specification BIT-FOR-BIT, at every
  /// size shape (empty, sub-lane, exact blocks, remainders, large).
  #[test]
  fn simd_paths_match_the_scalar_specification_bitwise() {
    for &len in &[0usize, 1, 3, 7, 8, 9, 15, 16, 17, 63, 64, 65, 255, 256, 1000, 4096] {
      for seed in 1..=5u64 {
        let (a, b) = vectors(len, seed * 7919);
        assert_eq!(
          l2_sq(&a, &b).to_bits(),
          l2_sq_scalar(&a, &b).to_bits(),
          "l2_sq diverged at len {len} seed {seed}"
        );
        assert_eq!(
          sum_sq(&a).to_bits(),
          sum_sq_scalar(&a).to_bits(),
          "sum_sq diverged at len {len} seed {seed}"
        );
        assert_eq!(
          dot_wide(&a, &b).to_bits(),
          dot_wide_scalar(&a, &b).to_bits(),
          "dot_wide diverged at len {len} seed {seed}"
        );
      }
    }
  }

  #[test]
  fn kernels_compute_the_right_values() {
    // Hand-checked: a = [1, 2, 3], b = [0, 4, 6] → l2 = 1 + 4 + 9 = 14;
    // sum_sq(a) = 14; dot = 0 + 8 + 18 = 26.
    let a = [1.0f32, 2.0, 3.0];
    let b = [0.0f32, 4.0, 6.0];
    assert_eq!(l2_sq(&a, &b), 14.0);
    assert_eq!(sum_sq(&a), 14.0);
    assert_eq!(dot_wide(&a, &b), 26.0);
  }
}
