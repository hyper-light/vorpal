//! The labelled retrieval-quality suite (IMPROVEMENTS #9): repository queries with
//! hand-labelled relevant definitions, measured as recall@k and MRR — with per-channel
//! ablations (lexical / vector / graph / fusion) derived from the ranking provenance every
//! hit carries — plus printed latency, artifact-size, and update-cost figures.
//!
//! Like the resolution harness, the fixture is the contract, not a field sample: the corpus
//! is designed so every labelled target is retrievable, so **recall@5 = 1.0 and MRR ≥ 0.7
//! are gates**, not aspirations. A ranking change that drops a labelled target below k
//! fails here and must justify itself. Latency/size numbers are printed for the record,
//! never asserted (machine-dependent).

use std::fs;
use std::time::Instant;

use vorpal_index::{SearchFilter, build_index, search_records_filtered};

/// One labelled query: the text a developer would type, and every definition name that
/// counts as relevant (any of them at rank r scores 1/r for MRR; all must appear in top-5
/// for recall).
const QUERIES: &[(&str, &[&str])] = &[
  // Exact-name lookups.
  ("resolve_import_path", &["resolve_import_path"]),
  ("WidgetRegistry", &["WidgetRegistry"]),
  // Token permutations and case variants of real names.
  ("import path resolve", &["resolve_import_path"]),
  ("registry widget", &["WidgetRegistry"]),
  ("parse config", &["parse_config"]),
  // Subset queries: fewer tokens than the target name.
  ("checkpoint", &["write_checkpoint_file", "load_checkpoint"]),
  ("tokenizer", &["StreamingTokenizer"]),
  // Descriptive, cross-file intent.
  ("flush dirty pages", &["flush_dirty_pages"]),
  ("retry backoff", &["retry_with_backoff"]),
  // Disambiguation: two same-token candidates; the heavily-called one should rank first,
  // but both are relevant.
  ("frobnicate", &["frobnicate_core", "frobnicate_shim"]),
];

fn corpus() -> Vec<(&'static str, String)> {
  vec![
    (
      "resolver.rs",
      "pub fn resolve_import_path(p: &str) -> String { p.to_string() }\n\
       pub fn resolve_symbol(p: &str) -> String { p.to_string() }\n"
        .to_string(),
    ),
    (
      "widgets.rs",
      "pub struct WidgetRegistry;\npub struct WidgetHandle;\n\
       pub fn register_widget() -> u32 { 1 }\n"
        .to_string(),
    ),
    (
      "config.py",
      "def parse_config(path):\n    return path\n\ndef merge_config(a, b):\n    return a\n"
        .to_string(),
    ),
    (
      "checkpoint.rs",
      "pub fn write_checkpoint_file() -> u32 { 1 }\npub fn load_checkpoint() -> u32 { 2 }\n"
        .to_string(),
    ),
    (
      "tokenizer.ts",
      "export class StreamingTokenizer {}\nexport function tokenLength(): number { return 1 }\n"
        .to_string(),
    ),
    (
      "pages.rs",
      "pub fn flush_dirty_pages() -> u32 { 1 }\npub fn mark_page_dirty() -> u32 { 2 }\n"
        .to_string(),
    ),
    (
      "retry.py",
      "def retry_with_backoff(fn):\n    return fn\n\ndef give_up(fn):\n    return fn\n"
        .to_string(),
    ),
    // frobnicate_core is called from several places; frobnicate_shim from none — the graph
    // channel's in-degree disambiguation should put core first.
    (
      "frob.rs",
      "pub fn frobnicate_core() -> u32 { 1 }\npub fn frobnicate_shim() -> u32 { frobnicate_core() }\n"
        .to_string(),
    ),
    (
      "frob_users.rs",
      "pub fn user_a() -> u32 { frobnicate_core() }\npub fn user_b() -> u32 { frobnicate_core() }\n"
        .to_string(),
    ),
    // Distractors: plausible names that must not displace labelled targets.
    (
      "noise.rs",
      "pub fn resolve_nothing() -> u32 { 1 }\npub fn config_shape() -> u32 { 2 }\n\
       pub fn page_count() -> u32 { 3 }\npub fn token_bucket() -> u32 { 4 }\n"
        .to_string(),
    ),
  ]
}

/// Score one ranking (names in rank order) against the labels: (all-relevant-in-top-k,
/// reciprocal rank of the best-placed relevant name).
fn score(ranking: &[String], relevant: &[&str], k: usize) -> (bool, f64) {
  let top_k = &ranking[..ranking.len().min(k)];
  let recall = relevant.iter().all(|r| top_k.iter().any(|n| n == r));
  let rr = ranking
    .iter()
    .position(|n| relevant.contains(&n.as_str()))
    .map(|rank| 1.0 / (rank + 1) as f64)
    .unwrap_or(0.0);
  (recall, rr)
}

