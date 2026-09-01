//! The scoped-resolution oracle (SUBSECOND.md P4.5c-2, slice c2-i): re-resolving ONE
//! defs-stable-edited file against the PRIOR generation must produce exactly the outcomes
//! a full from-scratch build of the edited tree produces for that file — evidence rows
//! (edge and no-edge), edge sequences per source node, dataflow rows, and sketch rows,
//! field for field. This isolates the resolution-equality question from the artifact
//! surgery that composes on top of it (c2-iii): when THIS holds, a compose divergence is
//! surgery; when this breaks, no surgery can be right.

use std::fs;
use std::path::Path;

use vorpal_ingest::{OutlineExtractor, PackReader, encode_product_into};
use vorpal_kg::NodeId;

fn live(root: &Path) -> std::path::PathBuf {
  vorpal_kg::resolve_index_dir(root)
}

fn write_fixture(src: &Path) {
  // Python: keyword-bound call args (G-M5), a rets chain (maker().render()), and — inside
  // the file that will be EDITED — one function past the 32-token signing floor (sigs
  // non-vacuity) that is a near-clone of nothing.
  fs::write(
    src.join("b.py"),
    "def beta(x, k=None):\n    return x\n\nclass Widget:\n    def render(self):\n        return 1\n\ndef maker() -> Widget:\n    return Widget()\n",
  )
  .unwrap();
  fs::write(src.join("a.py"), edited_a(false)).unwrap();
  // A second language rides along: the scoped path must be language-blind.
  fs::write(
    src.join("lib.rs"),
    "pub fn helper(value: i32) -> i32 {\n    value + 1\n}\n\npub fn entry(seed: i32) -> i32 {\n    helper(seed)\n}\n",
  )
  .unwrap();
}

/// The edited file's two states. Defs-stable by construction: identical items, members,
/// signatures, imports, params, and returns — only bodies (and therefore references,
/// spans, and sketches) move. The edit rebinds beta's call shape, drops the chain call,
/// adds a call to an UNDEFINED name (an external no-edge row must appear), and reworks
/// the signed function's body (its sketch must change).
fn edited_a(edited: bool) -> String {
  let alpha_body = if edited {
    "    v = 7\n    return beta(v, k=v)\n"
  } else {
    "    w = 1\n    return beta(w)\n"
  };
  let chain_body = "    return maker().render()\n";
  let ghost_body = if edited {
    "    return ghost_call()\n"
  } else {
    "    return 0\n"
  };
  let churn_body = if edited {
    "    total = 3\n    for item in items:\n        if item > ceiling:\n            total += ceiling\n        elif item < floor:\n            total += floor\n        else:\n            total -= item\n    return total - len(items)\n"
  } else {
    "    total = 0\n    for item in items:\n        if item < floor:\n            total += floor\n        elif item > ceiling:\n            total += ceiling\n        else:\n            total += item\n    return total + len(items)\n"
  };
  format!(
    "from b import beta\n\ndef alpha():\n{alpha_body}\n\ndef use_chain():\n{chain_body}\n\ndef ghost():\n{ghost_body}\n\ndef churn(items, floor, ceiling):\n{churn_body}"
  )
}

