//! Vendored code-specialized encoder (semantic-tier Stage 6, owner-waived):
//! CodeRankEmbed (MIT, cornstack/Nomic — 137M-param NomicBert bi-encoder) run
//! through vorpal's OWNED inference: an mmap'd strict safetensors loader, an owned
//! WordPiece pipeline, and an owned forward pass with f64-accumulated deterministic
//! reductions. NEVER a runtime download — the model directory is vendored beside
//! the install and validated at open (config semantics, tensor shapes, tokenizer
//! pipeline all refused if they are not exactly what this implementation
//! reproduces). Correctness is pinned by an independent numpy reference forward
//! and the reference `tokenizers` library's goldens (`tests/encoder.rs`, gated on
//! `VORPAL_CODERANK_DIR` since the 547 MB artifact cannot live in the repo).
//!
//! SCALE LAW (the physics that shaped the integration): encoding a kernel-scale
//! row population through 137M parameters is ~10¹² FLOPs — hours of CPU — so this
//! encoder can never be the warm-time document embedder. Its shape is the opt-in
//! QUERY-TIME RERANKER: one prefixed query plus the fused top-K candidate
//! surfaces per search, nothing precomputed.

mod f16;
mod forward;
mod safetensors;
mod tokenizer;

use std::path::Path;

use forward::{LayerWeights, ModelWeights};
pub use forward::l2_normalize;
pub use safetensors::{convert_safetensors_f32_to_f16, safetensors_is_f16};

/// The task instruction the model card requires on every QUERY (verbatim;
/// documents embed without it).
pub const QUERY_PREFIX: &str = "Represent this query for searching relevant code: ";

pub struct CodeEncoder {
  weights: safetensors::SafeTensors,
  tokenizer: tokenizer::WordPiece,
  dim: usize,
  heads: usize,
  layers: usize,
  inner: usize,
  layer_norm_eps: f64,
  rotary_base: f64,
  vocab_rows: usize,
  max_positions: usize,
}

impl CodeEncoder {
  /// Open a vendored model directory (`model.safetensors` + `tokenizer.json` +
  /// `config.json`). Every architecture constant is READ from the model's own
  /// config; any semantics this implementation does not reproduce (pre-norm, RMS
  /// norm, interleaved or partial rotary, biased projections, causal masking) is
  /// a typed refusal — never a silently different forward pass.
  pub fn open(model_dir: &Path) -> Result<CodeEncoder, String> {
    let config_bytes = std::fs::read(model_dir.join("config.json"))
      .map_err(|e| format!("config.json: {e}"))?;
    let config: serde_json::Value =
      serde_json::from_slice(&config_bytes).map_err(|e| format!("config.json parse: {e}"))?;
    let number = |key: &str| -> Result<usize, String> {
      config
        .get(key)
        .and_then(|v| v.as_u64())
        .and_then(|v| usize::try_from(v).ok())
        .ok_or_else(|| format!("config.json: missing numeric {key}"))
    };
    let float = |key: &str| -> Result<f64, String> {
      config
        .get(key)
        .and_then(|v| v.as_f64())
        .ok_or_else(|| format!("config.json: missing float {key}"))
    };
    let expect_str = |key: &str, want: &str| -> Result<(), String> {
      if config.get(key).and_then(|v| v.as_str()) == Some(want) {
        Ok(())
      } else {
        Err(format!("config.json: {key} must be {want:?} for this implementation"))
      }
    };
    let refuse_true = |key: &str| -> Result<(), String> {
      if config.get(key).and_then(|v| v.as_bool()) == Some(true) {
        Err(format!("config.json: {key}=true is not a semantics this implementation reproduces"))
      } else {
        Ok(())
      }
    };
    expect_str("model_type", "nomic_bert")?;
    expect_str("activation_function", "swiglu")?;
    refuse_true("prenorm")?;
    refuse_true("causal")?;
    refuse_true("use_rms_norm")?;
    refuse_true("rotary_emb_interleaved")?;
    refuse_true("qkv_proj_bias")?;
    refuse_true("mlp_fc1_bias")?;
    refuse_true("mlp_fc2_bias")?;
    if float("rotary_emb_fraction")? != 1.0 {
      return Err("config.json: partial rotary fraction is not reproduced here".to_string());
    }
    let (dim, heads, layers, inner) =
      (number("n_embd")?, number("n_head")?, number("n_layer")?, number("n_inner")?);
    let vocab_rows = number("vocab_size")?;
    let max_positions = number("max_trained_positions")?;
    if max_positions < 4 {
      return Err("config.json: degenerate position budget".to_string());
    }
    let tokenizer_bytes = std::fs::read(model_dir.join("tokenizer.json"))
      .map_err(|e| format!("tokenizer.json: {e}"))?;
    let tokenizer = tokenizer::WordPiece::from_tokenizer_json(&tokenizer_bytes)?;
    let weights = safetensors::SafeTensors::open(&model_dir.join("model.safetensors"))?;
    let encoder = CodeEncoder {
      weights,
      tokenizer,
      dim,
      heads,
      layers,
      inner,
      layer_norm_eps: float("layer_norm_epsilon")?,
      rotary_base: float("rotary_emb_base")?,
      vocab_rows,
      max_positions,
    };
    // Fail at open, not first embed: every tensor must exist with its exact shape.
    encoder.model_weights()?;
    Ok(encoder)
  }

