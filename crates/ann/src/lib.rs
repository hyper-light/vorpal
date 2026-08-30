//! `vorpal-ann` — the semantic search tier (§10), adaptive from a handful of vectors to large
//! corpora (ConfigForN: nothing pre-sized).
//!
//! Three search tiers, chosen per corpus size, every one ending in an **exact rerank** so the
//! final ordering is always computed on full-precision vectors (approximation only affects which
//! candidates are considered, never how they are ranked):
//! - **flat exact** — small corpora: a full scan *is* the fastest correct algorithm;
//! - **flat quantized** — 1-bit sign codes + Hamming pre-filter (u64 popcount) → exact rerank;
//! - **Vamana** — greedy beam search over a robust-pruned graph (seeded, deterministic build)
//!   → exact rerank.
//!
//! Embeddings are **pluggable** ([`Embedder`]); the built-in default is a deterministic,
//! dependency-free *lexical* feature-hashing embedder — honest about what it is (identifier /
//! token similarity, not a neural model). Neural embedders plug in behind the same trait.
//! RaBitQ-grade quantization and IVF sharding are the §10 successors behind the same seams.

mod embed;
mod index;
mod qmatrix;
mod quant;
mod scan;
mod vamana;

pub use embed::{Embedder, LEXICAL_EMBED_VERSION, LexicalEmbedder, ModelProvenance, tokenize};
pub use index::{AnnConfig, AnnIndex};
pub use quant::SignQuantizer;
pub use scan::exhaustive_semantic;

/// Squared L2 distance. On unit-normalized vectors this orders identically to cosine distance.
pub(crate) fn l2_sq(a: &[f32], b: &[f32]) -> f32 {
  a.iter()
    .zip(b)
    .map(|(x, y)| {
      let d = x - y;
      d * d
    })
    .sum()
}

/// L2-normalize in place (no-op for the zero vector).
pub(crate) fn normalize(v: &mut [f32]) {
  let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
  if norm > 0.0 {
    for x in v.iter_mut() {
      *x /= norm;
    }
  }
}

/// Deterministic xorshift64* — build order and tests must be reproducible (bit-identical
/// rebuilds are part of the §10 acceptance bar), so no ambient randomness anywhere.
#[derive(Clone)]
pub(crate) struct Rng(u64);

impl Rng {
  pub fn new(seed: u64) -> Self {
    Self(seed.max(1))
  }

  pub fn next(&mut self) -> u64 {
    let mut x = self.0;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    self.0 = x;
    x.wrapping_mul(0x2545_F491_4F6C_DD1D)
  }

  /// Uniform in `[0, bound)`.
  pub fn below(&mut self, bound: usize) -> usize {
    (self.next() % bound.max(1) as u64) as usize
  }
}

/// Build-phase telemetry, VORPAL_PHASE_TRACE-gated like the pipeline's phase stamps.
pub(crate) fn trace(label: &str) {
  if std::env::var_os("VORPAL_PHASE_TRACE").is_some() {
    eprintln!("[ann] {label}");
  }
}
