//! The DEFS-STABLE compose's convergence gates (P4.5c-2, slice iii): a composed
//! generation must equal a full from-scratch build of the same tree BYTE FOR BYTE —
//! under the Merkle id (the generation name) AND the full-rehash fold — across the edit
//! shapes the surgery claims: reference rewiring, dataflow movement, a CALLS cycle
//! forming across files (the scc_size ripple patches OTHER buckets' node slabs), a
//! near-clone pair appearing (the similar segments of OTHER buckets' edge slabs), and
//! the honest decline of a definition-adding edit.

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

/// a.py's two states. Defs-stable: same four functions, same signatures, same import.
/// The edit rewires alpha's call args, points ghost at beta — forming the CALLS cycle
/// beta → helper_from_a → ghost → beta, whose scc_size ripple reaches b.py's bucket —
/// and reworks churn's signed body (sketch movement without pair changes).
fn a_py(edited: bool) -> String {
  let alpha_body = if edited {
    "    v = 7\n    return beta(v, k=v)\n"
  } else {
    "    w = 1\n    return beta(w)\n"
  };
  let ghost_body = if edited {
    "    g = 2\n    return beta(g)\n"
  } else {
    "    return 0\n"
  };
  let churn_body = if edited {
    "    total = 3\n    for item in items:\n        if item > ceiling:\n            total += ceiling\n        elif item < floor:\n            total += floor\n        else:\n            total -= item\n    return total - len(items)\n"
  } else {
    "    total = 0\n    for item in items:\n        if item < floor:\n            total += floor\n        elif item > ceiling:\n            total += ceiling\n        else:\n            total += item\n    return total + len(items)\n"
  };
  format!(
    "from b import beta\n\ndef alpha():\n{alpha_body}\ndef helper_from_a():\n    return ghost()\n\ndef ghost():\n{ghost_body}\ndef use_chain():\n    return maker_local().render()\n\nclass Widget:\n    def render(self):\n        return 1\n\ndef maker_local() -> Widget:\n    return Widget()\n\ndef churn(items, floor, ceiling):\n{churn_body}"
  )
}

fn write_fixture(src: &Path) {
  fs::write(src.join("a.py"), a_py(false)).unwrap();
  // b.beta calls back into a: the return edge of the cycle the edit completes.
  fs::write(
    src.join("b.py"),
    "from a import helper_from_a\n\ndef beta(x, k=None):\n    return helper_from_a() if x else x\n",
  )
  .unwrap();
  fs::write(
    src.join("lib.rs"),
    "pub fn helper(value: i32) -> i32 {\n    value + 1\n}\n\npub fn entry(seed: i32) -> i32 {\n    helper(seed)\n}\n",
  )
  .unwrap();
}

