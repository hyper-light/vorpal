//! x86-64 f32 GEMM kernels for [`super::GemmPath::Throughput`] — the rung the
//! non-macOS builds were missing (there `Throughput` silently WAS the fixed
//! lanes; ENCODER_RESEARCH §6: the first-order lever is E, the effective rate).
//!
//! Shape (the forward's only GEMM form): `C[rows × rows_out] = X[rows × dim_in]
//! · Wᵀ`, W row-major `[rows_out][dim_in]` — both operands contract over their
//! CONTIGUOUS axis, so every output element is one dot product and the kernel is
//! a register-tiled dot-product grid rather than a packed BLAS GEMM:
//!
//! * micro-tile: `MR` x rows × `NR` w rows held as `MR × NR` vector accumulators,
//!   one FMA per (row, col) per vector step of `LANES` along `dim_in` —
//!   `MR × NR` FMAs per `MR + NR` loads (AVX2 2×4 = 11 of 16 ymm; AVX-512 4×4 =
//!   21 of 32 zmm — sized to the register file, not tuned);
//! * L2 panel: within one rayon row-shard the w rows are walked in panels of
//!   [`panel_rows`] (derived from the L2 size CPUID enumerates — the panel takes at
//!   most half the L2, the GotoBLAS margin for the streaming x tiles, the C
//!   write-allocates and set conflicts) so W streams from L3 once per shard, not
//!   once per x tile; the x tile of `MR` rows re-streams once per panel;
//! * row shards: the shard count is [`super::throughput_shards`] (derived from
//!   `available_parallelism()`), exactly the Accelerate path's split.
//!
//! DETERMINISM: each output element's reduction has ONE fixed structure whatever
//! tile, panel or shard it lands in — `LANES` FMA lanes over ascending `dim_in`
//! blocks, a fixed pairwise horizontal tree, then the scalar tail in ascending
//! order (every tail variant `<1, NR>`, `<MR, 1>`, `<1, 1>` is the same generic
//! kernel) — so results are bit-identical across thread counts and shard counts
//! BY CONSTRUCTION. The AVX2 kernel's structure is exactly the fixed-order lanes'
//! (eight `fma` lanes, `((l0+l4)+(l1+l5))+((l2+l6)+(l3+l7))`, ascending tail), so
//! on an AVX2 machine the `Throughput` GEMM reproduces the fixed-order GEMM bit
//! for bit (the unit test below asserts it; `gemm_bench` reports Δ = 0). The
//! whole forward under `Throughput` still differs from `FixedOrder` by the SwiGLU
//! gate's f32 `exp_fast` (forward.rs), so the real-encoder parity remains the
//! cosine oracle. AVX-512 accumulates sixteen lanes and differs from the fixed lanes
//! by summation order alone — the parity oracle (cosine ≥ 0.9999) is its bound,
//! and its throughput is a CI datum (no AVX-512 hardware or emulation reaches
//! the development machine; BENCHMARKS records what ubuntu-latest measured).
//!
//! Every `unsafe` block states its invariant: the ISA was verified by CPUID
//! (`is_x86_feature_detected!`, cached once) and every pointer offset lies inside
//! a slice whose length the caller validated (`super::gemm` checks the operand
//! shapes before dispatch; the driver re-checks its sub-slices).

use std::arch::x86_64::*;
use std::sync::OnceLock;

use rayon::prelude::*;

/// The vector ISA the kernels run under — detected once per process by CPUID.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Isa {
  /// 512-bit lanes (`avx512f`): the Zen 4/5 and Xeon rung.
  Avx512F,
  /// 256-bit lanes + fused multiply-add (`avx2` + `fma`): every x86-64 CPU since
  /// Haswell/Zen 1; the Core Ultra rung.
  Avx2Fma,
}

impl Isa {
  /// The provenance label a sidecar built under this ISA records.
  pub(super) fn label(self) -> &'static str {
    match self {
      Isa::Avx512F => "avx512f-sgemm",
      Isa::Avx2Fma => "avx2-fma-sgemm",
    }
  }
}

/// The best ISA this CPU offers, or `None` (pre-Haswell / no FMA — the fixed
/// lanes serve). CPUID is read once; the answer is a process constant.
pub(super) fn detect() -> Option<Isa> {
  static ISA: OnceLock<Option<Isa>> = OnceLock::new();
  *ISA.get_or_init(|| {
    if is_x86_feature_detected!("avx512f") {
      Some(Isa::Avx512F)
    } else if is_x86_feature_detected!("avx2") && is_x86_feature_detected!("fma") {
      Some(Isa::Avx2Fma)
    } else {
      None
    }
  })
}

