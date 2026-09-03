//! int8 GEMM for the doc-side path ([`super::GemmPath::Int8`]): per-output-row
//! max-abs int8 weights (the ANN v5 i8+rescore precedent — `dense::quantize_row`
//! applied to weight rows at first use), per-row int8 activations quantized at
//! each GEMM, exact int8×int8→i32 dots, one f64 rescale per output element:
//!
//! `out[s][o] = (Σ_d qx[s][d]·qw[o][d]) · sx[s] · sw[o]`
//!
//! Kernels, runtime-detected once: NEON `sdot` (aarch64 `dotprod`, via stable
//! inline asm — the `vdotq_s32` intrinsic is still unstable on the pinned
//! toolchain), AVX-512-VNNI `vpdpbusd` (64 B/step), AVX-VNNI `vpdpbusd` (32 B),
//! AVX2 `pmaddwd` over sign-extended i16 (16 B, exact — `pmaddubsw` was rejected:
//! it saturates at 255·127·2 > i16), and a portable i32 loop. The VNNI forms
//! multiply UNSIGNED bytes by signed, so the activation codes are shifted to u8
//! by flipping the sign bit (`q ^ 0x80 = q + 128`) and the driver subtracts
//! `128 · Σ_d qw[o][d]` (the per-row sum recorded at quantization) — exact in i32
//! (≤ 255·127·3072 ≈ 10⁸ per element).
//!
//! DETERMINISM: every kernel computes the SAME integer — an exact i32 sum — so
//! the int8 path is bit-identical across ISAs, tiles, shards and thread counts
//! (the unit tests assert every present kernel against the portable loop); the
//! only rounding is the per-row f32 quantization (a fixed formula) and the final
//! f64 rescale. Its DISTANCE from the f32 forward is the retention question the
//! gated test measures against the derived bar (`tests/encoder.rs`).
//!
//! Tiling mirrors `gemm_x86`: `MR × NR` i32 vector accumulators per tile, w
//! panels sized to half the L2 (`cache::l2_cache_bytes`), rows sharded across
//! the rayon pool by [`super::forward::throughput_shards`]. Every `unsafe`
//! block states its ISA invariant (verified by the cached detection) and its
//! bounds invariant (the driver hands each tile whole rows).

use rayon::prelude::*;

/// A `[rows][cols]` f32 matrix quantized per row to int8 (module doc).
pub struct QuantizedMatrix {
  codes: Vec<i8>,
  /// Per row: `max|w| / 127` (0 for an all-zero row, whose codes are 0).
  scales: Vec<f32>,
  /// Per row: `Σ codes` — the u8-shift correction the VNNI kernels need.
  sums: Vec<i32>,
  rows: usize,
  cols: usize,
}

impl QuantizedMatrix {
  /// Quantize `w` (row-major `rows × cols`) — rows in parallel, each row's
  /// arithmetic independent (exact, order-free: max and a per-element round).
  pub fn quantize(w: &[f32], rows: usize, cols: usize) -> Result<QuantizedMatrix, String> {
    if rows == 0 || cols == 0 || w.len() != rows * cols {
      return Err("encoder: int8 quantization shape disagrees with the matrix".to_string());
    }
    let mut codes = vec![0i8; rows * cols];
    let mut scales = vec![0.0f32; rows];
    let mut sums = vec![0i32; rows];
    codes
      .par_chunks_exact_mut(cols)
      .zip(scales.par_iter_mut())
      .zip(sums.par_iter_mut())
      .zip(w.par_chunks_exact(cols))
      .for_each(|(((codes, scale), sum), row)| {
        *scale = quantize_row(row, codes);
        *sum = codes.iter().map(|&c| c as i32).sum();
      });
    Ok(QuantizedMatrix { codes, scales, sums, rows, cols })
  }

  pub fn rows(&self) -> usize {
    self.rows
  }

  pub fn cols(&self) -> usize {
    self.cols
  }

  /// Bytes held (codes + scales + sums) — the memory the int8 weights cost.
  pub fn bytes(&self) -> usize {
    self.codes.len() + self.scales.len() * 4 + self.sums.len() * 4
  }
}

