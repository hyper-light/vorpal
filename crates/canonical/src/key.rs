//! The content-addressed canonical key.

/// A 32-byte `blake3` digest identifying an entity by `path:entityPath` (§9.2). This is the
/// permanent external identity; the [`NodeId`](vorpal_segment::NodeId) it maps to is a locator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CanonicalKey([u8; 32]);

impl CanonicalKey {
  /// The identity of one extracted entity: `blake3` over the **length-prefixed** `path` and
  /// `entity_path`. Length framing (not a `:` delimiter) makes the encoding injective, so a `:`
  /// — or any byte — inside either component cannot shift the boundary and collide two entities.
  pub fn of(path: &str, entity_path: &str) -> Self {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&(path.len() as u64).to_le_bytes());
    hasher.update(path.as_bytes());
    hasher.update(&(entity_path.len() as u64).to_le_bytes());
    hasher.update(entity_path.as_bytes());
    Self(*hasher.finalize().as_bytes())
  }

  pub fn from_bytes(bytes: [u8; 32]) -> Self {
    Self(bytes)
  }

  pub fn as_bytes(&self) -> &[u8; 32] {
    &self.0
  }
}
