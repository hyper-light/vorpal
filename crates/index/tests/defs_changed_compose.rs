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
