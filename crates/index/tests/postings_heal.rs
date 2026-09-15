//! The lexical posting tier heals on the served generation. A commit carries the ANN tier
//! forward but not the postings (they name the node ids of one node segment), and a daemon
//! whose live tier is healthy never runs the full warm that would rebuild them, so a name
//! query after an incremental commit tokenized every node until `heal_postings` existed.
//! The heal builds the tier once, hands it to the open searcher without a reopen, and the
//! ranking is identical with and without it (the scan is the correctness anchor).

use std::fs;

use vorpal_index::{SearchFilter, build_index, heal_postings, search_records_filtered};

#[test]
fn heal_builds_the_missing_tier_once_and_the_ranking_does_not_move() {
  let base = std::env::temp_dir().join(format!("vorpal-postings-heal-{}", std::process::id()));
  let src = base.join("src");
  let out = base.join("index");
  let _ = fs::remove_dir_all(&base);
  fs::create_dir_all(&src).unwrap();
  fs::write(
    src.join("a.rs"),
    "pub fn resolve_import_path(a: u32) -> u32 { a }\npub fn resolve_index_dir(a: u32) -> u32 { a }\npub fn import_path_of(a: u32) -> u32 { resolve_import_path(a) }\n",
  )
  .unwrap();
  fs::write(
    src.join("b.rs"),
    "pub fn tool_result(a: u32) -> u32 { a }\npub fn other_thing(a: u32) -> u32 { tool_result(a) }\n",
  )
  .unwrap();
  build_index(&src, &out).unwrap();
  let generation = vorpal_kg::resolve_index_dir(&out);
  // The state a carried generation is in: no lexical tier on disk.
  let _ = fs::remove_file(generation.join("postings.bin"));
  let names = |query: &str| {
    search_records_filtered(&out, query, 5, &SearchFilter::default())
      .unwrap()
      .into_iter()
      .map(|hit| hit.node.name)
      .collect::<Vec<_>>()
  };
  let before = (names("resolve import path"), names("tool_result"));
  assert_eq!(before.0[0], "resolve_import_path", "{before:?}");
  assert_eq!(before.1[0], "tool_result", "{before:?}");

  assert!(heal_postings(&out).unwrap(), "the first heal builds the tier");
  assert!(generation.join("postings.bin").exists());
  assert!(!heal_postings(&out).unwrap(), "the second heal finds it in place");

  let after = (names("resolve import path"), names("tool_result"));
  assert_eq!(before, after, "the posting path and the scan rank alike");
  let _ = fs::remove_dir_all(&base);
}
