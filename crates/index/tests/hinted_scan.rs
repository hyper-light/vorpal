//! Hinted manifest patching (SUBSECOND.md 1c): a watched build that patches the prior
//! manifest from a COMPLETE change set must commit the exact generation a full stat sweep
//! commits — for modified files, deleted files, and (via fallback) files the prior manifest
//! never carried.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

fn gen_id(out: &Path) -> String {
  fs::read_to_string(out.join("CURRENT")).expect("CURRENT exists")
}

#[test]
fn hinted_builds_commit_the_full_scan_generation() {
  let root = std::env::temp_dir().join(format!("vorpal-hinted-{}", std::process::id()));
  let _ = fs::remove_dir_all(&root);
  let src = root.join("src");
  fs::create_dir_all(&src).unwrap();
  let a = src.join("a.c");
  let b = src.join("b.c");
  fs::write(&a, "int helper(int x) { return x + 1; }\nint main(void) { return helper(41); }\n").unwrap();
  fs::write(&b, "extern int helper(int);\nint twice(int v) { return helper(helper(v)); }\n").unwrap();

  let out = root.join("idx");
  vorpal_index::build_index(&src, &out).unwrap();

  // Modified file, hinted.
  fs::write(&b, "extern int helper(int);\nint twice(int v) { return helper(v) * 2; }\n").unwrap();
  let hints: HashSet<PathBuf> = [b.clone()].into();
  vorpal_index::build_index_watched(&src, &out, &hints).unwrap();
  let hinted = gen_id(&out);
  let scratch = root.join("scratch1");
  vorpal_index::build_index(&src, &scratch).unwrap();
  assert_eq!(hinted, gen_id(&scratch), "modified-file hint must equal full scan");

  // Deleted file, hinted.
  fs::remove_file(&b).unwrap();
  let hints: HashSet<PathBuf> = [b.clone()].into();
  vorpal_index::build_index_watched(&src, &out, &hints).unwrap();
  let hinted = gen_id(&out);
  let scratch = root.join("scratch2");
  vorpal_index::build_index(&src, &scratch).unwrap();
  assert_eq!(hinted, gen_id(&scratch), "deleted-file hint must equal full scan");

  // New file: the hint cannot prove walker equivalence — the build must fall back to the
  // sweep internally and still converge.
  let c = src.join("c.c");
  fs::write(&c, "int lonely(void) { return 7; }\n").unwrap();
  let hints: HashSet<PathBuf> = [c.clone()].into();
  vorpal_index::build_index_watched(&src, &out, &hints).unwrap();
  let hinted = gen_id(&out);
  let scratch = root.join("scratch3");
  vorpal_index::build_index(&src, &scratch).unwrap();
  assert_eq!(hinted, gen_id(&scratch), "new-file hint must fall back and converge");

  // Irrelevant hint (unhandled extension) alongside a real change.
  let junk = src.join("notes.txt");
  fs::write(&junk, "not code").unwrap();
  fs::write(&a, "int helper(int x) { return x + 2; }\nint main(void) { return helper(40); }\n").unwrap();
  let hints: HashSet<PathBuf> = [a.clone(), junk.clone()].into();
  vorpal_index::build_index_watched(&src, &out, &hints).unwrap();
  let hinted = gen_id(&out);
  let scratch = root.join("scratch4");
  vorpal_index::build_index(&src, &scratch).unwrap();
  assert_eq!(hinted, gen_id(&scratch), "irrelevant hints must not perturb the patch");

  let _ = fs::remove_dir_all(&root);
}
