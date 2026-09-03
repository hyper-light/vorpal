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
//!
//! Third pass (doc-side dense channel, ENCODER_RESEARCH §6/§8.2): the SAME forward
//! runs under a selectable GEMM path ([`GemmPath`]). `FixedOrder` is the layout
//! above — the query-side law. `Throughput` swaps ONLY the six GEMMs for the
//! platform's sgemm (Apple Accelerate on macOS; elsewhere it IS the fixed lanes)
//! and leaves every other reduction byte-identical, so the two paths differ by the
//! GEMM's summation order alone; the gated parity oracle (`tests/encoder.rs`)
//! pins cosine ≥ 0.9999 between them on the goldens and the bench records the
//! measured rate. Every element-wise pass (LayerNorm rows, residual adds, the
//! SwiGLU gate, the qkv unpack, rotary) is row-parallel under BOTH paths — each
//! row's arithmetic is unchanged, so the fixed path stays bit-stable.

use rayon::prelude::*;

use super::gemm_wgpu::GpuGemm;

/// Which GEMM numerics a forward pass runs under (module doc, third pass).
#[derive(Clone, Copy, Debug)]
pub enum GemmPath<'a> {
  /// Eight fixed f32 lanes, fixed reduction order — bit-stable at any thread
  /// count. The query-side rerank's law.
  FixedOrder,
  /// The platform's fastest sgemm — admissible only where thread-count
  /// bit-stability is not a law (the stamp-gated doc-side sidecar), and only
  /// within the recorded parity bound against `FixedOrder`. Where no platform
  /// sgemm is linked this is `FixedOrder` under another name
  /// ([`GemmPath::throughput_is_native`] says which).
  Throughput,
  /// The six GEMMs on a `wgpu` compute device with the weights resident
  /// (`gemm_wgpu.rs`) — the doc-side ladder's top rung, same admissibility and
  /// parity bound as `Throughput`. A runtime fault degrades THAT call and every
  /// later one to `Throughput` with the reason recorded on the handle.
  Gpu(&'a GpuGemm),
}

impl PartialEq for GemmPath<'_> {
  fn eq(&self, other: &Self) -> bool {
    match (self, other) {
      (GemmPath::FixedOrder, GemmPath::FixedOrder) | (GemmPath::Throughput, GemmPath::Throughput) => true,
      (GemmPath::Gpu(a), GemmPath::Gpu(b)) => std::ptr::eq(*a, *b),
      _ => false,
    }
  }
}

impl GemmPath<'_> {
  /// Whether `Throughput` actually dispatches to a platform sgemm on this build
  /// (macOS: Accelerate `cblas_sgemm`), or falls back to the fixed lanes.
  pub fn throughput_is_native() -> bool {
    cfg!(target_os = "macos")
  }

  /// The path's provenance label — written into any sidecar built under it
  /// (the GPU rung's device-qualified label comes from [`GpuGemm::label`];
  /// this is its static family name).
  pub fn label(self) -> &'static str {
    match self {
      GemmPath::FixedOrder => "fixed-order",
      GemmPath::Throughput if Self::throughput_is_native() => "accelerate-sgemm",
      GemmPath::Throughput => "fixed-order",
      GemmPath::Gpu(_) => "wgpu-compute",
    }
  }
}

/// Wall-clock stage attribution for one forward — printed to stderr when
/// `VORPAL_ENCODER_TRACE` is set (read once). Measurement aid only: it decides
/// nothing and costs six `Instant::now()` calls per layer.
#[derive(Default)]
struct StageClock {
  enabled: bool,
  gemm: f64,
  attention: f64,
  gate: f64,
  norm: f64,
  other: f64,
}

impl StageClock {
  fn new() -> StageClock {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    StageClock {
      enabled: *ENABLED.get_or_init(|| std::env::var_os("VORPAL_ENCODER_TRACE").is_some()),
      ..StageClock::default()
    }
  }

