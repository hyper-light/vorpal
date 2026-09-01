//! The DEFS-CHANGED compose's convergence gates (P4.5c-3, slice c3-ii): a composed
//! generation must equal a full from-scratch build BYTE FOR BYTE when the definition set
//! moves — a def ADDED above survivors (every later ordinal shifts; the usage-dirty
//! referrer re-resolves; a previously dangling reference binds), a def REMOVED (the
//! referrer's edge decays back to a no-edge row), and the honest decline lanes.

use std::fs;
use std::path::{Path, PathBuf};

fn live(root: &Path) -> PathBuf {
  vorpal_kg::resolve_index_dir(root)
}

fn content_id(generation: &Path) -> String {
  generation.file_name().unwrap().to_string_lossy().into_owned()
}

fn assert_converged(out_live: &Path, src: &Path, base: &Path, tag: &str) {
  let scratch_out = base.join(format!("scratch-{tag}"));
  vorpal_index::build_index(src, &scratch_out).expect("scratch build");
  let (live_gen, scratch_gen) = (live(out_live), live(&scratch_out));
  assert_eq!(
    content_id(&live_gen),
    content_id(&scratch_gen),
    "{tag}: composed generation must equal the scratch build (Merkle id)"
  );
  assert_eq!(
    vorpal_index::generation_content_id_full(&live_gen).unwrap(),
    vorpal_index::generation_content_id_full(&scratch_gen).unwrap(),
    "{tag}: …and under the full-rehash fold"
  );
}

fn b_py(with_gamma: bool) -> &'static str {
  if with_gamma {
    "def gamma_new():\n    return 1\n\ndef beta(x):\n    return x\n\ndef omega():\n    return 9\n"
  } else {
    "def beta(x):\n    return x\n\ndef omega():\n    return 9\n"
  }
}

const A_PY: &str = "from b import beta, gamma_new\n\ndef alpha(v):\n    return beta(v)\n\ndef alpha_v():\n    return gamma_new()\n";
const C_PY: &str = "def unrelated():\n    return 2\n";

fn write_fixture(src: &Path, with_gamma: bool) {
  fs::write(src.join("a.py"), A_PY).unwrap();
  fs::write(src.join("b.py"), b_py(with_gamma)).unwrap();
  fs::write(src.join("c.py"), C_PY).unwrap();
}

fn compose_fired(report: &vorpal_index::IndexReport) -> bool {
  report
    .cochange_note
    .as_deref()
    .is_some_and(|note| note.contains("defs-changed compose"))
}

#[test]
fn a_definition_added_above_survivors_converges() {
  unsafe { std::env::set_var("VORPAL_FORMAT", "next") };
  let base = std::env::temp_dir().join(format!("vorpal-dc-add-{}", std::process::id()));
  let _ = fs::remove_dir_all(&base);
  let src = base.join("repo");
  fs::create_dir_all(&src).unwrap();
  write_fixture(&src, false);
  let src = src.canonicalize().unwrap();

  let out = base.join("index");
  vorpal_index::build_index(&src, &out).expect("initial build");
  let gen_prior = live(&out);
  fs::write(src.join("b.py"), b_py(true)).unwrap();
  let report = vorpal_index::build_index(&src, &out).expect("incremental build");
  assert!(compose_fired(&report), "the def-adding edit must take the compose: {report:?}");
  assert_eq!(report.indexed, 2, "the edited file plus its one usage-dirty referrer");
  let gen_new = live(&out);
  #[cfg(unix)]
  {
    use std::os::unix::fs::MetadataExt;
    // Names changed: the successor names.idx REGENERATES (translation + fresh block),
    // never links.
    assert_ne!(
      fs::metadata(gen_prior.join("names.idx")).unwrap().ino(),
      fs::metadata(gen_new.join("names.idx")).unwrap().ino(),
      "the successor name index is fresh"
    );
    // The untouched file's bucket carries: c.py references nothing affected.
    let tree_root = src.to_string_lossy();
    let c_key = vorpal_kg::identity::FileKey::of(vorpal_kg::identity::tree_relative(
      &src.join("c.py").to_string_lossy(),
      &tree_root,
    ))
    .0;
    let map = vorpal_kg::NodeIdMap::from_dir(&gen_new).unwrap();
    let &(_, c_start, _) = map.files().iter().find(|&&(key, _, _)| key == c_key).unwrap();
    let bases = map.bases();
    let c_bucket = bases.partition_point(|&b| b <= c_start) - 1;
    let b_key = vorpal_kg::identity::FileKey::of(vorpal_kg::identity::tree_relative(
      &src.join("b.py").to_string_lossy(),
      &tree_root,
    ))
    .0;
    let a_key = vorpal_kg::identity::FileKey::of(vorpal_kg::identity::tree_relative(
      &src.join("a.py").to_string_lossy(),
      &tree_root,
    ))
    .0;
    let bucket_of = |key: u64| {
      let &(_, start, _) = map.files().iter().find(|&&(k, _, _)| k == key).unwrap();
      bases.partition_point(|&b| b <= start) - 1
    };
    if c_bucket != bucket_of(b_key) && c_bucket != bucket_of(a_key) {
      let name = format!("{c_bucket:04}.bin");
      assert_eq!(
        fs::metadata(gen_prior.join("evidence").join(&name)).unwrap().ino(),
        fs::metadata(gen_new.join("evidence").join(&name)).unwrap().ino(),
        "an unaffected file's evidence bucket must hard-link (the shift law)"
      );
    }
  }
  assert_converged(&out, &src, &base, "def-added");

  let _ = fs::remove_dir_all(&base);
}

