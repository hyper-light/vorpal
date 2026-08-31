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
//! Numerics (second pass — the measured optimization round): the hidden state is
//! f32 and the six GEMMs accumulate in EIGHT fixed f32 lanes reduced in a fixed
//! order (auto-vectorizes to NEON/AVX fma; ~an order of magnitude over the first
//! f64-scalar pass), while the sensitive reductions — LayerNorm moments, rotary
//! tables, attention dots, softmax, and the attention·V sums — keep f64
//! accumulation. Lane structure and reduction order are FIXED, and parallelism is
//! only across independent output rows, so outputs are bit-stable at any thread
//! count; the reference-parity oracle (≤ 1e-4 vs the f64 numpy forward)
//! re-arbitrates this numeric layout.

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

/// LayerNorm over each `dim`-row of `x`, in place: f64 moments (population
/// variance), f32 storage.
fn layer_norm(x: &mut [f32], dim: usize, weight: &[f32], bias: &[f32], eps: f64) {
  for row in x.chunks_exact_mut(dim) {
    let mut total = 0.0f64;
    for value in row.iter() {
      total += *value as f64;
    }
    let mean = total / dim as f64;
    let mut spread = 0.0f64;
    for value in row.iter() {
      let diff = *value as f64 - mean;
      spread += diff * diff;
    }
    let inv = 1.0 / (spread / dim as f64 + eps).sqrt();
    for (value, (w, b)) in row.iter_mut().zip(weight.iter().zip(bias)) {
      *value = (((*value as f64 - mean) * inv) * *w as f64 + *b as f64) as f32;
    }
  }
}

/// How many parallel f32 accumulator lanes each GEMM dot product uses — a FIXED
/// structural constant of the reduction order (part of the numeric layout the
/// parity oracle pins), sized to fill a 256-bit vector unit.
const GEMM_LANES: usize = 8;

/// `out[s][o] = Σ_d x[s][d] · w[o][d]` — w row-major `[rows_out][dim_in]`.
/// Eight fixed f32 lanes over ascending d, reduced pairwise in fixed order, scalar
/// tail; rows of `out` are independent (rayon-safe, bit-stable at any thread count).
fn gemm(x: &[f32], dim_in: usize, w: &[f32], rows_out: usize, out: &mut [f32]) {
  out
    .par_chunks_mut(rows_out)
    .enumerate()
    .for_each(|(s, out_row)| {
      let x_row = &x[s * dim_in..(s + 1) * dim_in];
      let blocks = dim_in / GEMM_LANES * GEMM_LANES;
      for (o, slot) in out_row.iter_mut().enumerate() {
        let w_row = &w[o * dim_in..(o + 1) * dim_in];
        let mut lanes = [0.0f32; GEMM_LANES];
        for (x_block, w_block) in x_row[..blocks]
          .chunks_exact(GEMM_LANES)
          .zip(w_row[..blocks].chunks_exact(GEMM_LANES))
        {
          for lane in 0..GEMM_LANES {
            lanes[lane] = x_block[lane].mul_add(w_block[lane], lanes[lane]);
          }
        }
        let mut total = ((lanes[0] + lanes[4]) + (lanes[1] + lanes[5]))
          + ((lanes[2] + lanes[6]) + (lanes[3] + lanes[7]));
        for (a, b) in x_row[blocks..].iter().zip(&w_row[blocks..]) {
          total = a.mul_add(*b, total);
        }
        *slot = total;
      }
    });
}

/// CLS embedding (pre-normalization) for one token sequence — the batch form with
/// one sequence; bitwise identical by construction (the batch oracle pins it).
pub fn forward(weights: &ModelWeights<'_>, ids: &[u32]) -> Result<Vec<f32>, String> {
  let mut batch = forward_batch(weights, &[ids])?;
  batch
    .pop()
    .ok_or_else(|| "encoder: batch of one produced no row (invariant)".to_string())
}

