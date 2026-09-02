//! Session-scoped string interner for resolution-time names and paths.
//!
//! At kernel scale a link pass carries ~6.8M references and ~2.4M symbols whose names and
//! paths draw from a vocabulary of only a few hundred thousand distinct strings — owning a
//! heap `String` per occurrence dominated peak memory. Interning stores each distinct string
//! once and every occurrence becomes a `u32` id: references and symbols collapse to PODs,
//! and resolution compares integers instead of hashing strings.
//!
//! The table is append-only and read-mostly (writes only on first sight of a string), and
//! **sharded 64 ways** by string hash: parallel link passes make ~10M interner calls across
//! all worker threads, and a single lock measurably serialized them. Each id carries its
//! shard in its high bits, so resolving text touches only that shard's lock. Interned ids
//! are process-internal and never reach any output, so run-to-run determinism is unaffected.
//!
//! **Lifecycle is ownership** (the scoped-interner contract): an [`Interner`] is created per
//! build session and dropped with it — reclaim is `Drop`, not an API. [`NameId`] carries the
//! session lifetime, so an id (or any type holding one — `Reference`, `SymbolTable`, the
//! spill) *cannot outlive its session at compile time*, and text borrowed via
//! [`Interner::text_of`] cannot either. Long-lived embedded hosts get bounded memory with no
//! reclaim call and no quiescence contract: each build's vocabulary frees when its interner
//! drops. [`Interner::retained_bytes`] / [`Interner::retained_strings`] remain as per-session
//! telemetry.

use std::hash::BuildHasher;
use std::marker::PhantomData;
use std::num::NonZeroU32;
use std::sync::RwLock;

/// An interned string (name, path, or qualifier), branded with its session's lifetime.
/// Compare, hash, and copy freely — it's a `u32` (6 shard bits + 26 per-shard index bits:
/// ~67M strings per shard), stored biased by one so `Option<NameId>` gets the niche and
/// costs 4 bytes, not 8. The phantom borrow is covariant: ids flow into shorter regions
/// freely, but can never outlive the [`Interner`] that minted them.
#[derive(Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NameId<'i>(NonZeroU32, PhantomData<&'i str>);

impl<'i> Clone for NameId<'i> {
  fn clone(&self) -> Self {
    *self
  }
}
impl<'i> Copy for NameId<'i> {}

impl<'i> NameId<'i> {
  /// The id's raw bit pattern — for process-private serialization (the reference spill).
  /// Bits are never 0 and never stable across sessions; roundtrip with
  /// [`Interner::id_from_bits`] against the same session only.
  pub fn to_bits(self) -> u32 {
    self.0.get()
  }

  fn from_raw(raw: u32) -> Self {
    // `raw + 1` cannot overflow: per-shard indices are held strictly below `INDEX_MASK`.
    NameId(
      NonZeroU32::new(raw + 1).expect("interner id overflow"),
      PhantomData,
    )
  }

  fn raw(self) -> u32 {
    self.0.get() - 1
  }

  /// `(shard, per-shard dense index)` — the id's structural decomposition. The per-shard
  /// index is assigned by insertion order, which is what makes a direct-indexed table over
  /// ids sound (see `SymbolTable`'s dense ranges). Process-private, like the id itself.
  pub(crate) fn shard_slot(self) -> (usize, usize) {
    let raw = self.raw();
    ((raw >> INDEX_BITS) as usize, (raw & INDEX_MASK) as usize)
  }
}

/// Shard read-lock acquisition with contention accounting (ledger builds): a
/// failed `try_read` means this thread is about to block behind a writer — the
/// exact event the parallelism audit counts. Plain builds compile to the bare
/// `read()`.
#[inline]
fn read_shard(lock: &RwLock<Shard>) -> std::sync::RwLockReadGuard<'_, Shard> {
  #[cfg(feature = "alloc-ledger")]
  {
    match lock.try_read() {
      Ok(guard) => return guard,
      Err(std::sync::TryLockError::WouldBlock) => {
        vorpal_kg::ledger::INTERN_READ_CONTENDED
          .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
      }
      Err(std::sync::TryLockError::Poisoned(_)) => {}
    }
  }
  lock.read().unwrap()
}

const SHARD_BITS: u32 = 6;
pub(crate) const SHARDS: usize = 1 << SHARD_BITS;
const INDEX_BITS: u32 = 32 - SHARD_BITS;
const INDEX_MASK: u32 = (1 << INDEX_BITS) - 1;

