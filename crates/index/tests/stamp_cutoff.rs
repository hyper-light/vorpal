//! The stamp-only commit cutoff (SUBSECOND.md Phase 1a): a stat change whose re-extraction
//! is byte-identical outside the stamp window commits a carried-forward generation that is
//! **content-id-identical to a from-scratch build of the same tree** — and anything else
//! falls through to the full pipeline.

use std::fs;
use std::path::Path;

fn gen_id(out: &Path) -> String {
  fs::read_to_string(out.join("CURRENT")).expect("CURRENT exists")
}

fn build(src: &Path, out: &Path) -> vorpal_index::IndexReport {
  vorpal_index::build_index(src, out).expect("build succeeds")
}

#[test]
fn restamp_class_commits_the_from_scratch_generation() {
  let root = std::env::temp_dir().join(format!("vorpal-cutoff-{}", std::process::id()));
  let _ = fs::remove_dir_all(&root);
  let src = root.join("src");
  fs::create_dir_all(&src).unwrap();
  fs::write(src.join("a.c"), "int helper(int x) { return x + 1; }\nint main(void) { return helper(41); }\n").unwrap();
  fs::write(src.join("b.c"), "extern int helper(int);\nint twice(int v) { return helper(helper(v)); }\n").unwrap();

  let out = root.join("idx");
  build(&src, &out);

  // Comment append: content and size change, extraction does not.
  let mut a = fs::read_to_string(src.join("a.c")).unwrap();
  a.push_str("// a trailing comment\n");
  fs::write(src.join("a.c"), a).unwrap();

  let report = build(&src, &out);
  assert!(report.graph_reused, "restamp class must reuse the graph");
  assert!(!report.reused, "files re-extracted and a new generation committed — not 'unchanged'");
  assert_eq!(report.indexed, 1, "exactly the edited file re-extracts");
  let cutoff_gen = gen_id(&out);

  // The oracle: a from-scratch build of the same tree lands on the same content id.
  let scratch = root.join("scratch");
  build(&src, &scratch);
  assert_eq!(
    cutoff_gen,
    gen_id(&scratch),
    "cutoff generation must be byte-identical to from-scratch"
  );

  // A semantic edit must fall through to the full pipeline and still converge.
  let mut b = fs::read_to_string(src.join("b.c")).unwrap();
  b.push_str("int third(int v) { return helper(v) * 3; }\n");
  fs::write(src.join("b.c"), b).unwrap();
  let report = build(&src, &out);
  assert!(!report.reused, "a semantic edit must rebuild");
  let full_gen = gen_id(&out);
  let scratch2 = root.join("scratch2");
  build(&src, &scratch2);
  assert_eq!(full_gen, gen_id(&scratch2));

  // An added file disqualifies the cutoff even when existing files only restamp.
  let mut a = fs::read_to_string(src.join("a.c")).unwrap();
  a.push_str("// another comment\n");
  fs::write(src.join("a.c"), a).unwrap();
  fs::write(src.join("c.c"), "int lonely(void) { return 7; }\n").unwrap();
  let report = build(&src, &out);
  assert!(!report.reused, "an added file must rebuild");
  let scratch3 = root.join("scratch3");
  build(&src, &scratch3);
  assert_eq!(gen_id(&out), gen_id(&scratch3));

  let _ = fs::remove_dir_all(&root);
}
