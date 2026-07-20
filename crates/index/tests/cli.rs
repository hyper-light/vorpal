//! CLI flow: build a persisted index from a directory, then cold-open and query it.

use std::fs;

use vorpal_index::{build_index, search_index};
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

#[test]
fn semantic_search_finds_definitions_by_description() {
  let base = std::env::temp_dir().join(format!("vorpal-index-search-{}", std::process::id()));
  let src = base.join("src");
  let out = base.join("index");
  let _ = fs::remove_dir_all(&base);
  fs::create_dir_all(&src).unwrap();
  fs::write(
    src.join("a.rs"),
    "pub fn resolve_import_path() -> i32 {\n    1\n}\n\npub fn hamming_distance() -> i32 {\n    2\n}\n",
  )
  .unwrap();

  build_index(&src, &out).unwrap();
  let rendered = search_index(&out, "import path resolution", 3).unwrap();
  let first = rendered.lines().next().unwrap_or("");
  assert!(
    first.contains("resolve_import_path"),
    "top hit should be the lexically-matching definition:\n{rendered}"
  );

  // Hybrid guarantee: querying a symbol by its exact name puts that symbol first.
  let rendered = search_index(&out, "hamming_distance", 3).unwrap();
  let first = rendered.lines().next().unwrap_or("");
  assert!(
    first.contains("hamming_distance [Function]"),
    "exact-name query ranks the exact node first:\n{rendered}"
  );

  let _ = fs::remove_dir_all(&base);
}

#[test]
fn incremental_reindex_reparses_only_changed_files() {
  let base = std::env::temp_dir().join(format!("vorpal-index-incr-{}", std::process::id()));
  let src = base.join("src");
  let out = base.join("index");
  let _ = fs::remove_dir_all(&base);
  fs::create_dir_all(&src).unwrap();
  fs::write(
    src.join("a.rs"),
    "pub fn caller() -> i32 {\n    target()\n}\n",
  )
  .unwrap();
  fs::write(src.join("b.rs"), "pub fn target() -> i32 {\n    0\n}\n").unwrap();
  fs::write(src.join("c.rs"), "pub fn lonely() -> i32 {\n    1\n}\n").unwrap();

  let first = build_index(&src, &out).unwrap();
  assert_eq!(first.indexed, 3, "{first:?}");
  assert_eq!(first.skipped, 0);

  // Modify only c.rs: the other two files replay from the product cache without a parse.
  fs::write(
    src.join("c.rs"),
    "pub fn lonely_two() -> i32 {\n    22222\n}\n",
  )
  .unwrap();
  let second = build_index(&src, &out).unwrap();
  assert!(!second.reused);
  assert_eq!(
    second.indexed, 1,
    "only the changed file parses: {second:?}"
  );
  assert_eq!(second.skipped, 2, "{second:?}");

  let kg = Kg::load(&out).unwrap();
  // The cross-file call between two REPLAYED files still resolves — relink from cache works.
  let callers: Vec<String> = kg
    .callers_of("target")
    .into_iter()
    .filter_map(|id| kg.node(id).map(|v| v.name.to_string()))
    .collect();
  assert!(callers.contains(&"caller".to_string()), "{callers:?}");
  // The changed file's world updated: old symbol gone, new one present.
  assert!(kg.nodes_named("lonely").is_empty());
  assert_eq!(kg.nodes_named("lonely_two").len(), 1);

  // Remove c.rs entirely: nothing re-parses, and its nodes vanish (full relink, no staleness).
  fs::remove_file(src.join("c.rs")).unwrap();
  let third = build_index(&src, &out).unwrap();
  assert_eq!(third.indexed, 0, "{third:?}");
  assert_eq!(third.skipped, 2, "{third:?}");
  let kg = Kg::load(&out).unwrap();
  assert!(kg.nodes_named("lonely_two").is_empty());
  let callers: Vec<String> = kg
    .callers_of("target")
    .into_iter()
    .filter_map(|id| kg.node(id).map(|v| v.name.to_string()))
    .collect();
  assert!(callers.contains(&"caller".to_string()), "{callers:?}");

  let _ = fs::remove_dir_all(&base);
}