#[test]
fn scoped_resolution_equals_scratch_for_a_defs_stable_edit() {
  unsafe { std::env::set_var("VORPAL_FORMAT", "next") };
  let base = std::env::temp_dir().join(format!("vorpal-scoped-oracle-{}", std::process::id()));
  let _ = fs::remove_dir_all(&base);
  let src = base.join("repo");
  fs::create_dir_all(&src).unwrap();
  write_fixture(&src);
  let src = src.canonicalize().unwrap();
  let tree_root = src.to_string_lossy().into_owned();
  let edited_path = src.join("a.py");
  let edited_str = edited_path.to_string_lossy().into_owned();

  // Prior generation, then the edit, then the scratch truth of the edited tree.
  let out_prior = base.join("index-prior");
  vorpal_index::build_index(&src, &out_prior).expect("prior build");
  let gen_prior = live(&out_prior);
  fs::write(&edited_path, edited_a(true)).unwrap();
  let out_truth = base.join("index-truth");
  vorpal_index::build_index(&src, &out_truth).expect("truth build");
  let gen_truth = live(&out_truth);

  // The ladder accepts this edit — and rejects a def-adding one.
  let extractor = OutlineExtractor::new().expect("extractor");
  let old_product = extractor
    .extract_product(&edited_str, &edited_a(false))
    .expect("old product");
  let new_product = extractor
    .extract_product(&edited_str, &edited_a(true))
    .expect("new product");
  let (mut old_bytes, mut new_bytes) = (Vec::new(), Vec::new());
  encode_product_into(&old_product, &mut old_bytes);
  encode_product_into(&new_product, &mut new_bytes);
  let old_view = vorpal_ingest::decode_product_view(&old_bytes).unwrap();
  let new_view = vorpal_ingest::decode_product_view(&new_bytes).unwrap();
  assert_eq!(
    vorpal_ingest::views_defs_stable_reject(&old_view, &new_view),
    None,
    "the constructed edit must be defs-stable"
  );
  let with_new_def = extractor
    .extract_product(&edited_str, &format!("{}\ndef fresh_def():\n    return 0\n", edited_a(true)))
    .expect("def-adding product");
  let mut with_new_def_bytes = Vec::new();
  encode_product_into(&with_new_def, &mut with_new_def_bytes);
  let with_new_def_view = vorpal_ingest::decode_product_view(&with_new_def_bytes).unwrap();
  assert_eq!(
    vorpal_ingest::views_defs_stable_reject(&old_view, &with_new_def_view),
    Some("definition set"),
    "a def-adding edit must be rejected"
  );

  // Scoped resolution against the PRIOR generation.
  let prior_kg = vorpal_kg::Kg::load(&gen_prior).expect("prior kg");
  let prior_map = vorpal_kg::NodeIdMap::from_dir(&gen_prior).expect("prior map");
  let pack = PackReader::open_rooted(&gen_prior, Some(&tree_root)).expect("prior pack");
  let file_key = vorpal_kg::identity::FileKey::of(
    vorpal_kg::identity::tree_relative(&edited_str, &tree_root),
  )
  .0;
  let interner = vorpal_ingest::Interner::default();
  let fetch = |path: &str| pack.get(path).map(<[u8]>::to_vec);
  // The layout→ordinal mapping through the writer's own duplicate collapse — the same
  // scratch ingest the compose performs.
  let layout_ords = {
    let mut scratch =
      vorpal_ingest::Ingestor::new(&interner, OutlineExtractor::new().unwrap());
    scratch.ingest_product_mapped(&edited_str, vorpal_ingest::decode_product(&new_bytes).unwrap())
  };
  let outcome = vorpal_ingest::scoped_resolve_file(
    &interner,
    &prior_kg,
    &prior_map,
    &vorpal_ingest::Resolver::new(),
    &fetch,
    &edited_str,
    file_key,
    &new_view,
    &layout_ords,
    usize::MAX,
  )
  .expect("scoped resolution");

  // The truth: the scratch build's rows for the same file. Defs-stability keeps the
  // universe, so the file's dense range must be IDENTICAL in both generations.
  let truth_kg = vorpal_kg::Kg::load(&gen_truth).expect("truth kg");
  let truth_map = vorpal_kg::NodeIdMap::from_dir(&gen_truth).expect("truth map");
  let &(_, base_prior, rows_prior) = prior_map
    .files()
    .iter()
    .find(|&&(key, _, _)| key == file_key)
    .expect("file in prior universe");
  let &(_, base_truth, rows_truth) = truth_map
    .files()
    .iter()
    .find(|&&(key, _, _)| key == file_key)
    .expect("file in truth universe");
  assert_eq!(
    (base_prior, rows_prior),
    (base_truth, rows_truth),
    "defs-stability must keep the file's dense range"
  );
  let file_range = base_truth..base_truth + u64::from(rows_truth);

  // Evidence: multiset equality, every field (the saver sorts canonically anyway).
  let row_key = |row: &vorpal_kg::EvidenceRow| {
    (
      row.from,
      row.to,
      row.name_hash,
      row.etype,
      row.reason,
      row.confidence,
      row.outcome as u8,
      row.candidates,
      row.span_start,
      row.span_end,
      row.alternatives.clone(),
    )
  };
  let mut truth_evidence: Vec<_> = file_range
    .clone()
    .flat_map(|id| truth_kg.evidence_from(NodeId::new(id)))
    .map(|row| row_key(&row))
    .collect();
  let mut scoped_evidence: Vec<_> = outcome.evidence.iter().map(row_key).collect();
  truth_evidence.sort_unstable();
  scoped_evidence.sort_unstable();
  assert_eq!(scoped_evidence, truth_evidence, "evidence rows must match the scratch build");
  assert!(
    scoped_evidence
      .iter()
      .any(|row| row.6 == vorpal_kg::EvidenceOutcome::External as u8),
    "the ghost_call site must yield an external no-edge row"
  );

  // Edges: per-source ORDERED equality over the non-containment segment — resolution
  // order with DATA_FLOWS spliced at first-pair positions, then the request tail. The
  // fixture has no near-clone pair and no git history, so SIMILAR and co-change are
  // structurally absent.
  let containment = |etype: vorpal_kg::EdgeType| {
    matches!(
      etype.base(),
      vorpal_kg::EdgeType::DEFINES | vorpal_kg::EdgeType::HAS_METHOD | vorpal_kg::EdgeType::HAS_FIELD
    )
  };
  for src_id in file_range.clone() {
    let truth_seq: Vec<(u32, u16)> = truth_kg
      .out_neighbors(NodeId::new(src_id))
      .into_iter()
      .filter(|(_, etype)| !containment(*etype))
      .map(|(dst, etype)| (dst.raw() as u32, etype.0))
      .collect();
    let mut scoped_seq: Vec<(u32, u16)> = outcome
      .edges
      .iter()
      .filter(|(from, _, _)| u64::from(*from) == src_id)
      .map(|(_, to, etype)| (*to, etype.0))
      .collect();
    scoped_seq.extend(
      outcome
        .request_edges
        .iter()
        .filter(|(from, _, _)| u64::from(*from) == src_id)
        .map(|(_, to, etype)| (*to, etype.0)),
    );
    assert_eq!(
      scoped_seq, truth_seq,
      "per-source edge sequence must match the scratch build (src {src_id})"
    );
  }

  // Dataflow rows for the file, in emission order.
  let truth_flows: Vec<_> = vorpal_kg::load_dataflow(&gen_truth)
    .expect("truth dataflow")
    .into_iter()
    .filter(|row| file_range.contains(&u64::from(row.from)))
    .map(|row| (row.from, row.to, row.span, row.arg_index, row.param_index, row.class, row.expr))
    .collect();
  let scoped_flows: Vec<_> = outcome
    .flows
    .iter()
    .map(|row| {
      (row.from, row.to, row.span, row.arg_index, row.param_index, row.class, row.expr.clone())
    })
    .collect();
  assert_eq!(scoped_flows, truth_flows, "dataflow rows must match the scratch build");
  let render_hash = (xxhash_rust::xxh3::xxh3_64(b"render") & 0xFFFF_FFFF) as u32;
  assert!(
    outcome.evidence.iter().any(|row| {
      row.name_hash == render_hash
        && row.outcome == vorpal_kg::EvidenceOutcome::Edge
        && row.to != vorpal_kg::NO_EDGE
    }),
    "the maker().render() chain must resolve through the rets ledger"
  );
  assert!(!scoped_flows.is_empty(), "beta(v, k=v) must carry traceable arguments");

  // Sketch rows: the file's sigs-family run, (ordinal, shingles, sketch)-exact.
  let truth_sigs = vorpal_kg::SigStore::open(&gen_truth).expect("truth sigs");
  let mut truth_rows: Vec<(u32, u32, Vec<u8>)> = truth_sigs
    .rows(&truth_map)
    .expect("sig rows resolve")
    .into_iter()
    .filter_map(|row| {
      let (key, ordinal) = truth_map.locate(row.node)?;
      (key == file_key).then(|| (ordinal, row.shingles, row.sketch.to_vec()))
    })
    .collect();
  let mut scoped_sigs: Vec<(u32, u32, Vec<u8>)> = outcome
    .sigs
    .iter()
    .map(|row| (((row.node - base_truth) as u32), row.shingles, row.sketch.to_vec()))
    .collect();
  truth_rows.sort_unstable();
  scoped_sigs.sort_unstable();
  assert_eq!(scoped_sigs, truth_rows, "sketch rows must match the scratch build");
  assert!(!scoped_sigs.is_empty(), "churn() must clear the signing floor");

  let _ = fs::remove_dir_all(&base);
}