#[test]
fn defs_stable_compose_converges_with_an_scc_ripple() {
  unsafe { std::env::set_var("VORPAL_FORMAT", "next") };
  let base = std::env::temp_dir().join(format!("vorpal-ds-compose-{}", std::process::id()));
  let _ = fs::remove_dir_all(&base);
  let src = base.join("repo");
  fs::create_dir_all(&src).unwrap();
  write_fixture(&src);
  let src = src.canonicalize().unwrap();

  let out = base.join("index");
  vorpal_index::build_index(&src, &out).expect("initial build");
  let gen_prior = live(&out);
  #[cfg(unix)]
  let names_ino = {
    use std::os::unix::fs::MetadataExt;
    fs::metadata(gen_prior.join("names.idx")).unwrap().ino()
  };

  fs::write(src.join("a.py"), a_py(true)).unwrap();
  let report = vorpal_index::build_index(&src, &out).expect("incremental build");
  assert!(
    report
      .cochange_note
      .as_deref()
      .is_some_and(|note| note.contains("defs-stable compose")),
    "the edit must take the defs-stable compose, got: {report:?}"
  );
  assert!(!report.reused && !report.graph_reused);
  assert_eq!(report.indexed, 1, "exactly the edited file re-resolves");
  let gen_new = live(&out);
  assert_ne!(gen_prior.file_name(), gen_new.file_name());
  #[cfg(unix)]
  {
    use std::os::unix::fs::MetadataExt;
    assert_eq!(
      names_ino,
      fs::metadata(gen_new.join("names.idx")).unwrap().ino(),
      "names are defs-stable — names.idx must hard-link (the compose's physical signature)"
    );
    // Edges changed, so the prior cache must not CARRY — the compose builds the
    // successor graph from the prior CSR + the delta and writes a FRESH cache (a
    // compose chain must not pay a slab-decode rebuild per build).
    assert!(gen_new.join("graph.bin").exists(), "the compose writes the successor cache");
    assert_ne!(
      fs::metadata(gen_prior.join("graph.bin")).unwrap().ino(),
      fs::metadata(gen_new.join("graph.bin")).unwrap().ino(),
      "the successor cache is fresh, never a link of the prior's"
    );
  }
  // The scc ripple is REAL in this fixture: the cycle spans a.py and b.py, so b.py's
  // bucket node slab must have been rewritten (different inode), not linked.
  #[cfg(unix)]
  {
    use std::os::unix::fs::MetadataExt;
    let tree_root = src.to_string_lossy();
    let b_key = vorpal_kg::identity::FileKey::of(vorpal_kg::identity::tree_relative(
      &src.join("b.py").to_string_lossy(),
      &tree_root,
    ))
    .0;
    let map = vorpal_kg::NodeIdMap::from_dir(&gen_new).unwrap();
    let &(_, b_start, _) = map.files().iter().find(|&&(key, _, _)| key == b_key).unwrap();
    let bases = map.bases();
    let b_bucket = bases.partition_point(|&b| b <= b_start) - 1;
    let name = format!("{b_bucket:04}.vseg");
    assert_ne!(
      fs::metadata(gen_prior.join("nodes").join(&name)).unwrap().ino(),
      fs::metadata(gen_new.join("nodes").join(&name)).unwrap().ino(),
      "the cycle's scc ripple must rewrite b.py's node slab"
    );
  }
  assert_converged(&out, &src, &base, "scc-ripple");

  // Graph answers over the composed generation match a fresh load (the lazy cache lane).
  let kg = vorpal_kg::Kg::load(&live(&out)).expect("composed generation loads");
  assert!(kg.node_count() > 0);

  let _ = fs::remove_dir_all(&base);
}

#[test]
fn a_multi_file_defs_stable_session_composes_and_converges() {
  // S2: THREE files body-edited in one build — two Python (the cross-file cycle forms
  // exactly as in the single-file gate) plus the Rust neighbor — must ride ONE
  // defs-stable session: one shared table, every edited run swapped in the pairing
  // repair, every family spliced per source, and the result byte-equal to scratch.
  unsafe { std::env::set_var("VORPAL_FORMAT", "next") };
  let base = std::env::temp_dir().join(format!("vorpal-ds-multi-{}", std::process::id()));
  let _ = fs::remove_dir_all(&base);
  let src = base.join("repo");
  fs::create_dir_all(&src).unwrap();
  write_fixture(&src);
  let src = src.canonicalize().unwrap();

  let out = base.join("index");
  vorpal_index::build_index(&src, &out).expect("initial build");
  let gen_prior = live(&out);
  #[cfg(unix)]
  let names_ino = {
    use std::os::unix::fs::MetadataExt;
    fs::metadata(gen_prior.join("names.idx")).unwrap().ino()
  };

  fs::write(src.join("a.py"), a_py(true)).unwrap();
  fs::write(
    src.join("b.py"),
    "from a import helper_from_a\n\ndef beta(x, k=None):\n    return helper_from_a() if k else x\n",
  )
  .unwrap();
  fs::write(
    src.join("lib.rs"),
    "pub fn helper(value: i32) -> i32 {\n    value * 3 - 1\n}\n\npub fn entry(seed: i32) -> i32 {\n    helper(seed)\n}\n",
  )
  .unwrap();
  let report = vorpal_index::build_index(&src, &out).expect("incremental build");
  assert!(
    report
      .cochange_note
      .as_deref()
      .is_some_and(|note| note.contains("defs-stable compose")),
    "the three-file body edit must take ONE defs-stable session, got: {report:?}"
  );
  assert_eq!(report.indexed, 3, "exactly the edited files re-resolve");
  let gen_new = live(&out);
  #[cfg(unix)]
  {
    use std::os::unix::fs::MetadataExt;
    assert_eq!(
      names_ino,
      fs::metadata(gen_new.join("names.idx")).unwrap().ino(),
      "names are defs-stable across the whole session — names.idx must hard-link"
    );
  }
  assert_converged(&out, &src, &base, "multi-file");

  let _ = fs::remove_dir_all(&base);
}

