//! Near-clone signatures: a one-permutation MinHash sketch per callable definition, captured
//! from the leaf-token stream of the SAME extraction walk that emits references — one hash
//! per token, one per shingle, no second pass.
//!
//! Token = a leaf node: anonymous leaves hash their kind id (the grammar's spelling of `(`,
//! `if`, `+`), named leaves hash kind id + text (identifiers, literals); comments are skipped.
//! Shingles are 3 consecutive tokens. The sketch is 64 bins over the shingle hash space, each
//! holding its minimum; empty bins are densified from the next occupied bin to the right
//! (rotation, offset by distance), and one byte per bin is persisted (b-bit MinHash). The
//! Jaccard similarity of two definitions' shingle sets is estimated at link time as the
//! fraction of equal bins (bias-corrected for the byte width) — see `similar.rs`.
//!
//! Only definitions with at least [`MIN_TOKENS`] tokens are signed: a one-line body has
//! nothing to be a near-clone of. Sketches are a pure function of the token stream and the
//! grammar (the hash seed), so equal bodies sign equally across files and builds.

use std::ops::Range;

use vorpal_core::{Doc, Node};

/// Folded into the extraction identity: any change to tokenisation, shingling, bin count,
/// or densification re-keys every product without a format bump.
pub const SIGNATURE_VERSION: u32 = 1;
/// Bins per sketch — one persisted byte each.
pub const BINS: usize = 64;
/// Tokens in a shingle.
const SHINGLE: usize = 3;
/// Smallest signed definition, in tokens.
pub const MIN_TOKENS: u32 = 32;
/// The shingle floor that [`MIN_TOKENS`] implies.
pub const MIN_SHINGLES: u32 = MIN_TOKENS - (SHINGLE as u32 - 1);
/// Rotation offset constant for densified bins (odd, so each distance shifts the byte).
const ROTATE: u64 = 0x9E37_79B9_7F4A_7C15;
const EMPTY: u64 = u64::MAX;

/// One definition's persisted sketch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sketch {
  /// Shingles hashed (with repeats) — a size prefilter at link time.
  pub shingles: u32,
  pub bins: [u8; BINS],
}

struct Acc {
  end: usize,
  window: [u64; SHINGLE],
  filled: usize,
  shingles: u32,
  bins: [u64; BINS],
}

impl Acc {
  fn push(&mut self, token: u64) {
    self.window.copy_within(1.., 0);
    self.window[SHINGLE - 1] = token;
    if self.filled < SHINGLE {
      self.filled += 1;
      if self.filled < SHINGLE {
        return;
      }
    }
    let [a, b, c] = self.window;
    // Order-sensitive mix of the three token hashes.
    let shingle = a
      .rotate_left(23)
      .wrapping_mul(ROTATE)
      .wrapping_add(b.rotate_left(47))
      .wrapping_mul(0xC2B2_AE3D_27D4_EB4F)
      ^ c;
    let shingle = shingle ^ (shingle >> 29);
    let bin = (shingle >> 58) as usize; // top 6 bits: 64 bins
    if shingle < self.bins[bin] {
      self.bins[bin] = shingle;
    }
    self.shingles = self.shingles.saturating_add(1);
  }

  fn sketch(&self) -> Sketch {
    let mut bins = [0u8; BINS];
    for (i, slot) in bins.iter_mut().enumerate() {
      // Densify: the nearest occupied bin to the right (cyclic), offset by its distance so
      // sketches that copied from different distances rarely agree by accident.
      let mut value = EMPTY;
      for t in 0..BINS {
        let source = self.bins[(i + t) % BINS];
        if source != EMPTY {
          value = source.wrapping_add((t as u64).wrapping_mul(ROTATE));
          break;
        }
      }
      *slot = value as u8;
    }
    Sketch {
      shingles: self.shingles,
      bins,
    }
  }
}

/// Streams a tree's leaf tokens into the sketches of the definitions that contain them.
/// Tokens must arrive in document order (the extraction DFS's order).
pub(crate) struct Signer {
  seed: u64,
  /// (span, entity index) for every signable definition, sorted by span start.
  spans: Vec<(Range<usize>, u32)>,
  acc: Vec<Acc>,
  next: usize,
  open: Vec<usize>,
  /// Per grammar kind id: 0 unknown, 1 comment, 2 token.
  kind_class: Vec<u8>,
  /// Leaves starting before this offset sit inside a comment subtree (some grammars give
  /// comments children — Rust's `line_comment` carries its `//` token) and are skipped.
  comment_until: usize,
}

impl Signer {
  /// `spans`: byte ranges of the callable definitions with their local entity indices.
  pub(crate) fn new(seed: u64, mut spans: Vec<(Range<usize>, u32)>) -> Self {
    spans.sort_by_key(|(range, entity)| (range.start, range.end, *entity));
    let acc = spans
      .iter()
      .map(|(range, _)| Acc {
        end: range.end,
        window: [0; SHINGLE],
        filled: 0,
        shingles: 0,
        bins: [EMPTY; BINS],
      })
      .collect();
    Self {
      seed,
      spans,
      acc,
      next: 0,
      open: Vec::new(),
      kind_class: Vec::new(),
      comment_until: 0,
    }
  }

  /// Start a new tree over the same definitions (an injected sub-tree parses under another
  /// grammar and restarts document order from its first range). Sketches accumulate.
  pub(crate) fn restart(&mut self, seed: u64) {
    self.seed = seed;
    self.next = 0;
    self.open.clear();
    self.kind_class.clear();
    self.comment_until = 0;
  }