  fn model_weights(&self) -> Result<ModelWeights<'_>, String> {
    let mut layers = Vec::with_capacity(self.layers);
    for layer in 0..self.layers {
      let prefix = format!("encoder.layers.{layer}.");
      layers.push(LayerWeights {
        wqkv: self
          .weights
          .matrix(&format!("{prefix}attn.Wqkv.weight"), 3 * self.dim, self.dim)?,
        out_proj: self
          .weights
          .matrix(&format!("{prefix}attn.out_proj.weight"), self.dim, self.dim)?,
        norm1_weight: self.weights.vector(&format!("{prefix}norm1.weight"), self.dim)?,
        norm1_bias: self.weights.vector(&format!("{prefix}norm1.bias"), self.dim)?,
        norm2_weight: self.weights.vector(&format!("{prefix}norm2.weight"), self.dim)?,
        norm2_bias: self.weights.vector(&format!("{prefix}norm2.bias"), self.dim)?,
        fc11: self
          .weights
          .matrix(&format!("{prefix}mlp.fc11.weight"), self.inner, self.dim)?,
        fc12: self
          .weights
          .matrix(&format!("{prefix}mlp.fc12.weight"), self.inner, self.dim)?,
        fc2: self
          .weights
          .matrix(&format!("{prefix}mlp.fc2.weight"), self.dim, self.inner)?,
      });
    }
    let token_type = self
      .weights
      .matrix("embeddings.token_type_embeddings.weight", 2, self.dim)?;
    Ok(ModelWeights {
      word_embeddings: self.weights.matrix(
        "embeddings.word_embeddings.weight",
        self.vocab_rows,
        self.dim,
      )?,
      token_type_row0: &token_type[..self.dim],
      emb_ln_weight: self.weights.vector("emb_ln.weight", self.dim)?,
      emb_ln_bias: self.weights.vector("emb_ln.bias", self.dim)?,
      layers,
      dim: self.dim,
      heads: self.heads,
      inner: self.inner,
      layer_norm_eps: self.layer_norm_eps,
      rotary_base: self.rotary_base,
      vocab_rows: self.vocab_rows,
    })
  }

  pub fn dim(&self) -> usize {
    self.dim
  }

  /// The tokenizer's ids for `text` — exposed for the reference-parity oracle.
  pub fn token_ids(&self, text: &str) -> Vec<u32> {
    self.tokenizer.encode(text)
  }

  /// Token ids clamped to the trained position budget: tail-truncate but keep
  /// the `[SEP]` terminator the template requires — a safety clamp far above the
  /// reranker's working regime.
  fn clamped_ids(&self, text: &str) -> Vec<u32> {
    let mut ids = self.tokenizer.encode(text);
    if ids.len() > self.max_positions {
      let sep = ids.last().copied().unwrap_or(0);
      ids.truncate(self.max_positions - 1);
      ids.push(sep);
    }
    ids
  }

  /// CLS embedding BEFORE normalization — the reference-parity surface.
  pub fn embed_raw(&self, text: &str) -> Result<Vec<f32>, String> {
    forward::forward(&self.model_weights()?, &self.clamped_ids(text))
  }

  /// L2-normalized embeddings for a BATCH of texts in ONE forward pass (the
  /// reranker's shape — every GEMM runs at full width over the concatenated token
  /// matrix). Per-text results are bitwise identical to [`Self::embed`], pinned
  /// by the gated batch oracle.
  pub fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, String> {
    let sequences: Vec<Vec<u32>> = texts.iter().map(|text| self.clamped_ids(text)).collect();
    let borrowed: Vec<&[u32]> = sequences.iter().map(Vec::as_slice).collect();
    let mut rows = forward::forward_batch(&self.model_weights()?, &borrowed)?;
    for row in &mut rows {
      l2_normalize(row);
    }
    Ok(rows)
  }

  /// L2-normalized document embedding (cosine-ready).
  pub fn embed(&self, text: &str) -> Result<Vec<f32>, String> {
    let mut cls = self.embed_raw(text)?;
    l2_normalize(&mut cls);
    Ok(cls)
  }

  /// L2-normalized QUERY embedding — the model card's required task prefix applied.
  pub fn embed_query(&self, query: &str) -> Result<Vec<f32>, String> {
    self.embed(&format!("{QUERY_PREFIX}{query}"))
  }
}
