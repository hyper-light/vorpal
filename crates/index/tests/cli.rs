//! CLI flow: build a persisted index from a directory, then cold-open and query it.

use std::fs;

use vorpal_index::{build_index, search_index};
use vorpal_kg::Kg;

/// The directory holding the live generation's artifacts (resolves the root's CURRENT
/// pointer; a legacy flat root resolves to itself).
fn live(root: &std::path::Path) -> std::path::PathBuf {
  vorpal_kg::resolve_index_dir(root)
}

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

/// A grammar change must defeat the whole-tree reuse fast path even when no file changed: the
/// manifest records a digest over the grammar set, and the fast path reuses only while it still
/// matches. (Simulated here by editing the persisted stamp — a rebuilt binary with an edited
/// grammar produces a different stamp the same way.)
#[test]
fn grammar_change_defeats_reuse_fast_path() {
  let base = std::env::temp_dir().join(format!("vorpal-index-grammar-{}", std::process::id()));
  let src = base.join("src");
  let out = base.join("index");
  let _ = fs::remove_dir_all(&base);
  fs::create_dir_all(&src).unwrap();
  fs::write(src.join("lib.rs"), "pub fn f() -> i32 {\n    1\n}\n").unwrap();

  assert!(!build_index(&src, &out).unwrap().reused);
  assert!(
    build_index(&src, &out).unwrap().reused,
    "unchanged tree + unchanged grammar is reused"
  );

  // The manifest is `VMAN`(4) + version(4) + grammar_stamp(8) + ...; flip the stamp to stand in
  // for a grammar edit. The next index must fall through to a rebuild rather than trust the
  // stat-only fast path.
  let manifest_path = live(&out).join("manifest.bin");
  let mut bytes = fs::read(&manifest_path).unwrap();
  assert_eq!(&bytes[0..4], b"VMAN", "manifest carries the versioned header");
  bytes[8] ^= 0xFF;
  fs::write(&manifest_path, &bytes).unwrap();

  assert!(
    !build_index(&src, &out).unwrap().reused,
    "a changed grammar stamp must force a re-index"
  );

  let _ = fs::remove_dir_all(&base);
}

/// Same-named overloads must each become their own graph node — signature (and kind)
/// disambiguate identity — rather than collapsing onto one canonical key.
#[test]
fn overloaded_definitions_stay_distinct() {
  let base = std::env::temp_dir().join(format!("vorpal-index-overload-{}", std::process::id()));
  let src = base.join("src");
  let out = base.join("index");
  let _ = fs::remove_dir_all(&base);
  fs::create_dir_all(&src).unwrap();
  fs::write(
    src.join("o.cpp"),
    "int area(int side) { return side * side; }\n\
     int area(int w, int h) { return w * h; }\n\
     double area(double r) { return 3.14 * r * r; }\n",
  )
  .unwrap();

  let report = build_index(&src, &out).unwrap();
  // 1 file node + 3 distinct overloads; a collapse would yield 2.
  assert_eq!(report.nodes, 4, "overloads did not stay distinct: {report:?}");

  let _ = fs::remove_dir_all(&base);
}

/// A reader must refuse an index whose graph and node segment came from different builds (what a
/// reader opening mid-rebuild could observe), rather than serve out-of-bounds/cross-generation
/// results.
#[test]
fn mixed_generation_index_is_rejected() {
  let base = std::env::temp_dir().join(format!("vorpal-index-mixed-{}", std::process::id()));
  let _ = fs::remove_dir_all(&base);
  let ssrc = base.join("ssrc");
  let small = base.join("small");
  fs::create_dir_all(&ssrc).unwrap();
  fs::write(ssrc.join("a.rs"), "pub fn a() -> i32 {\n    1\n}\n").unwrap();
  build_index(&ssrc, &small).unwrap();
  assert!(Kg::load(&small).is_ok(), "a coherent index loads");

  // A larger corpus → a different (bigger) node universe in its graph.
  let lsrc = base.join("lsrc");
  let large = base.join("large");
  fs::create_dir_all(&lsrc).unwrap();
  for i in 0..24 {
    fs::write(
      lsrc.join(format!("f{i}.rs")),
      format!("pub fn f{i}() -> i32 {{\n    {i}\n}}\n"),
    )
    .unwrap();
  }
  build_index(&lsrc, &large).unwrap();

  // Splice the large graph onto the small index: graph.node_count no longer matches the node
  // segment — a mixed generation the coherence gate must reject.
  fs::copy(live(&large).join("graph.bin"), live(&small).join("graph.bin")).unwrap();
  assert!(
    Kg::load(&small).is_err(),
    "mixed graph/node-segment generation must be rejected"
  );

  let _ = fs::remove_dir_all(&base);
}

/// Content-addressed generations preserve the determinism contract at the directory level: an
/// incremental rebuild converges to the *same generation id* — and byte-identical artifacts —
/// as a from-scratch build of the final tree.
#[test]
fn incremental_generation_converges_to_from_scratch() {
  let base = std::env::temp_dir().join(format!("vorpal-index-conv-{}", std::process::id()));
  let src = base.join("src");
  let inc = base.join("inc");
  let fresh = base.join("fresh");
  let _ = fs::remove_dir_all(&base);
  fs::create_dir_all(&src).unwrap();
  fs::write(src.join("a.rs"), "pub fn one() -> i32 {\n    1\n}\n").unwrap();
  fs::write(src.join("b.rs"), "pub fn two() -> i32 {\n    2\n}\n").unwrap();

  // v1 into `inc`, then edit + delete + add, then incremental rebuild.
  build_index(&src, &inc).unwrap();
  fs::write(src.join("a.rs"), "pub fn one_v2() -> i32 {\n    12\n}\n").unwrap();
  fs::remove_file(src.join("b.rs")).unwrap();
  fs::write(src.join("c.rs"), "pub fn three() -> i32 {\n    3\n}\n").unwrap();
  build_index(&src, &inc).unwrap();
  // From-scratch of the final tree into `fresh`.
  build_index(&src, &fresh).unwrap();

  let current = |root: &std::path::Path| fs::read_to_string(root.join("CURRENT")).unwrap();
  assert_eq!(
    current(&inc),
    current(&fresh),
    "incremental must converge to the from-scratch generation id"
  );
  // And the generation contents are byte-identical, artifact by artifact.
  let (inc_live, fresh_live) = (live(&inc), live(&fresh));
  let mut names: Vec<_> = fs::read_dir(&inc_live)
    .unwrap()
    .flatten()
    .map(|e| e.file_name())
    .collect();
  names.sort();
  for name in names {
    assert_eq!(
      fs::read(inc_live.join(&name)).unwrap(),
      fs::read(fresh_live.join(&name)).unwrap(),
      "artifact {name:?} differs between incremental and from-scratch"
    );
  }

  let _ = fs::remove_dir_all(&base);
}

