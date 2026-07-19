//! Pluggable embeddings (§10 / user decision: local-first, pluggable).

use crate::normalize;

/// Anything that turns text into a fixed-dimension unit vector. Implementations must be
/// deterministic: the index stores vectors, and queries must embed identically across runs.
pub trait Embedder {
  fn dim(&self) -> usize;
  fn embed(&self, text: &str) -> Vec<f32>;
}

/// The built-in default: deterministic lexical feature hashing. Tokens (split on non-alphanumeric
/// boundaries *and* camelCase humps, lowercased) are hashed into `dim` signed buckets and the
/// result unit-normalized — classic hashing-trick bag-of-tokens. It measures *lexical* similarity
/// (shared identifiers/words), which is what makes `search "resolve import path"` find
/// `resolve_import_path`; it is not a semantic neural model, and does not pretend to be.
pub struct LexicalEmbedder {
  dim: usize,
}

impl LexicalEmbedder {
  pub fn new(dim: usize) -> Self {
    Self { dim: dim.max(8) }
  }
}

impl Default for LexicalEmbedder {
  fn default() -> Self {
    Self::new(256)
  }
}

impl Embedder for LexicalEmbedder {
  fn dim(&self) -> usize {
    self.dim
  }

  fn embed(&self, text: &str) -> Vec<f32> {
    let mut v = vec![0.0f32; self.dim];
    for token in tokenize(text) {
      // Two independent hashes per token: a spurious similarity then needs two simultaneous
      // bucket collisions with agreeing signs, quashing the single-collision false positives a
      // one-hash sketch produces on short texts.
      let h1 = fnv1a64(token.as_bytes());
      let h2 = h1.wrapping_mul(0x9E37_79B9_7F4A_7C15).rotate_left(31);
      for h in [h1, h2] {
        let bucket = (h % self.dim as u64) as usize;
        let sign = if (h >> 32) & 1 == 0 { 1.0 } else { -1.0 };
        v[bucket] += sign;
      }
    }
    normalize(&mut v);
    v
  }
}

/// Split on non-alphanumeric boundaries and lower→Upper camel humps; lowercase every token.
fn tokenize(text: &str) -> Vec<String> {
  let mut tokens = Vec::new();
  let mut current = String::new();
  let mut prev_lower = false;
  for c in text.chars() {
    if c.is_alphanumeric() {
      if c.is_uppercase() && prev_lower && !current.is_empty() {
        tokens.push(std::mem::take(&mut current));
      }
      current.extend(c.to_lowercase());
      prev_lower = c.is_lowercase() || c.is_numeric();
    } else {
      if !current.is_empty() {
        tokens.push(std::mem::take(&mut current));
      }
      prev_lower = false;
    }
  }
  if !current.is_empty() {
    tokens.push(current);
  }
  tokens
}

fn fnv1a64(bytes: &[u8]) -> u64 {
  let mut hash = 0xcbf2_9ce4_8422_2325u64;
  for &b in bytes {
    hash ^= b as u64;
    hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
  }
  hash
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::l2_sq;

  #[test]
  fn embedding_is_deterministic_and_normalized() {
    let e = LexicalEmbedder::default();
    let a = e.embed("resolve_import_path");
    let b = e.embed("resolve_import_path");
    assert_eq!(a, b);
    let norm: f32 = a.iter().map(|x| x * x).sum();
    assert!((norm - 1.0).abs() < 1e-5, "unit norm, got {norm}");
  }

  #[test]
  fn camel_and_snake_forms_share_tokens() {
    let e = LexicalEmbedder::default();
    let snake = e.embed("resolve_import_path");
    let camel = e.embed("resolveImportPath");
    let other = e.embed("segment directory binary search");
    assert!(
      l2_sq(&snake, &camel) < l2_sq(&snake, &other),
      "token splitting must bridge naming conventions"
    );
  }

  #[test]
  fn related_text_is_closer_than_unrelated() {
    let e = LexicalEmbedder::default();
    let query = e.embed("import path resolution");
    let related = e.embed("resolve_import_path fn resolve import path table");
    let unrelated = e.embed("hamming distance popcount quantizer");
    assert!(l2_sq(&query, &related) < l2_sq(&query, &unrelated));
  }
}