/// How many w rows one L2 panel holds for a `dim_in`-wide GEMM under `nr`-row
/// tiles: the most whole tiles whose f32 rows fit HALF the L2 (module doc), at
/// least one tile. With no L2 figure the panel is one tile — no reuse is assumed
/// rather than a guessed size.
pub(super) fn panel_rows(dim_in: usize, nr: usize) -> usize {
  let row_bytes = dim_in * std::mem::size_of::<f32>();
  let tiles = super::cache::l2_cache_bytes()
    .map_or(1, |l2| (l2 / 2) / (row_bytes * nr).max(1))
    .max(1);
  tiles * nr
}

/// The four tile shapes one ISA's driver dispatches (full, row tail, column
/// tail, corner) — all the SAME generic kernel, so a row's arithmetic never
/// depends on which shape computed it (module doc, determinism).
struct Tiles {
  mr: usize,
  nr: usize,
  full: TileFn,
  row_tail: TileFn,
  col_tail: TileFn,
  corner: TileFn,
}

/// `(x, w, dim_in, out, ldc)`: x holds `MR` rows and w `NR` rows, each `dim_in`
/// wide and contiguous; `out[i * ldc + j]` receives row i · col j.
type TileFn = unsafe fn(&[f32], &[f32], usize, &mut [f32], usize);

const AVX2_TILES: Tiles = Tiles {
  mr: 2,
  nr: 4,
  full: tile_avx2::<2, 4>,
  row_tail: tile_avx2::<1, 4>,
  col_tail: tile_avx2::<2, 1>,
  corner: tile_avx2::<1, 1>,
};

const AVX512_TILES: Tiles = Tiles {
  mr: 4,
  nr: 4,
  full: tile_avx512::<4, 4>,
  row_tail: tile_avx512::<1, 4>,
  col_tail: tile_avx512::<4, 1>,
  corner: tile_avx512::<1, 1>,
};

/// `C = X · Wᵀ` under `isa`, row-sharded across the rayon pool (module doc).
/// Shapes were validated by `super::gemm`; a disagreement here is a typed error.
pub(super) fn sgemm(
  isa: Isa,
  x: &[f32],
  dim_in: usize,
  w: &[f32],
  rows_out: usize,
  rows: usize,
  out: &mut [f32],
) -> Result<(), String> {
  if x.len() < rows * dim_in || w.len() < rows_out * dim_in || out.len() != rows * rows_out {
    return Err("encoder: x86 GEMM operand shapes disagree".to_string());
  }
  let tiles = match isa {
    Isa::Avx512F => &AVX512_TILES,
    Isa::Avx2Fma => &AVX2_TILES,
  };
  let shards = super::forward::throughput_shards().clamp(1, rows.max(1));
  let shard_rows = rows.div_ceil(shards);
  let panel = panel_rows(dim_in, tiles.nr);
  out
    .par_chunks_mut(shard_rows * rows_out)
    .enumerate()
    .for_each(|(shard, out_shard)| {
      let first = shard * shard_rows;
      let count = out_shard.len() / rows_out;
      let x_shard = &x[first * dim_in..(first + count) * dim_in];
      shard_driver(tiles, x_shard, dim_in, w, rows_out, panel, out_shard);
    });
  Ok(())
}

