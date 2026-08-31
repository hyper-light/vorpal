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

#[test]
fn f16_conversion_round_trips_and_embeds_close_to_f32() {
  let Some(dir) = model_dir() else {
    eprintln!("skipped: VORPAL_CODERANK_DIR unset");
    return;
  };
  // Build a temporary f16 model directory from the f32 one.
  let f16_dir = std::env::temp_dir().join("vorpal-coderank-f16-test");
  let _ = std::fs::remove_dir_all(&f16_dir);
  std::fs::create_dir_all(&f16_dir).unwrap();
  for small in ["tokenizer.json", "config.json"] {
    std::fs::copy(dir.join(small), f16_dir.join(small)).unwrap();
  }
  vorpal_ann::encoder::convert_safetensors_f32_to_f16(
    &dir.join("model.safetensors"),
    &f16_dir.join("model.safetensors"),
  )
  .unwrap();
  assert!(
    vorpal_ann::encoder::safetensors_is_f16(&f16_dir.join("model.safetensors")).unwrap(),
    "converted file must read back as F16"
  );
  let halved = std::fs::metadata(f16_dir.join("model.safetensors")).unwrap().len();
  let original = std::fs::metadata(dir.join("model.safetensors")).unwrap().len();
  assert!(
    halved < original * 6 / 10,
    "f16 file must be about half the size ({halved} vs {original})"
  );

  let f32_encoder = CodeEncoder::open(&dir).unwrap();
  let f16_encoder = CodeEncoder::open(&f16_dir).unwrap();
  let texts = [
    "Represent this query for searching relevant code: Calculate the n-th factorial",
    "vx_socket_send buffer flush",
  ];
  for text in texts {
    let full = f32_encoder.embed(text).unwrap();
    let half = f16_encoder.embed(text).unwrap();
    let cosine: f64 = full
      .iter()
      .zip(&half)
      .map(|(a, b)| *a as f64 * *b as f64)
      .sum();
    eprintln!("f16-vs-f32 embedding cosine for {text:?}: {cosine:.6}");
    assert!(
      cosine >= 0.99,
      "f16 weights drifted too far from f32 on {text:?}: cosine {cosine:.6}"
    );
    // And the f16 path itself is bitwise reproducible.
    let again = f16_encoder.embed(text).unwrap();
    assert_eq!(
      half.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
      again.iter().map(|v| v.to_bits()).collect::<Vec<_>>()
    );
  }
  let _ = std::fs::remove_dir_all(&f16_dir);
}

#[test]
fn batched_embeddings_equal_individual_ones_bitwise() {
  let Some(dir) = model_dir() else {
    eprintln!("skipped: VORPAL_CODERANK_DIR unset");
    return;
  };
  let encoder = CodeEncoder::open(&dir).unwrap();
  let texts = [
    "Represent this query for searching relevant code: Calculate the n-th factorial",
    "def factorial(n): return 1 if n <= 1 else n * factorial(n - 1)",
    "vx_socket_send buffer flush",
  ];
  let batched = encoder.embed_batch(&texts).unwrap();
  assert_eq!(batched.len(), texts.len());
  for (text, batch_row) in texts.iter().zip(&batched) {
    let solo = encoder.embed(text).unwrap();
    assert_eq!(
      solo.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
      batch_row.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
      "batched embedding diverged from the solo one for {text:?}"
    );
  }
}