/// One layer's five projections in int8 — built by the encoder handle at the
/// first `Int8` embed (never at open: the query-side rerank pays nothing).
pub struct Int8Layer {
  pub wqkv: QuantizedMatrix,
  pub out_proj: QuantizedMatrix,
  pub fc11: QuantizedMatrix,
  pub fc12: QuantizedMatrix,
  pub fc2: QuantizedMatrix,
}

impl Int8Layer {
  pub fn bytes(&self) -> usize {
    self.wqkv.bytes() + self.out_proj.bytes() + self.fc11.bytes() + self.fc12.bytes() + self.fc2.bytes()
  }
}

/// Symmetric per-row int8 codes: `scale = max|x| / 127`, round-to-nearest of
/// `x / scale` clamped to ±127 — the sidecar's own quantizer, restated here so
/// the weight and activation codes share one formula.
fn quantize_row(row: &[f32], codes: &mut [i8]) -> f32 {
  let peak = row.iter().fold(0.0f32, |m, v| m.max(v.abs()));
  if peak == 0.0 || !peak.is_finite() {
    codes.fill(0);
    return 0.0;
  }
  let scale = peak / 127.0;
  for (code, value) in codes.iter_mut().zip(row) {
    *code = (value / scale).round().clamp(-127.0, 127.0) as i8;
  }
  scale
}

/// The int8 dot kernel this CPU runs — detected once.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Int8Isa {
  /// aarch64 `sdot` (ARMv8.2 dotprod; every Apple silicon core).
  NeonDotprod,
  /// `vpdpbusd` on 512-bit lanes.
  Avx512Vnni,
  /// `vpdpbusd` on 256-bit lanes (Alder Lake+, Zen 4+ without the 512 form).
  AvxVnni,
  /// `pmaddwd` over sign-extended i16 on 256-bit lanes.
  Avx2Madd,
  /// Scalar i32 (auto-vectorizable) — any ISA.
  Portable,
}

impl Int8Isa {
  /// The provenance label a sidecar built under this kernel records.
  pub fn label(self) -> &'static str {
    match self {
      Int8Isa::NeonDotprod => "int8-neon-sdot",
      Int8Isa::Avx512Vnni => "int8-avx512-vnni",
      Int8Isa::AvxVnni => "int8-avx-vnni",
      Int8Isa::Avx2Madd => "int8-avx2-madd",
      Int8Isa::Portable => "int8-portable",
    }
  }
}

/// The best int8 kernel this CPU offers — a process constant.
pub fn detect() -> Int8Isa {
  static ISA: std::sync::OnceLock<Int8Isa> = std::sync::OnceLock::new();
  *ISA.get_or_init(|| {
    #[cfg(target_arch = "aarch64")]
    {
      if std::arch::is_aarch64_feature_detected!("dotprod") {
        return Int8Isa::NeonDotprod;
      }
    }
    #[cfg(target_arch = "x86_64")]
    {
      if is_x86_feature_detected!("avx512f")
        && is_x86_feature_detected!("avx512bw")
        && is_x86_feature_detected!("avx512vnni")
      {
        return Int8Isa::Avx512Vnni;
      }
      if is_x86_feature_detected!("avx2") && is_x86_feature_detected!("avxvnni") {
        return Int8Isa::AvxVnni;
      }
      if is_x86_feature_detected!("avx2") {
        return Int8Isa::Avx2Madd;
      }
    }
    Int8Isa::Portable
  })
}

/// `(x codes, w codes, dim_in, out, ldc)`: `MR` x rows and `NR` w rows of
/// `dim_in` bytes; `out[i * ldc + j]` receives the raw i32 dot (shifted by
/// `128·Σw` when the kernel is a u8×s8 form — `Tiles::shifted`).
type TileFn = unsafe fn(&[i8], &[i8], usize, &mut [i32], usize);

struct Tiles {
  mr: usize,
  nr: usize,
  shifted: bool,
  full: TileFn,
  row_tail: TileFn,
  col_tail: TileFn,
  corner: TileFn,
}

