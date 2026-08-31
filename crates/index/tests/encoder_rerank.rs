//! Stage-6 reranker plumbing oracles. The UNGATED test proves the degradation law
//! with no model at all (a bad `encoder.dir` states itself and searches keep
//! serving); the GATED test (VORPAL_CODERANK_DIR) proves the live reranker is
//! deterministic and leaves record scores/channel ranks untouched.

use std::fs;

use vorpal_index::{SearchFilter, Searcher, build_index};

fn fixture(base_name: &str) -> (std::path::PathBuf, std::path::PathBuf) {
  unsafe { std::env::set_var("VORPAL_NO_AUTOWARM", "1") };
  let base = std::env::temp_dir().join(base_name);
  let src = base.join("src");
  let out = base.join("index");
  let _ = fs::remove_dir_all(&base);
  fs::create_dir_all(&src).unwrap();
  fs::write(
    src.join("lib.rs"),
    "pub fn socket_send(buf: u32) -> u32 { buf }\npub fn parser_open(path: u32) -> u32 { path }\npub fn mutex_lock(hold: u32) -> u32 { hold }\n",
  )
  .unwrap();
  build_index(&src, &out).unwrap();
  (base, out)
}

#[test]
fn bad_encoder_selection_degrades_stated_and_still_serves() {
  let (base, out) = fixture("vorpal-encoder-rerank-degrade");
  fs::write(out.join("encoder.dir"), "/nonexistent/model/dir\n").unwrap();
  let searcher = Searcher::open(&out).unwrap();
  let status = searcher.encoder_status().expect("a failed selection must state itself");
  assert!(status.contains("encoder disabled:"), "{status}");
  let hits = searcher.records("socket send", 5, &SearchFilter::default()).unwrap();
  assert!(!hits.is_empty(), "degraded encoder must never fail the search");
  let _ = fs::remove_dir_all(&base);
}

#[test]
fn live_rerank_is_deterministic_and_preserves_record_scores() {
  let Some(model_dir) = std::env::var_os("VORPAL_CODERANK_DIR") else {
    eprintln!("skipped: VORPAL_CODERANK_DIR unset");
    return;
  };
  let (base, out) = fixture("vorpal-encoder-rerank-live");
  // Baseline WITHOUT the encoder: capture ids, scores, and channel provenance.
  let baseline = Searcher::open(&out)
    .unwrap()
    .records("socket send", 3, &SearchFilter::default())
    .unwrap();
  fs::write(
    out.join("encoder.dir"),
    format!("{}\n", std::path::PathBuf::from(&model_dir).display()),
  )
  .unwrap();
  let searcher = Searcher::open(&out).unwrap();
  assert!(searcher.encoder_status().is_none(), "{:?}", searcher.encoder_status());
  let first = searcher.records("socket send", 3, &SearchFilter::default()).unwrap();
  let second = searcher.records("socket send", 3, &SearchFilter::default()).unwrap();
  let shape = |hits: &[vorpal_index::records::SearchHitRecord]| {
    hits
      .iter()
      .map(|h| (h.node.name.clone(), h.score.to_bits()))
      .collect::<Vec<_>>()
  };
  assert_eq!(shape(&first), shape(&second), "rerank must be deterministic");
  // The rerank may only REORDER: the (name, score) multiset must match baseline.
  let mut base_set = shape(&baseline);
  let mut rerank_set = shape(&first);
  base_set.sort();
  rerank_set.sort();
  assert_eq!(base_set, rerank_set, "rerank must preserve hits and their fused scores");
  let _ = fs::remove_dir_all(&base);
}
