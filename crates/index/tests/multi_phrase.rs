//! Multi-phrase semantic AND (semantic-tier plan, Stage AND): `"…" AND "…"` runs every
//! phrase through the full three-channel pass, intersects the fused pools left-to-right,
//! and scores survivors by the MINIMUM of their per-phrase RRF scores. Anything that does
//! not parse as the conjunction syntax flows through the single-phrase path with the
//! ORIGINAL query bytes — byte-identity for ordinary queries is structural, pinned here by
//! the parser table and the no-`phrase`-key serialization check.

use std::fs;
use std::path::Path;
use std::time::Instant;

use vorpal_index::{
  SearchFilter, build_index, parse_and_phrases, search_records_filtered, search_report_filtered,
};

#[test]
fn parser_claims_exactly_the_conjunction_syntax() {
  assert_eq!(
    parse_and_phrases("\"retry logic\" AND \"connection pool\""),
    Some(vec!["retry logic".to_string(), "connection pool".to_string()])
  );
  assert_eq!(
    parse_and_phrases("  \"a b\"   AND   \"c\"   AND \"d e f\"  "),
    Some(vec!["a b".to_string(), "c".to_string(), "d e f".to_string()])
  );
  for query in [
    "foo AND bar",              // unquoted terms
    "\"foo bar\"",              // a single quoted phrase is an ordinary query
    "\"a\" and \"b\"",          // lowercase and
    "AND",                      // bare separator
    "\"a\" AND",                // dangling separator
    "\"a\"AND \"b\"",           // no whitespace before AND
    "\"a\" AND\"b\"",           // no whitespace after AND
    "\"\" AND \"b\"",           // empty phrase
    "\" \" AND \"b\"",          // whitespace-only phrase
    "\"a\" AND \"b\" trailing", // stray text after the last phrase
    "pre \"a\" AND \"b\"",      // stray text before the first phrase
    "\"a\" OR \"b\"",           // wrong operator
    "\"a\" AND \"b",            // unterminated quote
    "",                         // empty query
  ] {
    assert_eq!(
      parse_and_phrases(query),
      None,
      "{query:?} must take the single-phrase path"
    );
  }
}

fn build_corpus(base: &Path, files: &[(String, String)]) -> std::path::PathBuf {
  let src = base.join("src");
  let out = base.join("index");
  let _ = fs::remove_dir_all(base);
  fs::create_dir_all(&src).unwrap();
  for (name, content) in files {
    fs::write(src.join(name), content).unwrap();
  }
  build_index(&src, &out).unwrap();
  out
}