const PORTABLE_TILES: Tiles = Tiles {
  mr: 1,
  nr: 1,
  shifted: false,
  full: tile_portable,
  row_tail: tile_portable,
  col_tail: tile_portable,
  corner: tile_portable,
};

#[cfg(target_arch = "aarch64")]
const NEON_TILES: Tiles = Tiles {
  mr: 4,
  nr: 4,
  shifted: false,
  full: neon::tile::<4, 4>,
  row_tail: neon::tile::<1, 4>,
  col_tail: neon::tile::<4, 1>,
  corner: neon::tile::<1, 1>,
};

#[cfg(target_arch = "x86_64")]
const AVX512_VNNI_TILES: Tiles = Tiles {
  mr: 4,
  nr: 4,
  shifted: true,
  full: x86::tile_avx512_vnni::<4, 4>,
  row_tail: x86::tile_avx512_vnni::<1, 4>,
  col_tail: x86::tile_avx512_vnni::<4, 1>,
  corner: x86::tile_avx512_vnni::<1, 1>,
};

#[cfg(target_arch = "x86_64")]
const AVX_VNNI_TILES: Tiles = Tiles {
  mr: 2,
  nr: 4,
  shifted: true,
  full: x86::tile_avx_vnni::<2, 4>,
  row_tail: x86::tile_avx_vnni::<1, 4>,
  col_tail: x86::tile_avx_vnni::<2, 1>,
  corner: x86::tile_avx_vnni::<1, 1>,
};

#[cfg(target_arch = "x86_64")]
const AVX2_MADD_TILES: Tiles = Tiles {
  mr: 2,
  nr: 4,
  shifted: false,
  full: x86::tile_avx2_madd::<2, 4>,
  row_tail: x86::tile_avx2_madd::<1, 4>,
  col_tail: x86::tile_avx2_madd::<2, 1>,
  corner: x86::tile_avx2_madd::<1, 1>,
};

fn tiles_for(isa: Int8Isa) -> &'static Tiles {
  match isa {
    #[cfg(target_arch = "aarch64")]
    Int8Isa::NeonDotprod => &NEON_TILES,
    #[cfg(target_arch = "x86_64")]
    Int8Isa::Avx512Vnni => &AVX512_VNNI_TILES,
    #[cfg(target_arch = "x86_64")]
    Int8Isa::AvxVnni => &AVX_VNNI_TILES,
    #[cfg(target_arch = "x86_64")]
    Int8Isa::Avx2Madd => &AVX2_MADD_TILES,
    _ => &PORTABLE_TILES,
  }
}

/// How many w rows one L2 panel holds (half the L2 in whole `nr` tiles; one
/// tile when no L2 is enumerated — `cache` module).
fn panel_rows(dim_in: usize, nr: usize) -> usize {
  let tiles = super::cache::l2_cache_bytes()
    .map_or(1, |l2| (l2 / 2) / (dim_in * nr).max(1))
    .max(1);
  tiles * nr
}

/// `out[rows × w.rows] = X[rows × w.cols] · Wᵀ` under the detected kernel;
/// `x` is f32 and quantized per row here (module doc). Shapes are validated —
/// a mismatch is a typed error.
pub fn gemm_i8(x: &[f32], w: &QuantizedMatrix, out: &mut [f32]) -> Result<(), String> {
  gemm_i8_under(detect(), x, w, out)
}

