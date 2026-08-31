//! Co-change edges from git history: files that changed together in recent commits gain
//! symmetric `changes_with` edges between their File nodes; a non-repository yields none,
//! with the reason stated; the pass is deterministic across double builds.

use std::fs;
use std::path::Path;
use std::process::Command;

use vorpal_index::build_index;
use vorpal_kg::{EdgeType, Kg, SymbolKind};

fn git(dir: &Path, args: &[&str]) {
  let status = Command::new("git")
    .arg("-C")
    .arg(dir)
    .args(args)
    .env("GIT_AUTHOR_NAME", "t")
    .env("GIT_AUTHOR_EMAIL", "t@t")
    .env("GIT_COMMITTER_NAME", "t")
    .env("GIT_COMMITTER_EMAIL", "t@t")
    .status()
    .expect("git runs");
  assert!(status.success(), "git {args:?}");
}

fn file_id(kg: &Kg, suffix: &str) -> vorpal_kg::NodeId {
  (0..kg.node_count() as u64)
    .map(vorpal_kg::NodeId::new)
    .find(|&id| {
      kg.node(id)
        .is_some_and(|v| v.kind == SymbolKind::File && v.path.ends_with(suffix))
    })
    .unwrap_or_else(|| panic!("file node {suffix}"))
}

#[test]
fn cochange_edges_follow_git_history() {
  let base = std::env::temp_dir().join(format!("vorpal-cochange-{}", std::process::id()));
  let src = base.join("repo");
  let out = base.join("index");
  let _ = fs::remove_dir_all(&base);
  fs::create_dir_all(&src).unwrap();
  git(&src, &["init", "-q"]);
  let write = |name: &str, body: &str| fs::write(src.join(name), body).unwrap();
  // Commit 1: a + b together.  Commit 2: a + b again.  Commit 3: c alone.  Commit 4: b + c.
  write("a.rs", "pub fn a() {}\n");
  write("b.rs", "pub fn b() {}\n");
  git(&src, &["add", "."]);
  git(&src, &["commit", "-qm", "one"]);
  write("a.rs", "pub fn a() { 1; }\n");
  write("b.rs", "pub fn b() { 1; }\n");
  git(&src, &["add", "."]);
  git(&src, &["commit", "-qm", "two"]);
  write("c.rs", "pub fn c() {}\n");
  git(&src, &["add", "."]);
  git(&src, &["commit", "-qm", "three"]);
  write("b.rs", "pub fn b() { 2; }\n");
  write("c.rs", "pub fn c() { 2; }\n");
  git(&src, &["add", "."]);
  git(&src, &["commit", "-qm", "four"]);
  // Commit 5: a sweep touching 60 files (57 unindexed text files + b, c, a) — judged on its
  // RAW size, so it couples nothing, even though only three indexed files are in it.
  for i in 0..57 {
    write(&format!("note{i}.txt"), &format!("sweep {i}\n"));
  }
  write("a.rs", "pub fn a() { 3; }\n");
  write("b.rs", "pub fn b() { 3; }\n");
  write("c.rs", "pub fn c() { 3; }\n");
  git(&src, &["add", "."]);
  git(&src, &["commit", "-qm", "five: sweep"]);

  let report = build_index(&src, &out).unwrap();
  assert_eq!(report.cochange_edges, 1, "{report:?}");
  assert!(report.cochange_note.is_none(), "{report:?}");
  let kg = Kg::load(&out).unwrap();
  let (a, b, c) = (file_id(&kg, "a.rs"), file_id(&kg, "b.rs"), file_id(&kg, "c.rs"));
  let partners = |id: vorpal_kg::NodeId| -> Vec<(vorpal_kg::NodeId, u8)> {
    kg.out_neighbors(id)
      .into_iter()
      .filter(|(_, e)| e.base() == EdgeType::CHANGES_WITH)
      .map(|(n, e)| (n, e.confidence()))
      .collect()
  };
  // a↔b changed together twice → symmetric edges at confidence 40 (2 × 20).
  assert_eq!(partners(a), vec![(b, 40)]);
  assert_eq!(partners(b), vec![(a, 40)]);
  // b↔c changed together only once → below the two-co-change floor.
  assert!(partners(c).is_empty(), "{:?}", partners(c));

  // Determinism: a second build from the same tree + history seals the same generation.
  let out2 = base.join("index2");
  build_index(&src, &out2).unwrap();
  assert_eq!(
    fs::read_to_string(out.join("CURRENT")).unwrap(),
    fs::read_to_string(out2.join("CURRENT")).unwrap()
  );

  // The HEAD-keyed cache: written by the first build, keyed by HEAD + window, and the
  // cached build produces the identical generation. A new commit re-keys it.
  let cache = fs::read_to_string(out.join("cochange.cache")).expect("cache written");
  let head = String::from_utf8(
    Command::new("git")
      .arg("-C")
      .arg(&src)
      .args(["rev-parse", "HEAD"])
      .output()
      .unwrap()
      .stdout,
  )
  .unwrap();
  assert!(cache.starts_with(&format!("vorpal-cochange/1 {} ", head.trim())), "{cache}");
  assert!(cache.lines().count() >= 2, "raw pairs stored: {cache}");
  // An extraction-visible edit (longer body → item spans shift), so the rebuild runs the
  // full pipeline and the cochange pass hits its HEAD-keyed cache. A same-length touch
  // would take the stamp-only commit cutoff instead — artifacts (cochange edges included)
  // carried forward byte-identically with the pass stated as not re-run on the report.
  write("a.rs", "pub fn a() { 400; }\n");
  let cached = build_index(&src, &out).unwrap();
  assert_eq!(cached.cochange_edges, 1, "{cached:?}");
  // A commit that couples a and c twice more flips c's partner set — the cache is stale
  // by HEAD and refreshes.
  write("a.rs", "pub fn a() { 5; }\n");
  write("c.rs", "pub fn c() { 5; }\n");
  git(&src, &["add", "."]);
  git(&src, &["commit", "-qm", "six"]);
  write("a.rs", "pub fn a() { 6; }\n");
  write("c.rs", "pub fn c() { 6; }\n");
  git(&src, &["add", "."]);
  git(&src, &["commit", "-qm", "seven"]);
  let refreshed = build_index(&src, &out).unwrap();
  assert_eq!(refreshed.cochange_edges, 2, "a↔b and a↔c: {refreshed:?}");
  let cache = fs::read_to_string(out.join("cochange.cache")).unwrap();
  assert!(!cache.starts_with(&format!("vorpal-cochange/1 {} ", head.trim())), "re-keyed");

  // Not a repository: zero edges, reason stated, build succeeds.
  let plain = base.join("plain");
  fs::create_dir_all(&plain).unwrap();
  fs::write(plain.join("x.rs"), "pub fn x() {}\n").unwrap();
  let report = build_index(&plain, &base.join("index-plain")).unwrap();
  assert_eq!(report.cochange_edges, 0);
  assert!(
    report
      .cochange_note
      .as_deref()
      .is_some_and(|n| n.contains("not a git repository")),
    "{report:?}"
  );

  let _ = fs::remove_dir_all(&base);
}
