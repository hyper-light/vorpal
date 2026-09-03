//! The encoder GEMM rate on THIS machine, weights-free: the six per-layer GEMM
//! shapes of the vendored CodeRankEmbed forward (dim 768, inner 3072) on random
//! operands, under every `GemmPath` — the CI datum for the platforms the
//! development machine cannot run (x86 AVX2 / AVX-512 / VNNI throughput is
//! recorded from `ubuntu-latest`'s log; BENCHMARKS' cross-platform section).
//!
//! ```text
//! cargo run --release -p vorpal-ann --example gemm_bench [-- <tokens> [reps]]
//! ```
//!
//! `tokens` defaults to 4690 — the recorded 256-surface batch (BENCHMARKS Stage
//! A) — so rates compare with the real-encoder sweep's row. FLOPs per GEMM =
//! 2 × tokens × dim_in × rows_out; int8 rates are reported in the same unit
//! (multiply-adds as FLOPs) so the columns compare. Each cell is the median of
//! `reps` timings. Correctness beside the rate: max |Δ| / max |ref| of every
//! path against the fixed lanes on the same operands.

use vorpal_ann::encoder::{GemmPath, QuantizedMatrix, bench_gemm, throughput_shards};

fn fill(seed: &mut u64, out: &mut [f32]) {
  for slot in out {
    *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    *slot = ((*seed >> 40) as f32 / (1u64 << 23) as f32) - 1.0;
  }
}

fn median(samples: &mut [f64]) -> f64 {
  samples.sort_by(f64::total_cmp);
  samples.get(samples.len() / 2).copied().unwrap_or(0.0)
}

fn main() -> Result<(), String> {
  let args: Vec<String> = std::env::args().skip(1).collect();
  let tokens: usize = args.first().map_or(Ok(4690), |a| a.parse()).map_err(|e| format!("tokens: {e}"))?;
  let reps: usize = args.get(1).map_or(Ok(3), |a| a.parse()).map_err(|e| format!("reps: {e}"))?;
  let (dim, inner) = (768usize, 3072usize);
  println!(
    "gemm_bench: {tokens} tokens, median of {reps}; throughput = {} (native: {}), int8 = {}, shards {}, rayon threads {}",
    GemmPath::Throughput.label(),
    GemmPath::throughput_is_native(),
    GemmPath::Int8.label(),
    throughput_shards(),
    rayon::current_num_threads(),
  );
  println!("| GEMM | shape (tokens × in → out) | fixed-order | throughput | int8 | throughput Δ | int8 Δ |");
  println!("|---|---|---:|---:|---:|---:|---:|");
  let mut seed = 0x9E37_79B9_7F4A_7C15u64;
  let shapes = [
    ("qkv", dim, 3 * dim),
    ("out_proj", dim, dim),
    ("fc11", dim, inner),
    ("fc12", dim, inner),
    ("fc2", inner, dim),
  ];
  let mut totals = [0.0f64; 3];
  for (name, dim_in, rows_out) in shapes {
    let mut x = vec![0.0f32; tokens * dim_in];
    let mut w = vec![0.0f32; rows_out * dim_in];
    fill(&mut seed, &mut x);
    fill(&mut seed, &mut w);
    let q = QuantizedMatrix::quantize(&w, rows_out, dim_in)?;
    let flops = 2.0 * tokens as f64 * dim_in as f64 * rows_out as f64;
    let mut reference = vec![0.0f32; tokens * rows_out];
    bench_gemm(GemmPath::FixedOrder, &x, dim_in, &w, None, rows_out, &mut reference)?;
    let scale = reference.iter().fold(0.0f32, |m, v| m.max(v.abs())) as f64;
    let mut cells = Vec::new();
    let mut deltas = Vec::new();
    for (slot, path) in [GemmPath::FixedOrder, GemmPath::Throughput, GemmPath::Int8].into_iter().enumerate() {
      let mut out = vec![0.0f32; tokens * rows_out];
      let mut secs = Vec::with_capacity(reps);
      for _ in 0..reps {
        let started = std::time::Instant::now();
        bench_gemm(path, &x, dim_in, &w, Some(&q), rows_out, &mut out)?;
        secs.push(started.elapsed().as_secs_f64());
      }
      let s = median(&mut secs);
      totals[slot] += s;
      cells.push(format!("{s:.4} s / {:.0} GFLOPS", flops / s / 1e9));
      if path != GemmPath::FixedOrder {
        let worst = out.iter().zip(&reference).fold(0.0f64, |m, (a, b)| m.max((*a as f64 - *b as f64).abs()));
        deltas.push(format!("{:.2e}", worst / scale.max(f64::MIN_POSITIVE)));
      }
    }
    println!(
      "| {name} | {tokens} × {dim_in} → {rows_out} | {} | {} | {} | {} | {} |",
      cells[0], cells[1], cells[2], deltas[0], deltas[1]
    );
  }
  // The per-layer GEMM sum × 12 layers = the forward's GEMM floor at this batch.
  let layer_flops = 2.0 * tokens as f64 * (4.0 * (dim * dim) as f64 + 3.0 * (dim * inner) as f64);
  println!(
    "per-layer GEMM sum: fixed-order {:.3} s ({:.0} GFLOPS), throughput {:.3} s ({:.0}), int8 {:.3} s ({:.0}) — ×12 layers = {:.2} / {:.2} / {:.2} s of GEMM per forward",
    totals[0],
    layer_flops / totals[0] / 1e9,
    totals[1],
    layer_flops / totals[1] / 1e9,
    totals[2],
    layer_flops / totals[2] / 1e9,
    totals[0] * 12.0,
    totals[1] * 12.0,
    totals[2] * 12.0,
  );
  Ok(())
}