/// [`gemm_i8`] under an explicit kernel — the cross-kernel oracle's seam (a
/// kernel the CPU lacks is a typed error, never a fault).
pub fn gemm_i8_under(
  isa: Int8Isa,
  x: &[f32],
  w: &QuantizedMatrix,
  out: &mut [f32],
) -> Result<(), String> {
  let (dim_in, rows_out) = (w.cols, w.rows);
  let rows = out.len() / rows_out;
  if out.len() != rows * rows_out || x.len() < rows * dim_in {
    return Err("encoder: int8 GEMM operand shapes disagree".to_string());
  }
  if isa != Int8Isa::Portable && isa != detect() && !kernel_present(isa) {
    return Err(format!("encoder: int8 kernel {} is not present on this CPU", isa.label()));
  }
  if rows == 0 {
    return Ok(());
  }
  // Activations: per-row codes + scales, rows in parallel.
  let mut xq = vec![0i8; rows * dim_in];
  let mut xs = vec![0.0f32; rows];
  xq
    .par_chunks_exact_mut(dim_in)
    .zip(xs.par_iter_mut())
    .zip(x[..rows * dim_in].par_chunks_exact(dim_in))
    .for_each(|((codes, scale), row)| *scale = quantize_row(row, codes));
  let tiles = tiles_for(isa);
  let shards = super::forward::throughput_shards().clamp(1, rows);
  let shard_rows = rows.div_ceil(shards);
  let panel = panel_rows(dim_in, tiles.nr);
  out
    .par_chunks_mut(shard_rows * rows_out)
    .enumerate()
    .for_each(|(shard, out_shard)| {
      let first = shard * shard_rows;
      let count = out_shard.len() / rows_out;
      let x_shard = &xq[first * dim_in..(first + count) * dim_in];
      let mut acc = vec![0i32; count * rows_out];
      shard_driver(tiles, x_shard, dim_in, &w.codes, rows_out, panel, &mut acc);
      for (s, (acc_row, out_row)) in acc.chunks_exact(rows_out).zip(out_shard.chunks_exact_mut(rows_out)).enumerate() {
        let sx = xs[first + s] as f64;
        for (o, (&raw, slot)) in acc_row.iter().zip(out_row.iter_mut()).enumerate() {
          let dot = if tiles.shifted { raw - 128 * w.sums[o] } else { raw };
          *slot = (dot as f64 * sx * w.scales[o] as f64) as f32;
        }
      }
    });
  Ok(())
}

/// Whether `isa`'s instructions exist on this CPU (the explicit-kernel seam).
fn kernel_present(isa: Int8Isa) -> bool {
  match isa {
    Int8Isa::Portable => true,
    #[cfg(target_arch = "aarch64")]
    Int8Isa::NeonDotprod => std::arch::is_aarch64_feature_detected!("dotprod"),
    #[cfg(target_arch = "x86_64")]
    Int8Isa::Avx512Vnni => {
      is_x86_feature_detected!("avx512f")
        && is_x86_feature_detected!("avx512bw")
        && is_x86_feature_detected!("avx512vnni")
    }
    #[cfg(target_arch = "x86_64")]
    Int8Isa::AvxVnni => is_x86_feature_detected!("avx2") && is_x86_feature_detected!("avxvnni"),
    #[cfg(target_arch = "x86_64")]
    Int8Isa::Avx2Madd => is_x86_feature_detected!("avx2"),
    #[allow(unreachable_patterns)]
    _ => false,
  }
}

/// Every kernel this CPU can run — the cross-kernel oracle iterates it.
pub fn present_kernels() -> Vec<Int8Isa> {
  [
    Int8Isa::NeonDotprod,
    Int8Isa::Avx512Vnni,
    Int8Isa::AvxVnni,
    Int8Isa::Avx2Madd,
    Int8Isa::Portable,
  ]
  .into_iter()
  .filter(|&isa| kernel_present(isa))
  .collect()
}

