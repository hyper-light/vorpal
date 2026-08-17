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
//! only that shard's lock. Interned ids are process-internal and never reach any output, so
//! run-to-run determinism is unaffected.
//!
//! **Lifecycle (embedded hosts)**: within one build session memory is bounded by the union
//! of vocabularies interned; across sessions in a long-lived host it grows monotonically —
//! indexing many distinct repos over weeks accumulates every distinct name ever seen.
//! Strings therefore live in per-shard **arenas, not leaks**: [`reclaim_all`] frees every
//! arena between sessions under an explicit safety contract (no prior [`NameId`] and no
//! `&str` from [`text_of`] may be used afterwards), and [`retained_bytes`] /
//! [`retained_strings`] let a host observe growth and decide when. One-shot processes (the
//! CLI) never need to call it — process exit is the reclaim.

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
  /// Owns every string the two maps borrow. `Box<str>` contents are heap-stable, so the
  /// vector may grow freely while `&'static str` borrows into the boxes circulate; entries
  /// drop only in [`reclaim_all`], whose contract guarantees no such borrow is still alive.
  arena: Vec<Box<str>>,
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

/// Intern `text`, returning its stable id (allocating into the shard arena on first sight).
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
  let boxed: Box<str> = text.to_string().into_boxed_str();
  // The box's heap contents are address-stable; extending the borrow to `'static` is sound
  // while the arena entry lives, which the `reclaim_all` contract guarantees for every
  // handed-out borrow.
  let interned: &'static str = unsafe { &*(boxed.as_ref() as *const str) };
  shard.arena.push(boxed);
  let index = shard.by_index.len() as u32;
  assert!(index < INDEX_MASK, "interner shard overflow");
  shard.by_index.push(interned);
  shard.by_text.insert(interned, index);
  NameId::from_raw(((shard_index as u32) << INDEX_BITS) | index)
}

/// Total bytes of interned string content currently retained — the number an embedded host
/// watches to decide when a [`reclaim_all`] boundary is worth taking.
pub fn retained_bytes() -> usize {
  shards()
    .iter()
    .map(|lock| {
      lock
        .read()
        .unwrap()
        .arena
        .iter()
        .map(|boxed| boxed.len())
        .sum::<usize>()
    })
    .sum()
}

/// Distinct strings currently retained.
pub fn retained_strings() -> usize {
  shards()
    .iter()
    .map(|lock| lock.read().unwrap().arena.len())
    .sum()
}

/// Free every interned string and reset the id space — the session boundary for long-lived
/// embedded hosts. One-shot processes never need this.
///
/// # Safety
///
/// After this call, **nothing minted before it may be used**:
/// - no prior [`NameId`] may be passed to [`text_of`] (ids restart from zero; a stale id
///   would name an unrelated new string, or panic on an empty shard), and
/// - no `&str` previously returned by [`text_of`] (or held via any type that captured one)
///   may still be alive — their arenas are dropped here.
///
/// In vorpal's own architecture both hold structurally between builds: `Reference`,
/// `SymbolTable`, and the spill are dropped before `build_index` returns, and persisted
/// artifacts never contain interner ids. Callers embedding the library must uphold the same
/// discipline: quiesce every vorpal call, drop every vorpal value that predates the reclaim,
/// then call this. `vorpal_index::reclaim_session_memory` layers a build-in-flight check on
/// top and is the entry point hosts should prefer.
pub unsafe fn reclaim_all() {
  for lock in shards() {
    let mut shard = lock.write().unwrap();
    shard.by_text.clear();
    shard.by_index.clear();
    shard.arena.clear();
  }
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