#[test]
fn labelled_queries_meet_recall_and_mrr_gates_with_ablations() {
  unsafe { std::env::set_var("VORPAL_NO_AUTOWARM", "1") };
  let base = std::env::temp_dir().join(format!("vorpal-retrieval-{}", std::process::id()));
  let src = base.join("src");
  let out = base.join("index");
  let _ = fs::remove_dir_all(&base);
  fs::create_dir_all(&src).unwrap();
  for (name, content) in corpus() {
    fs::write(src.join(name), content).unwrap();
  }

  // Index size + build/warm cost, for the record.
  let build_started = Instant::now();
  build_index(&src, &out).unwrap();
  let build_ms = build_started.elapsed().as_millis();
  let warm_started = Instant::now();
  vorpal_index::warm_ann(&out).unwrap();
  let warm_ms = warm_started.elapsed().as_millis();
  let gen_dir = vorpal_kg::resolve_index_dir(&out);
  let size_of = |name: &str| fs::metadata(gen_dir.join(name)).map(|m| m.len()).unwrap_or(0);

  // Per-channel ablations come from the provenance ranks each hit carries: ordering the
  // fetched pool by one channel's rank IS that channel's ranking over the fixture (the
  // corpus is small enough that k=25 covers every candidate any channel ranked).
  let channel_ranking = |hits: &[vorpal_index::records::SearchHitRecord], channel: &str| {
    let mut ranked: Vec<(usize, String)> = hits
      .iter()
      .filter_map(|hit| {
        hit
          .channels
          .iter()
          .find(|c| c.channel == channel)
          .map(|c| (c.rank, hit.node.name.clone()))
      })
      .collect();
    ranked.sort_by_key(|&(rank, _)| rank);
    ranked.into_iter().map(|(_, name)| name).collect::<Vec<_>>()
  };

  let mut per_channel: std::collections::BTreeMap<&str, (usize, f64)> =
    [("fusion", (0, 0.0)), ("name", (0, 0.0)), ("vector", (0, 0.0)), ("graph", (0, 0.0))]
      .into_iter()
      .collect();
  let mut total_query_us = 0u128;
  let mut failures = Vec::new();

  for (query, relevant) in QUERIES {
    let started = Instant::now();
    let hits = search_records_filtered(&out, query, 25, &SearchFilter::default()).unwrap();
    total_query_us += started.elapsed().as_micros();
    let fused: Vec<String> = hits.iter().map(|h| h.node.name.clone()).collect();

    let (recall, rr) = score(&fused, relevant, 5);
    let entry = per_channel.get_mut("fusion").unwrap();
    entry.0 += recall as usize;
    entry.1 += rr;
    if !recall {
      failures.push(format!("query {query:?}: labelled {relevant:?} not all in top-5 of {fused:?}"));
    }

    for channel in ["name", "vector", "graph"] {
      let ranking = channel_ranking(&hits, channel);
      let (recall, rr) = score(&ranking, relevant, 5);
      let entry = per_channel.get_mut(channel).unwrap();
      entry.0 += recall as usize;
      entry.1 += rr;
    }
  }

  let n = QUERIES.len() as f64;
  println!("== retrieval eval ({} labelled queries) ==", QUERIES.len());
  for (channel, (recalls, rr_sum)) in &per_channel {
    println!(
      "{channel:>7}: recall@5 {:.3}  MRR {:.3}",
      *recalls as f64 / n,
      rr_sum / n
    );
  }
  println!(
    "build {build_ms} ms; warm {warm_ms} ms; mean query {} µs; ann.bin {} B; postings.bin {} B",
    total_query_us / QUERIES.len() as u128,
    size_of("ann.bin"),
    size_of("postings.bin"),
  );

  // Update cost: touch one file, re-index (product replay path), for the record.
  fs::write(
    src.join("noise.rs"),
    "pub fn resolve_nothing() -> u32 { 9 }\npub fn config_shape() -> u32 { 2 }\n\
     pub fn page_count() -> u32 { 3 }\npub fn token_bucket() -> u32 { 4 }\n",
  )
  .unwrap();
  let update_started = Instant::now();
  build_index(&src, &out).unwrap();
  println!("one-file update: {} ms", update_started.elapsed().as_millis());

  // The gates: the fixture is designed fully retrievable — fused recall@5 is 1.0, MRR ≥ 0.7.
  let (fusion_recalls, fusion_rr) = per_channel["fusion"];
  assert!(
    failures.is_empty(),
    "fused recall@5 must be 1.0 on the labelled fixture:\n{}",
    failures.join("\n")
  );
  assert_eq!(fusion_recalls, QUERIES.len());
  let fusion_mrr = fusion_rr / n;
  assert!(fusion_mrr >= 0.7, "fused MRR {fusion_mrr:.3} fell below 0.7");

  // Fusion must not be worse than any single channel on these labels — the point of RRF.
  for channel in ["name", "vector", "graph"] {
    let (_, rr_sum) = per_channel[channel];
    assert!(
      fusion_rr >= rr_sum - 1e-9,
      "fusion MRR {:.3} lost to {channel} {:.3}",
      fusion_rr / n,
      rr_sum / n
    );
  }

  // The in-degree disambiguation case: the heavily-called frobnicate_core ranks above the
  // uncalled shim in the fused order.
  let hits = search_records_filtered(&out, "frobnicate", 10, &SearchFilter::default()).unwrap();
  let names: Vec<&str> = hits.iter().map(|h| h.node.name.as_str()).collect();
  let core = names.iter().position(|n| *n == "frobnicate_core");
  let shim = names.iter().position(|n| *n == "frobnicate_shim");
  assert!(
    core.is_some() && shim.is_some() && core < shim,
    "graph channel should put the heavily-called definition first: {names:?}"
  );

  let _ = fs::remove_dir_all(&base);
}
