//! End-to-end learned tier (semantic-tier plan, Stage 1 plumbing): the `semantic.tier`
//! selection drives warm-time training; the persisted record carries tier +
//! checksum-verified weights; queries go through EXACTLY the persisted model; every
//! failure states itself and routes to the lexical fallback — never a silent zero and
//! never mixed embedders.

use std::fs;

use vorpal_index::{
  SearchFilter, SemanticTier, build_index, persisted_model_provenance, search_records_filtered,
  warm_ann, write_tier_selection,
};

/// Six token families across sixty functions, shaped like real code's Zipfian token
/// skew: the project prefix `vx` appears ~4× per definition (nginx's `ngx_` pattern —
/// name echoes into the signature, and both parameters carry it), family/role tokens
/// repeat 10–20× (clearing the min-count floor), and two unique-per-fn parameter
/// tokens inflate |V| so uSIF's α threshold ≈ n/|V| sits well below p(vx).
fn write_corpus(src: &std::path::Path) {
  let families = ["socket", "parser", "mutex", "inode", "folio", "cipher"];
  let roles = ["send", "recv", "open", "close", "poll", "flush", "init", "drop", "scan", "seal"];
  for (index, family) in families.iter().enumerate() {
    let mut content = String::new();
    for role in roles {
      content.push_str(&format!(
        "pub fn vx_{family}_{role}(vx_u{family}{role}: u32, vx_w{family}{role}: u32) -> u32 {{ 1 }}\n"
      ));
    }
    fs::write(src.join(format!("f{index}.rs")), content).unwrap();
  }
}

fn json_of(out: &std::path::Path) -> String {
  let gen_dir = vorpal_kg::resolve_index_dir(out);
  fs::read_to_string(gen_dir.join("ann.model.json")).unwrap_or_default()
}

#[test]
fn learned_selection_trains_persists_serves_and_falls_back() {
  unsafe { std::env::set_var("VORPAL_NO_AUTOWARM", "1") };
  let base = std::env::temp_dir().join("vorpal-learned-tier");
  let src = base.join("src");
  let out = base.join("index");
  let _ = fs::remove_dir_all(&base);
  fs::create_dir_all(&src).unwrap();
  write_corpus(&src);
  build_index(&src, &out).unwrap();
  write_tier_selection(&out, SemanticTier::Learned).unwrap();

  // Warm trains, persists, and commits the learned tier.
  warm_ann(&out).unwrap();
  let gen_dir = vorpal_kg::resolve_index_dir(&out);
  let model_path = gen_dir.join("ann.model.bin");
  assert!(model_path.exists(), "warm must persist ann.model.bin");
  let json = json_of(&out);
  assert!(json.contains("\"tier\":\"learned\""), "{json}");
  assert!(json.contains("\"learned\":true"), "{json}");
  assert!(!json.contains("\"weights_hash\":null"), "{json}");
  let provenance = persisted_model_provenance(&gen_dir).expect("record readable");
  assert!(provenance.learned);
  assert_eq!(provenance.model_id, "learned-static");
  assert!(provenance.dim >= 1);

  // Queries serve through the persisted model, deterministically.
  let hits = search_records_filtered(&out, "socket recv", 10, &SearchFilter::default()).unwrap();
  assert!(!hits.is_empty());
  let again = search_records_filtered(&out, "socket recv", 10, &SearchFilter::default()).unwrap();
  let names = |hs: &[vorpal_index::records::SearchHitRecord]| {
    hs.iter().map(|h| (h.node.name.clone(), h.score.to_bits())).collect::<Vec<_>>()
  };
  assert_eq!(names(&hits), names(&again));

  // Double-warm byte identity (the plan's determinism gate): break freshness, re-warm,
  // and the retrained artifacts must be BIT-IDENTICAL.
  let first_model = fs::read(&model_path).unwrap();
  let first_ann = fs::read(gen_dir.join("ann.bin")).unwrap();
  fs::remove_file(gen_dir.join("ann.stamp")).unwrap();
  warm_ann(&out).unwrap();
  assert_eq!(first_model, fs::read(&model_path).unwrap(), "ann.model.bin not reproducible");
  assert_eq!(first_ann, fs::read(gen_dir.join("ann.bin")).unwrap(), "ann.bin not reproducible");

  // Tampering with the model file: the coherence gate refuses it and a FRESH handle
  // serves through the lexical fallback over the exact paths — answers keep coming,
  // wrong vectors never do.
  let mut tampered = first_model.clone();
  let middle = tampered.len() / 2;
  tampered[middle] ^= 0x01;
  fs::write(&model_path, &tampered).unwrap();
  let searcher = vorpal_index::Searcher::open(&out).unwrap();
  let fallback_hits = searcher.records("socket recv", 10, &SearchFilter::default()).unwrap();
  assert!(!fallback_hits.is_empty(), "tamper must degrade, never fail");
  fs::write(&model_path, &first_model).unwrap();

  // A tier FLIP is staleness: the next warm rebuilds under the new selection and the
  // lexical build leaves no model file behind.
  write_tier_selection(&out, SemanticTier::Lexical).unwrap();
  warm_ann(&out).unwrap();
  let json = json_of(&out);
  assert!(json.contains("\"tier\":\"lexical\""), "{json}");
  assert!(json.contains("\"learned\":false"), "{json}");
  assert!(!model_path.exists(), "lexical build must remove the stale model file");

  // And flipping back retrains.
  write_tier_selection(&out, SemanticTier::Learned).unwrap();
  warm_ann(&out).unwrap();
  assert!(json_of(&out).contains("\"tier\":\"learned\""));

  let _ = fs::remove_dir_all(&base);
}

#[test]
fn below_floor_corpus_states_its_lexical_fallback() {
  unsafe { std::env::set_var("VORPAL_NO_AUTOWARM", "1") };
  let base = std::env::temp_dir().join("vorpal-learned-floor");
  let src = base.join("src");
  let out = base.join("index");
  let _ = fs::remove_dir_all(&base);
  fs::create_dir_all(&src).unwrap();
  // Two functions: nowhere near the min-count floor — the learned tier must refuse,
  // fall back to lexical, and SAY SO in the persisted record.
  fs::write(src.join("tiny.rs"), "pub fn alpha() -> u32 { 1 }\npub fn beta() -> u32 { 2 }\n")
    .unwrap();
  build_index(&src, &out).unwrap();
  write_tier_selection(&out, SemanticTier::Learned).unwrap();
  warm_ann(&out).unwrap();

  let json = json_of(&out);
  assert!(json.contains("\"tier\":\"lexical\""), "{json}");
  assert!(json.contains("\"note\":"), "the fallback must state itself: {json}");
  assert!(json.contains("fell back to lexical"), "{json}");
  // The fallback tier still serves.
  let hits = search_records_filtered(&out, "alpha", 5, &SearchFilter::default()).unwrap();
  assert!(!hits.is_empty());

  let _ = fs::remove_dir_all(&base);
}