/// The pairing-repair oracle (slice c2-ii): an edit that turns the file's signed function
/// into a near-clone of an existing pair must reproduce the scratch build's GLOBAL
/// similar-pair set — LSH banding, star caps, partner limits, ceiling and all — from the
/// prior sigs family with only the edited file's run swapped, and the diff must name
/// exactly the endpoints whose similar segments change.
#[test]
fn scoped_pairing_repair_equals_scratch_pair_set() {
  unsafe { std::env::set_var("VORPAL_FORMAT", "next") };
  let base = std::env::temp_dir().join(format!("vorpal-scoped-pairs-{}", std::process::id()));
  let _ = fs::remove_dir_all(&base);
  let src = base.join("repo");
  fs::create_dir_all(&src).unwrap();
  let clone_body = "    total = 0\n    for item in items:\n        if item < floor:\n            total += floor\n        elif item > ceiling:\n            total += ceiling\n        else:\n            total += item\n    return total + len(items)\n";
  fs::write(
    src.join("clones.py"),
    format!(
      "def sim_a(items, floor, ceiling):\n{clone_body}\n\ndef sim_b(items, floor, ceiling):\n{body2}",
      body2 = clone_body.replace("total += item", "total += item + 1"),
    ),
  )
  .unwrap();
  let distinct_body = "    acc = 1469598103934665603\n    for byte in blob:\n        acc = acc ^ byte\n        acc = (acc * 1099511628211) % 18446744073709551616\n        acc = acc + (acc >> 7)\n    return acc - len(blob)\n";
  let target = |edited: bool| {
    let body = if edited { clone_body } else { distinct_body };
    format!("def target_fn(items, floor, ceiling):\n{body}")
  };
  // The distinct body ignores two params — same SIGNATURE either way (defs-stable).
  fs::write(src.join("target.py"), target(false).replace("blob", "items")).unwrap();
  let src = src.canonicalize().unwrap();
  let tree_root = src.to_string_lossy().into_owned();
  let target_path = src.join("target.py");
  let target_str = target_path.to_string_lossy().into_owned();

  let out_prior = base.join("index-prior");
  vorpal_index::build_index(&src, &out_prior).expect("prior build");
  let gen_prior = live(&out_prior);
  fs::write(&target_path, target(true)).unwrap();
  let out_truth = base.join("index-truth");
  vorpal_index::build_index(&src, &out_truth).expect("truth build");
  let gen_truth = live(&out_truth);

  let prior_kg = vorpal_kg::Kg::load(&gen_prior).expect("prior kg");
  let prior_map = vorpal_kg::NodeIdMap::from_dir(&gen_prior).expect("prior map");
  let truth_kg = vorpal_kg::Kg::load(&gen_truth).expect("truth kg");

  // The edited file's fresh sketch rows, through the same scoped entry c2-i proved.
  let extractor = OutlineExtractor::new().expect("extractor");
  let product = extractor.extract_product(&target_str, &target(true)).expect("product");
  let mut bytes = Vec::new();
  encode_product_into(&product, &mut bytes);
  let view = vorpal_ingest::decode_product_view(&bytes).unwrap();
  let pack = PackReader::open_rooted(&gen_prior, Some(&tree_root)).expect("prior pack");
  let file_key = vorpal_kg::identity::FileKey::of(
    vorpal_kg::identity::tree_relative(&target_str, &tree_root),
  )
  .0;
  let interner = vorpal_ingest::Interner::default();
  let fetch = |path: &str| pack.get(path).map(<[u8]>::to_vec);
  let layout_ords = {
    let mut scratch =
      vorpal_ingest::Ingestor::new(&interner, OutlineExtractor::new().unwrap());
    scratch.ingest_product_mapped(&target_str, vorpal_ingest::decode_product(&bytes).unwrap())
  };
  let outcome = vorpal_ingest::scoped_resolve_file(
    &interner,
    &prior_kg,
    &prior_map,
    &vorpal_ingest::Resolver::new(),
    &fetch,
    &target_str,
    file_key,
    &view,
    &layout_ords,
    usize::MAX,
  )
  .expect("scoped resolution");

  // Prior side: the sigs family (canonical order) + the sealed pair set.
  let prior_rows: Vec<vorpal_ingest::SigRow> = vorpal_kg::SigStore::open(&gen_prior)
    .expect("prior sigs")
    .rows(&prior_map)
    .expect("rows resolve")
    .into_iter()
    .map(|row| vorpal_ingest::SigRow {
      node: u64::from(row.node),
      shingles: row.shingles,
      sketch: row.sketch,
    })
    .collect();
  let prior_pairs = vorpal_ingest::similar_pairs_of_kg(&prior_kg);
  let repair = vorpal_ingest::scoped_similar_repair(
    &prior_map,
    prior_rows,
    &prior_pairs,
    &[(file_key, outcome.sigs.as_slice())],
  )
  .expect("pairing repair");

  // THE gate: the repaired global pair set equals the scratch build's, exactly.
  let truth_pairs = vorpal_ingest::similar_pairs_of_kg(&truth_kg);
  assert_eq!(repair.fresh_pairs, truth_pairs, "repaired pair set must equal scratch");
  assert_ne!(
    prior_pairs, truth_pairs,
    "the edit must actually change the pair set (non-vacuity)"
  );

  // The diff names exactly the endpoints of the symmetric difference.
  let mut expected_changed: Vec<u32> = Vec::new();
  for pair in prior_pairs.iter().filter(|pair| !truth_pairs.contains(pair)) {
    expected_changed.push(pair.0 as u32);
    expected_changed.push(pair.1 as u32);
  }
  for pair in truth_pairs.iter().filter(|pair| !prior_pairs.contains(pair)) {
    expected_changed.push(pair.0 as u32);
    expected_changed.push(pair.1 as u32);
  }
  expected_changed.sort_unstable();
  expected_changed.dedup();
  assert_eq!(repair.changed_srcs, expected_changed, "changed endpoints must be exact");

  let _ = fs::remove_dir_all(&base);
}