  fn time<T>(&mut self, slot: fn(&mut StageClock) -> &mut f64, work: impl FnOnce() -> T) -> T {
    if !self.enabled {
      return work();
    }
    let started = std::time::Instant::now();
    let out = work();
    *slot(self) += started.elapsed().as_secs_f64();
    out
  }

  fn report(&self, path: GemmPath<'_>, tokens: usize) {
    if self.enabled {
      let total = self.gemm + self.attention + self.gate + self.norm + self.other;
      eprintln!(
        "encoder trace ({}, {tokens} tokens): gemm {:.3}s attention {:.3}s gate {:.3}s norm {:.3}s other {:.3}s = {:.3}s",
        path.label(),
        self.gemm,
        self.attention,
        self.gate,
        self.norm,
        self.other,
        total
      );
    }
  }
}

/// A fast, deterministic f32 `exp` for the throughput path's SwiGLU gate — the
/// Cephes `expf` polynomial (range-reduced by `ln 2`, degree-5 remainder, the
/// exponent rebuilt from bits; relative error ≈ 2 × 10⁻⁷, below one f32 ulp at
/// the gate's scale), written so the compiler vectorizes it across a row.
/// Measured motivation: the f64 libm `exp` over tokens × 3072 × 12 layers was
/// the largest single frame in the fill's profile. The fixed-order path keeps
/// the f64 gate — its bits are the query-side law.
#[inline]
// The Cephes coefficients are quoted verbatim (their rounding IS the recorded
// polynomial); clippy's nearest-constant / precision hints would change bits.
#[allow(clippy::excessive_precision, clippy::approx_constant)]
fn exp_fast(x: f32) -> f32 {
  // ln 2 split into an f32-exact high part (355/512) and a low remainder, the
  // Cephes reduction; the f32 literals below are the f32 roundings of Cephes'
  // decimal coefficients (the last is exactly 0.5 in f32).
  const LN2_HI: f32 = 0.693_359_4;
  const LN2_LO: f32 = -2.121_944_4e-4;
  let x = x.clamp(-87.0, 88.0);
  let n = (x * std::f32::consts::LOG2_E).round();
  let r = x - n * LN2_HI - n * LN2_LO;
  let p = ((((1.987_569_2e-4 * r + 1.398_199_9e-3) * r + 8.333_452e-3) * r + 4.166_579_6e-2) * r
    + 1.666_666_5e-1)
    * r
    + 0.5;
  let p = p * r * r + r + 1.0;
  let scale = f32::from_bits(((n as i32 + 127) as u32) << 23);
  p * scale
}

/// Apple Accelerate's row-major `cblas_sgemm` (the framework ships with every
/// macOS SDK; AMX-backed on Apple silicon). Linked only on macOS — nothing else
/// in the crate references the framework.
#[cfg(target_os = "macos")]
mod accelerate {
  pub const CBLAS_ROW_MAJOR: i32 = 101;
  pub const CBLAS_NO_TRANS: i32 = 111;
  pub const CBLAS_TRANS: i32 = 112;

  #[link(name = "Accelerate", kind = "framework")]
  unsafe extern "C" {
    #[allow(clippy::too_many_arguments)]
    pub fn cblas_sgemm(
      order: i32,
      trans_a: i32,
      trans_b: i32,
      m: i32,
      n: i32,
      k: i32,
      alpha: f32,
      a: *const f32,
      lda: i32,
      b: *const f32,
      ldb: i32,
      beta: f32,
      c: *mut f32,
      ldc: i32,
    );
  }
}

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
/// variance), f32 storage. Rows are independent — row-parallel, per-row order
/// unchanged (bit-identical to the serial loop).
fn layer_norm(x: &mut [f32], dim: usize, weight: &[f32], bias: &[f32], eps: f64) {
  x.par_chunks_exact_mut(dim).for_each(|row| {
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
  });
}

/// How many parallel f32 accumulator lanes each GEMM dot product uses — a FIXED
/// structural constant of the reduction order (part of the numeric layout the
/// parity oracle pins), sized to fill a 256-bit vector unit.
const GEMM_LANES: usize = 8;