/// The atomic-generation guarantee: a reader that loaded before a rebuild keeps serving its
/// complete generation; a reader that loads after sees the complete new one — never a mixture.
#[test]
fn concurrent_reader_survives_rebuild() {
  let base = std::env::temp_dir().join(format!("vorpal-index-reader-{}", std::process::id()));
  let src = base.join("src");
  let out = base.join("index");
  let _ = fs::remove_dir_all(&base);
  fs::create_dir_all(&src).unwrap();
  fs::write(src.join("a.rs"), "pub fn alpha() -> i32 {\n    1\n}\n").unwrap();
  build_index(&src, &out).unwrap();

  // Reader pins generation 1.
  let old_reader = Kg::load(&out).unwrap();
  let old_nodes = old_reader.node_count();
  assert_eq!(old_reader.nodes_named("alpha").len(), 1);

  // Rebuild to generation 2 (grow the tree).
  fs::write(src.join("b.rs"), "pub fn beta() -> i32 {\n    2\n}\n").unwrap();
  fs::write(src.join("c.rs"), "pub fn gamma() -> i32 {\n    3\n}\n").unwrap();
  build_index(&src, &out).unwrap();

  // The pinned reader still serves its complete old generation…
  assert_eq!(old_reader.node_count(), old_nodes);
  assert_eq!(old_reader.nodes_named("alpha").len(), 1);
  assert_eq!(
    old_reader.nodes_named("beta").len(),
    0,
    "the old generation must not see the new tree"
  );
  // …and a fresh reader sees the complete new one.
  let new_reader = Kg::load(&out).unwrap();
  assert!(new_reader.node_count() > old_nodes);
  assert_eq!(new_reader.nodes_named("beta").len(), 1);

  // A third rebuild retires generation 1 (GC keeps {new, prior}); the pinned reader's mmaps
  // stay valid (POSIX), so it still answers from its complete — now unlinked — generation.
  fs::write(src.join("d.rs"), "pub fn delta() -> i32 {\n    4\n}\n").unwrap();
  build_index(&src, &out).unwrap();
  assert_eq!(old_reader.node_count(), old_nodes);
  assert_eq!(old_reader.nodes_named("alpha").len(), 1);

  let _ = fs::remove_dir_all(&base);
}

/// A legacy flat index (artifacts at the root, no CURRENT) keeps serving reads and its next
/// rebuild migrates it into the generation layout, sweeping the superseded flat artifacts.
#[test]
fn legacy_flat_index_migrates_on_rebuild() {
  let base = std::env::temp_dir().join(format!("vorpal-index-legacy-{}", std::process::id()));
  let src = base.join("src");
  let out = base.join("index");
  let _ = fs::remove_dir_all(&base);
  fs::create_dir_all(&src).unwrap();
  fs::write(src.join("a.rs"), "pub fn legacy_fn() -> i32 {\n    1\n}\n").unwrap();
  build_index(&src, &out).unwrap();

  // Flatten: move the live generation's artifacts to the root and drop the pointer — exactly
  // what an index written before the generation layout looks like.
  let gen_dir = live(&out);
  for entry in fs::read_dir(&gen_dir).unwrap().flatten() {
    fs::rename(entry.path(), out.join(entry.file_name())).unwrap();
  }
  fs::remove_file(out.join("CURRENT")).unwrap();
  fs::remove_dir_all(out.join("gen")).unwrap();

  // Legacy reads work (resolution falls back to the flat root)…
  assert_eq!(Kg::load(&out).unwrap().nodes_named("legacy_fn").len(), 1);
  // …an unchanged tree still takes the fast path against the flat prior…
  assert!(build_index(&src, &out).unwrap().reused);
  // …and a real rebuild migrates: generation layout in, flat artifacts swept.
  fs::write(src.join("b.rs"), "pub fn newer_fn() -> i32 {\n    2\n}\n").unwrap();
  assert!(!build_index(&src, &out).unwrap().reused);
  assert!(out.join("CURRENT").is_file(), "migrated root gains CURRENT");
  assert!(
    !out.join("nodes.vseg").exists(),
    "superseded flat artifacts are swept"
  );
  let kg = Kg::load(&out).unwrap();
  assert_eq!(kg.nodes_named("legacy_fn").len(), 1);
  assert_eq!(kg.nodes_named("newer_fn").len(), 1);

  let _ = fs::remove_dir_all(&base);
}

/// Every persisted relation can answer "why does this relation exist?" (§5): the evidence
/// sidecar retains span, resolver reason, grade, and candidate count per edge occurrence.
#[test]
fn edges_explain_why_they_exist() {
  let base = std::env::temp_dir().join(format!("vorpal-index-why-{}", std::process::id()));
  let src = base.join("src");
  let out = base.join("index");
  let _ = fs::remove_dir_all(&base);
  fs::create_dir_all(&src).unwrap();
  fs::write(src.join("b.rs"), "pub fn target() -> i32 {\n    0\n}\n").unwrap();
  fs::write(
    src.join("a.rs"),
    "pub fn caller() -> i32 {\n    target() + target()\n}\n",
  )
  .unwrap();
  build_index(&src, &out).unwrap();

  let kg = Kg::load(&out).unwrap();
  let caller = kg.nodes_named("caller")[0];
  let target = kg.nodes_named("target")[0];

  // Library surface: both occurrences retained, with the exact resolver branch.
  let rows = kg.edge_evidence(caller, target);
  assert_eq!(rows.len(), 2, "both call sites must be retained: {rows:?}");
  for row in &rows {
    assert_eq!(vorpal_kg::EdgeType(row.etype), vorpal_kg::EdgeType::CALLS);
    assert_eq!(row.reason, 6, "single visible export: {row:?}"); // VisibleExport
    assert_eq!(row.candidates, 1);
    assert!(row.span_start < row.span_end);
  }
  assert!(rows[0].span_start < rows[1].span_start, "canonical span order");

  // Rendered surface: grade + reason + span, and the snippet names the referenced token.
  let rendered =
    vorpal_index::explain_edge(&out, caller.raw(), target.raw()).unwrap();
  assert!(rendered.contains("calls"), "{rendered}");
  assert!(rendered.contains("constrained"), "{rendered}");
  assert!(rendered.contains("visible-export"), "{rendered}");
  assert!(rendered.contains("target"), "{rendered}");

  // A pair with no edge answers honestly.
  let none = vorpal_index::explain_edge(&out, target.raw(), caller.raw()).unwrap();
  assert!(none.contains("no recorded evidence"), "{none}");

  let _ = fs::remove_dir_all(&base);
}