/// One shard (w panels outermost, x tiles within, w tiles innermost) — the
/// `gemm_x86` driver over byte rows and i32 outputs.
fn shard_driver(
  tiles: &Tiles,
  x: &[i8],
  dim_in: usize,
  w: &[i8],
  rows_out: usize,
  panel: usize,
  out: &mut [i32],
) {
  let rows = out.len() / rows_out;
  let (mr, nr) = (tiles.mr, tiles.nr);
  let mut col0 = 0usize;
  while col0 < rows_out {
    let cols = panel.min(rows_out - col0);
    let mut row0 = 0usize;
    while row0 < rows {
      let tile_rows = mr.min(rows - row0);
      let x_tile = &x[row0 * dim_in..(row0 + tile_rows) * dim_in];
      let out_tile = &mut out[row0 * rows_out..(row0 + tile_rows) * rows_out];
      let mut col = col0;
      while col < col0 + cols {
        let tile_cols = nr.min(col0 + cols - col);
        let w_tile = &w[col * dim_in..(col + tile_cols) * dim_in];
        let kernel = match (tile_rows == mr, tile_cols == nr) {
          (true, true) => Some(tiles.full),
          (false, true) if tile_rows == 1 => Some(tiles.row_tail),
          (true, false) if tile_cols == 1 => Some(tiles.col_tail),
          (false, false) if tile_rows == 1 && tile_cols == 1 => Some(tiles.corner),
          _ => None,
        };
        match kernel {
          // SAFETY (ISA): `tiles` was chosen for a kernel `kernel_present`
          // verified. SAFETY (bounds): `x_tile` holds `tile_rows` whole rows,
          // `w_tile` `tile_cols` whole rows, and `out_tile` `tile_rows` rows of
          // `rows_out` with `col + tile_cols ≤ rows_out`.
          Some(kernel) => unsafe { kernel(x_tile, w_tile, dim_in, &mut out_tile[col..], rows_out) },
          None => {
            for i in 0..tile_rows {
              let x_row = &x_tile[i * dim_in..(i + 1) * dim_in];
              let out_row = &mut out_tile[i * rows_out + col..i * rows_out + col + tile_cols];
              if tile_cols == nr {
                // SAFETY: as above — one x row, `nr` w rows, `nr` slots.
                unsafe { (tiles.row_tail)(x_row, w_tile, dim_in, out_row, rows_out) }
              } else {
                for j in 0..tile_cols {
                  let w_row = &w_tile[j * dim_in..(j + 1) * dim_in];
                  // SAFETY: as above — one x row, one w row, one slot.
                  unsafe { (tiles.corner)(x_row, w_row, dim_in, &mut out_row[j..], rows_out) }
                }
              }
            }
          }
        }
        col += tile_cols;
      }
      row0 += tile_rows;
    }
    col0 += cols;
  }
}

/// The portable kernel: one row × one column, four independent i32 lanes so
/// the compiler can vectorize (exact — integer addition is associative).
unsafe fn tile_portable(x: &[i8], w: &[i8], dim_in: usize, out: &mut [i32], _ldc: usize) {
  let (x, w) = (&x[..dim_in], &w[..dim_in]);
  let mut lanes = [0i32; 4];
  for (xs, ws) in x.chunks_exact(4).zip(w.chunks_exact(4)) {
    for lane in 0..4 {
      lanes[lane] += xs[lane] as i32 * ws[lane] as i32;
    }
  }
  let tail = dim_in / 4 * 4;
  let mut total = lanes.iter().sum::<i32>();
  for (a, b) in x[tail..].iter().zip(&w[tail..]) {
    total += *a as i32 * *b as i32;
  }
  out[0] = total;
}

#[cfg(target_arch = "aarch64")]
mod neon {
  use std::arch::aarch64::*;

  /// `sdot` micro-tile: `MR × NR` 4-lane i32 accumulators, 16 bytes per step.
  /// Contract as [`super::TileFn`]; `dotprod` verified by the caller.
  #[target_feature(enable = "neon,dotprod")]
  pub(super) unsafe fn tile<const MR: usize, const NR: usize>(
    x: &[i8],
    w: &[i8],
    dim_in: usize,
    out: &mut [i32],
    ldc: usize,
  ) {
    debug_assert!(x.len() >= MR * dim_in && w.len() >= NR * dim_in);
    debug_assert!(out.len() >= (MR - 1) * ldc + NR);
    const STEP: usize = 16;
    let blocks = dim_in / STEP * STEP;
    let mut acc = [[vdupq_n_s32(0); NR]; MR];
    let mut k = 0usize;
    while k < blocks {
      // SAFETY: k + STEP ≤ blocks ≤ dim_in and every row offset lies inside the
      // slice (the contract).
      let xv: [int8x16_t; MR] = std::array::from_fn(|i| unsafe { vld1q_s8(x.as_ptr().add(i * dim_in + k)) });
      // SAFETY: as above, for w's NR rows.
      let wv: [int8x16_t; NR] = std::array::from_fn(|j| unsafe { vld1q_s8(w.as_ptr().add(j * dim_in + k)) });
      for (row_acc, xv) in acc.iter_mut().zip(&xv) {
        for (slot, wv) in row_acc.iter_mut().zip(&wv) {
          // SAFETY: `sdot` is the dotprod extension the caller verified; the
          // operands are register values, no memory is touched.
          unsafe {
            std::arch::asm!(
              "sdot {acc:v}.4s, {a:v}.16b, {b:v}.16b",
              acc = inout(vreg) *slot,
              a = in(vreg) *xv,
              b = in(vreg) *wv,
              options(pure, nomem, nostack)
            );
          }
        }
      }
      k += STEP;
    }
    for (i, row_acc) in acc.iter().enumerate() {
      for (j, slot) in row_acc.iter().enumerate() {
        let mut total = vaddvq_s32(*slot);
        for kk in blocks..dim_in {
          total += x[i * dim_in + kk] as i32 * w[j * dim_in + kk] as i32;
        }
        out[i * ldc + j] = total;
      }
    }
  }
}

