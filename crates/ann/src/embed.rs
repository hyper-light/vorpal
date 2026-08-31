//! Pluggable embeddings (§10 / user decision: local-first, pluggable).

use crate::normalize;

/// Anything that turns text into a fixed-dimension unit vector. Implementations must be
/// deterministic: the index stores vectors, and queries must embed identically across runs.
pub trait Embedder {
  fn dim(&self) -> usize;
  fn embed(&self, text: &str) -> Vec<f32>;
}

/// Embedding-semantics version of the lexical hasher: bumped whenever tokenization, hashing,
/// weighting, or normalization changes, so vectors persisted under older semantics never
/// silently compare against fresh query vectors.
// v2 (2026-08-31): node embeddings tokenize TREE-RELATIVE names — the canonicalized
// mount prefix used to enter File-node vectors, making near-tie rankings a function of
// WHERE the tree was checked out (macOS vs CI temp layouts split pinned retrieval
// baselines one rank apart). Vectors are now a pure function of the tree's content.
pub const LEXICAL_EMBED_VERSION: u32 = 2;

/// The complete provenance of an embedding model — the public configuration contract
/// (IMPROVEMENTS #9): everything a consumer needs to decide whether two vector sets are
/// comparable. Persisted beside the vector tier and validated before the tier is trusted;
/// any mismatch routes queries to the exact fallback until a re-warm rebuilds under the
/// active model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelProvenance {
  /// Stable model identifier.
  pub model_id: String,
  /// Vector dimensionality.
  pub dim: usize,
  /// Normalization applied to every vector.
  pub normalization: String,
  /// Embedding-semantics version (see [`LEXICAL_EMBED_VERSION`]).
  pub version: u32,
  /// Whether the model carries learned weights. `false` = deterministic, offline,
  /// bit-reproducible — the honest label the default carries; a learned adapter must say
  /// `true` and never masquerade as the deterministic default.
  pub learned: bool,
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

  /// This model's complete provenance (deterministic lexical hashing, L2-normalized).
  pub fn provenance(&self) -> ModelProvenance {
    ModelProvenance {
      model_id: "lexical-hash".to_string(),
      dim: self.dim,
      normalization: "l2".to_string(),
      version: LEXICAL_EMBED_VERSION,
      learned: false,
    }
  }
}

impl Default for LexicalEmbedder {
  fn default() -> Self {
    Self::new(256)
  }
}

impl LexicalEmbedder {
  /// Embed the concatenation of `parts` exactly as if they were joined with spaces — without
  /// materializing the joined string. Part boundaries are token boundaries (a space never
  /// survives tokenization), so `embed_parts(&[a, b])` ≡ `embed(&format!("{a} {b}"))`, which
  /// the tests pin. This is the per-node indexing path: zero intermediate allocations.
  pub fn embed_parts(&self, parts: &[&str]) -> Vec<f32> {
    let mut v = vec![0.0f32; self.dim];
    self.embed_parts_into(parts, &mut v);
    v
  }

  /// [`LexicalEmbedder::embed_parts`] into a caller-owned slice (`len == dim`), zeroed first —
  /// the bulk-indexing path fills one flat matrix row per node with zero per-row allocation.
  pub fn embed_parts_into(&self, parts: &[&str], out: &mut [f32]) {
    debug_assert_eq!(out.len(), self.dim);
    out.fill(0.0);
    for part in parts {
      for_each_token_hash(part, |h1| accumulate(out, h1));
    }
    normalize(out);
  }
}

/// Scatter one token hash into the signed feature buckets.
///
/// Two independent hashes per token: a spurious similarity then needs two simultaneous bucket
/// collisions with agreeing signs, quashing the single-collision false positives a one-hash
/// sketch produces on short texts.
fn accumulate(v: &mut [f32], h1: u64) {
  let h2 = h1.wrapping_mul(0x9E37_79B9_7F4A_7C15).rotate_left(31);
  for h in [h1, h2] {
    let bucket = (h % v.len() as u64) as usize;
    let sign = if (h >> 32) & 1 == 0 { 1.0 } else { -1.0 };
    v[bucket] += sign;
  }
}

