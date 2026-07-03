//! The LSM-shaped canonical index: hot overlay + sorted sealed segments (§9.6).

use std::collections::HashMap;

use vorpal_segment::NodeId;

use crate::key::CanonicalKey;

/// Result of interning a key: whether a fresh id was assigned or an existing one returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Assignment {
  /// The key was new; this dense id was just assigned (§9.2).
  Assigned(NodeId),
  /// The key already existed; this is its stable id (dedup).
  Existing(NodeId),
}

impl Assignment {
  pub fn node_id(self) -> NodeId {
    match self {
      Assignment::Assigned(id) | Assignment::Existing(id) => id,
    }
  }

  pub fn is_new(self) -> bool {
    matches!(self, Assignment::Assigned(_))
  }
}

#[derive(Debug, Clone, Copy)]
struct Entry {
  id: u64,
  content_hash: u64,
}

/// An immutable, key-sorted frozen tier (§11.6 freeze-on-seal). Parallel arrays, binary-searched.
struct SealedSegment {
  keys: Vec<[u8; 32]>,
  ids: Vec<u64>,
  content_hashes: Vec<u64>,
}

impl SealedSegment {
  fn get(&self, key: &[u8; 32]) -> Option<Entry> {
    let idx = self.keys.binary_search(key).ok()?;
    Some(Entry {
      id: self.ids[idx],
      content_hash: self.content_hashes[idx],
    })
  }
}

/// The canonical `blake3`→`NodeId` index. Assigns dense monotone ids from a base (typically a
/// segment directory's `next_id_base`, §9.2), dedups identity, and answers the hash-skip check.
pub struct CanonicalIndex {
  base: u64,
  next_id: u64,
  hot: HashMap<[u8; 32], Entry>,
  /// Newest last; reads scan in reverse so a newer entry shadows an older sealed one.
  sealed: Vec<SealedSegment>,
}

impl CanonicalIndex {
  /// Create an index that assigns ids starting at `next_id_base`.
  pub fn new(next_id_base: u64) -> Self {
    Self {
      base: next_id_base,
      next_id: next_id_base,
      hot: HashMap::new(),
      sealed: Vec::new(),
    }
  }

  /// The current entry for `key`, honoring overlay-over-sealed (newest wins).
  fn current(&self, key: &[u8; 32]) -> Option<Entry> {
    if let Some(entry) = self.hot.get(key) {
      return Some(*entry);
    }
    self.sealed.iter().rev().find_map(|seg| seg.get(key))
  }

  /// Intern `key`: return its existing id (dedup) or assign the next dense id. A changed
  /// `content_hash` for an existing key updates the overlay but keeps the id stable (§9.2).
  pub fn get_or_assign(&mut self, key: CanonicalKey, content_hash: u64) -> Assignment {
    if let Some(entry) = self.current(key.as_bytes()) {
      if entry.content_hash != content_hash {
        self.hot.insert(
          *key.as_bytes(),
          Entry {
            id: entry.id,
            content_hash,
          },
        );
      }
      Assignment::Existing(NodeId::new(entry.id))
    } else {
      let id = self.next_id;
      self.next_id += 1;
      self.hot.insert(*key.as_bytes(), Entry { id, content_hash });
      Assignment::Assigned(NodeId::new(id))
    }
  }

  /// Resolve a key to its id, if interned.
  pub fn lookup(&self, key: &CanonicalKey) -> Option<NodeId> {
    self.current(key.as_bytes()).map(|e| NodeId::new(e.id))
  }

  /// The current content hash recorded for a key, if interned.
  pub fn content_hash(&self, key: &CanonicalKey) -> Option<u64> {
    self.current(key.as_bytes()).map(|e| e.content_hash)
  }

  /// The incremental-skip check (§3.4): the key exists and its content is unchanged, so ingest
  /// can skip read/parse/extract/embed for it.
  pub fn is_unchanged(&self, key: &CanonicalKey, content_hash: u64) -> bool {
    self
      .current(key.as_bytes())
      .is_some_and(|e| e.content_hash == content_hash)
  }

  /// Freeze the hot overlay into a sorted sealed segment (§11.6). Idempotent when hot is empty.
  pub fn seal(&mut self) {
    if self.hot.is_empty() {
      return;
    }
    let mut entries: Vec<([u8; 32], Entry)> = self.hot.drain().collect();
    entries.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    let mut keys = Vec::with_capacity(entries.len());
    let mut ids = Vec::with_capacity(entries.len());
    let mut content_hashes = Vec::with_capacity(entries.len());
    for (k, e) in entries {
      keys.push(k);
      ids.push(e.id);
      content_hashes.push(e.content_hash);
    }
    self.sealed.push(SealedSegment {
      keys,
      ids,
      content_hashes,
    });
  }

  /// The next id that would be assigned — pairs with a segment covering `[base, next_id())`.
  pub fn next_id(&self) -> u64 {
    self.next_id
  }

  /// Number of distinct interned entities (each assignment consumes one dense id).
  pub fn len(&self) -> usize {
    (self.next_id - self.base) as usize
  }

  pub fn is_empty(&self) -> bool {
    self.next_id == self.base
  }

  /// Keys resident in the mutable overlay (not yet sealed).
  pub fn hot_len(&self) -> usize {
    self.hot.len()
  }

  pub fn sealed_segments(&self) -> usize {
    self.sealed.len()
  }
}