#[test]
fn defs_stable_compose_converges_when_a_pair_appears() {
  unsafe { std::env::set_var("VORPAL_FORMAT", "next") };
  let base = std::env::temp_dir().join(format!("vorpal-ds-pairs-{}", std::process::id()));
  let _ = fs::remove_dir_all(&base);
  let src = base.join("repo");
  fs::create_dir_all(&src).unwrap();
  let clone_body = "    total = 0\n    for item in items:\n        if item < floor:\n            total += floor\n        elif item > ceiling:\n            total += ceiling\n        else:\n            total += item\n    return total + len(items)\n";
  fs::write(
    src.join("clones.py"),
    format!(
      "def sim_a(items, floor, ceiling):\n{clone_body}\ndef sim_b(items, floor, ceiling):\n{body2}",
      body2 = clone_body.replace("total += item", "total += item + 1"),
    ),
  )
  .unwrap();
  let distinct_body = "    acc = 1469598103934665603\n    for byte in items:\n        acc = acc ^ byte\n        acc = (acc * 1099511628211) % 18446744073709551616\n        acc = acc + (acc >> 7)\n    return acc - len(items)\n";
  let target =
    |body: &str| format!("def target_fn(items, floor, ceiling):\n{body}");
  fs::write(src.join("target.py"), target(distinct_body)).unwrap();
  let src = src.canonicalize().unwrap();

  let out = base.join("index");
  vorpal_index::build_index(&src, &out).expect("initial build");
  fs::write(src.join("target.py"), target(clone_body)).unwrap();
  let report = vorpal_index::build_index(&src, &out).expect("incremental build");
  assert!(
    report
      .cochange_note
      .as_deref()
      .is_some_and(|note| note.contains("defs-stable compose")),
    "the near-clone edit must take the defs-stable compose, got: {report:?}"
  );
  assert_converged(&out, &src, &base, "pair-appears");
  // Non-vacuity: the composed generation really holds the new pair.
  let kg = vorpal_kg::Kg::load(&live(&out)).unwrap();
  let pairs = vorpal_ingest::similar_pairs_of_kg(&kg);
  assert!(!pairs.is_empty(), "the clone edit must produce SIMILAR pairs");

  let _ = fs::remove_dir_all(&base);
}

#[test]
fn a_definition_adding_edit_declines_to_the_full_pipeline() {
  unsafe { std::env::set_var("VORPAL_FORMAT", "next") };
  let base = std::env::temp_dir().join(format!("vorpal-ds-decline-{}", std::process::id()));
  let _ = fs::remove_dir_all(&base);
  let src = base.join("repo");
  fs::create_dir_all(&src).unwrap();
  fs::write(src.join("a.py"), "def alpha():\n    return 1\n").unwrap();
  let src = src.canonicalize().unwrap();

  let out = base.join("index");
  vorpal_index::build_index(&src, &out).expect("initial build");
  fs::write(
    src.join("a.py"),
    "def alpha():\n    return 1\n\ndef fresh_def():\n    return alpha()\n",
  )
  .unwrap();
  let report = vorpal_index::build_index(&src, &out).expect("incremental build");
  assert!(
    report.cochange_note.as_deref().is_none_or(|note| !note.contains("defs-stable compose")),
    "a def-adding edit must NOT take the compose"
  );
  assert_converged(&out, &src, &base, "decline");

  let _ = fs::remove_dir_all(&base);
}