#[cfg(target_arch = "x86_64")]
mod x86 {
  use std::arch::x86_64::*;

  /// Exact sum of eight i32 lanes.
  #[inline]
  #[target_feature(enable = "avx2")]
  fn hsum_epi32_256(v: __m256i) -> i32 {
    let s = _mm_add_epi32(_mm256_castsi256_si128(v), _mm256_extracti128_si256::<1>(v));
    let s = _mm_add_epi32(s, _mm_shuffle_epi32::<0b01_00_11_10>(s));
    let s = _mm_add_epi32(s, _mm_shuffle_epi32::<0b00_00_00_01>(s));
    _mm_cvtsi128_si32(s)
  }

  /// `vpdpbusd` on 512-bit lanes: 64 bytes per step, x sign-flipped to u8
  /// (the driver subtracts `128·Σw`). Contract as [`super::TileFn`];
  /// AVX-512F/BW/VNNI verified by the caller. (512-bit intrinsics are stable
  /// since 1.89 — the pinned toolchain is 1.98; the declared MSRV predates the
  /// let-chains this tree already uses.)
  #[allow(clippy::incompatible_msrv)]
  #[target_feature(enable = "avx512f,avx512bw,avx512vnni")]
  pub(super) unsafe fn tile_avx512_vnni<const MR: usize, const NR: usize>(
    x: &[i8],
    w: &[i8],
    dim_in: usize,
    out: &mut [i32],
    ldc: usize,
  ) {
    debug_assert!(x.len() >= MR * dim_in && w.len() >= NR * dim_in);
    debug_assert!(out.len() >= (MR - 1) * ldc + NR);
    const STEP: usize = 64;
    let blocks = dim_in / STEP * STEP;
    let flip = _mm512_set1_epi8(-128);
    let mut acc = [[_mm512_setzero_si512(); NR]; MR];
    let mut k = 0usize;
    while k < blocks {
      // SAFETY: k + STEP ≤ blocks ≤ dim_in and every row offset lies inside the
      // slice (the contract).
      let xv: [__m512i; MR] = std::array::from_fn(|i| unsafe {
        _mm512_xor_si512(_mm512_loadu_si512(x.as_ptr().add(i * dim_in + k).cast()), flip)
      });
      // SAFETY: as above, for w's NR rows.
      let wv: [__m512i; NR] =
        std::array::from_fn(|j| unsafe { _mm512_loadu_si512(w.as_ptr().add(j * dim_in + k).cast()) });
      for (row_acc, xv) in acc.iter_mut().zip(&xv) {
        for (slot, wv) in row_acc.iter_mut().zip(&wv) {
          *slot = _mm512_dpbusd_epi32(*slot, *xv, *wv);
        }
      }
      k += STEP;
    }
    for (i, row_acc) in acc.iter().enumerate() {
      for (j, slot) in row_acc.iter().enumerate() {
        let mut total = _mm512_reduce_add_epi32(*slot);
        for kk in blocks..dim_in {
          total += (x[i * dim_in + kk] as i32 + 128) * w[j * dim_in + kk] as i32;
        }
        out[i * ldc + j] = total;
      }
    }
  }