/// Durable external identity (IMPROVEMENTS 07-29 §2): a client can bookmark a symbol's
/// external id, rebuild the index (shifting dense ids), and re-resolve the same logical
/// symbol; a rename is an explicit identity transition — the old id resolves to nothing.
#[test]
fn external_id_bookmarks_survive_rebuilds() {
  let base = std::env::temp_dir().join(format!("vorpal-index-eid-{}", std::process::id()));
  let src = base.join("src");
  let out = base.join("index");
  let _ = fs::remove_dir_all(&base);
  fs::create_dir_all(&src).unwrap();
  fs::write(src.join("m.rs"), "pub fn bookmark_me() -> u32 { 1 }\n").unwrap();

  build_index(&src, &out).unwrap();
  let kg = Kg::load(&out).unwrap();
  let id_v1 = kg.nodes_named("bookmark_me")[0];
  let eid = kg.node(id_v1).unwrap().external_id.expect("eid persisted");
  drop(kg);

  // A new file that sorts earlier shifts every dense id; the bookmark must not care.
  fs::write(src.join("a.rs"), "pub fn earlier() -> u32 { 0 }\n").unwrap();
  build_index(&src, &out).unwrap();
  let kg = Kg::load(&out).unwrap();
  let id_v2 = kg.nodes_named("bookmark_me")[0];
  assert_ne!(id_v1, id_v2, "dense ids shifted (fixture precondition)");
  let resolved = kg.nodes_with_external_id(eid);
  assert_eq!(resolved, vec![id_v2], "bookmark resolves the same logical symbol");
  assert_eq!(kg.node(id_v2).unwrap().external_id, Some(eid), "id is stable");

  // The selector wire form: `eid:<hex>` as a name resolves it on every query surface.
  let rendered = vorpal_index::graph_query_on(
    &kg,
    "node",
    &vorpal_index::GraphTarget {
      name: format!("eid:{eid:032x}"),
      ..vorpal_index::GraphTarget::default()
    },
  )
  .unwrap();
  assert!(rendered.contains("bookmark_me"), "{rendered}");
  assert!(rendered.contains(&format!("eid:{eid:032x}")), "{rendered}");
  drop(kg);

  // Rename: an explicit identity transition — the old bookmark resolves to nothing.
  fs::write(src.join("m.rs"), "pub fn bookmark_renamed() -> u32 { 1 }\n").unwrap();
  build_index(&src, &out).unwrap();
  let kg = Kg::load(&out).unwrap();
  assert!(
    kg.nodes_with_external_id(eid).is_empty(),
    "a renamed symbol must not silently keep the old identity"
  );

  let _ = fs::remove_dir_all(&base);
}

/// Explicit cache-validity modes (IMPROVEMENTS 07-29 §3), adversarial search-fed path: a
/// banked product whose source then suffers a preserved-mtime same-size edit replays stale
/// under `fast-stat` outside the racy window (the documented blind spot), and `Verified`
/// catches it — as a first-class mode, not an env var.
#[test]
fn verified_mode_catches_stale_banked_products() {
  use std::time::Duration;
  let base = std::env::temp_dir().join(format!("vorpal-index-mode-{}", std::process::id()));
  let src = base.join("src");
  let out = src.join(".vorpal").join("index");
  let _ = fs::remove_dir_all(&base);
  fs::create_dir_all(&src).unwrap();
  let victim = src.join("victim.rs");
  let other = src.join("other.rs");
  fs::write(&victim, "pub fn mode_one() -> u32 { 1 }\n").unwrap();
  fs::write(&other, "pub fn other_a() -> u32 { 0 }\n").unwrap();
  // Push the victim's mtime 10s into the past so nothing here lands in the racy window
  // (which would verify digests and hide the blind spot this test documents).
  let past = std::time::SystemTime::now() - Duration::from_secs(10);
  let set_past = |p: &std::path::Path| {
    let h = fs::OpenOptions::new().write(true).open(p).unwrap();
    h.set_times(fs::FileTimes::new().set_modified(past)).unwrap();
  };
  set_past(&victim);
  let first = build_index(&src, &out).unwrap();
  assert_eq!(first.cache_mode, "fast-stat", "default mode is reported");

  // Search banks a product for v2 (same size as v1, mtime restored to the past stamp)…
  fs::write(&victim, "pub fn mode_two() -> u32 { 2 }\n").unwrap();
  set_past(&victim);
  assert!(vorpal_index::warm_product_cache(&victim).unwrap(), "banked");
  // …then the file is edited again, same size, mtime restored: the banked product's stat
  // still matches the file, but its content does not.
  fs::write(&victim, "pub fn mode_ten() -> u32 { 3 }\n").unwrap();
  set_past(&victim);
  // Change the other file so the whole-tree fast path cannot short-circuit the bank replay.
  fs::write(&other, "pub fn other_b() -> u32 { 9 }\n").unwrap();

  // fast-stat, selected EXPLICITLY (another test sets VORPAL_VERIFY_CACHE=1 process-wide,
  // and this assertion is about the mode contract, not the env default): the stale banked
  // product replays — the documented blind spot.
  vorpal_index::build_index_with(&src, &out, vorpal_index::CacheMode::FastStat).unwrap();
  let kg = Kg::load(&out).unwrap();
  assert_eq!(
    kg.nodes_named("mode_two").len(),
    1,
    "fast-stat replays the stale bank (documented blind spot)"
  );
  drop(kg);

  // Verified: content-authoritative — the stale bank is rejected and the file re-parses.
  let report =
    vorpal_index::build_index_with(&src, &out, vorpal_index::CacheMode::Verified).unwrap();
  assert_eq!(report.cache_mode, "verified", "mode is reported");
  let kg = Kg::load(&out).unwrap();
  assert_eq!(kg.nodes_named("mode_ten").len(), 1, "current content indexed");
  assert!(kg.nodes_named("mode_two").is_empty(), "stale symbol gone");

  let _ = fs::remove_dir_all(&base);
}