  /// Feed one node of the document-order walk: comment subtrees are skipped whole, leaves
  /// become tokens, everything else is free.
  pub(crate) fn visit<D: Doc>(&mut self, node: &Node<'_, D>) {
    let kind_id = node.kind_id() as usize;
    if self.kind_class.len() <= kind_id {
      self.kind_class.resize(kind_id + 1, 0);
    }
    let class = match self.kind_class[kind_id] {
      0 => {
        let class = if node.kind().contains("comment") { 1 } else { 2 };
        self.kind_class[kind_id] = class;
        class
      }
      class => class,
    };
    let range = node.range();
    if class == 1 {
      self.comment_until = self.comment_until.max(range.end);
      return;
    }
    if range.start < self.comment_until || !node.is_leaf() {
      return;
    }
    let offset = range.start;
    self.advance(offset);
    if self.open.is_empty() {
      return;
    }
    let kind_mix = (kind_id as u64 + 1).wrapping_mul(ROTATE) ^ self.seed;
    let token = if node.is_named() {
      xxhash_rust::xxh3::xxh3_64_with_seed(node.text().as_bytes(), kind_mix)
    } else {
      kind_mix ^ (kind_mix >> 31)
    };
    for &i in &self.open {
      self.acc[i].push(token);
    }
  }

  fn advance(&mut self, offset: usize) {
    self.open.retain(|&i| self.acc[i].end > offset);
    while self.next < self.spans.len() && self.spans[self.next].0.start <= offset {
      if self.spans[self.next].0.end > offset {
        self.open.push(self.next);
      }
      self.next += 1;
    }
  }

  /// The signed definitions: (entity index, sketch), entity-ordered; unsigned (too small)
  /// definitions are absent.
  pub(crate) fn finish(self) -> Vec<(u32, Sketch)> {
    let mut out: Vec<(u32, Sketch)> = self
      .spans
      .iter()
      .zip(&self.acc)
      .filter(|(_, acc)| acc.shingles >= MIN_SHINGLES)
      .map(|((_, entity), acc)| (*entity, acc.sketch()))
      .collect();
    out.sort_by_key(|(entity, _)| *entity);
    out.dedup_by_key(|(entity, _)| *entity);
    out
  }
}

/// Estimated Jaccard similarity of two sketches: the equal-bin fraction, corrected for the
/// 1/256 chance that unequal minima share a byte. In [0, 1].
pub fn estimate(a: &[u8], b: &[u8]) -> f64 {
  debug_assert_eq!(a.len(), BINS);
  debug_assert_eq!(b.len(), BINS);
  let matches = a.iter().zip(b).filter(|(x, y)| x == y).count() as f64;
  let raw = matches / BINS as f64;
  ((raw - 1.0 / 256.0) / (1.0 - 1.0 / 256.0)).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
  use super::*;

  fn sign(source: &str, spans: Vec<(Range<usize>, u32)>) -> Vec<(u32, Sketch)> {
    use vorpal_core::tree_sitter::LanguageExt;
    let grep = vorpal_lang_registry::SgLang::from(vorpal_language::SupportLang::Rust).grep(source);
    let mut signer = Signer::new(7, spans);
    for node in grep.root().dfs() {
      signer.visit(&node);
    }
    signer.finish()
  }

  const BODY: &str = "fn alpha(a: u32, b: u32) -> u32 {\n  let mut s = a + b; // sum\n  if s > 10 { s = s / 2; } else { s = s * 3; }\n  while s < 100 { s += a; }\n  s - b\n}\n";

  #[test]
  fn identical_bodies_sign_identically_and_tiny_bodies_are_unsigned() {
    let renamed = BODY.replace("alpha", "beta");
    let source = format!("{BODY}\n{renamed}\nfn tiny() -> u32 {{ 1 }}\n");
    let b_start = BODY.len() + 1;
    let c_start = b_start + renamed.len() + 1;
    let spans = vec![
      (0..BODY.len(), 1),
      (b_start..b_start + renamed.len(), 2),
      (c_start..source.len() - 1, 3),
    ];
    let signed = sign(&source, spans);
    assert_eq!(signed.iter().map(|(e, _)| *e).collect::<Vec<_>>(), vec![1, 2]);
    let (a, b) = (&signed[0].1, &signed[1].1);
    assert!(a.shingles >= MIN_SHINGLES);
    // One renamed token (the name) touches at most three shingles.
    assert!(estimate(&a.bins, &b.bins) > 0.8, "{}", estimate(&a.bins, &b.bins));
    // Comments never contribute: dropping one leaves the sketch untouched.
    let uncommented = BODY.replace(" // sum", "");
    let alone = sign(&uncommented, vec![(0..uncommented.len(), 1)]);
    assert_eq!(alone[0].1, *a);
    // Deterministic across runs.
    assert_eq!(sign(&source, vec![(0..BODY.len(), 1)])[0].1, *a);
  }

  #[test]
  fn unrelated_bodies_estimate_low() {
    let other = "fn gamma(v: &[u8]) -> usize {\n  let mut n = 0;\n  for byte in v { if *byte == b'x' { n += 1; } }\n  match n { 0 => 7, _ => n * 2 }\n}\n";
    let a = sign(BODY, vec![(0..BODY.len(), 1)]);
    let b = sign(other, vec![(0..other.len(), 1)]);
    assert!(estimate(&a[0].1.bins, &b[0].1.bins) < 0.3);
  }
}
