//! Owned NomicBert forward pass (semantic-tier Stage 6, CodeRankEmbed weights).
//!
//! Semantics pinned from the reference modeling source and verified against an
//! independent numpy implementation (`tests/encoder.rs`, gated on the model dir):
//!
//! * embeddings = LN(word_emb\[ids\] + token_type_emb\[0\]) — no position table,
//!   positions are ROTARY (base from config, non-interleaved rotate-half over the
//!   full head dim).
//! * per layer, POST-norm: h = LN1(x + Attn(x)); out = LN2(h + MLP(h)).
//! * Attn: qkv = x·Wqkvᵀ (no bias) → \[seq, 3, heads, 64\]; rotary on q,k;
//!   softmax(q·kᵀ/√64) (max-subtracted); heads concat · out_projᵀ (no bias).
//! * MLP: fc2( fc11(x) ⊙ silu(fc12(x)) ) — fc12 carries the gate.
//! * pool: CLS (row 0) of the final hidden state.
//!
//! CORRECTNESS-FIRST numerics: every reduction accumulates in f64 in a fixed
//! order, parallelism is only across independent output rows — bit-stable at any
//! thread count. Throughput is the NEXT item (profile-after law); this pass exists
//! to be provably right against the reference before any kernel work.

use rayon::prelude::*;

/// One layer's weight slices (all row-major `[out][in]`, biasless per config).
pub struct LayerWeights<'a> {
  pub wqkv: &'a [f32],
  pub out_proj: &'a [f32],
  pub norm1_weight: &'a [f32],
  pub norm1_bias: &'a [f32],
  pub norm2_weight: &'a [f32],
  pub norm2_bias: &'a [f32],
  pub fc11: &'a [f32],
  pub fc12: &'a [f32],
  pub fc2: &'a [f32],
}

/// The whole model's weights + the architecture constants read from config.json.
pub struct ModelWeights<'a> {
  pub word_embeddings: &'a [f32],
  pub token_type_row0: &'a [f32],
  pub emb_ln_weight: &'a [f32],
  pub emb_ln_bias: &'a [f32],
  pub layers: Vec<LayerWeights<'a>>,
  pub dim: usize,
  pub heads: usize,
  pub inner: usize,
  pub layer_norm_eps: f64,
  pub rotary_base: f64,
  pub vocab_rows: usize,
}

/// LayerNorm over each `dim`-row of `x`, in place: population variance, f64.
fn layer_norm(x: &mut [f64], dim: usize, weight: &[f32], bias: &[f32], eps: f64) {
  for row in x.chunks_exact_mut(dim) {
    let mean = row.iter().sum::<f64>() / dim as f64;
    let variance = row.iter().map(|v| (v - mean) * (v - mean)).sum::<f64>() / dim as f64;
    let inv = 1.0 / (variance + eps).sqrt();
    for (value, (w, b)) in row.iter_mut().zip(weight.iter().zip(bias)) {
      *value = (*value - mean) * inv * *w as f64 + *b as f64;
    }
  }
}

/// `out[s][o] = Σ_d x[s][d] · w[o][d]` — w row-major `[rows_out][dim_in]`; f64
/// accumulation in ascending d; rows of `out` are independent (rayon-safe).
fn gemm(x: &[f64], dim_in: usize, w: &[f32], rows_out: usize, out: &mut [f64]) {
  out
    .par_chunks_mut(rows_out)
    .enumerate()
    .for_each(|(s, out_row)| {
      let x_row = &x[s * dim_in..(s + 1) * dim_in];
      for (o, slot) in out_row.iter_mut().enumerate() {
        let w_row = &w[o * dim_in..(o + 1) * dim_in];
        let mut total = 0.0f64;
        for (value, weight) in x_row.iter().zip(w_row) {
          total += value * *weight as f64;
        }
        *slot = total;
      }
    });
}

