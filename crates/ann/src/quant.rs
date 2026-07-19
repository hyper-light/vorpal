//! 1-bit sign quantization + Hamming distance (§10.1's shape; RaBitQ-grade codes are the
//! drop-in successor behind the same interface).

/// Quantizes vectors to one sign bit per dimension, packed into u64 words; Hamming distance over
/// the packed words (popcount) approximates angular distance and is the cheap pre-filter — the
/// exact rerank downstream makes candidate quality a recall knob, never a correctness one.
pub struct SignQuantizer {
  dim: usize,
}

impl SignQuantizer {
  pub fn new(dim: usize) -> Self {
    Self { dim }
  }

  /// Packed words per code.
  pub fn words(&self) -> usize {
    self.dim.div_ceil(64)
  }

  pub fn encode(&self, v: &[f32]) -> Vec<u64> {
    let mut code = vec![0u64; self.words()];
    for (i, &x) in v.iter().take(self.dim).enumerate() {
      if x >= 0.0 {
        code[i / 64] |= 1 << (i % 64);
      }
    }
    code
  }

  pub fn hamming(a: &[u64], b: &[u64]) -> u32 {
    a.iter().zip(b).map(|(x, y)| (x ^ y).count_ones()).sum()
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn hamming_tracks_agreement() {
    let q = SignQuantizer::new(128);
    let a = q.encode(&vec![1.0; 128]);
    let b = q.encode(&vec![-1.0; 128]);
    assert_eq!(SignQuantizer::hamming(&a, &a), 0);
    assert_eq!(SignQuantizer::hamming(&a, &b), 128);

    let mut half = vec![1.0f32; 128];
    for x in half.iter_mut().take(64) {
      *x = -1.0;
    }
    let h = q.encode(&half);
    assert_eq!(SignQuantizer::hamming(&a, &h), 64);
  }
}
