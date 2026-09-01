//! The defs-changed resolution oracle (P4.5c-3, slice c3-i): for an edit that CHANGES
//! the definition set — here a def added at the top of the file, shifting every later
//! ordinal — the overlay session must reproduce a full scratch build's outcomes for the
//! edited file AND every usage-dirty referrer, in the successor dense space, field for
//! field. The structural pins come first: the shift law's file table must equal the
//! scratch build's exactly, and the usage-derived dirty set must name exactly the
//! referrers (including a referrer whose reference did not resolve before the edit —
//! the no-edge rows are what make ADDED definitions findable).

use std::fs;
use std::path::{Path, PathBuf};

use vorpal_ingest::{OutlineExtractor, PackReader, encode_product_into};
use vorpal_kg::NodeId;

fn live(root: &Path) -> PathBuf {
  vorpal_kg::resolve_index_dir(root)
}

fn b_py(edited: bool) -> &'static str {
  if edited {
    // gamma_new lands ABOVE beta/omega: both survivors' ordinals shift.
    "def gamma_new():\n    return 1\n\ndef beta(x):\n    return x\n\ndef omega():\n    return 9\n"
  } else {
    "def beta(x):\n    return x\n\ndef omega():\n    return 9\n"
  }
}

const A_PY: &str = "from b import beta, gamma_new\n\ndef alpha(v):\n    return beta(v)\n\ndef alpha_v():\n    return gamma_new()\n";
const C_PY: &str = "def unrelated():\n    return 2\n";

fn name_hash(name: &str) -> u32 {
  (xxhash_rust::xxh3::xxh3_64(name.as_bytes()) & 0xFFFF_FFFF) as u32
}