/// CLS embedding (pre-normalization) for one token sequence. `ids` must be within
/// the embedding table; the sequence length is the caller's truncation decision.
pub fn forward(weights: &ModelWeights<'_>, ids: &[u32]) -> Result<Vec<f32>, String> {
  let (dim, heads) = (weights.dim, weights.heads);
  if dim == 0 || heads == 0 || dim % heads != 0 {
    return Err("encoder: degenerate architecture constants".to_string());
  }
  let head_dim = dim / heads;
  if head_dim % 2 != 0 {
    return Err("encoder: rotary needs an even head dim".to_string());
  }
  let seq = ids.len();
  if seq == 0 {
    return Err("encoder: empty token sequence".to_string());
  }
  for &id in ids {
    if id as usize >= weights.vocab_rows {
      return Err(format!("encoder: token id {id} outside the embedding table"));
    }
  }

  // Embeddings + emb_ln.
  let mut x = vec![0.0f64; seq * dim];
  for (s, &id) in ids.iter().enumerate() {
    let word = &weights.word_embeddings[id as usize * dim..(id as usize + 1) * dim];
    let row = &mut x[s * dim..(s + 1) * dim];
    for ((slot, w), t) in row.iter_mut().zip(word).zip(weights.token_type_row0) {
      *slot = *w as f64 + *t as f64;
    }
  }
  layer_norm(&mut x, dim, weights.emb_ln_weight, weights.emb_ln_bias, weights.layer_norm_eps);

  // Rotary tables: [seq][head_dim/2].
  let half = head_dim / 2;
  let mut cos = vec![0.0f64; seq * half];
  let mut sin = vec![0.0f64; seq * half];
  for s in 0..seq {
    for f in 0..half {
      let inv_freq = 1.0 / weights.rotary_base.powf((2 * f) as f64 / head_dim as f64);
      let angle = s as f64 * inv_freq;
      cos[s * half + f] = angle.cos();
      sin[s * half + f] = angle.sin();
    }
  }
  let rotate = |vectors: &mut [f64]| {
    // vectors: [seq][heads][head_dim], rotate-half non-interleaved.
    for s in 0..seq {
      for h in 0..heads {
        let base = (s * heads + h) * head_dim;
        for f in 0..half {
          let (c, n) = (cos[s * half + f], sin[s * half + f]);
          let (a, b) = (vectors[base + f], vectors[base + half + f]);
          vectors[base + f] = a * c - b * n;
          vectors[base + half + f] = b * c + a * n;
        }
      }
    }
  };

  let mut qkv = vec![0.0f64; seq * 3 * dim];
  let mut attn_out = vec![0.0f64; seq * dim];
  let mut context = vec![0.0f64; seq * dim];
  let mut mlp_y = vec![0.0f64; seq * weights.inner];
  let mut mlp_gate = vec![0.0f64; seq * weights.inner];
  let mut mlp_out = vec![0.0f64; seq * dim];
  let scale = 1.0 / (head_dim as f64).sqrt();

  for layer in &weights.layers {
    // qkv: [seq][3*dim] rows (t-major, then head, then component); unpack into
    // contiguous q/k/v matrices [seq][heads][head_dim] for the attention walk.
    gemm(&x, dim, layer.wqkv, 3 * dim, &mut qkv);
    let mut q = vec![0.0f64; seq * dim];
    let mut k = vec![0.0f64; seq * dim];
    let mut v = vec![0.0f64; seq * dim];
    for s in 0..seq {
      let row = &qkv[s * 3 * dim..(s + 1) * 3 * dim];
      q[s * dim..(s + 1) * dim].copy_from_slice(&row[0..dim]);
      k[s * dim..(s + 1) * dim].copy_from_slice(&row[dim..2 * dim]);
      v[s * dim..(s + 1) * dim].copy_from_slice(&row[2 * dim..3 * dim]);
    }
    rotate(&mut q);
    rotate(&mut k);
    // Attention per (head, query-row): independent outputs, deterministic inner
    // walks in ascending key order.
    context
      .par_chunks_mut(dim)
      .enumerate()
      .for_each(|(s, out_row)| {
        let mut weights_buffer = vec![0.0f64; seq];
        for h in 0..heads {
          let q_row = &q[(s * heads + h) * head_dim..(s * heads + h + 1) * head_dim];
          let mut max_score = f64::NEG_INFINITY;
          for (t, slot) in weights_buffer.iter_mut().enumerate() {
            let k_row = &k[(t * heads + h) * head_dim..(t * heads + h + 1) * head_dim];
            let mut dot = 0.0f64;
            for (a, b) in q_row.iter().zip(k_row) {
              dot += a * b;
            }
            let score = dot * scale;
            *slot = score;
            if score > max_score {
              max_score = score;
            }
          }
          let mut total = 0.0f64;
          for slot in weights_buffer.iter_mut() {
            *slot = (*slot - max_score).exp();
            total += *slot;
          }
          let out_head = &mut out_row[h * head_dim..(h + 1) * head_dim];
          out_head.fill(0.0);
          for (t, weight) in weights_buffer.iter().enumerate() {
            let value = weight / total;
            let v_row = &v[(t * heads + h) * head_dim..(t * heads + h + 1) * head_dim];
            for (slot, component) in out_head.iter_mut().zip(v_row) {
              *slot += value * component;
            }
          }
        }
      });
    gemm(&context, dim, layer.out_proj, dim, &mut attn_out);
    for (value, add) in x.iter_mut().zip(&attn_out) {
      *value += add;
    }
    layer_norm(&mut x, dim, layer.norm1_weight, layer.norm1_bias, weights.layer_norm_eps);
    gemm(&x, dim, layer.fc11, weights.inner, &mut mlp_y);
    gemm(&x, dim, layer.fc12, weights.inner, &mut mlp_gate);
    for (y, gate) in mlp_y.iter_mut().zip(&mlp_gate) {
      *y *= gate / (1.0 + (-gate).exp());
    }
    gemm(&mlp_y, weights.inner, layer.fc2, dim, &mut mlp_out);
    for (value, add) in x.iter_mut().zip(&mlp_out) {
      *value += add;
    }
    layer_norm(&mut x, dim, layer.norm2_weight, layer.norm2_bias, weights.layer_norm_eps);
  }
  Ok(x[..dim].iter().map(|v| *v as f32).collect())
}

/// L2-normalize in place; a zero vector stays zero (stated by the caller's report).
pub fn l2_normalize(v: &mut [f32]) {
  let norm = v.iter().map(|x| (*x as f64) * (*x as f64)).sum::<f64>().sqrt();
  if norm > 0.0 {
    for value in v {
      *value = (*value as f64 / norm) as f32;
    }
  }
}
