//! CLI flow: build a persisted index from a directory, then cold-open and query it.

use std::fs;

use vorpal_index::build_index;
use vorpal_kg::Kg;

#[test]
fn builds_persists_and_queries_an_index() {
  let base = std::env::temp_dir().join(format!("vorpal-index-cli-{}", std::process::id()));
  let src = base.join("src");
  let out = base.join("index");
  let _ = fs::remove_dir_all(&base);
  fs::create_dir_all(&src).unwrap();
  fs::write(src.join("b.rs"), "pub fn target() -> i32 {\n    0\n}\n").unwrap();
  fs::write(
    src.join("a.rs"),
    "pub fn caller() -> i32 {\n    target()\n}\n",
  )
  .unwrap();

  let report = build_index(&src, &out).unwrap();
  assert!(!report.reused, "first index is a full build");
  assert_eq!(report.indexed, 2);
  assert!(report.nodes >= 2, "{report:?}");
  assert!(report.resolved >= 1, "cross-file call resolved: {report:?}");

  // Cold-open the persisted index and query it (what the `callers` subcommand does).
  let kg = Kg::load(&out).unwrap();
  let callers: Vec<String> = kg
    .callers_of("target")
    .into_iter()
    .filter_map(|id| kg.node(id).map(|v| v.name.to_string()))
    .collect();
  assert!(
    callers.contains(&"caller".to_string()),
    "callers of target: {callers:?}"
  );

  let _ = fs::remove_dir_all(&base);
}

#[test]
fn reindex_of_unchanged_tree_is_reused() {
  let base = std::env::temp_dir().join(format!("vorpal-index-reuse-{}", std::process::id()));
  let src = base.join("src");
  let out = base.join("index");
  let _ = fs::remove_dir_all(&base);
  fs::create_dir_all(&src).unwrap();
  fs::write(src.join("lib.rs"), "pub fn f() -> i32 {\n    1\n}\n").unwrap();

  let first = build_index(&src, &out).unwrap();
  assert!(!first.reused);
  let nodes = first.nodes;

  // Re-index with no changes: detected unchanged, reused without re-parsing.
  let second = build_index(&src, &out).unwrap();
  assert!(second.reused, "unchanged tree should be reused");
  assert_eq!(second.indexed, 0);
  assert_eq!(second.nodes, nodes);

  // Change a file (different size): full rebuild.
  fs::write(src.join("lib.rs"), "pub fn f() -> i32 {\n    12345\n}\n").unwrap();
  let third = build_index(&src, &out).unwrap();
  assert!(!third.reused, "changed tree should rebuild");

  let _ = fs::remove_dir_all(&base);
}