/// `out[s][o] = Σ_d x[s][d] · w[o][d]` — w row-major `[rows_out][dim_in]` — under
/// the selected path. Shapes are validated once here (the platform sgemm takes
/// raw pointers and 32-bit extents): a mismatch is a typed error, never a
/// silent out-of-bounds read.
fn gemm(
  path: GemmPath<'_>,
  x: &[f32],
  dim_in: usize,
  w: &[f32],
  rows_out: usize,
  out: &mut [f32],
) -> Result<(), String> {
  if dim_in == 0 || rows_out == 0 {
    return Err("encoder: zero-extent GEMM".to_string());
  }
  let rows = out.len() / rows_out;
  if out.len() != rows * rows_out || x.len() < rows * dim_in || w.len() < rows_out * dim_in {
    return Err("encoder: GEMM operand shapes disagree".to_string());
  }
  match path {
    GemmPath::FixedOrder => gemm_fixed_order(x, dim_in, w, rows_out, out),
    GemmPath::Throughput => gemm_throughput(x, dim_in, w, rows_out, rows, out)?,
    // The ladder's runtime step: a GPU fault retires the rung (stated reason on
    // the handle, surfaced by the sidecar record) and this GEMM — plus every
    // later one — runs on the next rung. The fill never fails because of the GPU.
    GemmPath::Gpu(gpu) => {
      if let Err(reason) = gpu.gemm(x, dim_in, w, rows_out, rows, out) {
        gpu.retire(reason);
        gemm_throughput(x, dim_in, w, rows_out, rows, out)?;
      }
    }
  }
  Ok(())
}

/// How many row-shards each throughput GEMM splits into, each shard an
/// independent `cblas_sgemm` call on a rayon worker (Accelerate is thread-safe;
/// output rows are disjoint). 0 = derive at first use from
/// `available_parallelism()` (the shard sweep in `examples/sweep_encoder.rs` sets
/// it explicitly; its recorded result pins the derivation).
static THROUGHPUT_SHARDS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Set the throughput GEMM's shard count (bench seam; 0 restores the derived
/// default). Row shards never change a result: every output row's reduction runs
/// inside one `cblas_sgemm` call whatever the shard boundaries are.
pub fn set_throughput_shards(shards: usize) {
  THROUGHPUT_SHARDS.store(shards, std::sync::atomic::Ordering::Relaxed);
}

/// The effective shard count: the explicit setting, else the machine's parallelism.
pub fn throughput_shards() -> usize {
  match THROUGHPUT_SHARDS.load(std::sync::atomic::Ordering::Relaxed) {
    0 => std::thread::available_parallelism().map_or(1, |n| n.get()),
    n => n,
  }
}

/// The `Throughput` dispatch: Accelerate on macOS (row-major `C[rows×rows_out] =
/// X[rows×dim_in] · Wᵀ`, W stored `[rows_out][dim_in]`), the fixed lanes elsewhere.
/// The row range is split into [`throughput_shards`] contiguous shards, each an
/// independent `cblas_sgemm` on its own rayon task — the measured lever when the
/// framework's own threading leaves cores idle (the shard sweep records whether
/// the AMX units or the thread count is the ceiling on this machine).
#[cfg(target_os = "macos")]
fn gemm_throughput(
  x: &[f32],
  dim_in: usize,
  w: &[f32],
  rows_out: usize,
  rows: usize,
  out: &mut [f32],
) -> Result<(), String> {
  let extent = |n: usize| -> Result<i32, String> {
    i32::try_from(n).map_err(|_| "encoder: GEMM extent exceeds the BLAS interface".to_string())
  };
  let (n, k) = (extent(rows_out)?, extent(dim_in)?);
  let shards = throughput_shards().clamp(1, rows.max(1));
  let shard_rows = rows.div_ceil(shards);
  let failures: usize = out
    .par_chunks_mut(shard_rows * rows_out)
    .enumerate()
    .map(|(shard, out_shard)| {
      let first = shard * shard_rows;
      let count = out_shard.len() / rows_out;
      let Ok(m) = extent(count) else {
        return 1usize;
      };
      let x_shard = &x[first * dim_in..(first + count) * dim_in];
      // SAFETY: extents were validated against the slice lengths by `gemm` (x holds
      // rows×dim_in, w holds rows_out×dim_in, out holds rows×rows_out, all
      // row-major with leading dimensions dim_in / dim_in / rows_out); this shard's
      // x rows and out rows are disjoint sub-slices that outlive the call, and
      // `out_shard` is exclusively borrowed — the framework writes only inside it.
      unsafe {
        accelerate::cblas_sgemm(
          accelerate::CBLAS_ROW_MAJOR,
          accelerate::CBLAS_NO_TRANS,
          accelerate::CBLAS_TRANS,
          m,
          n,
          k,
          1.0,
          x_shard.as_ptr(),
          k,
          w.as_ptr(),
          k,
          0.0,
          out_shard.as_mut_ptr(),
          n,
        );
      }
      0usize
    })
    .sum();
  if failures > 0 {
    return Err("encoder: GEMM shard extent exceeds the BLAS interface".to_string());
  }
  Ok(())
}