/// One shard: w panels outermost (L2 residency), x tiles within, w tiles
/// innermost. `x` holds exactly `out.len() / rows_out` rows.
fn shard_driver(
  tiles: &Tiles,
  x: &[f32],
  dim_in: usize,
  w: &[f32],
  rows_out: usize,
  panel: usize,
  out: &mut [f32],
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
          // SAFETY (ISA): `sgemm` was handed an `Isa` that `detect` verified by
          // CPUID, and each tile fn is compiled for exactly that ISA. SAFETY
          // (bounds): `x_tile` holds `tile_rows` full rows, `w_tile` holds
          // `tile_cols` full rows, and `out_tile` holds `tile_rows` rows of
          // `rows_out` with `col + tile_cols ≤ rows_out` — the kernel's contract.
          Some(kernel) => unsafe { kernel(x_tile, w_tile, dim_in, &mut out_tile[col..], rows_out) },
          // A tail of 2..mr rows or 2..nr columns: one row / one column at a
          // time through the single-row and single-column shapes (same
          // per-element arithmetic — module doc).
          None => {
            for i in 0..tile_rows {
              let x_row = &x_tile[i * dim_in..(i + 1) * dim_in];
              let out_row = &mut out_tile[i * rows_out + col..i * rows_out + col + tile_cols];
              if tile_cols == nr {
                // SAFETY: as above — one x row, `nr` w rows, `nr` output slots.
                unsafe { (tiles.row_tail)(x_row, w_tile, dim_in, out_row, rows_out) }
              } else {
                for j in 0..tile_cols {
                  let w_row = &w_tile[j * dim_in..(j + 1) * dim_in];
                  // SAFETY: as above — one x row, one w row, one output slot.
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

/// Fixed pairwise tree over eight lanes — the fixed-order lanes' exact reduction
/// `((l0+l4)+(l1+l5))+((l2+l6)+(l3+l7))`.
#[inline]
#[target_feature(enable = "avx2,fma")]
fn hsum8(v: __m256) -> f32 {
  // (l0+l4, l1+l5, l2+l6, l3+l7)
  let s = _mm_add_ps(_mm256_castps256_ps128(v), _mm256_extractf128_ps::<1>(v));
  // (s0+s1, ·, s2+s3, ·)
  let t = _mm_add_ps(s, _mm_movehdup_ps(s));
  // (s0+s1)+(s2+s3)
  _mm_cvtss_f32(_mm_add_ss(t, _mm_movehl_ps(t, t)))
}

/// Sixteen lanes: fold the upper eight onto the lower (lane i + lane i+8), then
/// the eight-lane tree. AVX-512F only (no DQ): the upper half is reached by a
/// 128-bit-lane shuffle placing lanes 8..15 at the bottom.
#[inline]
#[allow(clippy::incompatible_msrv)]
#[target_feature(enable = "avx512f,avx2,fma")]
fn hsum16(v: __m512) -> f32 {
  // imm fields (low to high) 2, 3, 2, 3 → (v[2], v[3], v[2], v[3]) as 128-bit
  // lanes; only the low two survive the cast.
  const UPPER: i32 = 0b1110_1110;
  let upper = _mm512_castps512_ps256(_mm512_shuffle_f32x4::<UPPER>(v, v));
  hsum8(_mm256_add_ps(_mm512_castps512_ps256(v), upper))
}

/// AVX2+FMA micro-tile (module doc). Contract: `x` holds `MR` rows, `w` `NR`
/// rows, each `dim_in` f32 wide; `out` has `MR` rows of stride `ldc` with `NR`
/// slots addressable per row; the ISA was verified by the caller.
#[target_feature(enable = "avx2,fma")]
unsafe fn tile_avx2<const MR: usize, const NR: usize>(
  x: &[f32],
  w: &[f32],
  dim_in: usize,
  out: &mut [f32],
  ldc: usize,
) {
  debug_assert!(x.len() >= MR * dim_in && w.len() >= NR * dim_in);
  debug_assert!(out.len() >= (MR - 1) * ldc + NR);
  const LANES: usize = 8;
  let blocks = dim_in / LANES * LANES;
  let mut acc = [[_mm256_setzero_ps(); NR]; MR];
  let mut k = 0usize;
  while k < blocks {
    // SAFETY: k + LANES ≤ blocks ≤ dim_in and every row offset is inside the
    // slice (the contract above).
    let xv: [__m256; MR] =
      std::array::from_fn(|i| unsafe { _mm256_loadu_ps(x.as_ptr().add(i * dim_in + k)) });
    // SAFETY: as above, for w's NR rows.
    let wv: [__m256; NR] =
      std::array::from_fn(|j| unsafe { _mm256_loadu_ps(w.as_ptr().add(j * dim_in + k)) });
    for (row_acc, xv) in acc.iter_mut().zip(&xv) {
      for (slot, wv) in row_acc.iter_mut().zip(&wv) {
        *slot = _mm256_fmadd_ps(*xv, *wv, *slot);
      }
    }
    k += LANES;
  }
  for (i, row_acc) in acc.iter().enumerate() {
    for (j, slot) in row_acc.iter().enumerate() {
      let mut total = hsum8(*slot);
      for kk in blocks..dim_in {
        total = x[i * dim_in + kk].mul_add(w[j * dim_in + kk], total);
      }
      out[i * ldc + j] = total;
    }
  }
}

/// AVX-512F micro-tile — sixteen lanes, otherwise [`tile_avx2`]'s contract.
/// The 512-bit intrinsics are stable since Rust 1.89 (the workspace pins 1.98;
/// its declared `rust-version` predates the let-chains the tree already uses).
#[allow(clippy::incompatible_msrv)]
#[target_feature(enable = "avx512f,avx2,fma")]
unsafe fn tile_avx512<const MR: usize, const NR: usize>(
  x: &[f32],
  w: &[f32],
  dim_in: usize,
  out: &mut [f32],
  ldc: usize,
) {
  debug_assert!(x.len() >= MR * dim_in && w.len() >= NR * dim_in);
  debug_assert!(out.len() >= (MR - 1) * ldc + NR);
  const LANES: usize = 16;
  let blocks = dim_in / LANES * LANES;
  let mut acc = [[_mm512_setzero_ps(); NR]; MR];
  let mut k = 0usize;
  while k < blocks {
    // SAFETY: k + LANES ≤ blocks ≤ dim_in and every row offset is inside the
    // slice (the contract above).
    let xv: [__m512; MR] =
      std::array::from_fn(|i| unsafe { _mm512_loadu_ps(x.as_ptr().add(i * dim_in + k)) });
    // SAFETY: as above, for w's NR rows.
    let wv: [__m512; NR] =
      std::array::from_fn(|j| unsafe { _mm512_loadu_ps(w.as_ptr().add(j * dim_in + k)) });
    for (row_acc, xv) in acc.iter_mut().zip(&xv) {
      for (slot, wv) in row_acc.iter_mut().zip(&wv) {
        *slot = _mm512_fmadd_ps(*xv, *wv, *slot);
      }
    }
    k += LANES;
  }
  for (i, row_acc) in acc.iter().enumerate() {
    for (j, slot) in row_acc.iter().enumerate() {
      let mut total = hsum16(*slot);
      for kk in blocks..dim_in {
        total = x[i * dim_in + kk].mul_add(w[j * dim_in + kk], total);
      }
      out[i * ldc + j] = total;
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Deterministic pseudo-random f32 in (-1, 1) — an LCG, no dependency.
  fn fill(seed: &mut u64, out: &mut [f32]) {
    for slot in out {
      *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
      *slot = ((*seed >> 40) as f32 / (1u64 << 23) as f32) - 1.0;
    }
  }

  fn reference(x: &[f32], dim_in: usize, w: &[f32], rows_out: usize) -> Vec<f32> {
    let rows = x.len() / dim_in;
    let mut out = vec![0.0f32; rows * rows_out];
    super::super::forward::gemm_fixed_order(x, dim_in, w, rows_out, &mut out);
    out
  }

  fn bits(v: &[f32]) -> Vec<u32> {
    v.iter().map(|f| f.to_bits()).collect()
  }

  /// The x86 path against the fixed lanes on every tail shape: AVX2 must be
  /// bit-identical (module doc), AVX-512 within f32 summation-order noise; both
  /// bit-identical across shard counts.
  #[test]
  fn x86_sgemm_matches_fixed_order_on_all_tail_shapes() {
    let Some(isa) = detect() else {
      eprintln!("skipped: no AVX2+FMA on this CPU");
      return;
    };
    let mut seed = 0x9E37_79B9_7F4A_7C15u64;
    // (rows, dim_in, rows_out): full tiles, row tails, column tails, a scalar
    // tail along dim_in, and a matrix smaller than one tile.
    for &(rows, dim_in, rows_out) in
      &[(8, 64, 8), (7, 64, 9), (9, 37, 6), (1, 5, 1), (13, 768, 30), (3, 3072, 5)]
    {
      let mut x = vec![0.0f32; rows * dim_in];
      let mut w = vec![0.0f32; rows_out * dim_in];
      fill(&mut seed, &mut x);
      fill(&mut seed, &mut w);
      let want = reference(&x, dim_in, &w, rows_out);
      let mut got = vec![0.0f32; rows * rows_out];
      for shards in [0usize, 1, 3, 64] {
        super::super::forward::set_throughput_shards(shards);
        sgemm(isa, &x, dim_in, &w, rows_out, rows, &mut got).unwrap();
        match isa {
          Isa::Avx2Fma => assert_eq!(bits(&got), bits(&want), "avx2 {rows}×{dim_in}×{rows_out} shards {shards}"),
          Isa::Avx512F => {
            for (g, r) in got.iter().zip(&want) {
              let tolerance = 1e-5 * dim_in as f32;
              assert!((g - r).abs() <= tolerance, "avx512 {rows}×{dim_in}×{rows_out}: {g} vs {r}");
            }
          }
        }
        // Shard invariance: the first shard count's bits are the law for the rest.
        if shards == 0 {
          seed ^= 1;
        }
      }
      let mut single = vec![0.0f32; rows * rows_out];
      super::super::forward::set_throughput_shards(1);
      sgemm(isa, &x, dim_in, &w, rows_out, rows, &mut single).unwrap();
      super::super::forward::set_throughput_shards(0);
      assert_eq!(bits(&single), bits(&got), "shard count changed the bits");
    }
  }

  #[test]
  fn panel_rows_is_at_least_one_tile_and_whole_tiles() {
    for nr in [1usize, 4] {
      for dim_in in [1usize, 768, 3072, 1 << 20] {
        let rows = panel_rows(dim_in, nr);
        assert!(rows >= nr && rows % nr == 0, "dim {dim_in} nr {nr}: {rows}");
      }
    }
  }
}
