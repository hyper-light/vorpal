//! Byte trigrams: the primitive under the text tier.
//!
//! A trigram is every overlapping 3-byte window of a file, packed into a 24-bit key
//! (`a << 16 | b << 8 | c`) — injective, so the key is its own collision-free hash. Beside
//! each (trigram, file) posting the tier keeps an 8-bit Bloom mask of the bytes that follow
//! that trigram anywhere in the file, so a query literal `abcd` can reject a file that holds
//! `abc` and `bcd` but never `abcd`: the planner asks for `abc` with expected next byte `d`.
//!
//! Extraction is case-sensitive: ast-grep patterns and the structural tools compare tokens
//! byte-for-byte, and a case-insensitive regex's folded parts compile to classes that promise
//! no literal at all, so a lowercase pass would serve nothing today (tgrep pays it for `-i`).
//!
//! Ported from microsoft/tgrep's `trigram.rs` / `query.rs` (MIT), reduced to what the tier uses.

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};

/// Pack three bytes into the 24-bit trigram key.
#[inline]
pub const fn key(a: u8, b: u8, c: u8) -> u32 {
  ((a as u32) << 16) | ((b as u32) << 8) | (c as u32)
}

/// The Bloom bit of a byte that follows a trigram: `1 << ((b * 0x9E) >> 5 & 7)`.
#[inline]
pub const fn next_bit(b: u8) -> u8 {
  1u8 << ((b.wrapping_mul(0x9E) >> 5) & 0x07)
}

/// Whether a posting's next-byte mask admits `expected` (`None` admits everything).
#[inline]
pub fn mask_admits(mask: u8, expected: Option<u8>) -> bool {
  match expected {
    Some(b) => mask & next_bit(b) != 0,
    None => true,
  }
}

/// A hasher for trigram keys: the key is already collision-free, so a multiply-xorshift is
/// enough. The shift is mandatory: hashbrown takes bucket bits from the low end, and the low
/// bits of `key * K` depend only on the trigram's last byte.
#[derive(Default, Clone, Copy)]
pub struct TrigramHasher(u64);

impl Hasher for TrigramHasher {
  #[inline]
  fn finish(&self) -> u64 {
    self.0
  }
  #[inline]
  fn write(&mut self, bytes: &[u8]) {
    for &b in bytes {
      self.write_u32(b as u32);
    }
  }
  #[inline]
  fn write_u32(&mut self, value: u32) {
    let mixed = (value as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    self.0 = mixed ^ (mixed >> 32);
  }
}

/// A map from trigram key to next-byte mask, hashed by [`TrigramHasher`].
pub type MaskMap = HashMap<u32, u8, BuildHasherDefault<TrigramHasher>>;

/// The distinct trigrams of `bytes` with their OR'd next-byte masks, appended to `out` as
/// `(key, mask)` pairs sorted by key. `scratch` is a reusable map (cleared on entry); files
/// shorter than three bytes contribute nothing.
pub fn extract(bytes: &[u8], scratch: &mut MaskMap, out: &mut Vec<(u32, u8)>) {
  scratch.clear();
  if bytes.len() < 3 {
    return;
  }
  for (i, w) in bytes.windows(3).enumerate() {
    let k = key(w[0], w[1], w[2]);
    let mask = bytes.get(i + 3).map(|&b| next_bit(b)).unwrap_or(0);
    *scratch.entry(k).or_insert(0) |= mask;
  }
  let start = out.len();
  out.extend(scratch.iter().map(|(&k, &m)| (k, m)));
  out[start..].sort_unstable_by_key(|&(k, _)| k);
}

/// A posting packed for sorting and bucket encoding: `key << 32 | ordinal << 8 | mask`. The
/// natural u64 order is exactly (key, ordinal); ordinals are file positions within a bucket
/// (< 2^24 by the bucket law).
#[inline]
pub const fn pack(key: u32, ordinal: u32, mask: u8) -> u64 {
  ((key as u64) << 32) | ((ordinal as u64) << 8) | (mask as u64)
}

/// [`extract`], but appending packed postings for file `ordinal` straight into a bucket's
/// sort buffer — no per-file vector between extraction and encoding.
pub fn extract_packed(bytes: &[u8], ordinal: u32, scratch: &mut MaskMap, out: &mut Vec<u64>) {
  scratch.clear();
  if bytes.len() < 3 {
    return;
  }
  for (i, w) in bytes.windows(3).enumerate() {
    let k = key(w[0], w[1], w[2]);
    let mask = bytes.get(i + 3).map(|&b| next_bit(b)).unwrap_or(0);
    *scratch.entry(k).or_insert(0) |= mask;
  }
  out.reserve(scratch.len());
  out.extend(scratch.iter().map(|(&k, &m)| pack(k, ordinal, m)));
}

/// One AND term of a text plan: the file must hold `key`, and if `expected_next` is set the
/// byte after some occurrence must be that byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlanTerm {
  pub key: u32,
  pub expected_next: Option<u8>,
}

