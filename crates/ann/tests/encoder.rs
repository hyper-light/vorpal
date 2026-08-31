//! Gated end-to-end oracles for the vendored encoder (semantic-tier Stage 6).
//!
//! The 547 MB weights cannot live in the repo, so these tests run only where
//! `VORPAL_CODERANK_DIR` points at a model directory that also carries
//! `goldens.json` from the independent reference generator
//! (scratchpad `pyref/ref_forward.py`: the real `tokenizers` library for ids, an
//! independent numpy NomicBert forward for activations). Unset, every test states
//! its skip and passes — the ungated algorithmic oracles live in the unit tests.

use vorpal_ann::encoder::CodeEncoder;

fn model_dir() -> Option<std::path::PathBuf> {
  std::env::var_os("VORPAL_CODERANK_DIR").map(Into::into)
}

fn goldens(dir: &std::path::Path) -> serde_json::Value {
  serde_json::from_str(&std::fs::read_to_string(dir.join("goldens.json")).unwrap()).unwrap()
}

fn golden_texts(goldens: &serde_json::Value) -> Vec<String> {
  goldens
    .as_object()
    .unwrap()
    .keys()
    .filter(|k| *k != "__tokenizer_battery__")
    .cloned()
    .collect()
}

#[test]
fn tokenizer_matches_the_reference_library() {
  let Some(dir) = model_dir() else {
    eprintln!("skipped: VORPAL_CODERANK_DIR unset");
    return;
  };
  let goldens = goldens(&dir);
  let encoder = CodeEncoder::open(&dir).unwrap();
  let expect_ids = |value: &serde_json::Value| -> Vec<u32> {
    value
      .as_array()
      .unwrap()
      .iter()
      .map(|id| id.as_u64().unwrap() as u32)
      .collect()
  };
  for (text, ids) in goldens["__tokenizer_battery__"].as_object().unwrap() {
    assert_eq!(
      encoder.token_ids(text),
      expect_ids(ids),
      "tokenizer diverged from the reference on {text:?}"
    );
  }
  for text in golden_texts(&goldens) {
    assert_eq!(
      encoder.token_ids(&text),
      expect_ids(&goldens[&text]["ids"]),
      "tokenizer diverged from the reference on {text:?}"
    );
  }
}

#[test]
fn forward_matches_the_numpy_reference() {
  let Some(dir) = model_dir() else {
    eprintln!("skipped: VORPAL_CODERANK_DIR unset");
    return;
  };
  let goldens = goldens(&dir);
  let encoder = CodeEncoder::open(&dir).unwrap();
  for text in golden_texts(&goldens) {
    let reference: Vec<f64> = goldens[&text]["cls_pre_norm"]
      .as_array()
      .unwrap()
      .iter()
      .map(|v| v.as_f64().unwrap())
      .collect();
    let ours = encoder.embed_raw(&text).unwrap();
    assert_eq!(ours.len(), reference.len());
    let scale = reference.iter().fold(1.0f64, |m, v| m.max(v.abs()));
    let mut worst = 0.0f64;
    for (a, b) in ours.iter().zip(&reference) {
      worst = worst.max((*a as f64 - b).abs());
    }
    assert!(
      worst / scale <= 1e-4,
      "forward diverged from the reference on {text:?}: max abs err {worst:.3e} (scale {scale:.3e})"
    );
  }
}

#[test]
fn embedding_is_deterministic_and_ranks_the_obvious_pair() {
  let Some(dir) = model_dir() else {
    eprintln!("skipped: VORPAL_CODERANK_DIR unset");
    return;
  };
  let encoder = CodeEncoder::open(&dir).unwrap();
  let query = "Calculate the n-th factorial";
  let first = encoder.embed_query(query).unwrap();
  let second = encoder.embed_query(query).unwrap();
  assert_eq!(
    first.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
    second.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
    "embedding must be bitwise reproducible"
  );
  let factorial = encoder
    .embed("def factorial(n): return 1 if n <= 1 else n * factorial(n - 1)")
    .unwrap();
  let unrelated = encoder.embed("vx_socket_send buffer flush").unwrap();
  let cosine = |a: &[f32], b: &[f32]| -> f64 {
    a.iter().zip(b).map(|(x, y)| *x as f64 * *y as f64).sum()
  };
  let (hit, miss) = (cosine(&first, &factorial), cosine(&first, &unrelated));
  assert!(
    hit > miss,
    "the factorial snippet must outrank the unrelated one ({hit:.4} vs {miss:.4})"
  );
}
