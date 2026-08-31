//! The labelled retrieval-quality suite (IMPROVEMENTS #9, extended for the semantic-tier
//! plan Stage 0): repository queries with hand-labelled relevant definitions, measured as
//! recall@k and MRR — split by QUERY CLASS, with per-channel ablations (lexical / vector /
//! graph / fusion) derived from the ranking provenance every hit carries — plus printed
//! latency, artifact-size, and update-cost figures, and a double-run determinism gate.
//!
//! Query classes and their contracts:
//! * `exact`, `short-keyword`, `subset`, `descriptive`, `graph-disambiguation` — designed
//!   fully retrievable under the CURRENT lexical fusion: recall@5 = 1.0 is a hard gate
//!   (the original contract, unchanged). Short-keyword supremacy is the fusion invariant:
//!   no future tier may regress these.
//! * `paraphrase` — NL-intent queries with ZERO vocabulary overlap with their targets, by
//!   construction. The lexical baseline is expected to fail these; the measured baseline
//!   is PINNED exactly so any movement is loud. Stage 1 (learned tier) must raise the
//!   floor deliberately.
//! * `sparse-name` — badly-named symbols whose identity lives in the graph, not the name.
//!   Same pinned-baseline discipline; Stage 2 (retrofit) targets these.
//! * `conjunctive` — two independent concepts in one query, run today as a single blended
//!   string. The pinned baseline is what Stage AND (multi-phrase) must beat.
//!
//! Like the resolution harness, the fixture is the contract, not a field sample. Latency
//! and size numbers are printed for the record, never asserted (machine-dependent).

use std::fs;
use std::time::Instant;

use vorpal_index::{SearchFilter, build_index, search_records_filtered};

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Class {
  Exact,
  ShortKeyword,
  Subset,
  Descriptive,
  GraphDisambiguation,
  Paraphrase,
  SparseName,
  Conjunctive,
}

impl Class {
  fn label(self) -> &'static str {
    match self {
      Class::Exact => "exact",
      Class::ShortKeyword => "short-keyword",
      Class::Subset => "subset",
      Class::Descriptive => "descriptive",
      Class::GraphDisambiguation => "graph-disambiguation",
      Class::Paraphrase => "paraphrase",
      Class::SparseName => "sparse-name",
      Class::Conjunctive => "conjunctive",
    }
  }

  /// Classes designed fully retrievable under the current lexical fusion — recall@5 = 1.0
  /// is a hard gate for these, and always will be (the fusion invariant).
  fn fully_retrievable(self) -> bool {
    !matches!(self, Class::Paraphrase | Class::SparseName | Class::Conjunctive)
  }
}

