//! Chunk-scoped parsing oracle: for every fixture and pattern, `code_search` with the chunk
//! plan returns exactly what the whole-file parse returns, and the plan actually fired.
//! The fixtures hold the shapes that break naive definition-scoped chunking — C `#if`
//! blocks straddling definitions, file-scope macro calls between functions, a TS IIFE,
//! `export`/`export default`, a `describe` block, `module.exports`, a file-scope `if`
//! wrapping a function, Python decorators and a file-scope `if` wrapping a def, Rust
//! `mod`/`impl`/`macro_rules!` items, and the ASI trap: a semicolon-less `let seed = fetchJson`
//! followed by a skipped statement and then an IIFE, which concatenated ranges would read as
//! `fetchJson(function…)()` — one parse per chunk keeps it a bare identifier.
use std::fs;
use std::path::Path;

use vorpal_core::matcher::PatternSpec;
use vorpal_index::records::{CodeSearchReport, code_search};

fn rows(report: &CodeSearchReport) -> Vec<(String, String, u32, u32)> {
  let mut rows: Vec<(String, String, u32, u32)> = report
    .records
    .iter()
    .map(|r| (r.node.path.clone(), r.node.name.clone(), r.matches, r.first_line))
    .collect();
  rows.sort();
  rows
}

#[test]
fn chunk_scoped_code_search_equals_the_whole_file_parse() {
  // SAFETY: single-test binary; the tier must not build itself in the background here.
  unsafe { std::env::set_var("VORPAL_NO_AUTOWARM", "1") };
  let base = std::env::temp_dir().join(format!("vorpal-chunks-{}", std::process::id()));
  let _ = fs::remove_dir_all(&base);
  let src = base.join("src");
  fs::create_dir_all(&src).unwrap();
  let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/chunks");
  for name in ["straddle.c", "wrapped.ts", "decorated.py", "items.rs"] {
    fs::copy(fixtures.join(name), src.join(name)).unwrap();
  }
  let out = base.join("idx");
  vorpal_index::build_index(&src, &out).expect("scratch build");
  let generation = vorpal_kg::resolve_index_dir(&out);
  // The text tier prunes nothing here (every file holds every literal); the heal makes the
  // candidate path the one the daemon runs.
  vorpal_index::trigrams::heal(&generation).unwrap();
  let kg = vorpal_kg::Kg::load(&generation).unwrap();

  let cases: &[(&str, &str, u64)] = &[
    // (pattern, lang, matches from the whole-file parse — counted by hand against the
    // fixtures; `fetchJson` sits in every top-level statement of wrapped.ts, so that case
    // proves the plan declines to chunk when nothing would be skipped)
    ("kmalloc($A, $B)", "c", 3),
    ("kmalloc($$$)", "c", 3),
    ("return $X;", "c", 8),
    ("EXPORT_SYMBOL($A)", "c", 1),
    // tree-sitter recovers inside these calls (`struct probe` as an argument, `kfree(p, )`):
    // ast-grep skips the ERROR child and counts the MISSING one, so the reference path must
    // decline and the parse must answer.
    ("container_of($A, $B, $C)", "c", 1),
    ("container_of($A, $B, $C, $D)", "c", 0),
    ("kfree($A)", "c", 2),
    ("kfree($A, $B)", "c", 0),
    // a macro call followed by a parenthesized statement parses as a call of a call; ast-grep
    // matches the inner call, the reference walk records only the outer → parse path
    ("list_for_each_entry($A, $B, $C)", "c", 1),
    ("probe_init($A, $B)", "c", 2),
    ("fetchJson($A)", "typescript", 8),
    ("export function $F($$$) { $$$ }", "typescript", 1),
    ("function $F() { $$$ }", "typescript", 4),
    ("describe($A, $B)", "typescript", 1),
    ("os.stat($A)", "python", 6),
    ("def $F($$$): $$$", "python", 6),
    ("fs::metadata($A)", "rust", 3),
    ("pub fn $F($$$) -> $R { $$$ }", "rust", 4),
  ];
  let mut chunked_total = 0u64;
  let mut memo_hits = 0u64;
  let mut callsite_total = 0u64;
  let mut moved: Vec<String> = Vec::new();
  for &(pattern, lang, expected) in cases {
    let spec = PatternSpec::plain(pattern);
    let with = code_search(&kg, Some(&generation), &spec, Some(lang), None, 100).unwrap();
    // The veto is read once per process through a OnceLock, so the exhaustive arm runs in a
    // child process below; here we only compare against it.
    chunked_total += with.chunk_parsed_files;
    callsite_total += with.callsite_files;
    if with.callsite_files > 0 {
      assert_eq!(with.chunk_parsed_files, 0, "{pattern}: a call shape never parses");
    }
    // A repeat answers every chunk from the memo and must be identical.
    let again = code_search(&kg, Some(&generation), &spec, Some(lang), None, 100).unwrap();
    assert_eq!(rows(&again), rows(&with), "{pattern}: memo replay diverged");
    assert_eq!(again.total_matches, with.total_matches);
    if with.chunk_parsed_files > 0 && with.callsite_files == 0 {
      assert!(again.chunk_memo_hits > 0, "{pattern}: the memo never hit on a repeat: {again:?}");
      memo_hits += again.chunk_memo_hits;
    }
    let (plain, plain_total) = exhaustive(&generation, pattern, lang);
    eprintln!("{pattern:40} {lang:10} whole={plain_total} chunked={} chunk_parsed={} callsite={}", with.total_matches, with.chunk_parsed_files, with.callsite_files);
    assert_eq!(rows(&with), plain, "{pattern}: chunk-scoped parse diverged from the whole-file parse");
    assert_eq!(with.total_matches, plain_total, "{pattern}: match counts diverged");
    if plain_total != expected {
      moved.push(format!("{pattern} ({lang}): whole-file parse found {plain_total}, pinned {expected}"));
    }
  }
  assert!(moved.is_empty(), "pinned counts moved:\n{}", moved.join("\n"));
  assert!(chunked_total > 0, "the chunk plan never fired");
  assert!(memo_hits > 0, "the chunk memo never fired");
  assert!(callsite_total > 0, "the call-site path never fired");
  let _ = fs::remove_dir_all(&base);
}