/// The traversal contract (IMPROVEMENTS 07-29 §6): reachable is selector-consistent (ambiguous
/// seeds list candidates), returns each reached node WITH its path back to the seed, and a
/// grade floor stops traversal at the first edge below it.
#[test]
fn reachable_returns_paths_and_respects_grade_floor() {
  let base = std::env::temp_dir().join(format!("vorpal-index-reach-{}", std::process::id()));
  let src = base.join("src");
  let out = base.join("index");
  let _ = fs::remove_dir_all(&base);
  fs::create_dir_all(&src).unwrap();
  // top → mid (cross-file constrained) → bottom (same-file exact); plus an ambiguous callee
  // to give the chain a heuristic tail: mid also calls amb(), defined in TWO other files.
  fs::write(
    src.join("a_top.rs"),
    "pub fn top() -> u32 { mid() }\n",
  )
  .unwrap();
  fs::write(
    src.join("b_mid.rs"),
    "pub fn mid() -> u32 { bottom() + amb() }\npub fn bottom() -> u32 { 3 }\n",
  )
  .unwrap();
  fs::write(src.join("c_amb1.rs"), "pub fn amb() -> u32 { 1 }\n").unwrap();
  fs::write(src.join("d_amb2.rs"), "pub fn amb() -> u32 { 2 }\n").unwrap();
  build_index(&src, &out).unwrap();
  let kg = Kg::load(&out).unwrap();

  let query = |grade: Option<&str>| {
    vorpal_index::reachable_query_on(
      &kg,
      &vorpal_index::GraphTarget {
        name: "top".into(),
        ..vorpal_index::GraphTarget::default()
      },
      vorpal_index::Direction::Out,
      &[vorpal_index::EdgeType::CALLS],
      None,
      vorpal_index::min_confidence_for_grade(grade).unwrap(),
    )
    .unwrap()
  };

  // Paths, not sets: every reached node carries its chain back to the seed.
  let rendered = query(None);
  assert!(rendered.contains("top -calls→ mid"), "{rendered}");
  assert!(
    rendered.contains("top -calls→ mid -calls→ bottom"),
    "{rendered}"
  );
  assert!(rendered.contains("-calls→ amb"), "heuristic tail present:\n{rendered}");

  // Grade floor `constrained`: the heuristic amb() hop is not crossed; the rest survives.
  let floored = query(Some("constrained"));
  assert!(floored.contains("mid"), "{floored}");
  assert!(floored.contains("bottom"), "{floored}");
  assert!(!floored.contains("amb"), "heuristic edge crossed the floor:\n{floored}");

  // Selector consistency: an ambiguous seed lists candidates instead of unioning namesakes.
  let ambiguous = vorpal_index::reachable_query_on(
    &kg,
    &vorpal_index::GraphTarget {
      name: "amb".into(),
      ..vorpal_index::GraphTarget::default()
    },
    vorpal_index::Direction::In,
    &[vorpal_index::EdgeType::CALLS],
    None,
    0,
  )
  .unwrap();
  assert!(ambiguous.contains("ambiguous"), "{ambiguous}");
  assert!(ambiguous.contains("refine"), "{ambiguous}");

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
  fs::remove_file(live(&out).join("manifest.bin")).unwrap();
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

/// A cold search (no ann.bin) must serve real results without building or creating any
/// vector-tier state — the fused exhaustive fallback, not a blocking build.
#[test]
fn cold_search_serves_without_ann_bin() {
  let base = std::env::temp_dir().join(format!("vorpal-index-coldscan-{}", std::process::id()));
  let src = base.join("src");
  let out = base.join("index");
  let _ = fs::remove_dir_all(&base);
  fs::create_dir_all(&src).unwrap();
  fs::write(
    src.join("resolver.rs"),
    "pub fn resolve_import_path(path: &str) -> bool {\n  !path.is_empty()\n}\n",
  )
  .unwrap();
  fs::write(
    src.join("other.rs"),
    "pub fn unrelated_helper() -> u32 {\n  1\n}\n",
  )
  .unwrap();
  build_index(&src, &out).unwrap();

  assert!(
    !out.join("ann.bin").exists(),
    "index run must not build the vector tier"
  );
  let rendered = search_index(&out, "import path resolution", 3).unwrap();
  assert!(
    rendered
      .lines()
      .next()
      .unwrap_or("")
      .contains("resolve_import_path"),
    "cold fallback should rank the descriptive match first:\n{rendered}"
  );
  assert!(
    !out.join("ann.bin").exists(),
    "a cold search must not create vector-tier state as a side effect"
  );
  let _ = fs::remove_dir_all(&base);
}

/// At flat-exact scale the warm tier IS an exhaustive scan, so cold and warm searches must
/// render byte-identical output — the strongest cross-tier gate available.
#[test]
fn cold_scan_matches_warm_flat_exact_results() {
  let base = std::env::temp_dir().join(format!("vorpal-index-coldeq-{}", std::process::id()));
  let src = base.join("src");
  let out = base.join("index");
  let _ = fs::remove_dir_all(&base);
  fs::create_dir_all(&src).unwrap();
  for i in 0..12 {
    fs::write(
      src.join(format!("mod_{i}.rs")),
      format!(
        "pub fn helper_{i}(value: u32) -> u32 {{\n  value + {i}\n}}\npub struct Widget{i} {{ pub field: u32 }}\n"
      ),
    )
    .unwrap();
  }
  build_index(&src, &out).unwrap();

  let cold_a = search_index(&out, "widget helper value", 5).unwrap();
  let cold_b = search_index(&out, "widget helper value", 5).unwrap();
  assert_eq!(cold_a, cold_b, "cold scan must be deterministic");

  vorpal_index::warm_ann(&out).unwrap();
  assert!(live(&out).join("ann.bin").exists());
  let warm = search_index(&out, "widget helper value", 5).unwrap();
  assert_eq!(
    cold_a, warm,
    "flat-exact warm results must equal the cold scan byte-for-byte"
  );
  let _ = fs::remove_dir_all(&base);
}

/// The cross-process build lock: a second warm attempted while the first holds the lock
/// must skip (return Ok, build nothing) rather than double-build.
#[test]
fn build_lock_excludes_second_builder() {
  let base = std::env::temp_dir().join(format!("vorpal-index-lock-{}", std::process::id()));
  let src = base.join("src");
  let out = base.join("index");
  let _ = fs::remove_dir_all(&base);
  fs::create_dir_all(&src).unwrap();
  fs::write(src.join("a.rs"), "pub fn locked_symbol() -> u32 { 3 }\n").unwrap();
  build_index(&src, &out).unwrap();

  // Hold the lock as "another process" would, then ask for a warm: it must skip.
  let lock_file = std::fs::OpenOptions::new()
    .create(true)
    .truncate(false)
    .write(true)
    .open(out.join("ann.build.lock"))
    .unwrap();
  let mut lock = fd_lock::RwLock::new(lock_file);
  let guard = lock.try_write().unwrap();
  vorpal_index::warm_ann(&out).unwrap();
  assert!(
    !out.join("ann.bin").exists(),
    "a lock-skipped warm must not build or write anything"
  );
  drop(guard);

  // Lock released: the warm proceeds and commits.
  vorpal_index::warm_ann(&out).unwrap();
  assert!(live(&out).join("ann.bin").exists());
  assert!(live(&out).join("ann.stamp").exists());
  let _ = fs::remove_dir_all(&base);
}

/// Unregistered processes (this test binary) must never spawn a detached warm — a cold
/// search in a test may not leave background work behind.
#[test]
fn autowarm_never_spawns_unregistered() {
  let base = std::env::temp_dir().join(format!("vorpal-index-nospawn-{}", std::process::id()));
  let src = base.join("src");
  let out = base.join("index");
  let _ = fs::remove_dir_all(&base);
  fs::create_dir_all(&src).unwrap();
  fs::write(src.join("a.rs"), "pub fn spawnless_symbol() -> u32 { 4 }\n").unwrap();
  build_index(&src, &out).unwrap();

  let _ = search_index(&out, "spawnless symbol", 3).unwrap();
  // Give any (incorrect) spawned child a moment, then verify nothing built the tier.
  std::thread::sleep(std::time::Duration::from_millis(300));
  assert!(
    !out.join("ann.bin").exists(),
    "an unregistered process must not have spawned a background warm"
  );
  let _ = fs::remove_dir_all(&base);
}

/// `ann.files` invariants: ranges partition the node-id space in order, the digest reacts
/// to content changes, stays put across no-op rebuilds, and both artifacts carry the same
/// generation stamp as the header of ann.bin.
#[test]
fn ann_files_partition_and_stamp_cohere() {
  let base = std::env::temp_dir().join(format!("vorpal-index-annfiles-{}", std::process::id()));
  let src = base.join("src");
  let out = base.join("index");
  let _ = fs::remove_dir_all(&base);
  fs::create_dir_all(&src).unwrap();
  fs::write(src.join("a.rs"), "pub fn alpha_fn() -> u32 { 1 }\n").unwrap();
  fs::write(src.join("b.rs"), "pub fn beta_fn() -> u32 { 2 }\n").unwrap();
  build_index(&src, &out).unwrap();
  vorpal_index::warm_ann(&out).unwrap();

  let kg = Kg::load(&out).unwrap();
  let (files_stamp, runs) = vorpal_index::annfiles::load(&live(&out)).unwrap();
  // Partition: sorted, contiguous, exactly covering [0, node_count).
  let mut expected_start = 0u64;
  for run in &runs {
    assert_eq!(
      run.start, expected_start,
      "runs must be contiguous in id order"
    );
    expected_start += run.len as u64;
  }
  assert_eq!(
    expected_start,
    kg.node_count() as u64,
    "runs must cover every node"
  );
  assert_eq!(runs.len(), 2);

  // Generation coherence: bin header stamp == files stamp == stamp file.
  let (_dim, bin_stamp) = vorpal_index::peek_ann_header(&out).unwrap();
  assert_eq!(bin_stamp, files_stamp);
  let stored =
    u64::from_le_bytes(fs::read(live(&out).join("ann.stamp")).unwrap().try_into().unwrap());
  assert_eq!(stored, files_stamp);

  // Digest stability: rebuild with no changes → identical bytes.
  let before = fs::read(live(&out).join("ann.files")).unwrap();
  vorpal_index::warm_ann(&out).unwrap();
  assert_eq!(
    before,
    fs::read(live(&out).join("ann.files")).unwrap(),
    "no-op warm must not rewrite"
  );

  // Digest sensitivity: change one file, re-index, re-warm → that run's digest changes,
  // the other's does not.
  std::thread::sleep(std::time::Duration::from_millis(1100));
  fs::write(src.join("a.rs"), "pub fn alpha_fn_changed() -> u32 { 9 }\n").unwrap();
  build_index(&src, &out).unwrap();
  vorpal_index::warm_ann(&out).unwrap();
  let (_, runs_after) = vorpal_index::annfiles::load(&out).unwrap();
  let digest_of = |runs: &[vorpal_index::annfiles::FileRun], suffix: &str| {
    runs
      .iter()
      .find(|r| r.path.ends_with(suffix))
      .unwrap()
      .digest
  };
  assert_ne!(digest_of(&runs, "a.rs"), digest_of(&runs_after, "a.rs"));
  assert_eq!(digest_of(&runs, "b.rs"), digest_of(&runs_after, "b.rs"));
  let _ = fs::remove_dir_all(&base);
}

/// The overlay path end-to-end: after an edit + re-index (base now stale), a search must
/// find the new symbol (exact overlay), never return the deleted one (tombstones), and
/// still find symbols from unchanged files whose ids SHIFTED (remap) — all without any
/// ANN rebuild.
#[test]
fn overlay_search_serves_edits_without_rebuild() {
  let base = std::env::temp_dir().join(format!("vorpal-index-overlay-{}", std::process::id()));
  let src = base.join("src");
  let out = base.join("index");
  let _ = fs::remove_dir_all(&base);
  fs::create_dir_all(&src).unwrap();
  // aa.rs sorts first: editing it shifts every later file's node ids. The filler files keep
  // the edit under the overlay-size threshold so the OVERLAY path (not the fallback) serves
  // — asserted directly on OverlayView below.
  fs::write(src.join("aa.rs"), "pub fn doomed_symbol() -> u32 { 1 }\n").unwrap();
  fs::write(
    src.join("mm.rs"),
    "pub fn middle_helper(v: u32) -> u32 { v }\n",
  )
  .unwrap();
  for i in 0..40 {
    fs::write(
      src.join(format!("pp_{i:02}.rs")),
      format!("pub fn padding_fn_{i}(v: u32) -> u32 {{ v + {i} }}\n"),
    )
    .unwrap();
  }
  fs::write(
    src.join("zz.rs"),
    "pub fn tail_anchor_symbol() -> u32 { 3 }\n",
  )
  .unwrap();
  build_index(&src, &out).unwrap();
  vorpal_index::warm_ann(&out).unwrap();
  let ann_before = fs::read(live(&out).join("ann.bin")).unwrap();

  // Edit the first file: replace the doomed symbol with two fresh ones (range LENGTH grows,
  // so later files' ids shift and aa.rs is overlay by construction).
  std::thread::sleep(std::time::Duration::from_millis(1100));
  fs::write(
    src.join("aa.rs"),
    "pub fn fresh_overlay_symbol() -> u32 { 7 }\npub fn second_fresh_symbol() -> u32 { 8 }\n",
  )
  .unwrap();
  build_index(&src, &out).unwrap();

  // Prove the overlay path is live for this state: the view assembles, covers exactly the
  // edited file's rows, and remaps a shifted unchanged-file id correctly.
  {
    let kg = Kg::load(&out).unwrap();
    let view = vorpal_index::annfiles::OverlayView::assemble(&out, &kg, 256)
      .expect("edit is under the size threshold: the overlay path must engage");
    assert!(
      view.tombstoned_nodes > 0,
      "the edited file's base rows must be dead"
    );
    assert!(
      !view.overlay_ids.is_empty(),
      "the edited file's current rows must be overlay candidates"
    );
    // aa.rs grew by one node, so every later id shifted by +1: base id N must remap to N+1
    // for a node in an unchanged file (pick one well past aa.rs's range).
    let probe = (kg.node_count() as u64) - 2;
    assert_eq!(
      view.remap(probe - 1),
      Some(probe),
      "unchanged-file ids must remap by the exact shift"
    );
  }

  let fresh = search_index(&out, "fresh overlay symbol", 3).unwrap();
  assert!(
    fresh
      .lines()
      .next()
      .unwrap_or("")
      .contains("fresh_overlay_symbol"),
    "edited-file symbol must be immediately searchable:\n{fresh}"
  );
  let doomed = search_index(&out, "doomed symbol", 5).unwrap();
  assert!(
    !doomed.contains("doomed_symbol"),
    "deleted symbol must never surface:\n{doomed}"
  );
  let shifted = search_index(&out, "tail anchor symbol", 3).unwrap();
  assert!(
    shifted
      .lines()
      .next()
      .unwrap_or("")
      .contains("tail_anchor_symbol"),
    "unchanged-file symbol must survive the id shift via remap:\n{shifted}"
  );
  // And all of it without touching the base artifact — which the new generation carried
  // forward from the prior one (hardlinked at commit), so the overlay engages at all.
  assert_eq!(
    ann_before,
    fs::read(live(&out).join("ann.bin")).unwrap(),
    "overlay search must not rebuild or rewrite the base"
  );

  // Determinism: same state, same bytes.
  assert_eq!(
    search_index(&out, "tail anchor symbol", 3).unwrap(),
    shifted,
    "overlay search must be deterministic"
  );

  // Deleted file: remove zz.rs entirely — its symbol must vanish, others still fine.
  fs::remove_file(src.join("zz.rs")).unwrap();
  build_index(&src, &out).unwrap();
  let gone = search_index(&out, "tail anchor symbol", 5).unwrap();
  assert!(
    !gone.contains("tail_anchor_symbol"),
    "deleted file's symbols must vanish:\n{gone}"
  );
  let still = search_index(&out, "middle helper", 3).unwrap();
  assert!(
    still.contains("middle_helper"),
    "surviving files still searchable:\n{still}"
  );
  let _ = fs::remove_dir_all(&base);
}

/// Torn artifact combinations must never produce wrong results — every mismatch routes to
/// the exhaustive fallback, whose output is the reference by definition.
#[test]
fn torn_artifacts_route_to_fallback() {
  let base = std::env::temp_dir().join(format!("vorpal-index-torn-{}", std::process::id()));
  let src = base.join("src");
  let out = base.join("index");
  let _ = fs::remove_dir_all(&base);
  fs::create_dir_all(&src).unwrap();
  fs::write(src.join("a.rs"), "pub fn torn_case_symbol() -> u32 { 1 }\n").unwrap();
  build_index(&src, &out).unwrap();
  vorpal_index::warm_ann(&out).unwrap();
  let reference = search_index(&out, "torn case symbol", 3).unwrap();

  // Corrupt ann.files (bad magic) → overlay refused; fresh base still fine.
  fs::write(out.join("ann.files"), b"XXXXGARBAGE").unwrap();
  assert_eq!(
    search_index(&out, "torn case symbol", 3).unwrap(),
    reference
  );

  // Stale stamp (wrong generation) + garbage files → fallback; results still correct.
  fs::write(out.join("ann.stamp"), 0xDEAD_BEEF_u64.to_le_bytes()).unwrap();
  assert_eq!(
    search_index(&out, "torn case symbol", 3).unwrap(),
    reference
  );

  // Truncated bin header + stale stamp → fallback; still correct.
  fs::write(out.join("ann.bin"), b"short").unwrap();
  assert_eq!(
    search_index(&out, "torn case symbol", 3).unwrap(),
    reference
  );
  let _ = fs::remove_dir_all(&base);
}

/// Past the overlay-size ceiling (a huge fraction of files changed), assemble must refuse
/// and the search must take the exhaustive fallback — correct either way.
#[test]
fn overlay_size_threshold_falls_back() {
  let base = std::env::temp_dir().join(format!("vorpal-index-ovthresh-{}", std::process::id()));
  let src = base.join("src");
  let out = base.join("index");
  let _ = fs::remove_dir_all(&base);
  fs::create_dir_all(&src).unwrap();
  fs::write(src.join("a.rs"), "pub fn thresh_alpha() -> u32 { 1 }\n").unwrap();
  fs::write(src.join("b.rs"), "pub fn thresh_beta() -> u32 { 2 }\n").unwrap();
  build_index(&src, &out).unwrap();
  vorpal_index::warm_ann(&out).unwrap();

  // Rewrite half the corpus: way past 15% of nodes.
  std::thread::sleep(std::time::Duration::from_millis(1100));
  fs::write(
    src.join("a.rs"),
    "pub fn thresh_alpha_two() -> u32 { 3 }\npub fn thresh_alpha_three() -> u32 { 4 }\n",
  )
  .unwrap();
  build_index(&src, &out).unwrap();

  let kg = Kg::load(&out).unwrap();
  assert!(
    vorpal_index::annfiles::OverlayView::assemble(&out, &kg, 256).is_none(),
    "an oversized overlay must refuse assembly"
  );
  // And the search still answers correctly (fallback).
  let rendered = search_index(&out, "thresh alpha two", 3).unwrap();
  assert!(rendered.contains("thresh_alpha_two"), "{rendered}");
  let _ = fs::remove_dir_all(&base);
}

/// IMPROVEMENTS §1 acceptance: same-named definitions are selectable independently;
/// ambiguous names yield candidates (never a silent merged neighborhood); --all merges
/// explicitly; and the persisted name index agrees with the scan fallback.
#[test]
fn symbol_selector_disambiguates_namesakes() {
  let base = std::env::temp_dir().join(format!("vorpal-index-selector-{}", std::process::id()));
  let src = base.join("src");
  let out = base.join("index");
  let _ = fs::remove_dir_all(&base);
  fs::create_dir_all(&src).unwrap();
  // Two same-named `shared_fn` definitions in different files, each with a distinct caller.
  fs::write(
    src.join("alpha.rs"),
    "pub fn shared_fn() -> u32 { 1 }\npub fn alpha_caller() -> u32 { shared_fn() }\n",
  )
  .unwrap();
  fs::write(
    src.join("beta.rs"),
    "pub fn shared_fn() -> u32 { 2 }\npub fn beta_caller() -> u32 { shared_fn() }\n",
  )
  .unwrap();
  build_index(&src, &out).unwrap();

  // Bare name is ambiguous → candidates with ids, no merged callers.
  let ambiguous = vorpal_index::graph_query(&out, "callers", "shared_fn").unwrap();
  assert!(ambiguous.contains("ambiguous"), "{ambiguous}");
  assert!(
    ambiguous.contains("alpha.rs") && ambiguous.contains("beta.rs"),
    "{ambiguous}"
  );
  assert!(
    !ambiguous.contains("alpha_caller") && !ambiguous.contains("beta_caller"),
    "candidates only — no silently merged results:\n{ambiguous}"
  );

  // Path refinement selects exactly one definition.
  let alpha_only = vorpal_index::graph_query_selected(
    &out,
    "callers",
    &vorpal_index::GraphTarget {
      name: "shared_fn".into(),
      path_suffix: Some("alpha.rs".into()),
      ..vorpal_index::GraphTarget::default()
    },
  )
  .unwrap();
  assert!(alpha_only.contains("alpha_caller"), "{alpha_only}");
  assert!(!alpha_only.contains("beta_caller"), "{alpha_only}");

  // Id refinement (ids discovered via the `node` verb) selects the beta definition.
  let listing = vorpal_index::graph_query(&out, "node", "shared_fn").unwrap();
  let beta_id: u64 = listing
    .lines()
    .find(|l| l.contains("beta.rs"))
    .and_then(|l| l.strip_prefix("id "))
    .and_then(|l| l.split_whitespace().next())
    .and_then(|n| n.parse().ok())
    .expect("node listing carries ids");
  let beta_only = vorpal_index::graph_query_selected(
    &out,
    "callers",
    &vorpal_index::GraphTarget {
      name: "shared_fn".into(),
      id: Some(beta_id),
      ..vorpal_index::GraphTarget::default()
    },
  )
  .unwrap();
  assert!(beta_only.contains("beta_caller"), "{beta_only}");
  assert!(!beta_only.contains("alpha_caller"), "{beta_only}");

  // Explicit merge restores the historical union.
  let merged = vorpal_index::graph_query_selected(
    &out,
    "callers",
    &vorpal_index::GraphTarget {
      name: "shared_fn".into(),
      merge_all: true,
      ..vorpal_index::GraphTarget::default()
    },
  )
  .unwrap();
  assert!(
    merged.contains("alpha_caller") && merged.contains("beta_caller"),
    "{merged}"
  );

  // names.idx: the persisted index and the scan fallback agree exactly.
  let kg = Kg::load(&out).unwrap();
  assert!(live(&out).join("names.idx").exists());
  let indexed = kg.nodes_named("shared_fn");
  assert_eq!(indexed.len(), 2);
  fs::remove_file(live(&out).join("names.idx")).unwrap();
  let scanned = Kg::load(&out).unwrap().nodes_named("shared_fn");
  assert_eq!(indexed, scanned, "index and scan fallback must agree");
  let _ = fs::remove_dir_all(&base);
}

/// IMPROVEMENTS §6 acceptance: an edit that preserves byte length and restores the mtime —
/// invisible to stat — is caught by content-digest validation, both in the racy window
/// (automatic) and under VORPAL_VERIFY_CACHE=1 (everywhere).
#[test]
fn same_size_restored_mtime_edit_is_caught() {
  let base = std::env::temp_dir().join(format!("vorpal-index-racy-{}", std::process::id()));
  let src = base.join("src");
  let out = base.join("index");
  let _ = fs::remove_dir_all(&base);
  fs::create_dir_all(&src).unwrap();
  let file = src.join("victim.rs");
  fs::write(&file, "pub fn victim_one() -> u32 { 1 }\n").unwrap();
  build_index(&src, &out).unwrap();

  // Same byte length, different content; restore the original mtime exactly.
  let original_mtime = fs::metadata(&file).unwrap().modified().unwrap();
  fs::write(&file, "pub fn victim_two() -> u32 { 2 }\n").unwrap();
  let handle = fs::OpenOptions::new().write(true).open(&file).unwrap();
  handle
    .set_times(fs::FileTimes::new().set_modified(original_mtime))
    .unwrap();
  drop(handle);
  assert_eq!(
    fs::metadata(&file).unwrap().modified().unwrap(),
    original_mtime,
    "mtime restoration is the attack precondition"
  );

  // Within seconds of the previous index, the racy window verifies digests automatically.
  let report = build_index(&src, &out).unwrap();
  assert_eq!(
    report.indexed, 1,
    "racy-window digest check must force a re-parse"
  );
  let renamed = search_index(&out, "victim two", 3).unwrap();
  assert!(renamed.contains("victim_two"), "{renamed}");

  // Outside the window, VORPAL_VERIFY_CACHE=1 verifies everything: repeat the attack and
  // check the paranoid mode catches it too (window may or may not still apply; the env
  // makes it unconditional).
  let original_mtime = fs::metadata(&file).unwrap().modified().unwrap();
  fs::write(&file, "pub fn victim_ten() -> u32 { 3 }\n").unwrap();
  let handle = fs::OpenOptions::new().write(true).open(&file).unwrap();
  handle
    .set_times(fs::FileTimes::new().set_modified(original_mtime))
    .unwrap();
  drop(handle);
  unsafe { std::env::set_var("VORPAL_VERIFY_CACHE", "1") };
  let report = build_index(&src, &out).unwrap();
  unsafe { std::env::remove_var("VORPAL_VERIFY_CACHE") };
  assert_eq!(
    report.indexed, 1,
    "verify mode must catch the stat-invisible edit"
  );
  let _ = fs::remove_dir_all(&base);
}

/// IMPROVEMENTS §5 fixture-matrix seed: cross-file call resolution and evidence labels,
/// per major language family, end-to-end (extract → resolve → link → query). Each language
/// asserts (a) the caller resolves, and (b) the edge carries a non-structural confidence
/// label through the query surface.
#[test]
fn resolution_matrix_across_languages() {
  let base = std::env::temp_dir().join(format!("vorpal-index-matrix-{}", std::process::id()));
  let src = base.join("src");
  let out = base.join("index");
  let _ = fs::remove_dir_all(&base);
  fs::create_dir_all(&src).unwrap();

  // Rust: cross-file exported call.
  fs::write(src.join("rs_def.rs"), "pub fn rust_target() -> u32 { 1 }\n").unwrap();
  fs::write(
    src.join("rs_use.rs"),
    "pub fn rust_caller() -> u32 { rust_target() }\n",
  )
  .unwrap();
  // Python: cross-file call.
  fs::write(src.join("py_def.py"), "def py_target():\n    return 1\n").unwrap();
  fs::write(
    src.join("py_use.py"),
    "def py_caller():\n    return py_target()\n",
  )
  .unwrap();
  // TypeScript: cross-file exported call.
  fs::write(
    src.join("ts_def.ts"),
    "export function tsTarget(): number { return 1 }\n",
  )
  .unwrap();
  fs::write(
    src.join("ts_use.ts"),
    "export function tsCaller(): number { return tsTarget() }\n",
  )
  .unwrap();
  build_index(&src, &out).unwrap();

  for (target, caller) in [
    ("rust_target", "rust_caller"),
    ("py_target", "py_caller"),
    ("tsTarget", "tsCaller"),
  ] {
    let rendered = vorpal_index::graph_query_selected(
      &out,
      "callers",
      &vorpal_index::GraphTarget {
        name: target.into(),
        show_ids: true,
        ..vorpal_index::GraphTarget::default()
      },
    )
    .unwrap();
    assert!(
      rendered.contains(caller),
      "{target}: caller must resolve cross-file:\n{rendered}"
    );
    assert!(
      rendered.contains("constrained") || rendered.contains("exact"),
      "{target}: the edge must carry a resolution grade:\n{rendered}"
    );
  }
  let _ = fs::remove_dir_all(&base);
}

/// Parse-health telemetry is language-agnostic graceful degradation: a file tree-sitter
/// cannot fully parse is *counted*, never silently dropped, in any language — and a clean
/// tree reports zero.
#[test]
fn parse_errors_are_counted_across_languages() {
  let base = std::env::temp_dir().join(format!("vorpal-index-health-{}", std::process::id()));
  let src = base.join("src");
  let out = base.join("index");
  let _ = fs::remove_dir_all(&base);
  fs::create_dir_all(&src).unwrap();
  fs::write(src.join("ok.rs"), "pub fn good() -> u32 { 1 }\n").unwrap();
  fs::write(src.join("ok.py"), "def good():\n    return 1\n").unwrap();
  let clean = build_index(&src, &out).unwrap();
  assert_eq!(
    clean.error_files, 0,
    "clean corpus must report zero parse errors"
  );

  // A file tree-sitter genuinely cannot parse to completion (unbalanced signature).
  fs::write(src.join("broken.rs"), "pub fn broken( -> u32 {\n").unwrap();
  let degraded = build_index(&src, &out).unwrap();
  assert!(
    degraded.error_files >= 1,
    "a broken file must be counted, not silently dropped: {}",
    degraded.error_files
  );

  // The signal persists through replay: re-index unchanged → still counted (from cache).
  let replayed = build_index(&src, &out).unwrap();
  assert!(
    replayed.reused || replayed.error_files >= 1,
    "health survives replay"
  );
  let _ = fs::remove_dir_all(&base);
}

/// IMPROVEMENTS #9: the persisted lexical tier answers the name channel without scanning —
/// and answers it IDENTICALLY. Cold (no postings) and warm (posting intersection) searches
/// must render byte-for-byte the same, the tier must be stamped fresh after a warm, and the
/// artifact must be deterministic.
#[test]
fn posting_tier_matches_the_scan_byte_for_byte() {
  // Detached autowarm children would race this test's own warm (and a re-commit can rewrite
  // the generation dir under us); the veto keeps the sequence deterministic.
  unsafe { std::env::set_var("VORPAL_NO_AUTOWARM", "1") };
  let base = std::env::temp_dir().join(format!("vorpal-postings-eq-{}", std::process::id()));
  let src = base.join("src");
  let out = base.join("index");
  let _ = fs::remove_dir_all(&base);
  fs::create_dir_all(&src).unwrap();
  fs::write(
    src.join("a.rs"),
    "pub fn parse_config() -> u32 { 1 }\n\
     pub fn config_parser() -> u32 { 2 }\n\
     pub fn parse_thing() -> u32 { parse_config() + config_parser() }\n",
  )
  .unwrap();
  fs::write(src.join("b.rs"), "pub fn unrelated_helper() -> u32 { 3 }\n").unwrap();
  vorpal_index::build_index(&src, &out).unwrap();
  let gen_dir = live(&out);

  // Cold: no posting tier yet — the exhaustive scan answers.
  assert!(
    vorpal_index::postings::Postings::load(&gen_dir).is_none(),
    "no postings before a warm"
  );
  let cold_multi = vorpal_index::search_index_explained(&out, "parse config", 10).unwrap();
  let cold_single = vorpal_index::search_index_explained(&out, "parse", 10).unwrap();
  let cold_exact = vorpal_index::search_index_explained(&out, "config_parser", 10).unwrap();

  // Warm builds ANN + postings under the same stamp discipline.
  vorpal_index::warm_ann(&out).unwrap();
  let postings = vorpal_index::postings::Postings::load(&gen_dir).expect("warm built postings");
  let first_bytes = fs::read(gen_dir.join("postings.bin")).unwrap();

  let warm_multi = vorpal_index::search_index_explained(&out, "parse config", 10).unwrap();
  let warm_single = vorpal_index::search_index_explained(&out, "parse", 10).unwrap();
  let warm_exact = vorpal_index::search_index_explained(&out, "config_parser", 10).unwrap();
  assert_eq!(cold_multi, warm_multi, "multi-token query must not change");
  assert_eq!(cold_single, warm_single, "single-token query must not change");
  assert_eq!(cold_exact, warm_exact, "exact-name query must not change");
  assert!(
    warm_multi.contains("parse_config") && warm_multi.contains("config_parser"),
    "{warm_multi}"
  );

  // The intersection itself: "parse"+"config" admits only parse_config (config_parser
  // tokenizes to config+parser — "parser" ≠ "parse"), while "config" alone covers both.
  let both = postings
    .candidates(&["config".to_string(), "parse".to_string()])
    .unwrap();
  assert_eq!(both.len(), 1, "only parse_config carries both tokens: {both:?}");
  let config_only = postings.candidates(&["config".to_string()]).unwrap();
  assert_eq!(config_only.len(), 2, "parse_config + config_parser: {config_only:?}");

  // Determinism: rebuilding the tier reproduces identical bytes.
  fs::remove_file(gen_dir.join("postings.bin")).unwrap();
  vorpal_index::warm_ann(&out).unwrap();
  let second_bytes = fs::read(gen_dir.join("postings.bin")).unwrap();
  assert_eq!(first_bytes, second_bytes, "postings.bin must be bit-reproducible");

  let _ = fs::remove_dir_all(&base);
}