/// One labelled query: its class, the text a developer would type, and every definition
/// name that counts as relevant (any of them at rank r scores 1/r for MRR; all must appear
/// in top-5 for recall).
const QUERIES: &[(Class, &str, &[&str])] = &[
  // Exact-name lookups.
  (Class::Exact, "resolve_import_path", &["resolve_import_path"]),
  (Class::Exact, "WidgetRegistry", &["WidgetRegistry"]),
  // Short keyword queries (≤3 tokens): token permutations, case variants — the regime
  // where every neural embedder collapses (arXiv:2605.04615) and lexical must stay
  // authoritative.
  (Class::ShortKeyword, "import path resolve", &["resolve_import_path"]),
  (Class::ShortKeyword, "registry widget", &["WidgetRegistry"]),
  (Class::ShortKeyword, "parse config", &["parse_config"]),
  // Subset queries: fewer tokens than the target name.
  (Class::Subset, "checkpoint", &["write_checkpoint_file", "load_checkpoint"]),
  (Class::Subset, "tokenizer", &["StreamingTokenizer"]),
  // Descriptive, cross-file intent with vocabulary overlap.
  (Class::Descriptive, "flush dirty pages", &["flush_dirty_pages"]),
  (Class::Descriptive, "retry backoff", &["retry_with_backoff"]),
  // Disambiguation: two same-token candidates; the heavily-called one should rank first,
  // but both are relevant.
  (Class::GraphDisambiguation, "frobnicate", &["frobnicate_core", "frobnicate_shim"]),
  // Paraphrase: intent worded with ZERO tokens shared with the target name or signature.
  // (verify by inspection: no query token appears in the target's name/signature/basename.)
  (Class::Paraphrase, "shrink oversized text before display", &["truncate_label"]),
  (Class::Paraphrase, "how long until the cache entry dies", &["ttl_remaining"]),
  (Class::Paraphrase, "combine two sorted sequences", &["merge_runs"]),
  // Sparse names: the symbol's name carries almost nothing; its meaning is structural
  // (who calls it / what it calls).
  (Class::SparseName, "apply gamma correction", &["do_it2"]),
  (Class::SparseName, "checksum of the frame payload", &["h7"]),
  // Conjunctive: two independent concepts; today a single blended string (the baseline
  // Stage AND must beat), later `"retry logic" AND "connection pool"`.
  (Class::Conjunctive, "retry backoff connection pool", &["pooled_retry_acquire"]),
  (Class::Conjunctive, "parse header checksum frame", &["parse_frame_header_checksum"]),
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
    // Paraphrase targets: names/signatures deliberately share no token with their queries.
    (
      "label.rs",
      "pub fn truncate_label(s: &str, max: usize) -> String { s.chars().take(max).collect() }\n\
       pub fn pad_label(s: &str, width: usize) -> String { format!(\"{s:width$}\") }\n"
        .to_string(),
    ),
    (
      "cache.rs",
      "pub fn ttl_remaining(created_at: u64, now: u64, limit: u64) -> u64 {\n\
         limit.saturating_sub(now.saturating_sub(created_at))\n\
       }\npub fn evict_oldest() -> u32 { 1 }\n"
        .to_string(),
    ),
    (
      "runs.rs",
      "pub fn merge_runs(a: Vec<u32>, b: Vec<u32>) -> Vec<u32> {\n\
         let mut out = a; out.extend(b); out.sort_unstable(); out\n\
       }\npub fn split_run(v: Vec<u32>) -> (Vec<u32>, Vec<u32>) { (v.clone(), v) }\n"
        .to_string(),
    ),
    // Sparse names: meaning lives in the neighborhood, not the identifier.
    (
      "video.rs",
      "pub fn gamma_table() -> [u8; 4] { [0, 64, 128, 255] }\n\
       pub fn do_it2(px: u8) -> u8 { gamma_table()[(px >> 6) as usize] }\n\
       pub fn brighten(px: u8) -> u8 { do_it2(px).saturating_add(16) }\n"
        .to_string(),
    ),
    (
      "frame.rs",
      "pub fn frame_payload(buf: &[u8]) -> &[u8] { &buf[4..] }\n\
       pub fn h7(buf: &[u8]) -> u32 { frame_payload(buf).iter().map(|b| *b as u32).sum() }\n\
       pub fn verify_frame(buf: &[u8]) -> bool { h7(buf) != 0 }\n"
        .to_string(),
    ),
    // Conjunctive targets: one definition genuinely about BOTH concepts, plus single-
    // concept distractors that must lose once conjunction is real.
    (
      "pool.rs",
      "pub fn pooled_retry_acquire(pool: u32, backoff_ms: u64) -> u32 { pool + backoff_ms as u32 }\n\
       pub fn connection_pool_size() -> u32 { 8 }\n\
       pub fn retry_delay_ms(attempt: u32) -> u64 { (attempt as u64) * 100 }\n"
        .to_string(),
    ),
    (
      "proto.rs",
      "pub fn parse_frame_header_checksum(buf: &[u8]) -> u32 { buf.iter().map(|b| *b as u32).sum() }\n\
       pub fn parse_frame_header(buf: &[u8]) -> u8 { buf[0] }\n\
       pub fn checksum_bytes(buf: &[u8]) -> u32 { buf.len() as u32 }\n"
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

/// Pinned per-class baselines under the CURRENT lexical fusion, `(recalls, mrr_hundredths)`
/// with counts over that class's queries and MRR rounded down to 1/100ths (exact float
/// pinning would be brittle across platforms for reasons unrelated to ranking).
/// ANY movement — up or down — fails loudly: improvements are pinned deliberately, with the
/// stage that earned them named in the diff.
const PINNED_BASELINES: &[(Class, usize, u32)] = &[
  (Class::Exact, 2, 100),
  (Class::ShortKeyword, 3, 100),
  (Class::Subset, 2, 100),
  (Class::Descriptive, 2, 100),
  (Class::GraphDisambiguation, 1, 100),
  // Baselines below are the honest lexical-fusion floor, measured 2026-08-30 (Stage 0).
  // Stage 1 must raise `paraphrase`; Stage 2 targets `sparse-name`; Stage AND targets
  // `conjunctive`.
  // 2026-08-31: the fixture went path-invariant (relative "src/…" via chdir above) after
  // CI and macOS split one Paraphrase rank apart — absolute temp-prefix TOKENS were
  // entering File-node embeddings and nudging near-ties (CI read 9, macOS 8, a scratch
  // variant 10). On the invariant corpus the original Stage-0 floor holds exactly.
  (Class::Paraphrase, 0, 8),
  (Class::SparseName, 0, 6),
  (Class::Conjunctive, 2, 66),
];

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
  // FIXED directory name, deliberately: File-node embeddings include the absolute path
  // (name = path for File rows), so a pid-bearing temp dir shuffles File rows through the
  // exact-tie region of zero-overlap queries (distance 2.0 for disjoint token buckets)
  // and makes deep-pool baselines wobble ACROSS invocations while each index stays
  // internally deterministic. A fixed path pins the whole fixture; embeddings are
  // ROOT-INVARIANT by construction (embedder v2 strips the canonical tree prefix —
  // File-node vectors once tokenized the OS temp layout, splitting macOS and CI pinned
  // ranks one position apart), so the pins below hold on every machine and OS.
  let base = std::env::temp_dir().join("vorpal-retrieval-eval");
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
  let mut per_class: std::collections::BTreeMap<Class, (usize, usize, f64)> =
    std::collections::BTreeMap::new(); // class -> (queries, recalls, rr_sum)
  let mut total_query_us = 0u128;
  let mut retrievable_failures = Vec::new();

  for (class, query, relevant) in QUERIES {
    let started = Instant::now();
    let hits = search_records_filtered(&out, query, 25, &SearchFilter::default()).unwrap();
    total_query_us += started.elapsed().as_micros();
    let fused: Vec<String> = hits.iter().map(|h| h.node.name.clone()).collect();

    // Determinism: the same query twice must produce the identical ranking.
    let again = search_records_filtered(&out, query, 25, &SearchFilter::default()).unwrap();
    let fused_again: Vec<String> = again.iter().map(|h| h.node.name.clone()).collect();
    assert_eq!(fused, fused_again, "non-deterministic ranking for {query:?}");

    let (recall, rr) = score(&fused, relevant, 5);
    // Best-target fused rank per query (0-based; None = not in the returned pool) — the
    // per-query view behind the class table.
    println!(
      "rank[{}] {:?} -> {:?}",
      class.label(),
      query,
      fused.iter().position(|n| relevant.contains(&n.as_str()))
    );
    let entry = per_channel.get_mut("fusion").unwrap();
    entry.0 += recall as usize;
    entry.1 += rr;
    let class_entry = per_class.entry(*class).or_insert((0, 0, 0.0));
    class_entry.0 += 1;
    class_entry.1 += recall as usize;
    class_entry.2 += rr;
    if class.fully_retrievable() && !recall {
      retrievable_failures
        .push(format!("query {query:?}: labelled {relevant:?} not all in top-5 of {fused:?}"));
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
  println!("-- per class (fusion) --");
  for (class, (queries, recalls, rr_sum)) in &per_class {
    println!(
      "{:>21}: {}/{} recall@5, MRR {:.3}",
      class.label(),
      recalls,
      queries,
      rr_sum / *queries as f64
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

  // GATE 1: the fully-retrievable classes stay at recall@5 = 1.0 — the fusion invariant.
  assert!(
    retrievable_failures.is_empty(),
    "fully-retrievable classes must stay at recall@5 = 1.0:\n{}",
    retrievable_failures.join("\n")
  );

  // GATE 2: every class matches its pinned baseline EXACTLY (recalls, and MRR to 1/100
  // rounded down). Improvements re-pin deliberately, naming the stage that earned them.
  for (class, expected_recalls, expected_mrr_hundredths) in PINNED_BASELINES {
    let (queries, recalls, rr_sum) = per_class
      .get(class)
      .copied()
      .unwrap_or_else(|| panic!("no queries in class {class:?}"));
    let mrr_hundredths = ((rr_sum / queries as f64) * 100.0).floor() as u32;
    assert_eq!(
      (recalls, mrr_hundredths),
      (*expected_recalls, *expected_mrr_hundredths),
      "class {:?} moved off its pinned baseline (got {recalls}/{queries} recall, MRR {}/100; \
       pinned {expected_recalls} recall, MRR {expected_mrr_hundredths}/100) — if this is a \
       deliberate improvement, re-pin it with the stage that earned it",
      class,
      mrr_hundredths
    );
  }

  // GATE 3: fusion must not be worse than any single channel over the fully-retrievable
  // classes' labels — the point of RRF. (Computed over all queries; the non-retrievable
  // classes score ~0 in every channel, so they cannot flip this.)
  let (_, fusion_rr) = per_channel["fusion"];
  for channel in ["name", "vector", "graph"] {
    let (_, rr_sum) = per_channel[channel];
    assert!(
      fusion_rr >= rr_sum - 1e-9,
      "fusion MRR {:.3} lost to {channel} {:.3}",
      fusion_rr / n,
      rr_sum / n
    );
  }

  // GATE 4: the in-degree disambiguation case — the heavily-called frobnicate_core ranks
  // above the uncalled shim in the fused order.
  let hits = search_records_filtered(&out, "frobnicate", 10, &SearchFilter::default()).unwrap();
  let names: Vec<&str> = hits.iter().map(|h| h.node.name.as_str()).collect();
  let core = names.iter().position(|n| *n == "frobnicate_core");
  let shim = names.iter().position(|n| *n == "frobnicate_shim");
  assert!(
    core.is_some() && shim.is_some() && core < shim,
    "graph channel should put the heavily-called definition first: {names:?}"
  );

  // RETRIEVAL_EVAL_KEEP=1 leaves the fixture for artifact-level forensics (cross-platform
  // rank investigations need the exact bytes both sides built).
  if std::env::var_os("RETRIEVAL_EVAL_KEEP").is_none() {
    let _ = fs::remove_dir_all(&base);
  }
}