/// CLS embeddings (pre-normalization) for a BATCH of token sequences — the rerank
/// path's shape: all sequences concatenate into ONE token matrix so every GEMM and
/// LayerNorm runs at full width (rows are per-token and never mix), while attention
/// and rotary positions stay strictly per-sequence (block-diagonal by the offset
/// table). Per-row math is IDENTICAL to the single-sequence form — batched and
/// individual embeddings are bitwise equal, pinned by the gated batch oracle.
pub fn forward_batch(
  weights: &ModelWeights<'_>,
  sequences: &[&[u32]],
) -> Result<Vec<Vec<f32>>, String> {
  let (dim, heads) = (weights.dim, weights.heads);
  if dim == 0 || heads == 0 || dim % heads != 0 {
    return Err("encoder: degenerate architecture constants".to_string());
  }
  let head_dim = dim / heads;
  if head_dim % 2 != 0 {
    return Err("encoder: rotary needs an even head dim".to_string());
  }
  if sequences.is_empty() {
    return Ok(Vec::new());
  }
  let mut offsets = Vec::with_capacity(sequences.len() + 1);
  offsets.push(0usize);
  let mut longest = 0usize;
  for ids in sequences {
    if ids.is_empty() {
      return Err("encoder: empty token sequence".to_string());
    }
    for &id in *ids {
      if id as usize >= weights.vocab_rows {
        return Err(format!("encoder: token id {id} outside the embedding table"));
      }
    }
    longest = longest.max(ids.len());
    offsets.push(offsets[offsets.len() - 1] + ids.len());
  }
  let total = offsets[offsets.len() - 1];
  // Per global row: (sequence start, sequence end) — attention's block bounds.
  let mut bounds = vec![(0u32, 0u32); total];
  for window in offsets.windows(2) {
    let (start, end) = (window[0], window[1]);
    for bound in &mut bounds[start..end] {
      *bound = (start as u32, end as u32);
    }
  }

  // Embeddings + emb_ln (per token; sequence-independent).
  let mut x = vec![0.0f32; total * dim];
  for (sequence, ids) in sequences.iter().enumerate() {
    for (local, &id) in ids.iter().enumerate() {
      let global = offsets[sequence] + local;
      let word = &weights.word_embeddings[id as usize * dim..(id as usize + 1) * dim];
      let row = &mut x[global * dim..(global + 1) * dim];
      for ((slot, w), t) in row.iter_mut().zip(word).zip(weights.token_type_row0) {
        *slot = w + t;
      }
    }
  }
  layer_norm(&mut x, dim, weights.emb_ln_weight, weights.emb_ln_bias, weights.layer_norm_eps);

  // Rotary tables over LOCAL positions, sized by the longest sequence; every
  // sequence indexes them by its own position (identical to its solo run).
  let half = head_dim / 2;
  let mut cos = vec![0.0f64; longest * half];
  let mut sin = vec![0.0f64; longest * half];
  for position in 0..longest {
    for f in 0..half {
      let inv_freq = 1.0 / weights.rotary_base.powf((2 * f) as f64 / head_dim as f64);
      let angle = position as f64 * inv_freq;
      cos[position * half + f] = angle.cos();
      sin[position * half + f] = angle.sin();
    }
  }
  let rotate = |vectors: &mut [f32], bounds: &[(u32, u32)]| {
    // vectors: [total][heads][head_dim], rotate-half non-interleaved, f64 mid-math,
    // positions LOCAL to each sequence.
    for (global, &(start, _)) in bounds.iter().enumerate() {
      let position = global - start as usize;
      for h in 0..heads {
        let base = (global * heads + h) * head_dim;
        for f in 0..half {
          let (c, n) = (cos[position * half + f], sin[position * half + f]);
          let (a, b) = (vectors[base + f] as f64, vectors[base + half + f] as f64);
          vectors[base + f] = (a * c - b * n) as f32;
          vectors[base + half + f] = (b * c + a * n) as f32;
        }
      }
    }
  };

  let mut qkv = vec![0.0f32; total * 3 * dim];
  let mut attn_out = vec![0.0f32; total * dim];
  let mut context = vec![0.0f32; total * dim];
  let mut mlp_y = vec![0.0f32; total * weights.inner];
  let mut mlp_gate = vec![0.0f32; total * weights.inner];
  let mut mlp_out = vec![0.0f32; total * dim];
  let scale = 1.0 / (head_dim as f64).sqrt();

  for layer in &weights.layers {
    // qkv: [total][3*dim] rows (t-major, then head, then component); unpack into
    // contiguous q/k/v matrices [total][heads][head_dim] for the attention walk.
    gemm(&x, dim, layer.wqkv, 3 * dim, &mut qkv);
    let mut q = vec![0.0f32; total * dim];
    let mut k = vec![0.0f32; total * dim];
    let mut v = vec![0.0f32; total * dim];
    for s in 0..total {
      let row = &qkv[s * 3 * dim..(s + 1) * 3 * dim];
      q[s * dim..(s + 1) * dim].copy_from_slice(&row[0..dim]);
      k[s * dim..(s + 1) * dim].copy_from_slice(&row[dim..2 * dim]);
      v[s * dim..(s + 1) * dim].copy_from_slice(&row[2 * dim..3 * dim]);
    }
    rotate(&mut q, &bounds);
    rotate(&mut k, &bounds);
    // Attention per (query-row, head), keys bounded to the row's OWN sequence
    // (block-diagonal): independent outputs; f64 dots, stable softmax, f64 A·V
    // accumulation in ascending key order — per-row identical to the solo run.
    context
      .par_chunks_mut(dim)
      .enumerate()
      .for_each(|(s, out_row)| {
        let (start, end) = (bounds[s].0 as usize, bounds[s].1 as usize);
        let keys = end - start;
        let mut weights_buffer = vec![0.0f64; keys];
        let mut head_accumulator = vec![0.0f64; head_dim];
        for h in 0..heads {
          let q_row = &q[(s * heads + h) * head_dim..(s * heads + h + 1) * head_dim];
          let mut max_score = f64::NEG_INFINITY;
          for (local, slot) in weights_buffer.iter_mut().enumerate() {
            let t = start + local;
            let k_row = &k[(t * heads + h) * head_dim..(t * heads + h + 1) * head_dim];
            let mut dot = 0.0f64;
            for (a, b) in q_row.iter().zip(k_row) {
              dot += *a as f64 * *b as f64;
            }
            let score = dot * scale;
            *slot = score;
            if score > max_score {
              max_score = score;
            }
          }
          let mut total_weight = 0.0f64;
          for slot in weights_buffer.iter_mut() {
            *slot = (*slot - max_score).exp();
            total_weight += *slot;
          }
          head_accumulator.fill(0.0);
          for (local, weight) in weights_buffer.iter().enumerate() {
            let t = start + local;
            let value = weight / total_weight;
            let v_row = &v[(t * heads + h) * head_dim..(t * heads + h + 1) * head_dim];
            for (slot, component) in head_accumulator.iter_mut().zip(v_row) {
              *slot += value * *component as f64;
            }
          }
          let out_head = &mut out_row[h * head_dim..(h + 1) * head_dim];
          for (slot, value) in out_head.iter_mut().zip(&head_accumulator) {
            *slot = *value as f32;
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
      let g = *gate as f64;
      *y = (*y as f64 * (g / (1.0 + (-g).exp()))) as f32;
    }
    gemm(&mlp_y, weights.inner, layer.fc2, dim, &mut mlp_out);
    for (value, add) in x.iter_mut().zip(&mlp_out) {
      *value += add;
    }
    layer_norm(&mut x, dim, layer.norm2_weight, layer.norm2_bias, weights.layer_norm_eps);
  }
  Ok(
    offsets[..sequences.len()]
      .iter()
      .map(|&start| x[start * dim..(start + 1) * dim].to_vec())
      .collect(),
  )
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