#[test]
fn products_survive_an_interrupted_run_via_their_stat_stamps() {
  let base = std::env::temp_dir().join(format!("vorpal-index-interrupt-{}", std::process::id()));
  let src = base.join("src");
  let out = base.join("index");
  let _ = fs::remove_dir_all(&base);
  fs::create_dir_all(&src).unwrap();
  fs::write(src.join("a.rs"), "pub fn alpha() -> i32 {\n    1\n}\n").unwrap();
  fs::write(src.join("b.rs"), "pub fn beta() -> i32 {\n    2\n}\n").unwrap();

  let first = build_index(&src, &out).unwrap();
  assert_eq!(first.indexed, 2);

  // Simulate a run killed after products were written but before the manifest committed:
  // products are self-validating, so nothing re-parses.
  fs::remove_file(out.join("manifest.bin")).unwrap();
  let recovered = build_index(&src, &out).unwrap();
  assert!(!recovered.reused, "no manifest → no whole-tree fast path");
  assert_eq!(
    recovered.indexed, 0,
    "stamped products replay without a manifest: {recovered:?}"
  );
  assert_eq!(recovered.skipped, 2);

  let _ = fs::remove_dir_all(&base);
}

#[test]
fn warm_product_cache_lets_search_matches_replay_at_index_time() {
  let base = std::env::temp_dir().join(format!("vorpal-index-warm-{}", std::process::id()));
  let src = base.join("repo");
  let out = src.join(".vorpal").join("index");
  let _ = fs::remove_dir_all(&base);
  fs::create_dir_all(&src).unwrap();
  fs::write(src.join("a.rs"), "pub fn alpha() -> i32 {\n    1\n}\n").unwrap();

  // An index must already exist: searches feed indexes, they never create them.
  build_index(&src, &out).unwrap();

  // A "search" now matches a brand-new file and banks its product.
  let new_file = src.join("fresh.rs");
  fs::write(&new_file, "pub fn fresh() -> i32 {\n    alpha()\n}\n").unwrap();
  assert!(
    vorpal_index::warm_product_cache(&new_file).unwrap(),
    "match in an indexed tree banks a product"
  );
  // Re-warming an unchanged file is a no-op.
  assert!(!vorpal_index::warm_product_cache(&new_file).unwrap());

  // The next index replays the banked product: zero parses despite the new file.
  let report = build_index(&src, &out).unwrap();
  assert!(!report.reused, "tree changed (new file), full relink runs");
  assert_eq!(
    report.indexed, 0,
    "search-banked product replays: {report:?}"
  );
  assert_eq!(report.skipped, 2);

  // A stale banked product (file changed after the match) must NOT replay.
  fs::write(
    &new_file,
    "pub fn fresh() -> i64 {\n    alpha() as i64\n}\n",
  )
  .unwrap();
  let report = build_index(&src, &out).unwrap();
  assert_eq!(report.indexed, 1, "stale stamp re-parses: {report:?}");

  let _ = fs::remove_dir_all(&base);
}

#[test]
fn warm_product_cache_never_creates_index_state() {
  let base = std::env::temp_dir().join(format!("vorpal-index-nowrite-{}", std::process::id()));
  let src = base.join("bare");
  let _ = fs::remove_dir_all(&base);
  fs::create_dir_all(&src).unwrap();
  let file = src.join("a.rs");
  fs::write(&file, "pub fn alpha() {}\n").unwrap();

  assert!(
    !vorpal_index::warm_product_cache(&file).unwrap(),
    "no existing index → nothing to feed"
  );
  assert!(
    !src.join(".vorpal").exists(),
    "search must not create index state in un-indexed trees"
  );

  let _ = fs::remove_dir_all(&base);
}