  /// `vpdpbusd` on 256-bit lanes (AVX-VNNI): 32 bytes per step, x sign-flipped
  /// to u8 (the driver subtracts `128·Σw`). AVX2+AVX-VNNI verified by the caller.
  /// (The intrinsic is stable since 1.89 — see `tile_avx512_vnni` on the MSRV.)
  #[allow(clippy::incompatible_msrv)]
  #[target_feature(enable = "avx2,avxvnni")]
  pub(super) unsafe fn tile_avx_vnni<const MR: usize, const NR: usize>(
    x: &[i8],
    w: &[i8],
    dim_in: usize,
    out: &mut [i32],
    ldc: usize,
  ) {
    debug_assert!(x.len() >= MR * dim_in && w.len() >= NR * dim_in);
    debug_assert!(out.len() >= (MR - 1) * ldc + NR);
    const STEP: usize = 32;
    let blocks = dim_in / STEP * STEP;
    let flip = _mm256_set1_epi8(-128);
    let mut acc = [[_mm256_setzero_si256(); NR]; MR];
    let mut k = 0usize;
    while k < blocks {
      // SAFETY: k + STEP ≤ blocks ≤ dim_in and every row offset lies inside the
      // slice (the contract).
      let xv: [__m256i; MR] = std::array::from_fn(|i| unsafe {
        _mm256_xor_si256(_mm256_loadu_si256(x.as_ptr().add(i * dim_in + k).cast()), flip)
      });
      // SAFETY: as above, for w's NR rows.
      let wv: [__m256i; NR] =
        std::array::from_fn(|j| unsafe { _mm256_loadu_si256(w.as_ptr().add(j * dim_in + k).cast()) });
      for (row_acc, xv) in acc.iter_mut().zip(&xv) {
        for (slot, wv) in row_acc.iter_mut().zip(&wv) {
          *slot = _mm256_dpbusd_avx_epi32(*slot, *xv, *wv);
        }
      }
      k += STEP;
    }
    for (i, row_acc) in acc.iter().enumerate() {
      for (j, slot) in row_acc.iter().enumerate() {
        let mut total = hsum_epi32_256(*slot);
        for kk in blocks..dim_in {
          total += (x[i * dim_in + kk] as i32 + 128) * w[j * dim_in + kk] as i32;
        }
        out[i * ldc + j] = total;
      }
    }
  }