#[cfg(not(target_os = "macos"))]
fn gemm_throughput(
  x: &[f32],
  dim_in: usize,
  w: &[f32],
  rows_out: usize,
  _rows: usize,
  out: &mut [f32],
) -> Result<(), String> {
  gemm_fixed_order(x, dim_in, w, rows_out, out);
  Ok(())
}

/// Eight fixed f32 lanes over ascending d, reduced pairwise in fixed order, scalar
/// tail; rows of `out` are independent (rayon-safe, bit-stable at any thread count).
fn gemm_fixed_order(x: &[f32], dim_in: usize, w: &[f32], rows_out: usize, out: &mut [f32]) {
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
  forward_batch_with(weights, sequences, GemmPath::FixedOrder)
}

/// [`forward_batch`] under a selected GEMM path — the doc-side sidecar's entry
/// (`Throughput`); everything outside the six GEMMs is byte-identical across paths.
pub fn forward_batch_with(
  weights: &ModelWeights<'_>,
  sequences: &[&[u32]],
  path: GemmPath<'_>,
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
    // positions LOCAL to each sequence. Row-parallel: each token row's math is
    // independent and unchanged.
    vectors
      .par_chunks_exact_mut(dim)
      .zip(bounds.par_iter())
      .enumerate()
      .for_each(|(global, (row, &(start, _)))| {
        let position = global - start as usize;
        for h in 0..heads {
          let base = h * head_dim;
          for f in 0..half {
            let (c, n) = (cos[position * half + f], sin[position * half + f]);
            let (a, b) = (row[base + f] as f64, row[base + half + f] as f64);
            row[base + f] = (a * c - b * n) as f32;
            row[base + half + f] = (b * c + a * n) as f32;
          }
        }
      });
  };
  // Element-wise passes are row-parallel too — per element the arithmetic is the
  // serial loop's, so the fixed path stays bit-stable and the throughput path
  // stops serializing on O(total × inner) work between its GEMMs.
  let residual_add = |x: &mut [f32], add: &[f32]| {
    x.par_chunks_exact_mut(dim)
      .zip(add.par_chunks_exact(dim))
      .for_each(|(row, add_row)| {
        for (value, add) in row.iter_mut().zip(add_row) {
          *value += add;
        }
      });
  };

  let mut qkv = vec![0.0f32; total * 3 * dim];
  let mut q = vec![0.0f32; total * dim];
  let mut k = vec![0.0f32; total * dim];
  let mut v = vec![0.0f32; total * dim];
  let mut attn_out = vec![0.0f32; total * dim];
  let mut context = vec![0.0f32; total * dim];
  let mut mlp_y = vec![0.0f32; total * weights.inner];
  let mut mlp_gate = vec![0.0f32; total * weights.inner];
  let mut mlp_out = vec![0.0f32; total * dim];
  let scale = 1.0 / (head_dim as f64).sqrt();
  let mut clock = StageClock::new();

  for layer in &weights.layers {
    // qkv: [total][3*dim] rows (t-major, then head, then component); unpack into
    // contiguous q/k/v matrices [total][heads][head_dim] for the attention walk.
    clock.time(|c| &mut c.gemm, || gemm(path, &x, dim, layer.wqkv, 3 * dim, &mut qkv))?;
    clock.time(
      |c| &mut c.other,
      || {
        qkv
          .par_chunks_exact(3 * dim)
          .zip(q.par_chunks_exact_mut(dim))
          .zip(k.par_chunks_exact_mut(dim))
          .zip(v.par_chunks_exact_mut(dim))
          .for_each(|(((row, q_row), k_row), v_row)| {
            q_row.copy_from_slice(&row[0..dim]);
            k_row.copy_from_slice(&row[dim..2 * dim]);
            v_row.copy_from_slice(&row[2 * dim..3 * dim]);
          });
        rotate(&mut q, &bounds);
        rotate(&mut k, &bounds);
      },
    );
    // Attention per (query-row, head), keys bounded to the row's OWN sequence
    // (block-diagonal): independent outputs; f64 dots, stable softmax, f64 A·V
    // accumulation in ascending key order — per-row identical to the solo run.
    clock.time(|c| &mut c.attention, || context
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
      }));
    clock.time(|c| &mut c.gemm, || gemm(path, &context, dim, layer.out_proj, dim, &mut attn_out))?;
    clock.time(|c| &mut c.other, || residual_add(&mut x, &attn_out));
    clock.time(|c| &mut c.norm, || {
      layer_norm(&mut x, dim, layer.norm1_weight, layer.norm1_bias, weights.layer_norm_eps)
    });
    // The GPU rung runs the whole MLP block fused on the device (fc11, fc12,
    // gate, fc2 — the intermediates never cross the host boundary); a fault
    // retires the rung and the block falls through to the per-GEMM form below,
    // whose GEMMs then take the next rung.
    let fused = clock.time(|c| &mut c.gemm, || match path {
      GemmPath::Gpu(gpu) => gpu
        .mlp(&x, dim, layer.fc11, layer.fc12, layer.fc2, weights.inner, total, &mut mlp_out)
        .map_err(|reason| gpu.retire(reason))
        .is_ok(),
      GemmPath::FixedOrder | GemmPath::Throughput => false,
    });
    if !fused {
      clock.time(|c| &mut c.gemm, || gemm(path, &x, dim, layer.fc11, weights.inner, &mut mlp_y))?;
      clock.time(|c| &mut c.gemm, || gemm(path, &x, dim, layer.fc12, weights.inner, &mut mlp_gate))?;
      // SwiGLU gate: f64 libm on the fixed-order path (the query-side law); the
      // vectorizable f32 `exp_fast` on the throughput path (parity-oracle bound).
      clock.time(|c| &mut c.gate, || {
        mlp_y
          .par_chunks_exact_mut(weights.inner)
          .zip(mlp_gate.par_chunks_exact(weights.inner))
          .for_each(|(y_row, gate_row)| match path {
            GemmPath::FixedOrder => {
              for (y, gate) in y_row.iter_mut().zip(gate_row) {
                let g = *gate as f64;
                *y = (*y as f64 * (g / (1.0 + (-g).exp()))) as f32;
              }
            }
            GemmPath::Throughput | GemmPath::Gpu(_) => {
              for (y, gate) in y_row.iter_mut().zip(gate_row) {
                let g = *gate;
                *y *= g / (1.0 + exp_fast(-g));
              }
            }
          })
      });
      clock.time(|c| &mut c.gemm, || gemm(path, &mlp_y, weights.inner, layer.fc2, dim, &mut mlp_out))?;
    }
    clock.time(|c| &mut c.other, || residual_add(&mut x, &mlp_out));
    clock.time(|c| &mut c.norm, || {
      layer_norm(&mut x, dim, layer.norm2_weight, layer.norm2_bias, weights.layer_norm_eps)
    });
  }
  clock.report(path, total);
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