#[test]
fn a_definition_removed_converges() {
  unsafe { std::env::set_var("VORPAL_FORMAT", "next") };
  let base = std::env::temp_dir().join(format!("vorpal-dc-remove-{}", std::process::id()));
  let _ = fs::remove_dir_all(&base);
  let src = base.join("repo");
  fs::create_dir_all(&src).unwrap();
  write_fixture(&src, true);
  let src = src.canonicalize().unwrap();

  let out = base.join("index");
  vorpal_index::build_index(&src, &out).expect("initial build");
  fs::write(src.join("b.py"), b_py(false)).unwrap();
  let report = vorpal_index::build_index(&src, &out).expect("incremental build");
  assert!(compose_fired(&report), "the def-removing edit must take the compose: {report:?}");
  assert_converged(&out, &src, &base, "def-removed");

  let _ = fs::remove_dir_all(&base);
}

#[test]
fn a_duplicate_collapsed_file_composes_and_converges() {
  // THE ORDER LAW's end-to-end pin: a C file carrying two cfg-arm DEFINITIONS of the
  // same function (same entity_path, same signature — the writer collapses them onto
  // one node id, and BOTH arms are big enough to sign with different sketches) must
  // ride the compose and byte-converge. Pre-law, the fresh sig run for such a file was
  // a layout-ordered multiset while the family held one arrangement-picked survivor
  // per node — equality by coincidence, not construction.
  unsafe { std::env::set_var("VORPAL_FORMAT", "next") };
  let base = std::env::temp_dir().join(format!("vorpal-dc-dup-{}", std::process::id()));
  let _ = fs::remove_dir_all(&base);
  let src = base.join("repo");
  fs::create_dir_all(&src).unwrap();
  let arm = |seed: u32| {
    let mut body = String::new();
    for i in 0..12 {
      body.push_str(&format!("  total = total * {seed} + input[{i}] - {i};\n"));
    }
    body
  };
  let dup_c = |with_probe: bool| {
    let mut file = format!(
      "#ifdef CONFIG_FAST\nstatic long compute_thing(const long *input)\n{{\n  long total = 1;\n{}  return total;\n}}\n#else\nstatic long compute_thing(const long *input)\n{{\n  long total = 2;\n{}  return total;\n}}\n#endif\n\nlong use_thing(const long *input)\n{{\n  return compute_thing(input) + 1;\n}}\n",
      arm(3),
      arm(5)
    );
    if with_probe {
      file.push_str("\nstatic int vorpal_probe_fn(void)\n{\n  return 42;\n}\n");
    }
    file
  };
  fs::write(src.join("dup.c"), dup_c(false)).unwrap();
  fs::write(src.join("neighbor.c"), "long neighbor_fn(void)\n{\n  return 7;\n}\n").unwrap();
  let src = src.canonicalize().unwrap();

  // NON-VACUITY: the premise must hold or this test pins nothing — the two arms'
  // signature records must map through the layout bridge onto ONE node id with
  // DIFFERENT sketches (the duplicate the survivor law exists for).
  {
    let extractor = vorpal_ingest::OutlineExtractor::new().expect("extractor");
    let dup_path = src.join("dup.c").to_string_lossy().into_owned();
    let product = extractor.extract_product(&dup_path, &dup_c(false)).expect("product");
    let mut bytes = Vec::new();
    vorpal_ingest::encode_product_into(&product, &mut bytes);
    let view = vorpal_ingest::decode_product_view(&bytes).unwrap();
    let interner = vorpal_ingest::Interner::default();
    let mut scratch =
      vorpal_ingest::Ingestor::new(&interner, vorpal_ingest::OutlineExtractor::new().unwrap());
    let ords = scratch
      .ingest_product_mapped(&dup_path, vorpal_ingest::decode_product(&bytes).unwrap());
    let signed: Vec<(u64, &[u8])> = view
      .signatures
      .iter()
      .filter_map(|sig| ords.get(sig.entity_index as usize).map(|&ord| (ord, sig.sketch)))
      .collect();
    let mut nodes: Vec<u64> = signed.iter().map(|&(ord, _)| ord).collect();
    nodes.sort_unstable();
    nodes.dedup();
    assert!(
      signed.len() > nodes.len(),
      "premise broken: no signature collapse — {} signed records over {} distinct nodes",
      signed.len(),
      nodes.len()
    );
    let dup_node = signed
      .iter()
      .find(|&&(ord, _)| signed.iter().filter(|&&(o, _)| o == ord).count() > 1)
      .map(|&(ord, _)| ord)
      .unwrap();
    let sketches: Vec<&[u8]> =
      signed.iter().filter(|&&(o, _)| o == dup_node).map(|&(_, s)| s).collect();
    assert!(
      sketches.windows(2).any(|w| w[0] != w[1]),
      "premise broken: the collapsed arms sign identically"
    );
  }

  let out = base.join("index");
  vorpal_index::build_index(&src, &out).expect("initial build");
  // Toggle a tiny (unsigned) definition on and off in the DUP file: both directions
  // must compose and equal the scratch build of the same tree.
  fs::write(src.join("dup.c"), dup_c(true)).unwrap();
  let report = vorpal_index::build_index(&src, &out).expect("append build");
  assert!(compose_fired(&report), "the def-adding edit must take the compose: {report:?}");
  assert_converged(&out, &src, &base, "dup-append");
  fs::write(src.join("dup.c"), dup_c(false)).unwrap();
  let report = vorpal_index::build_index(&src, &out).expect("remove build");
  assert!(compose_fired(&report), "the def-removing edit must take the compose: {report:?}");
  assert_converged(&out, &src, &base, "dup-remove");

  let _ = fs::remove_dir_all(&base);
}

#[test]
fn a_file_addition_declines_to_the_full_pipeline() {
  unsafe { std::env::set_var("VORPAL_FORMAT", "next") };
  let base = std::env::temp_dir().join(format!("vorpal-dc-decline-{}", std::process::id()));
  let _ = fs::remove_dir_all(&base);
  let src = base.join("repo");
  fs::create_dir_all(&src).unwrap();
  write_fixture(&src, false);
  let src = src.canonicalize().unwrap();

  let out = base.join("index");
  vorpal_index::build_index(&src, &out).expect("initial build");
  fs::write(src.join("d.py"), "def newcomer():\n    return 4\n").unwrap();
  let report = vorpal_index::build_index(&src, &out).expect("incremental build");
  assert!(
    !compose_fired(&report),
    "a file ADDITION is outside the class — the full pipeline runs"
  );
  assert_converged(&out, &src, &base, "file-added");

  let _ = fs::remove_dir_all(&base);
}
