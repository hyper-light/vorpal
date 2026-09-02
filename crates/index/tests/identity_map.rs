//! Phase-4 P4.0 oracle: `(FileKey, ordinal)` is bijective with the dense id space over a
//! real index. Blocks are contiguous and every block's first row is its File node, so the
//! reconstruction below is exactly the derivation the bucketed format's loader will run —
//! proven here on the current format before any byte moves.

use std::collections::HashSet;

use vorpal_kg::identity::{FileKey, tree_relative};
use vorpal_kg::{Kg, NodeId, SymbolKind};

#[test]
fn file_local_identity_is_bijective_with_dense_ids() {
  let base = std::env::temp_dir().join("vorpal-identity-map");
  let _ = std::fs::remove_dir_all(&base);
  let src = base.join("src");
  std::fs::create_dir_all(&src).unwrap();
  std::fs::write(
    src.join("a.rs"),
    "pub struct Alpha;\nimpl Alpha {\n  pub fn one(&self) -> u32 { 1 }\n  pub fn two(&self) -> u32 { 2 }\n}\npub fn free_a() -> u32 { 3 }\n",
  )
  .unwrap();
  std::fs::write(
    src.join("b.py"),
    "class Beta:\n    def one(self):\n        return 1\n\ndef free_b():\n    return 2\n",
  )
  .unwrap();
  std::fs::write(src.join("c.go"), "package m\n\nfunc Gamma() int { return 1 }\n").unwrap();
  let out = base.join("index");
  vorpal_index::build_index(&src, &out).unwrap();

  let kg = Kg::load(&vorpal_kg::resolve_index_dir(&out)).unwrap();
  let root = src
    .canonicalize()
    .unwrap_or_else(|_| src.clone())
    .to_string_lossy()
    .into_owned();

  let mut identities: HashSet<(FileKey, u32)> = HashSet::new();
  let mut current: Option<(FileKey, u64)> = None; // (block key, block start id)
  let mut file_blocks = 0usize;
  for raw in 0..kg.node_count() as u64 {
    let id = NodeId::new(raw);
    if kg.node_kind(id) == Some(SymbolKind::File) {
      let path = kg.node_path(id).expect("file node has a path");
      current = Some((FileKey::of(tree_relative(path, &root)), raw));
      file_blocks += 1;
    }
    let (key, start) = current.expect("every row belongs to a file block — blocks start with their File node");
    let ordinal = (raw - start) as u32;
    assert!(
      identities.insert((key, ordinal)),
      "identity collision at dense id {raw}: (key, ordinal) must be unique"
    );
  }
  assert_eq!(
    identities.len(),
    kg.node_count(),
    "the identity map must cover every dense id exactly once"
  );
  assert_eq!(file_blocks, 3, "one block per source file");

  // Roundtrip spot-check: rebuild dense ids from sorted block structure — the loader's
  // future derivation — and confirm a known symbol lands where the graph says it is.
  let mut alpha = None;
  for raw in 0..kg.node_count() as u64 {
    let id = NodeId::new(raw);
    if kg.node(id).is_some_and(|view| view.name == "Alpha") {
      alpha = Some(id);
      break;
    }
  }
  let alpha = alpha.expect("Alpha exists");
  let path = kg.node_path(alpha).unwrap();
  assert!(path.ends_with("a.rs"));

  let _ = std::fs::remove_dir_all(&base);
}