/// The whole-file arm, run in a child process so `VORPAL_NO_CHUNK_PARSE` is read fresh.
fn exhaustive(generation: &Path, pattern: &str, lang: &str) -> (Vec<(String, String, u32, u32)>, u64) {
  if std::env::var_os("VORPAL_CHUNKS_CHILD").is_some() {
    unreachable!();
  }
  let exe = std::env::current_exe().unwrap();
  let out = std::process::Command::new(exe)
    .args(["--exact", "child_exhaustive", "--nocapture", "--include-ignored"])
    .env("VORPAL_NO_CHUNK_PARSE", "1")
    .env("VORPAL_NO_CALLSITE_PATH", "1")
    .env("VORPAL_NO_AUTOWARM", "1")
    .env("VORPAL_CHUNKS_CHILD", "1")
    .env("VORPAL_CHUNKS_GEN", generation)
    .env("VORPAL_CHUNKS_PATTERN", pattern)
    .env("VORPAL_CHUNKS_LANG", lang)
    .output()
    .unwrap();
  assert!(out.status.success(), "child: {}", String::from_utf8_lossy(&out.stderr));
  let text = String::from_utf8(out.stdout).unwrap();
  let total: u64 = text
    .lines()
    .find_map(|l| l.strip_prefix("TOTAL\t"))
    .and_then(|t| t.parse().ok())
    .expect("child printed its total");
  let mut rows: Vec<(String, String, u32, u32)> = text
    .lines()
    .filter_map(|l| l.strip_prefix("ROW\t"))
    .map(|l| {
      let mut f = l.split('\t');
      (
        f.next().unwrap().to_string(),
        f.next().unwrap().to_string(),
        f.next().unwrap().parse().unwrap(),
        f.next().unwrap().parse().unwrap(),
      )
    })
    .collect();
  rows.sort();
  (rows, total)
}

#[test]
#[ignore]
fn child_exhaustive() {
  if std::env::var_os("VORPAL_CHUNKS_CHILD").is_none() {
    return;
  }
  let generation = std::path::PathBuf::from(std::env::var_os("VORPAL_CHUNKS_GEN").unwrap());
  let pattern = std::env::var("VORPAL_CHUNKS_PATTERN").unwrap();
  let lang = std::env::var("VORPAL_CHUNKS_LANG").unwrap();
  let kg = vorpal_kg::Kg::load(&generation).unwrap();
  let report = code_search(&kg, Some(&generation), &PatternSpec::plain(&pattern), Some(&lang), None, 100).unwrap();
  assert_eq!(report.chunk_parsed_files, 0, "the veto must hold in the child");
  assert_eq!(report.callsite_files, 0, "the call-site veto must hold in the child");
  println!("TOTAL\t{}", report.total_matches);
  for (path, name, matches, line) in rows(&report) {
    println!("ROW\t{path}\t{name}\t{matches}\t{line}");
  }
}