#[derive(Default)]
struct Shard {
  /// Dedup lookup only — a raw `HashTable` probed with the caller-supplied hash, so each
  /// intern hashes its string exactly ONCE (the same fixed-seed hash picks the shard and
  /// probes the table; the previous form hashed once for the shard and again inside the
  /// map). Entries pair the per-shard index with the key for the probe's equality check.
  /// Keys borrow the arena boxes below; `'static` is an internal shorthand for "as long as
  /// the arena entry", which [`Interner`]'s ownership guarantees for every handed-out
  /// borrow. The id is (shard, insertion-index) — never this table's layout — so the hash
  /// function cannot affect any output byte (pinned by the content-id A/B gate).
  by_text: hashbrown::HashTable<(u32, &'static str)>,
  by_index: Vec<&'static str>,
  /// Owns every string the two maps borrow. `Box<str>` contents are heap-stable, so the
  /// vector may grow freely while borrows into the boxes circulate; everything drops
  /// together with the [`Interner`].
  arena: Vec<Box<str>>,
}

/// The single fixed-seed hash every interner call performs — deterministic across runs and
/// platforms by construction (foldhash's fixed state), shared by shard selection and probe.
#[inline]
fn hash_of(text: &str) -> u64 {
  foldhash::fast::FixedState::default().hash_one(text)
}

/// One build session's string table. Create per session, share by reference (`Sync`), drop
/// to free — see the module docs for the lifecycle contract the `NameId` brand enforces.
///
/// The brand is compile-time enforced — a session id cannot escape its session:
///
/// ```compile_fail
/// use vorpal_resolve::Interner;
/// let escaped = {
///   let interner = Interner::default();
///   interner.intern("name") // borrow of `interner` cannot outlive this block
/// };
/// ```
pub struct Interner {
  /// Cache-line padded so one shard's lock RMW never invalidates a neighbor shard's line
  /// (unpadded, the 112-byte shards packed ~1.14 per 128-byte Apple-Silicon line;
  /// `CachePadded` picks the right alignment per architecture).
  shards: [crossbeam_utils::CachePadded<RwLock<Shard>>; SHARDS],
}

impl Default for Interner {
  fn default() -> Self {
    Self {
      shards: std::array::from_fn(|_| crossbeam_utils::CachePadded::new(RwLock::new(Shard::default()))),
    }
  }
}

impl Interner {
  pub fn new() -> Self {
    Self::default()
  }

  /// Intern `text`, returning its stable id (allocating into the shard arena on first
  /// sight).
  pub fn intern<'i>(&'i self, text: &str) -> NameId<'i> {
    let hash = hash_of(text);
    let shard_index = (hash as usize) & (SHARDS - 1);
    let lock = &self.shards[shard_index];
    if let Some(&(index, _)) = read_shard(lock).by_text.find(hash, |&(_, key)| key == text) {
      return NameId::from_raw(((shard_index as u32) << INDEX_BITS) | index);
    }
    #[cfg(feature = "alloc-ledger")]
    vorpal_kg::ledger::INTERN_WRITES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut shard = lock.write().unwrap();
    if let Some(&(index, _)) = shard.by_text.find(hash, |&(_, key)| key == text) {
      return NameId::from_raw(((shard_index as u32) << INDEX_BITS) | index);
    }
    let boxed: Box<str> = text.to_string().into_boxed_str();
    // The box's heap contents are address-stable; the extended borrow lives exactly as
    // long as the arena entry — which lives exactly as long as `self`, the bound every
    // public borrow carries.
    let interned: &'static str = unsafe { &*(boxed.as_ref() as *const str) };
    let shard = &mut *shard;
    shard.arena.push(boxed);
    let index = shard.by_index.len() as u32;
    assert!(index < INDEX_MASK, "interner shard overflow");
    shard.by_index.push(interned);
    shard
      .by_text
      .insert_unique(hash, (index, interned), |&(_, key)| hash_of(key));
    NameId::from_raw(((shard_index as u32) << INDEX_BITS) | index)
  }

  /// The id of `text` iff it was ever interned — for speculative probes (path-form import
  /// joins) that must not grow the table with strings nothing will ever look up again.
  pub fn peek<'i>(&'i self, text: &str) -> Option<NameId<'i>> {
    let hash = hash_of(text);
    let shard_index = (hash as usize) & (SHARDS - 1);
    read_shard(&self.shards[shard_index])
      .by_text
      .find(hash, |&(_, key)| key == text)
      .map(|&(index, _)| NameId::from_raw(((shard_index as u32) << INDEX_BITS) | index))
  }

  /// The interned text of `id`, borrowed for as long as the session lives.
  pub fn text_of<'i>(&'i self, id: NameId<'i>) -> &'i str {
    let raw = id.raw();
    let shard_index = (raw >> INDEX_BITS) as usize;
    read_shard(&self.shards[shard_index]).by_index[(raw & INDEX_MASK) as usize]
  }

  /// Rebuild an id from [`NameId::to_bits`] output written by **this session** (the spill's
  /// decode path); `None` for 0, the "absent" sentinel.
  pub fn id_from_bits<'i>(&'i self, bits: u32) -> Option<NameId<'i>> {
    NonZeroU32::new(bits).map(|bits| NameId(bits, PhantomData))
  }

  /// Total bytes of interned string content this session retains.
  pub fn retained_bytes(&self) -> usize {
    self
      .shards
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

  /// Distinct strings this session retains.
  pub fn retained_strings(&self) -> usize {
    self
      .shards
      .iter()
      .map(|lock| lock.read().unwrap().arena.len())
      .sum()
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn interning_is_stable_and_peek_never_inserts() {
    let interner = Interner::new();
    let a = interner.intern("alpha_symbol");
    let b = interner.intern("alpha_symbol");
    assert_eq!(a, b);
    assert_eq!(interner.text_of(a), "alpha_symbol");
    assert_ne!(a, interner.intern("beta_symbol"));

    assert_eq!(interner.peek("alpha_symbol"), Some(a));
    assert_eq!(interner.peek("never_interned_probe_xyz"), None);
    // And the probe really did not insert.
    assert_eq!(interner.peek("never_interned_probe_xyz"), None);

    assert_eq!(interner.retained_strings(), 2);
    assert_eq!(interner.retained_bytes(), "alpha_symbol".len() + "beta_symbol".len());
    // Reclaim is Drop: a fresh session starts from zero.
    drop(interner);
    let fresh = Interner::new();
    assert_eq!(fresh.retained_strings(), 0);
  }
}
