//! Process-wide string interner for resolution-time names and paths.
//!
//! At kernel scale a link pass carries ~6.8M references and ~2.4M symbols whose names and
//! paths draw from a vocabulary of only a few hundred thousand distinct strings — owning a
//! heap `String` per occurrence dominated peak memory. Interning stores each distinct string
//! once (leaked, so lookups hand out `&'static str`) and every occurrence becomes a `u32`
//! id: references and symbols collapse to PODs, and resolution compares integers instead of
//! hashing strings.
//!
//! The table is append-only and read-mostly (writes only on first sight of a string), and
//! **sharded 64 ways** by string hash: parallel link passes make ~10M interner calls across
//! all worker threads, and a single lock measurably serialized them (pthread rwlock syscalls
//! under contention). Each id carries its shard in its high bits, so resolving text touches
//! only that shard's lock. Memory is bounded by the union of vocabularies ever interned in
//! the process — the same boundedness argument as the grammar-kind interner. Interned ids are
//! process-internal and never reach any output, so run-to-run determinism is unaffected.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::num::NonZeroU32;
use std::sync::{OnceLock, RwLock};

/// An interned string (name, path, or qualifier). Compare, hash, and copy freely — it's a
/// `u32` (6 shard bits + 26 per-shard index bits: ~67M strings per shard), stored biased by
/// one so `Option<NameId>` gets the niche and costs 4 bytes, not 8.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NameId(NonZeroU32);

impl NameId {
  /// The id's raw bit pattern — for process-private serialization (the reference spill).
  /// Bits are never 0 and never stable across processes; roundtrip with [`NameId::from_bits`]
  /// within one process only.
  pub fn to_bits(self) -> u32 {
    self.0.get()
  }

  /// Rebuild an id from [`NameId::to_bits`] output; `None` for 0 (the "absent" sentinel).
  pub fn from_bits(bits: u32) -> Option<Self> {
    NonZeroU32::new(bits).map(NameId)
  }

  fn from_raw(raw: u32) -> Self {
    // `raw + 1` cannot overflow: per-shard indices are held strictly below `INDEX_MASK`.
    NameId(NonZeroU32::new(raw + 1).expect("interner id overflow"))
  }

  fn raw(self) -> u32 {
    self.0.get() - 1
  }
}

const SHARD_BITS: u32 = 6;
const SHARDS: usize = 1 << SHARD_BITS;
const INDEX_BITS: u32 = 32 - SHARD_BITS;
const INDEX_MASK: u32 = (1 << INDEX_BITS) - 1;

#[derive(Default)]
struct Shard {
  by_text: HashMap<&'static str, u32>,
  by_index: Vec<&'static str>,
}

fn shards() -> &'static [RwLock<Shard>; SHARDS] {
  static INTERNER: OnceLock<[RwLock<Shard>; SHARDS]> = OnceLock::new();
  INTERNER.get_or_init(|| std::array::from_fn(|_| RwLock::new(Shard::default())))
}

fn shard_of(text: &str) -> usize {
  let mut hasher = std::collections::hash_map::DefaultHasher::new();
  text.hash(&mut hasher);
  (hasher.finish() as usize) & (SHARDS - 1)
}

/// Intern `text`, returning its stable id (allocating and leaking the string on first sight).
pub fn intern(text: &str) -> NameId {
  let shard_index = shard_of(text);
  let lock = &shards()[shard_index];
  if let Some(&index) = lock.read().unwrap().by_text.get(text) {
    return NameId::from_raw(((shard_index as u32) << INDEX_BITS) | index);
  }
  let mut shard = lock.write().unwrap();
  if let Some(&index) = shard.by_text.get(text) {
    return NameId::from_raw(((shard_index as u32) << INDEX_BITS) | index);
  }
  let leaked: &'static str = Box::leak(text.to_string().into_boxed_str());
  let index = shard.by_index.len() as u32;
  assert!(index < INDEX_MASK, "interner shard overflow");
  shard.by_index.push(leaked);
  shard.by_text.insert(leaked, index);
  NameId::from_raw(((shard_index as u32) << INDEX_BITS) | index)
}

/// The id of `text` iff it was ever interned — for speculative probes (path-form import
/// joins) that must not grow the table with strings nothing will ever look up again.
pub fn peek(text: &str) -> Option<NameId> {
  let shard_index = shard_of(text);
  shards()[shard_index]
    .read()
    .unwrap()
    .by_text
    .get(text)
    .map(|&index| NameId::from_raw(((shard_index as u32) << INDEX_BITS) | index))
}

/// The interned text of `id`.
pub fn text_of(id: NameId) -> &'static str {
  let raw = id.raw();
  let shard_index = (raw >> INDEX_BITS) as usize;
  shards()[shard_index].read().unwrap().by_index[(raw & INDEX_MASK) as usize]
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn interning_is_stable_and_peek_never_inserts() {
    let a = intern("alpha_symbol");
    let b = intern("alpha_symbol");
    assert_eq!(a, b);
    assert_eq!(text_of(a), "alpha_symbol");
    assert_ne!(a, intern("beta_symbol"));

    assert_eq!(peek("alpha_symbol"), Some(a));
    assert_eq!(peek("never_interned_probe_xyz"), None);
    // And the probe really did not insert.
    assert_eq!(peek("never_interned_probe_xyz"), None);
  }
}