  /// `pmaddwd` over sign-extended i16 (exact): 16 bytes per step, signed ×
  /// signed, no shift. AVX2 verified by the caller.
  #[target_feature(enable = "avx2")]
  pub(super) unsafe fn tile_avx2_madd<const MR: usize, const NR: usize>(
    x: &[i8],
    w: &[i8],
    dim_in: usize,
    out: &mut [i32],
    ldc: usize,
  ) {
    debug_assert!(x.len() >= MR * dim_in && w.len() >= NR * dim_in);
    debug_assert!(out.len() >= (MR - 1) * ldc + NR);
    const STEP: usize = 16;
    let blocks = dim_in / STEP * STEP;
    let mut acc = [[_mm256_setzero_si256(); NR]; MR];
    let mut k = 0usize;
    while k < blocks {
      // SAFETY: k + STEP ≤ blocks ≤ dim_in and every row offset lies inside the
      // slice (the contract).
      let xv: [__m256i; MR] = std::array::from_fn(|i| unsafe {
        _mm256_cvtepi8_epi16(_mm_loadu_si128(x.as_ptr().add(i * dim_in + k).cast()))
      });
      // SAFETY: as above, for w's NR rows.
      let wv: [__m256i; NR] = std::array::from_fn(|j| unsafe {
        _mm256_cvtepi8_epi16(_mm_loadu_si128(w.as_ptr().add(j * dim_in + k).cast()))
      });
      for (row_acc, xv) in acc.iter_mut().zip(&xv) {
        for (slot, wv) in row_acc.iter_mut().zip(&wv) {
          *slot = _mm256_add_epi32(*slot, _mm256_madd_epi16(*xv, *wv));
        }
      }
      k += STEP;
    }
    for (i, row_acc) in acc.iter().enumerate() {
      for (j, slot) in row_acc.iter().enumerate() {
        let mut total = hsum_epi32_256(*slot);
        for kk in blocks..dim_in {
          total += x[i * dim_in + kk] as i32 * w[j * dim_in + kk] as i32;
        }
        out[i * ldc + j] = total;
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn fill(seed: &mut u64, out: &mut [f32]) {
    for slot in out {
      *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
      *slot = ((*seed >> 40) as f32 / (1u64 << 23) as f32) - 1.0;
    }
  }

  /// The exact integer every kernel must reproduce, computed in i64 from the
  /// same codes — then rescaled exactly as the driver does.
  fn reference(x: &[f32], w: &QuantizedMatrix) -> Vec<f32> {
    let (dim_in, rows_out) = (w.cols, w.rows);
    let rows = x.len() / dim_in;
    let mut out = vec![0.0f32; rows * rows_out];
    let mut codes = vec![0i8; dim_in];
    for s in 0..rows {
      let sx = quantize_row(&x[s * dim_in..(s + 1) * dim_in], &mut codes) as f64;
      for o in 0..rows_out {
        let wr = &w.codes[o * dim_in..(o + 1) * dim_in];
        let dot: i64 = codes.iter().zip(wr).map(|(&a, &b)| a as i64 * b as i64).sum();
        out[s * rows_out + o] = (dot as f64 * sx * w.scales[o] as f64) as f32;
      }
    }
    out
  }

  fn bits(v: &[f32]) -> Vec<u32> {
    v.iter().map(|f| f.to_bits()).collect()
  }

  /// Every kernel this CPU has, on every tail shape and shard count, against
  /// the i64 reference — bit-identical (module doc: the int8 path computes one
  /// exact integer everywhere).
  #[test]
  fn every_present_int8_kernel_is_bit_identical_to_the_exact_reference() {
    let mut seed = 0x2545_F491_4F6C_DD1Du64;
    let kernels = present_kernels();
    eprintln!("int8 kernels present: {kernels:?} (default {:?})", detect());
    for &(rows, dim_in, rows_out) in
      &[(8, 64, 8), (7, 64, 9), (9, 37, 6), (1, 5, 1), (13, 768, 30), (3, 3072, 5), (5, 130, 3)]
    {
      let mut x = vec![0.0f32; rows * dim_in];
      let mut wf = vec![0.0f32; rows_out * dim_in];
      fill(&mut seed, &mut x);
      fill(&mut seed, &mut wf);
      let w = QuantizedMatrix::quantize(&wf, rows_out, dim_in).unwrap();
      let want = reference(&x, &w);
      for &isa in &kernels {
        for shards in [0usize, 1, 3, 64] {
          super::super::forward::set_throughput_shards(shards);
          let mut got = vec![0.0f32; rows * rows_out];
          gemm_i8_under(isa, &x, &w, &mut got).unwrap();
          assert_eq!(bits(&got), bits(&want), "{isa:?} {rows}×{dim_in}×{rows_out} shards {shards}");
        }
      }
      super::super::forward::set_throughput_shards(0);
    }
  }

  #[test]
  fn quantization_rejects_bad_shapes_and_handles_zero_rows() {
    assert!(QuantizedMatrix::quantize(&[1.0; 6], 2, 4).is_err());
    assert!(QuantizedMatrix::quantize(&[], 0, 4).is_err());
    let w = QuantizedMatrix::quantize(&[0.0, 0.0, 1.0, -2.0], 2, 2).unwrap();
    assert_eq!(w.scales[0], 0.0);
    assert_eq!(&w.codes[2..], &[64, -127]);
    assert_eq!(w.sums, vec![0, -63]);
    let mut out = vec![0.0f32; 2];
    gemm_i8(&[3.0, 4.0], &w, &mut out).unwrap();
    assert_eq!(out[0], 0.0);
    // x codes (95, 127) scale 4/127; w row 1 codes (64, -127) scale 2/127.
    let want = ((95 * 64 - 127 * 127) as f64 * (4.0f32 / 127.0) as f64 * (2.0f32 / 127.0) as f64) as f32;
    assert_eq!(out[1], want);
    let mut short = vec![0.0f32; 3];
    assert!(gemm_i8(&[1.0; 2], &w, &mut short).is_err());
  }
}
