//! The text tier end to end: a from-scratch build carries no family (cold rows untouched);
//! a heal fills every bucket from source; `code_search` with the tier prunes files and
//! returns exactly what the exhaustive scan returns; the family never enters generation
//! identity; an edit rebuilds one bucket through the compose lanes (or rides the commit
//! carry and is healed) and stays exact; a changed file's bucket reads as uncovered, never
//! as wrong.
use std::fs;
use std::path::{Path, PathBuf};

use vorpal_index::records::code_search;
use vorpal_index::trigrams::{TextIndex, heal};

fn live(root: &Path) -> PathBuf {
  vorpal_kg::resolve_index_dir(root)
}

fn write_fixture(src: &Path) {
  fs::create_dir_all(src.join("core")).unwrap();
  fs::create_dir_all(src.join("util")).unwrap();
  for i in 0..24 {
    fs::write(
      src.join("core").join(format!("mod_{i:02}.rs")),
      format!(
        "pub fn helper_{i}(value: i32) -> i32 {{\n    value + {i}\n}}\n\npub fn \
         entry_{i}(seed: i32) -> i32 {{\n    helper_{i}(seed)\n}}\n"
      ),
    )
    .unwrap();
  }
  for i in 0..8 {
    fs::write(
      src.join("util").join(format!("tool_{i}.rs")),
      format!(
        "{}pub fn tool_{i}() -> &'static str {{\n    \"quasar_{i}\"\n}}\n",
        if i == 0 { "// mirrors helper_3 in core\n" } else { "" }
      ),
    )
    .unwrap();
  }
}

/// The paths of every node named `name` plus the paths of the nodes that reference or call it
/// — what the graph's answer covers.
fn kg_paths_of(kg: &vorpal_kg::Kg, name: &str) -> Vec<String> {
  let mut out = Vec::new();
  for id in 0..kg.node_count() as u64 {
    let node = vorpal_kg::NodeId::new(id);
    let Some(view) = kg.node(node) else { continue };
    if view.name == name {
      out.push(view.path.to_string());
      for (caller, _) in kg.incoming_with_confidence(node, vorpal_kg::EdgeType::CALLS) {
        if let Some(c) = kg.node(caller) {
          out.push(c.path.to_string());
        }
      }
    }
  }
  out.sort();
  out.dedup();
  out
}

fn open_index(generation: &Path, src: &Path) -> Option<TextIndex> {
  let root = src.canonicalize().unwrap().to_string_lossy().into_owned();
  let pack = vorpal_ingest::PackReader::open_rooted(generation, Some(&root))?;
  TextIndex::open(generation, &pack)
}

fn matched(report: &vorpal_index::records::CodeSearchReport) -> Vec<(String, String, u32)> {
  let mut rows: Vec<(String, String, u32)> = report
    .records
    .iter()
    .map(|r| (r.node.path.clone(), r.node.name.clone(), r.matches))
    .collect();
  rows.sort();
  rows
}

fn without_tier<T>(f: impl FnOnce() -> T) -> T {
  // SAFETY: this test binary holds one #[test]; nothing else reads the variable concurrently.
  unsafe { std::env::set_var("VORPAL_NO_TEXT_INDEX", "1") };
  let out = f();
  unsafe { std::env::remove_var("VORPAL_NO_TEXT_INDEX") };
  out
}