/// The AND plan for a set of required literals: every 3-byte window of every literal, each
/// carrying the byte that follows it inside the literal. Windows that recur with different
/// following bytes drop their expectation (a file may hold either). Returns `None` when no
/// literal is at least three bytes long — nothing can be pruned, scan everything.
pub fn plan(literals: &[&[u8]]) -> Option<Vec<PlanTerm>> {
  let mut terms: Vec<PlanTerm> = Vec::new();
  for lit in literals {
    if lit.len() < 3 {
      continue;
    }
    for (i, w) in lit.windows(3).enumerate() {
      terms.push(PlanTerm {
        key: key(w[0], w[1], w[2]),
        expected_next: lit.get(i + 3).copied(),
      });
    }
  }
  if terms.is_empty() {
    return None;
  }
  terms.sort_unstable_by_key(|t| (t.key, t.expected_next));
  let mut out: Vec<PlanTerm> = Vec::with_capacity(terms.len());
  for t in terms {
    match out.last_mut() {
      Some(last) if last.key == t.key => {
        if last.expected_next != t.expected_next {
          last.expected_next = None;
        }
      }
      _ => out.push(t),
    }
  }
  Some(out)
}

#[cfg(test)]
mod test {
  use super::*;

  #[test]
  fn key_packs_bytes_and_next_bit_is_one_hot() {
    assert_eq!(key(b't', b'h', b'e'), 0x74_68_65);
    for b in 0..=255u8 {
      assert_eq!(next_bit(b).count_ones(), 1);
    }
    assert!(mask_admits(next_bit(b'x'), Some(b'x')));
    assert!(mask_admits(0, None));
  }

  #[test]
  fn extract_dedups_and_ors_masks() {
    let mut scratch = MaskMap::default();
    let mut out = Vec::new();
    extract(b"abcabd", &mut scratch, &mut out);
    // windows: abc(a) bca(b) cab(d) abd(-)
    let abc = out.iter().find(|(k, _)| *k == key(b'a', b'b', b'c')).unwrap();
    assert_eq!(abc.1, next_bit(b'a'));
    assert_eq!(out.len(), 4);
    assert!(out.windows(2).all(|w| w[0].0 < w[1].0));
    out.clear();
    extract(b"ab", &mut scratch, &mut out);
    assert!(out.is_empty());
    extract(b"aaaa", &mut scratch, &mut out);
    assert_eq!(out, vec![(key(b'a', b'a', b'a'), next_bit(b'a'))]);
  }

  #[test]
  fn plan_windows_and_conflict_rule() {
    let p = plan(&[b"hello"]).unwrap();
    assert_eq!(p.len(), 3);
    let hel = p.iter().find(|t| t.key == key(b'h', b'e', b'l')).unwrap();
    assert_eq!(hel.expected_next, Some(b'l'));
    let llo = p.iter().find(|t| t.key == key(b'l', b'l', b'o')).unwrap();
    assert_eq!(llo.expected_next, None);
    // `abcx` and `abcy` both want `abc`; the expectation is dropped, the key kept once.
    let p = plan(&[b"abcx", b"abcy"]).unwrap();
    let abc: Vec<_> = p.iter().filter(|t| t.key == key(b'a', b'b', b'c')).collect();
    assert_eq!(abc.len(), 1);
    assert_eq!(abc[0].expected_next, None);
    assert!(plan(&[b"ab", b""]).is_none());
    assert!(plan(&[]).is_none());
  }
}