#[test]
fn defs_changed_session_equals_scratch_for_the_closure() {
  unsafe { std::env::set_var("VORPAL_FORMAT", "next") };
  let base = std::env::temp_dir().join(format!("vorpal-dc-oracle-{}", std::process::id()));
  let _ = fs::remove_dir_all(&base);
  let src = base.join("repo");
  fs::create_dir_all(&src).unwrap();
  fs::write(src.join("a.py"), A_PY).unwrap();
  fs::write(src.join("b.py"), b_py(false)).unwrap();
  fs::write(src.join("c.py"), C_PY).unwrap();
  let src = src.canonicalize().unwrap();
  let tree_root = src.to_string_lossy().into_owned();
  let path_of = |file: &str| src.join(file).to_string_lossy().into_owned();
  let key_of = |file: &str| {
    vorpal_kg::identity::FileKey::of(vorpal_kg::identity::tree_relative(
      &path_of(file),
      &tree_root,
    ))
    .0
  };

  let out_prior = base.join("index-prior");
  vorpal_index::build_index(&src, &out_prior).expect("prior build");
  let gen_prior = live(&out_prior);
  fs::write(src.join("b.py"), b_py(true)).unwrap();
  let out_truth = base.join("index-truth");
  vorpal_index::build_index(&src, &out_truth).expect("truth build");
  let gen_truth = live(&out_truth);

  let prior_kg = vorpal_kg::Kg::load(&gen_prior).expect("prior kg");
  let prior_map = vorpal_kg::NodeIdMap::from_dir(&gen_prior).expect("prior map");
  let truth_map = vorpal_kg::NodeIdMap::from_dir(&gen_truth).expect("truth map");
  let pack = PackReader::open_rooted(&gen_prior, Some(&tree_root)).expect("prior pack");
  let interner = vorpal_ingest::Interner::default();
  let extractor = OutlineExtractor::new().expect("extractor");

  // The edited file's fresh product, view, layout bridge, and scratch seal.
  let b_path = path_of("b.py");
  let b_key = key_of("b.py");
  let product = extractor.extract_product(&b_path, b_py(true)).expect("fresh product");
  let mut fresh_bytes = Vec::new();
  encode_product_into(&product, &mut fresh_bytes);
  let fresh_view = vorpal_ingest::decode_product_view(&fresh_bytes).unwrap();
  let (fresh_ords, fresh_kg) = {
    let mut scratch = vorpal_ingest::Ingestor::new(&interner, OutlineExtractor::new().unwrap());
    let ords = scratch
      .ingest_product_mapped(&b_path, vorpal_ingest::decode_product(&fresh_bytes).unwrap());
    (ords, scratch.seal())
  };

  // The ladder accepts (grammar/errors stable), and the affected set is EXACT: the added
  // def plus BOTH ordinal-shifted survivors.
  let prior_bytes = pack.get(&b_path).expect("prior product");
  let old_view = vorpal_ingest::decode_product_view(prior_bytes).unwrap();
  assert_eq!(vorpal_ingest::views_defs_changed_reject(&old_view, &fresh_view), None);
  let &(_, b_start, b_rows) =
    prior_map.files().iter().find(|&&(key, _, _)| key == b_key).unwrap();
  let affected = vorpal_ingest::affected_def_names(&prior_kg, b_start, b_rows, &fresh_kg);
  assert_eq!(
    affected,
    vec!["beta".to_string(), "gamma_new".to_string(), "omega".to_string()],
    "adds AND ordinal-shifted survivors are affected"
  );

  // The usage-derived dirty set: exactly a.py — found through beta's edges AND
  // gamma_new's prior NO-EDGE rows (the added-def case) — and never c.py.
  let usage = vorpal_kg::UsageStore::open(&gen_prior).expect("prior usage");
  let mut dirty_keys: Vec<u64> = affected
    .iter()
    .flat_map(|name| usage.files_referencing(name_hash(name)))
    .filter(|&key| key != b_key)
    .collect();
  dirty_keys.sort_unstable();
  dirty_keys.dedup();
  assert_eq!(dirty_keys, vec![key_of("a.py")], "the dirty closure is exactly a.py");

  // The dirty file's inputs come from its UNCHANGED packed product.
  let a_path = path_of("a.py");
  let a_bytes: Vec<u8> = pack.get(&a_path).expect("a product").to_vec();
  let a_view = vorpal_ingest::decode_product_view(&a_bytes).unwrap();
  let a_ords = {
    let mut scratch = vorpal_ingest::Ingestor::new(&interner, OutlineExtractor::new().unwrap());
    scratch.ingest_product_mapped(&a_path, vorpal_ingest::decode_product(&a_bytes).unwrap())
  };

  let fetch = |path: &str| pack.get(path).map(<[u8]>::to_vec);
  let edited = vorpal_ingest::DirtyFileInput {
    path: b_path.clone(),
    file_key: b_key,
    view: &fresh_view,
    layout_ords: &fresh_ords,
  };
  let dirty = [vorpal_ingest::DirtyFileInput {
    path: a_path.clone(),
    file_key: key_of("a.py"),
    view: &a_view,
    layout_ords: &a_ords,
  }];
  let outcomes = vorpal_ingest::resolve_defs_changed(
    &interner,
    &prior_kg,
    &prior_map,
    &vorpal_ingest::Resolver::new(),
    &fetch,
    &edited,
    &fresh_kg,
    &dirty,
    usize::MAX,
  )
  .expect("defs-changed session");
  assert_eq!(outcomes.len(), 2);

  // STRUCTURAL PIN: the shift law's successor table must equal the scratch build's.
  let delta = fresh_kg.node_count() as i64 - i64::from(b_rows);
  let b_old_end = b_start + u64::from(b_rows);
  for &(key, prior_start, rows) in prior_map.files() {
    let (want_start, want_rows) = if key == b_key {
      (b_start, fresh_kg.node_count() as u32)
    } else if prior_start >= b_old_end {
      ((prior_start as i64 + delta) as u64, rows)
    } else {
      (prior_start, rows)
    };
    let &(_, truth_start, truth_rows) =
      truth_map.files().iter().find(|&&(k, _, _)| k == key).unwrap();
    assert_eq!(
      (want_start, want_rows),
      (truth_start, truth_rows),
      "shift law must reproduce the scratch file table (key {key:x})"
    );
  }

  // Outcome equality per file, in the successor space (== the scratch space by the pin).
  let truth_kg = vorpal_kg::Kg::load(&gen_truth).expect("truth kg");
  let containment = |etype: vorpal_kg::EdgeType| {
    matches!(
      etype.base(),
      vorpal_kg::EdgeType::DEFINES | vorpal_kg::EdgeType::HAS_METHOD | vorpal_kg::EdgeType::HAS_FIELD
    )
  };
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
  for (outcome, key) in outcomes.iter().zip([b_key, key_of("a.py")]) {
    let &(_, start, rows) = truth_map.files().iter().find(|&&(k, _, _)| k == key).unwrap();
    let range = start..start + u64::from(rows);
    let mut truth_evidence: Vec<_> = range
      .clone()
      .flat_map(|id| truth_kg.evidence_from(NodeId::new(id)))
      .map(|row| row_key(&row))
      .collect();
    let mut got_evidence: Vec<_> = outcome.evidence.iter().map(row_key).collect();
    truth_evidence.sort_unstable();
    got_evidence.sort_unstable();
    assert_eq!(got_evidence, truth_evidence, "evidence must match scratch (key {key:x})");
    for src_id in range {
      let truth_seq: Vec<(u32, u16)> = truth_kg
        .out_neighbors(NodeId::new(src_id))
        .into_iter()
        .filter(|(_, etype)| !containment(*etype))
        .map(|(dst, etype)| (dst.raw() as u32, etype.0))
        .collect();
      let mut got_seq: Vec<(u32, u16)> = outcome
        .edges
        .iter()
        .filter(|(from, _, _)| u64::from(*from) == src_id)
        .map(|(_, to, etype)| (*to, etype.0))
        .collect();
      got_seq.extend(
        outcome
          .request_edges
          .iter()
          .filter(|(from, _, _)| u64::from(*from) == src_id)
          .map(|(_, to, etype)| (*to, etype.0)),
      );
      assert_eq!(got_seq, truth_seq, "edge sequence must match scratch (src {src_id})");
    }
  }

  // Non-vacuity: a.py's gamma_new rows flipped from no-edge to Edge across the change.
  let gamma_hash = name_hash("gamma_new");
  assert!(
    outcomes[1].evidence.iter().any(|row| {
      row.name_hash == gamma_hash && row.outcome == vorpal_kg::EvidenceOutcome::Edge
    }),
    "the added definition must resolve a previously dangling reference"
  );

  let _ = fs::remove_dir_all(&base);
}