#[test]
fn text_tier_heals_prunes_and_stays_exact() {
  // SAFETY: single-test binary; the tier must not build itself in the background here.
  unsafe { std::env::set_var("VORPAL_NO_AUTOWARM", "1") };
  let base = std::env::temp_dir().join(format!("vorpal-text-tier-{}", std::process::id()));
  let _ = fs::remove_dir_all(&base);
  let src = base.join("src");
  write_fixture(&src);
  let out = base.join("out");
  vorpal_index::build_index(&src, &out).expect("scratch build");
  let generation = live(&out);

  // 1. A from-scratch build carries no family.
  assert!(!generation.join(vorpal_kg::TRIGRAMS_TOC).exists(), "cold build must not write the text tier");
  let identity_before = vorpal_index::generation_content_id_full(&generation).unwrap();

  // 2. The heal fills every bucket from source, and identity does not move.
  let report = heal(&generation).unwrap().expect("bucketed generation heals");
  assert_eq!(report.rebuilt_buckets, report.total_buckets);
  assert_eq!(report.files, 32);
  assert_eq!(report.unindexed, 0);
  assert!(report.postings > 0);
  assert_eq!(vorpal_index::generation_content_id_full(&generation).unwrap(), identity_before);
  let index = open_index(&generation, &src).expect("family opens");
  assert!(index.is_fresh(), "{}", index.status());

  // 3. code_search with the tier prunes and agrees with the exhaustive scan.
  let kg = vorpal_kg::Kg::load(&generation).unwrap();
  let with = code_search(&kg, Some(&generation), &vorpal_core::matcher::PatternSpec::plain("helper_3($A)"), Some("rust"), None, 50).unwrap();
  assert_eq!(with.text_index, "fresh");
  assert!(with.pruned_files > 0, "the tier must prune files lacking `helper_3`");
  assert!(with.total_matches >= 1);
  let plain = without_tier(|| code_search(&kg, Some(&generation), &vorpal_core::matcher::PatternSpec::plain("helper_3($A)"), Some("rust"), None, 50).unwrap());
  assert_eq!(plain.pruned_files, 0);
  assert_eq!(matched(&with), matched(&plain));
  assert_eq!(with.total_matches, plain.total_matches);
  // A pattern with no three-byte literal prunes nothing and stays exact.
  let bare = code_search(&kg, Some(&generation), &vorpal_core::matcher::PatternSpec::plain("$A"), Some("rust"), None, 5).unwrap();
  assert_eq!(bare.pruned_files, 0);
  let heal_again = heal(&generation).unwrap().unwrap();
  assert_eq!(heal_again.rebuilt_buckets, 0, "a fresh family heals nothing");
  // 3b. An alternation prunes through the union of its branches and stays exact: the
  //     files holding `helper_3` or `quasar_2`, and nothing else, are scanned.
  let alt = vorpal_index::textsearch::TextQuery {
    pattern: "helper_3|quasar_2",
    case_insensitive: false,
    lang: None,
    prefix: None,
    max_results: 100,
    symbol: None,
  };
  let with_alt = vorpal_index::textsearch::text_search(&kg, Some(&generation), &alt).unwrap();
  assert_eq!(with_alt.index, "trigram", "{:?}", with_alt.index_reason);
  assert_eq!(with_alt.scanned_files, 3, "exactly mod_03.rs, tool_0.rs (the comment), and tool_2.rs hold a branch literal");
  assert_eq!(with_alt.pruned_files, 29, "32 files, 3 scanned");
  let plain_alt = without_tier(|| vorpal_index::textsearch::text_search(&kg, Some(&generation), &alt).unwrap());
  assert_eq!(plain_alt.index, "full-scan");
  let lines = |r: &vorpal_index::textsearch::TextSearchReport| -> Vec<(String, u32, String)> {
    r.records.iter().map(|m| (m.path.clone(), m.line, m.text.clone())).collect()
  };
  assert_eq!(lines(&with_alt), lines(&plain_alt));
  assert_eq!(with_alt.total_matches, plain_alt.total_matches);
  assert!(with_alt.total_matches >= 3, "helper_3 twice in mod_03.rs, quasar_2 once");
  // 3c. Symbol scope: only entry_3's span is scanned — one line, attributed to entry_3.
  let scoped = vorpal_index::textsearch::text_search(
    &kg,
    Some(&generation),
    &vorpal_index::textsearch::TextQuery {
      pattern: "helper_3",
      case_insensitive: false,
      lang: None,
      prefix: None,
      max_results: 100,
      symbol: Some("entry_3"),
    },
  )
  .unwrap();
  assert_eq!(scoped.index, "symbol-scoped");
  assert_eq!(scoped.candidate_files, 1);
  assert_eq!(scoped.total_matches, 1, "{:?}", scoped.records);
  assert_eq!(scoped.records[0].symbol.as_deref(), Some("entry_3"));
  assert!(scoped.records[0].path.ends_with("mod_03.rs"));
  // 3d. Absence proof: the graph attributes helper_3 to mod_03.rs (definition + caller); the
  //     comment in tool_0.rs is the one textual mention it cannot see.
  let attributed_paths = kg_paths_of(&kg, "helper_3");
  let attributed: std::collections::HashSet<&str> = attributed_paths.iter().map(String::as_str).collect();
  let mentions = vorpal_index::textsearch::unattributed_mentions(&kg, Some(&generation), "helper_3", &attributed, 100).unwrap();
  assert_eq!(mentions.unattributed_files, 1, "{mentions:?}");
  assert!(mentions.records[0].path.ends_with("tool_0.rs"));
  assert_eq!(mentions.records[0].line, 1);
  assert!(mentions.complete);
  let tool_5_paths = kg_paths_of(&kg, "tool_5");
  let tool_5: std::collections::HashSet<&str> = tool_5_paths.iter().map(String::as_str).collect();
  let none = vorpal_index::textsearch::unattributed_mentions(&kg, Some(&generation), "quasar_5", &tool_5, 100).unwrap();
  assert_eq!(none.unattributed_files, 0, "quasar_5 is spelled only inside tool_5: {none:?}");
  assert!(none.complete);
  // 3e. Body channel: `quasar_2` names no definition (it is a string literal inside tool_2),
  //     so the name and BM25 lists are empty and the body list nominates tool_2.
  let report = vorpal_index::search_report_filtered(&out, "quasar_2", 10, &vorpal_index::SearchFilter::default()).unwrap();
  let tool_2 = report.hits.iter().find(|h| h.node.name == "tool_2").expect("body channel nominates tool_2");
  assert!(tool_2.channels.iter().any(|c| c.channel == "body"), "{:?}", tool_2.channels);
  let named = vorpal_index::search_report_filtered(&out, "helper_3", 10, &vorpal_index::SearchFilter::default()).unwrap();
  assert!(named.hits.iter().all(|h| h.channels.iter().all(|c| c.channel != "body")), "name evidence keeps the body list out");

  // 4. Edit one file: the compose lanes (or the commit carry) keep the family; the changed
  //    file's bucket is exact after the build, or uncovered and healed — never wrong.
  let target = src.join("core").join("mod_05.rs");
  let mut text = fs::read_to_string(&target).unwrap();
  text.push_str("\npub fn zephyr_probe() -> i32 {\n    helper_5(7)\n}\n");
  fs::write(&target, text).unwrap();
  vorpal_index::build_index(&src, &out).expect("incremental build");
  let generation2 = live(&out);
  assert_ne!(generation2, generation);
  let index2 = open_index(&generation2, &src).expect("family carried into the new generation");
  let (live_buckets, total) = index2.coverage();
  assert!(live_buckets + 1 >= total, "at most the edited file's bucket may be uncovered: {}", index2.status());
  let kg2 = vorpal_kg::Kg::load(&generation2).unwrap();
  let with2 = code_search(&kg2, Some(&generation2), &vorpal_core::matcher::PatternSpec::plain("zephyr_probe()"), Some("rust"), None, 50).unwrap();
  let plain2 = without_tier(|| code_search(&kg2, Some(&generation2), &vorpal_core::matcher::PatternSpec::plain("zephyr_probe()"), Some("rust"), None, 50).unwrap());
  assert_eq!(matched(&with2), matched(&plain2));
  assert_eq!(with2.total_matches, plain2.total_matches);
  heal(&generation2).unwrap().unwrap();
  let index2 = open_index(&generation2, &src).unwrap();
  assert!(index2.is_fresh(), "{}", index2.status());
  let with3 = code_search(&kg2, Some(&generation2), &vorpal_core::matcher::PatternSpec::plain("helper_5($A)"), Some("rust"), None, 50).unwrap();
  let plain3 = without_tier(|| code_search(&kg2, Some(&generation2), &vorpal_core::matcher::PatternSpec::plain("helper_5($A)"), Some("rust"), None, 50).unwrap());
  assert_eq!(matched(&with3), matched(&plain3));
  assert!(with3.pruned_files > 0);

  // 5. Bytes that changed after the generation uncover their bucket (a candidate, never a
  //    pruned file), and the family's slabs are hard-links where nothing changed.
  fs::write(&target, "pub fn rewritten() -> i32 { 1 }\n").unwrap();
  let root = src.canonicalize().unwrap().to_string_lossy().into_owned();
  let pack = vorpal_ingest::PackReader::open_rooted(&generation2, Some(&root)).unwrap();
  let stale = TextIndex::open(&generation2, &pack).unwrap();
  assert!(stale.is_fresh(), "the pack still describes the generation's bytes");
  let set = stale.candidates_for_literals(&["zephyr_probe"]).unwrap();
  let key = vorpal_kg::identity::FileKey::of("core/mod_05.rs").0;
  assert_eq!(stale.verdict(&set, key), vorpal_kg::trigramstore::Verdict::Candidate);
  #[cfg(unix)]
  {
    use std::os::unix::fs::MetadataExt;
    let edited_bucket = key & u64::from(total - 1);
    let mut linked = 0;
    for k in 0..total {
      let name = format!("{k:04}.tri");
      let (a, b) = (
        generation.join(vorpal_kg::TRIGRAMS_DIR).join(&name),
        generation2.join(vorpal_kg::TRIGRAMS_DIR).join(&name),
      );
      if a.exists() && b.exists() && fs::metadata(&a).unwrap().ino() == fs::metadata(&b).unwrap().ino() {
        linked += 1;
      } else {
        assert_eq!(u64::from(k), edited_bucket, "only the edited file's bucket may be rewritten");
      }
    }
    assert!(linked >= total - 1);
  }
  let _ = fs::remove_dir_all(&base);
}
