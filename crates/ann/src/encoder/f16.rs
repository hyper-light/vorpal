//! IEEE 754 binary16 ⇄ binary32 conversion, owned (semantic-tier Stage 6
//! packaging: the f16 weight variant). Round-to-nearest-even on narrowing — the
//! IEEE default and the deterministic choice; NaNs narrow to the canonical quiet
//! half NaN and widen quiet. The exhaustive oracle below round-trips every one of
//! the 65 536 half bit patterns.

/// Narrow one f32 to half bits, round-to-nearest-even.
pub fn f32_to_f16_bits(value: f32) -> u16 {
  let bits = value.to_bits();
  let sign = ((bits >> 16) & 0x8000) as u16;
  let exponent = ((bits >> 23) & 0xff) as i32;
  let mantissa = bits & 0x007f_ffff;
  if exponent == 0xff {
    // Inf stays inf; NaN narrows to the canonical quiet NaN.
    return if mantissa == 0 { sign | 0x7c00 } else { sign | 0x7e00 };
  }
  let unbiased = exponent - 127;
  if unbiased > 15 {
    return sign | 0x7c00; // overflow → signed infinity
  }
  if unbiased >= -14 {
    // Normal half: keep 10 mantissa bits, RNE on the 13 dropped ones. A rounding
    // carry rolls cleanly into the exponent field (up to infinity) by layout.
    let kept = mantissa >> 13;
    let rest = mantissa & 0x1fff;
    let half = (((unbiased + 15) as u32) << 10) | kept;
    let round_up = rest > 0x1000 || (rest == 0x1000 && (kept & 1) == 1);
    return sign | (half + round_up as u32) as u16;
  }
  if unbiased < -25 {
    return sign; // underflow → signed zero (2⁻²⁵ itself ties to even = zero)
  }
  // Subnormal half: explicit leading 1, shifted into the 10-bit field, RNE. A
  // carry rolls into the smallest normal — again layout-correct.
  let mantissa = mantissa | 0x0080_0000;
  let shift = (13 - 14 - unbiased) as u32; // bits dropped: 13 + (-14 - unbiased)
  let kept = mantissa >> shift;
  let rest = mantissa & ((1u32 << shift) - 1);
  let halfway = 1u32 << (shift - 1);
  let round_up = rest > halfway || (rest == halfway && (kept & 1) == 1);
  sign | (kept + round_up as u32) as u16
}

/// Widen half bits to f32 (exact — every half value is representable).
pub fn f16_bits_to_f32(bits: u16) -> f32 {
  let sign = ((bits & 0x8000) as u32) << 16;
  let exponent = ((bits >> 10) & 0x1f) as u32;
  let mantissa = (bits & 0x03ff) as u32;
  let value = match (exponent, mantissa) {
    (0, 0) => sign,
    (0, _) => {
      // Subnormal: normalize into f32's implicit-1 form.
      let mut biased = 113u32; // exponent of 2^-14 in f32 bias
      let mut wide = mantissa << 13;
      while wide & 0x0080_0000 == 0 {
        wide <<= 1;
        biased -= 1;
      }
      sign | (biased << 23) | (wide & 0x007f_ffff)
    }
    (0x1f, 0) => sign | 0x7f80_0000,
    (0x1f, _) => sign | 0x7fc0_0000 | (mantissa << 13), // quiet NaN, payload kept
    (e, m) => sign | ((e + 127 - 15) << 23) | (m << 13),
  };
  f32::from_bits(value)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn known_values_narrow_exactly() {
    assert_eq!(f32_to_f16_bits(0.0), 0x0000);
    assert_eq!(f32_to_f16_bits(-0.0), 0x8000);
    assert_eq!(f32_to_f16_bits(1.0), 0x3c00);
    assert_eq!(f32_to_f16_bits(0.5), 0x3800);
    assert_eq!(f32_to_f16_bits(-2.0), 0xc000);
    assert_eq!(f32_to_f16_bits(65504.0), 0x7bff, "largest finite half");
    assert_eq!(f32_to_f16_bits(65520.0), 0x7c00, "just past the cliff → inf");
    assert_eq!(f32_to_f16_bits(f32::INFINITY), 0x7c00);
    assert_eq!(f32_to_f16_bits(f32::NEG_INFINITY), 0xfc00);
    assert_eq!(f32_to_f16_bits(5.960_464_5e-8), 0x0001, "smallest subnormal");
    assert_eq!(f32_to_f16_bits(2.0f32.powi(-25)), 0x0000, "halfway ties to even zero");
    // 1 + 2^-11 sits exactly between 0x3c00 and 0x3c01 → ties to even (0x3c00).
    assert_eq!(f32_to_f16_bits(1.0 + 2.0f32.powi(-11)), 0x3c00);
    // Nudge past the tie and it must round up.
    assert_eq!(f32_to_f16_bits(1.0 + 2.0f32.powi(-11) + 2.0f32.powi(-20)), 0x3c01);
    assert!(f16_bits_to_f32(f32_to_f16_bits(f32::NAN)).is_nan());
  }

  #[test]
  fn widening_is_exact_and_round_trips_every_half() {
    assert_eq!(f16_bits_to_f32(0x3c00), 1.0);
    assert_eq!(f16_bits_to_f32(0x3800), 0.5);
    assert_eq!(f16_bits_to_f32(0x7bff), 65504.0);
    assert_eq!(f16_bits_to_f32(0x0001), 5.960_464_5e-8);
    assert_eq!(f16_bits_to_f32(0x7c00), f32::INFINITY);
    // EXHAUSTIVE: every half value widens then narrows back to its own bits
    // (NaNs compare as NaN-ness — payloads canonicalize).
    for bits in 0..=u16::MAX {
      let wide = f16_bits_to_f32(bits);
      if wide.is_nan() {
        assert!(f16_bits_to_f32(f32_to_f16_bits(wide)).is_nan());
      } else {
        assert_eq!(
          f32_to_f16_bits(wide),
          bits,
          "round-trip broke at half bit pattern {bits:#06x}"
        );
      }
    }
  }
}
