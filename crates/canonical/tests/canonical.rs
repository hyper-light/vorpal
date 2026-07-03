//! Canonical index: identity/dedup (§9.2), incremental skip (§3.4), freeze-on-seal (§11.6).

use vorpal_canonical::{Assignment, CanonicalIndex, CanonicalKey};
use vorpal_segment::NodeId;

fn key(path: &str, entity: &str) -> CanonicalKey {
  CanonicalKey::of(path, entity)
}

#[test]
fn assigns_dense_monotone_ids_from_base() {
  let mut idx = CanonicalIndex::new(1000);
  let a = idx.get_or_assign(key("a.rs", "foo"), 1);
  let b = idx.get_or_assign(key("a.rs", "bar"), 2);
  assert_eq!(a, Assignment::Assigned(NodeId::new(1000)));
  assert_eq!(b, Assignment::Assigned(NodeId::new(1001)));
  assert_eq!(idx.next_id(), 1002);
  assert_eq!(idx.len(), 2);
}

#[test]
fn dedups_repeated_keys_to_a_stable_id() {
  let mut idx = CanonicalIndex::new(0);
  let first = idx.get_or_assign(key("a.rs", "foo"), 42);
  let again = idx.get_or_assign(key("a.rs", "foo"), 42);
  assert!(first.is_new());
  assert!(!again.is_new());
  assert_eq!(first.node_id(), again.node_id());
  assert_eq!(idx.len(), 1, "re-seeing a key assigns no new id");
}

#[test]
fn incremental_skip_tracks_content_hash() {
  let mut idx = CanonicalIndex::new(0);
  let k = key("a.rs", "foo");
  idx.get_or_assign(k, 0xDEAD);

  assert!(idx.is_unchanged(&k, 0xDEAD), "same content → skip");
  assert!(!idx.is_unchanged(&k, 0xBEEF), "changed content → re-index");
  assert!(
    !idx.is_unchanged(&key("a.rs", "missing"), 0),
    "unknown key is not 'unchanged'"
  );

  // Re-interning with a changed hash keeps the id but updates the skip state.
  let before = idx.lookup(&k).unwrap();
  let reassigned = idx.get_or_assign(k, 0xBEEF);
  assert_eq!(reassigned, Assignment::Existing(before));
  assert_eq!(idx.content_hash(&k), Some(0xBEEF));
  assert!(idx.is_unchanged(&k, 0xBEEF));
}

#[test]
fn seal_freezes_hot_overlay_and_lookups_still_work() {
  let mut idx = CanonicalIndex::new(0);
  let k1 = key("a.rs", "foo");
  let k2 = key("b.rs", "bar");
  let id1 = idx.get_or_assign(k1, 1).node_id();
  let id2 = idx.get_or_assign(k2, 2).node_id();
  assert_eq!(idx.hot_len(), 2);

  idx.seal();
  assert_eq!(idx.hot_len(), 0);
  assert_eq!(idx.sealed_segments(), 1);

  // Lookups resolve from the sealed tier.
  assert_eq!(idx.lookup(&k1), Some(id1));
  assert_eq!(idx.lookup(&k2), Some(id2));
  // Re-seeing a sealed key dedups without a new id and without repopulating hot.
  let again = idx.get_or_assign(k1, 1);
  assert_eq!(again, Assignment::Existing(id1));
  assert_eq!(idx.hot_len(), 0);
  assert_eq!(idx.len(), 2);
}

#[test]
fn overlay_shadows_older_sealed_segments() {
  let mut idx = CanonicalIndex::new(0);
  let k = key("a.rs", "foo");
  let id = idx.get_or_assign(k, 100).node_id();
  idx.seal();

  // A changed content hash after seal lands in a fresh overlay entry...
  idx.get_or_assign(k, 200);
  assert_eq!(idx.content_hash(&k), Some(200));
  assert_eq!(idx.hot_len(), 1);

  // ...and after another seal, the newest sealed segment shadows the old one.
  idx.seal();
  assert_eq!(idx.sealed_segments(), 2);
  assert_eq!(idx.lookup(&k), Some(id), "id is stable across reseals");
  assert_eq!(idx.content_hash(&k), Some(200), "newest content wins");
  assert!(idx.is_unchanged(&k, 200));
  assert!(!idx.is_unchanged(&k, 100));
}

#[test]
fn keys_are_deterministic_and_distinct() {
  assert_eq!(
    CanonicalKey::of("a.rs", "foo"),
    CanonicalKey::of("a.rs", "foo")
  );
  assert_ne!(
    CanonicalKey::of("a.rs", "foo"),
    CanonicalKey::of("a.rs", "bar")
  );
  assert_ne!(
    CanonicalKey::of("a.rs", "foo"),
    CanonicalKey::of("b.rs", "foo")
  );
  // Length-prefix framing prevents path/entity boundary collisions (delimiter injection).
  assert_ne!(CanonicalKey::of("a:b", "c"), CanonicalKey::of("a", "b:c"));
  // Round-trips through raw bytes.
  let k = CanonicalKey::of("a.rs", "foo");
  assert_eq!(CanonicalKey::from_bytes(*k.as_bytes()), k);
}

#[test]
fn spans_multiple_sealed_segments() {
  let mut idx = CanonicalIndex::new(0);
  let mut ids = Vec::new();
  for i in 0..50 {
    let a = idx.get_or_assign(key("f.rs", &format!("e{i}")), i);
    ids.push((key("f.rs", &format!("e{i}")), a.node_id()));
    if i % 10 == 9 {
      idx.seal();
    }
  }
  idx.seal();
  assert!(idx.sealed_segments() >= 5);
  assert_eq!(idx.len(), 50);
  for (k, id) in ids {
    assert_eq!(idx.lookup(&k), Some(id));
  }
  assert_eq!(idx.lookup(&key("f.rs", "missing")), None);
}
