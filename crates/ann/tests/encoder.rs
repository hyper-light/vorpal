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

/// A fixed surface battery in the doc-side shape (name + signature + basename,
/// ~12 tokens) plus the prefixed query shape — the parity oracle's inputs when
/// no goldens file is present beside the model.
fn parity_battery(dir: &std::path::Path) -> Vec<String> {
  let mut texts: Vec<String> = vec![
    "Represent this query for searching relevant code: near duplicate code detection".into(),
    "Represent this query for searching relevant code: alloc_skb".into(),
    "similar_pairs pub fn similar_pairs(&self, min_similarity: f64) -> Vec<(NodeId, NodeId, f64)> kg.rs".into(),
    "alloc_skb static inline struct sk_buff *alloc_skb(unsigned int size, gfp_t priority) skbuff.h".into(),
    "tcp_cong_avoid_ai void tcp_cong_avoid_ai(struct tcp_sock *tp, u32 w, u32 acked) tcp_cong.c".into(),
    "request_threaded_irq int request_threaded_irq(unsigned int irq, irq_handler_t handler) manage.c".into(),
    "PyDict_SetItem int PyDict_SetItem(PyObject *op, PyObject *key, PyObject *value) dictobject.c".into(),
    "ObservedStore pub struct ObservedStore traces.rs".into(),
    "ingest_traces pub fn ingest_traces(index_dir: &Path, folded: &Path) -> Result<Report, Box<dyn Error>> traces.rs".into(),
    "rrf_fuse_explained fn rrf_fuse_explained(lists: &[Vec<u64>], k: usize) -> Vec<FusedHit> lib.rs".into(),
    "vx_socket_send buffer flush".into(),
    "def factorial(n): return 1 if n <= 1 else n * factorial(n - 1)".into(),
  ];
  if let Ok(text) = std::fs::read_to_string(dir.join("goldens.json"))
    && let Ok(goldens) = serde_json::from_str::<serde_json::Value>(&text)
    && let Some(object) = goldens.as_object()
  {
    texts.extend(object.keys().filter(|k| !k.starts_with("__")).cloned());
  }
  texts
}

fn cosine(a: &[f32], b: &[f32]) -> f64 {
  a.iter().zip(b).map(|(x, y)| *x as f64 * *y as f64).sum()
}

fn bits(rows: &[Vec<f32>]) -> Vec<u32> {
  rows.iter().flatten().map(|v| v.to_bits()).collect()
}

/// The doc-side parity oracle (ENCODER_RESEARCH §8.2, Stage A): the throughput
/// GEMM path stays within cosine 0.9999 of the fixed-order path on every
/// surface of the battery — the bound that admits its rows into a sidecar the
/// fixed-order query embedding is scored against.
#[test]
fn throughput_path_matches_fixed_order_within_cosine() {
  use vorpal_ann::encoder::GemmPath;
  let Some(dir) = model_dir() else {
    eprintln!("skipped: VORPAL_CODERANK_DIR unset");
    return;
  };
  let encoder = CodeEncoder::open(&dir).unwrap();
  let texts = parity_battery(&dir);
  let borrowed: Vec<&str> = texts.iter().map(String::as_str).collect();
  let fixed = encoder.embed_batch_with(&borrowed, GemmPath::FixedOrder).unwrap();
  let fast = encoder.embed_batch_with(&borrowed, GemmPath::Throughput).unwrap();
  let mut worst = 1.0f64;
  for ((text, a), b) in texts.iter().zip(&fixed).zip(&fast) {
    let c = cosine(a, b);
    worst = worst.min(c);
    assert!(c >= 0.9999, "throughput path drifted on {text:?}: cosine {c:.7}");
  }
  // The AVX2+FMA GEMM is the fixed lanes' exact reduction (unit oracle:
  // bit-equal), but the throughput FORWARD also runs the f32 `exp_fast` gate,
  // so whole-embedding parity stays the cosine bound on every rung; whether
  // the bits happen to agree is reported, never asserted.
  eprintln!(
    "throughput path ({}) vs fixed-order: min cosine {worst:.7} over {} surfaces ({})",
    GemmPath::Throughput.label(),
    texts.len(),
    if bits(&fixed) == bits(&fast) { "bit-identical" } else { "bits differ: the throughput gate is exp_fast" }
  );
}

/// The deviation int8 OUTPUT quantization (the sidecar's own `quantize_row`,
/// dequantized) imposes on a unit embedding — the perturbation under which the
/// published int8 retention (ENCODER_RESEARCH §6: 97 % MTEB retrieval for the
/// best-in-class 1024-d model, 99 % with ×4 rescore) was measured.
fn output_quantization_cosine(row: &[f32]) -> f64 {
  let mut codes = vec![0i8; row.len()];
  let scale = vorpal_ann::dense::quantize_row(row, &mut codes);
  let dequantized: Vec<f32> = codes.iter().map(|&c| c as f32 * scale).collect();
  let norm = dequantized.iter().map(|v| (*v as f64) * (*v as f64)).sum::<f64>().sqrt();
  if norm == 0.0 {
    return 0.0;
  }
  cosine(row, &dequantized) / norm
}