#[test]
fn conjunction_intersects_scores_by_min_and_reports_the_eliminator() {
  unsafe { std::env::set_var("VORPAL_NO_AUTOWARM", "1") };

  // ---- Small corpus: the conjunction-positive golden (fixed dir — absolute paths feed
  // File-row embeddings; see retrieval_eval.rs). ----
  let base = std::env::temp_dir().join("vorpal-multiphrase");
  let out = build_corpus(
    &base,
    &[
      (
        "pool.rs".to_string(),
        "pub fn pooled_retry_acquire(pool: u32, backoff_ms: u64) -> u32 { pool + backoff_ms as u32 }\n\
         pub fn connection_pool_size() -> u32 { 8 }\n\
         pub fn retry_delay_ms(attempt: u32) -> u64 { (attempt as u64) * 100 }\n"
          .to_string(),
      ),
      (
        "proto.rs".to_string(),
        "pub fn parse_frame_header_checksum(buf: &[u8]) -> u32 { buf.iter().map(|b| *b as u32).sum() }\n\
         pub fn parse_frame_header(buf: &[u8]) -> u8 { buf[0] }\n\
         pub fn checksum_bytes(buf: &[u8]) -> u32 { buf.len() as u32 }\n"
          .to_string(),
      ),
      (
        "noise.rs".to_string(),
        "pub fn resolve_nothing() -> u32 { 1 }\npub fn token_bucket() -> u32 { 4 }\n".to_string(),
      ),
    ],
  );

  let query = "\"retry backoff\" AND \"connection pool\"";
  let single_started = Instant::now();
  let blended =
    search_records_filtered(&out, "retry backoff connection pool", 10, &SearchFilter::default())
      .unwrap();
  let single_us = single_started.elapsed().as_micros();
  let multi_started = Instant::now();
  let report = search_report_filtered(&out, query, 10, &SearchFilter::default()).unwrap();
  let multi_us = multi_started.elapsed().as_micros();
  println!("latency: single-blend {single_us} µs, 2-phrase conjunction {multi_us} µs");

  let mp = report.multi_phrase.as_ref().expect("conjunction must parse");
  assert_eq!(mp.phrases, vec!["retry backoff", "connection pool"]);
  assert_eq!(mp.eliminated_by, None, "{mp:?}");
  assert_eq!(mp.per_phrase_pool.len(), 2);
  assert!(!report.hits.is_empty());
  // The definition genuinely about BOTH concepts wins; the blended single string is the
  // baseline the conjunction must not lose to.
  assert_eq!(
    report.hits[0].node.name,
    "pooled_retry_acquire",
    "top hits: {:?}",
    report.hits.iter().map(|h| &h.node.name).collect::<Vec<_>>()
  );
  // The conjunction must never rank the both-concepts definition WORSE than the blended
  // single string does (rank 0 here vs the blended baseline's rank).
  let blended_rank = blended
    .iter()
    .position(|h| h.node.name == "pooled_retry_acquire")
    .unwrap_or(usize::MAX);
  assert!(blended_rank >= 1, "blended baseline unexpectedly perfect — re-pin this check");

  // Containment + the exact min-score property: every hit appears in EVERY phrase's own
  // fused pool at the intersection depth, and its fused score equals the minimum of its
  // per-phrase RRF scores (the single-phrase path at pool depth IS the per-phrase pass).
  // The shallow rung (single-phrase rerank pool) already covers this tiny corpus, so the
  // rung list collapses to the exhaustive one: depth = node count, computed, not tuned.
  let small_n = vorpal_kg::Kg::load(&out).unwrap().node_count();
  assert!(small_n < 50, "corpus grew past the shallow rung — revisit these expectations");
  assert_eq!(mp.intersection_depth, small_n, "single collapsed rung: {mp:?}");
  let pool_depth = mp.intersection_depth;
  let phrase_pools: Vec<std::collections::HashMap<String, f32>> = mp
    .phrases
    .iter()
    .map(|phrase| {
      search_records_filtered(&out, phrase, pool_depth, &SearchFilter::default())
        .unwrap()
        .into_iter()
        .map(|h| (h.node.name, h.score))
        .collect()
    })
    .collect();
  for hit in &report.hits {
    let mut min = f32::INFINITY;
    for (index, pool) in phrase_pools.iter().enumerate() {
      let score = pool
        .get(&hit.node.name)
        .unwrap_or_else(|| panic!("hit {} missing from phrase {index}'s pool", hit.node.name));
      min = min.min(*score);
    }
    assert!(
      (hit.score - min).abs() <= f32::EPSILON,
      "min-score property violated for {}: fused {} vs per-phrase min {}",
      hit.node.name,
      hit.score,
      min
    );
    // Provenance: every channel rank is phrase-tagged and every phrase contributed.
    assert!(hit.channels.iter().all(|c| c.phrase.is_some()), "{:?}", hit.channels);
    for phrase in 0..mp.phrases.len() {
      assert!(
        hit.channels.iter().any(|c| c.phrase == Some(phrase)),
        "hit {} lacks phrase {phrase} provenance: {:?}",
        hit.node.name,
        hit.channels
      );
    }
  }

  // Determinism: the same conjunction twice is bit-identical (names and score bits).
  let again = search_report_filtered(&out, query, 10, &SearchFilter::default()).unwrap();
  let fingerprint = |hits: &[vorpal_index::records::SearchHitRecord]| {
    hits
      .iter()
      .map(|h| (h.node.name.clone(), h.score.to_bits()))
      .collect::<Vec<_>>()
  };
  assert_eq!(fingerprint(&report.hits), fingerprint(&again.hits));

  // Single-phrase surfaces stay byte-identical: no `phrase` key ever serializes.
  for query in [
    "retry backoff connection pool",
    "retry AND pool",
    "\"retry backoff\"",
    "\"a\" and \"b\"",
  ] {
    let hits = search_records_filtered(&out, query, 10, &SearchFilter::default()).unwrap();
    let json = serde_json::to_string(&hits).unwrap();
    assert!(
      !json.contains("\"phrase\""),
      "{query:?} leaked phrase provenance: {json}"
    );
  }

  // A nonsense phrase shares a token with nothing, so its lexical support is empty at
  // every depth and the conjunction reports it as the eliminator — never a page of
  // no-signal rows. (Support is exact token overlap over the embedded surface; vector
  // sign cannot serve as the criterion, since hash collisions hand unrelated rows
  // positive dot products.)
  let garbage = "\"retry backoff\" AND \"zzqxv nonexistent\"";
  let report = search_report_filtered(&out, garbage, 10, &SearchFilter::default()).unwrap();
  let mp = report.multi_phrase.as_ref().expect("conjunction must parse");
  assert!(
    report.hits.is_empty(),
    "{:?}",
    report.hits.iter().map(|h| &h.node.name).collect::<Vec<_>>()
  );
  assert_eq!(mp.eliminated_by, Some(1), "{mp:?}");

  let _ = fs::remove_dir_all(&base);

  // ---- Big corpus: enough definitions (488) that depth-50 fused pools truncate, so two
  // exclusive 60-member phrase families are disjoint at the first depth — the conjunction
  // must DEEPEN rather than falsely eliminate. ----
  let big_base = std::env::temp_dir().join("vorpal-multiphrase-big");
  let families = [
    "alphaqq", "zetaww", "gammaee", "deltarr", "epsilontt", "lambdauu", "sigmaoo", "omegapp",
  ];
  let files: Vec<(String, String)> = families
    .iter()
    .enumerate()
    .map(|(index, family)| {
      let mut content = String::new();
      for i in 0..60 {
        content.push_str(&format!("pub fn {family}_{i}() -> u32 {{ {i} }}\n"));
      }
      (format!("f{index}.rs"), content)
    })
    .collect();
  let big_out = build_corpus(&big_base, &files);

  // Two 60-member families with no shared vocabulary: each phrase's lexical support is
  // exactly its own family (token overlap over the embedded surface — hash-collision
  // "positives" cannot leak in), the intersection is empty at both rungs, and the
  // eliminator reports it at the exhaustive depth.
  let big_n = vorpal_kg::Kg::load(&big_out).unwrap().node_count();
  let disjoint = "\"alphaqq\" AND \"zetaww\"";
  let report = search_report_filtered(&big_out, disjoint, 10, &SearchFilter::default()).unwrap();
  let mp = report.multi_phrase.as_ref().expect("conjunction must parse");
  assert!(
    report.hits.is_empty(),
    "{:?}",
    report.hits.iter().map(|h| &h.node.name).collect::<Vec<_>>()
  );
  assert_eq!(mp.eliminated_by, Some(1), "{mp:?}");
  assert_eq!(mp.intersection_depth, big_n, "decided at the exhaustive rung: {mp:?}");
  // Each family's lexical support is exactly its own 60 definitions.
  assert_eq!(mp.per_phrase_pool, vec![60, 60], "{mp:?}");
  // The rendered surface states the eliminator instead of going silent.
  let rendered = vorpal_index::search_index(&big_out, disjoint, 10).unwrap();
  assert!(
    rendered.contains("(no results: phrase 2/2 \"zetaww\" eliminated all candidates"),
    "rendered: {rendered:?}"
  );

  // The eliminator: a filter that admits nothing empties every phrase pool at BOTH
  // rungs, reported with phrase, pools, and the exhaustive depth — never silence.
  // (Genuinely disjoint phrases at kernel scale hit this same path — recorded in
  // docs/wip/BENCHMARKS.md.)
  let nothing = SearchFilter {
    path_suffix: Some("nope.rs".to_string()),
    ..SearchFilter::default()
  };
  let report = search_report_filtered(&big_out, disjoint, 10, &nothing).unwrap();
  let mp = report.multi_phrase.as_ref().expect("conjunction must parse");
  assert!(report.hits.is_empty());
  assert_eq!(mp.eliminated_by, Some(0), "{mp:?}");
  assert_eq!(mp.per_phrase_pool, vec![0, 0], "{mp:?}");
  let rendered = vorpal_index::search_index_filtered(&big_out, disjoint, 10, &nothing).unwrap();
  let expected = format!(
    "(no results: phrase 1/2 \"alphaqq\" eliminated all candidates; \
     per-phrase pools: 0, 0 at depth {big_n})"
  );
  assert!(rendered.contains(&expected), "rendered: {rendered:?}");

  // And a satisfiable conjunction on the same big corpus still answers.
  let same = "\"alphaqq\" AND \"alphaqq\"";
  let report = search_report_filtered(&big_out, same, 5, &SearchFilter::default()).unwrap();
  assert_eq!(report.multi_phrase.as_ref().and_then(|mp| mp.eliminated_by), None);
  assert_eq!(report.hits.len(), 5);
  assert!(report.hits.iter().all(|h| h.node.name.starts_with("alphaqq_")));

  let _ = fs::remove_dir_all(&big_base);
}