impl Embedder for LexicalEmbedder {
  fn dim(&self) -> usize {
    self.dim
  }

  fn embed(&self, text: &str) -> Vec<f32> {
    let mut v = vec![0.0f32; self.dim];
    // Hash tokens as they stream — no per-token `String` (this runs once per KG node on every
    // index build; the materializing path allocated ~20 strings per node).
    for_each_token_hash(text, |h1| accumulate(&mut v, h1));
    normalize(&mut v);
    v
  }
}

/// Stream `fnv1a64(token)` for each token of `text` without materializing token strings —
/// byte-identical to `tokenize(text)` + `fnv1a64` (the same boundary rules feed the same
/// lowercased UTF-8 bytes into the same hash), which the tests pin down.
fn for_each_token_hash(text: &str, mut emit: impl FnMut(u64)) {
  const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
  let mut hash = FNV_OFFSET;
  let mut empty = true;
  let mut prev_lower = false;
  let mut utf8 = [0u8; 4];
  for c in text.chars() {
    if c.is_alphanumeric() {
      if c.is_uppercase() && prev_lower && !empty {
        emit(hash);
        hash = FNV_OFFSET;
      }
      for lower in c.to_lowercase() {
        for &b in lower.encode_utf8(&mut utf8).as_bytes() {
          hash ^= b as u64;
          hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
        }
      }
      empty = false;
      prev_lower = c.is_lowercase() || c.is_numeric();
    } else {
      if !empty {
        emit(hash);
        hash = FNV_OFFSET;
        empty = true;
      }
      prev_lower = false;
    }
  }
  if !empty {
    emit(hash);
  }
}

/// Split on non-alphanumeric boundaries and lower→Upper camel humps; lowercase every token —
/// the shared token model for embedding and for name matching (so `resolveImportPath`,
/// `resolve_import_path`, and `resolve import path` all meet on the same tokens).
pub fn tokenize(text: &str) -> Vec<String> {
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

#[cfg(test)]
mod tests {
  use super::*;
  use crate::l2_sq;

  /// The whole-token FNV-1a the streaming hasher must agree with.
  fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for &b in bytes {
      hash ^= b as u64;
      hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
    }
    hash
  }

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
  fn embed_parts_equals_embed_of_space_joined_text() {
    let e = LexicalEmbedder::default();
    for parts in [
      [
        "build_index",
        "build_index",
        "fn build_index(src)",
        "lib.rs",
      ],
      ["", "x", "", "üñïçødé Straße"],
      ["a b", "c", "CamelCase9", "-"],
    ] {
      assert_eq!(
        e.embed_parts(&parts),
        e.embed(&parts.join(" ")),
        "parts: {parts:?}"
      );
    }
  }

  #[test]
  fn streamed_token_hashes_match_materialized_tokenize() {
    // The no-alloc hash stream must agree with tokenize() + fnv1a64 byte-for-byte — including
    // camel humps, digits, Unicode lowercasing (İ expands to two chars), and delimiters.
    for text in [
      "resolveImportPath",
      "HTTPServer2Config",
      "a__b--c9 ÎмяȘİx",
      "  leading and trailing  ",
      "ONLYCAPS",
      "",
      "ΣΊΣΥΦΟΣ dès Straße",
    ] {
      let streamed = {
        let mut hashes = Vec::new();
        for_each_token_hash(text, |h| hashes.push(h));
        hashes
      };
      let materialized: Vec<u64> = tokenize(text)
        .iter()
        .map(|token| fnv1a64(token.as_bytes()))
        .collect();
      assert_eq!(streamed, materialized, "text: {text:?}");
    }
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