/// The int8 doc-side path (`GemmPath::Int8`): determinism by construction
/// (exact i32 dots — run-to-run AND thread-count bit-stable), a functional
/// floor (it must still rank the obvious pair), and its retention against the
/// DERIVED bar: the int8 forward's mean angular deviation from the f32
/// fixed-order embedding must not exceed the deviation int8 output quantization
/// imposes on the same embeddings — the perturbation the published ≥ 97 %
/// retention was measured under — so by that mapping (retention is monotone in
/// embedding perturbation) the int8 forward retains ≥ 97 %. The verdict is
/// REPORTED, not asserted: it decides the doc-side DEFAULT (recorded in
/// BENCHMARKS — measured FAILS on 2026-09-02: min cosine 0.971 on the
/// query-shaped surface, so the sidecar keeps `Throughput`), and a numerics
/// that fails a default-selection bar is not a broken kernel — the unit
/// oracles pin the kernels exactly.
#[test]
fn int8_path_is_deterministic_and_reports_retention_against_the_derived_bar() {
  use vorpal_ann::encoder::GemmPath;
  let Some(dir) = model_dir() else {
    eprintln!("skipped: VORPAL_CODERANK_DIR unset");
    return;
  };
  let encoder = CodeEncoder::open(&dir).unwrap();
  let texts = parity_battery(&dir);
  let borrowed: Vec<&str> = texts.iter().map(String::as_str).collect();
  let fixed = encoder.embed_batch_with(&borrowed, GemmPath::FixedOrder).unwrap();
  let first = encoder.embed_batch_with(&borrowed, GemmPath::Int8).unwrap();
  let second = encoder.embed_batch_with(&borrowed, GemmPath::Int8).unwrap();
  assert_eq!(bits(&first), bits(&second), "int8 path must be run-to-run reproducible");
  let single = rayon::ThreadPoolBuilder::new()
    .num_threads(1)
    .build()
    .unwrap()
    .install(|| encoder.embed_batch_with(&borrowed, GemmPath::Int8).unwrap());
  assert_eq!(bits(&single), bits(&first), "int8 path must be bit-stable across rayon thread counts (exact i32 dots)");
  // Functional floor: an int8 query embedding still ranks the factorial
  // snippet above the unrelated one (the battery's last two surfaces).
  let (hit, miss) = (&first[texts.len() - 1], &first[texts.len() - 2]);
  assert!(
    texts[texts.len() - 1].starts_with("def factorial") && texts[texts.len() - 2].starts_with("vx_socket"),
    "battery layout changed under the functional floor"
  );
  let factorial_query = encoder
    .embed_batch_with(&["Represent this query for searching relevant code: Calculate the n-th factorial"], GemmPath::Int8)
    .unwrap();
  assert!(
    cosine(&factorial_query[0], hit) > cosine(&factorial_query[0], miss),
    "int8 path must still rank the factorial snippet above the unrelated one"
  );
  let (mut min_int8, mut min_bar) = (1.0f64, 1.0f64);
  let (mut dev_int8, mut dev_bar) = (0.0f64, 0.0f64);
  let mut worst_text = "";
  for ((text, f), q) in texts.iter().zip(&fixed).zip(&first) {
    let c = cosine(f, q);
    let bar = output_quantization_cosine(f);
    if c < min_int8 {
      worst_text = text;
    }
    min_int8 = min_int8.min(c);
    min_bar = min_bar.min(bar);
    dev_int8 += 1.0 - c;
    dev_bar += 1.0 - bar;
  }
  let n = texts.len() as f64;
  let meets = dev_int8 / n <= dev_bar / n;
  eprintln!(
    "int8 path ({}) vs fixed-order over {} surfaces: min cosine {min_int8:.6} ({worst_text:?}), mean 1-cos {:.2e}; \
     output-int8 bar on the same embeddings: min cosine {min_bar:.6}, mean 1-cos {:.2e}; \
     verdict: {} the derived ≥ 97 % retention bar; int8 weights {} MB",
    GemmPath::Int8.label(),
    texts.len(),
    dev_int8 / n,
    dev_bar / n,
    if meets { "MEETS" } else { "FAILS" },
    encoder.int8_bytes() / (1 << 20),
  );
}

/// Determinism statement for the sidecar law: the throughput path must be
/// reproducible run-to-run at a fixed thread count (else a sidecar could not
/// even be rebuilt identically in one process), and the test REPORTS whether it
/// is also bit-stable across rayon thread counts (1 vs the default pool). The
/// framework's own threading (`VECLIB_MAXIMUM_THREADS`) is a process-level
/// setting, exercised by running the test under both values.
#[test]
fn throughput_path_reproducibility_is_stated() {
  use vorpal_ann::encoder::GemmPath;
  let Some(dir) = model_dir() else {
    eprintln!("skipped: VORPAL_CODERANK_DIR unset");
    return;
  };
  let encoder = CodeEncoder::open(&dir).unwrap();
  let texts = parity_battery(&dir);
  let borrowed: Vec<&str> = texts.iter().map(String::as_str).collect();
  let first = encoder.embed_batch_with(&borrowed, GemmPath::Throughput).unwrap();
  let second = encoder.embed_batch_with(&borrowed, GemmPath::Throughput).unwrap();
  assert_eq!(bits(&first), bits(&second), "throughput path must be run-to-run reproducible");
  let single = rayon::ThreadPoolBuilder::new()
    .num_threads(1)
    .build()
    .unwrap()
    .install(|| encoder.embed_batch_with(&borrowed, GemmPath::Throughput).unwrap());
  let fixed_default = encoder.embed_batch_with(&borrowed, GemmPath::FixedOrder).unwrap();
  let fixed_single = rayon::ThreadPoolBuilder::new()
    .num_threads(1)
    .build()
    .unwrap()
    .install(|| encoder.embed_batch_with(&borrowed, GemmPath::FixedOrder).unwrap());
  assert_eq!(
    bits(&fixed_default),
    bits(&fixed_single),
    "fixed-order path must be bit-stable across rayon thread counts (the query-side law)"
  );
  eprintln!(
    "throughput path ({}) 1-thread vs default rayon pool: {}",
    GemmPath::Throughput.label(),
    if bits(&single) == bits(&first) { "IDENTICAL bytes" } else { "DIFFERENT bytes (stamp-gated sidecar only)" }
  );
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
