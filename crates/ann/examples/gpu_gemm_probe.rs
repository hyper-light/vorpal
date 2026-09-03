//! Model-free probe of the `wgpu` GEMM kernel (`encoder/gemm_wgpu.rs`): the
//! dispatch-only rate of the encoder's six layer shapes under every tile
//! geometry the adapter admits, on synthetic operands — the kernel-tuning loop
//! and the recorded tile sweep that pins `Tile::derive`'s caps. Dev-only
//! target (an example never links into a production binary).
//!
//! ```text
//! cargo run --release -p vorpal-ann --example gpu_gemm_probe -- [rows] [dim] [inner]
//! ```
//! Defaults: 4690 rows (the 256-surface fill batch's token count), dim 768,
//! inner 3072 (CodeRankEmbed). Each cell is the mean of 20 back-to-back
//! submissions after 2 warm-ups; correctness of every tile is checked against
//! an f64 CPU reference on a ragged slice first.

use vorpal_ann::encoder::{GpuGemm, Tile};

fn synthetic(len: usize, seed: usize) -> Vec<f32> {
  (0..len).map(|i| (((i + seed) * 7919) % 197) as f32 / 61.0 - 1.6).collect()
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
  let args: Vec<usize> = std::env::args().skip(1).map(|a| a.parse()).collect::<Result<_, _>>()?;
  let rows = args.first().copied().unwrap_or(4690);
  let dim = args.get(1).copied().unwrap_or(768);
  let inner = args.get(2).copied().unwrap_or(3072);
  let shapes: [(usize, usize); 5] = [(dim, 3 * dim), (dim, dim), (dim, inner), (dim, inner), (inner, dim)];
  let weights: Vec<Vec<f32>> = shapes
    .iter()
    .enumerate()
    .map(|(i, &(k, n))| synthetic(k * n, i))
    .collect();
  // Ragged correctness slice: 37 rows of the (dim, 3·dim) shape.
  let x = synthetic(37 * dim, 11);
  let probe = GpuGemm::open(&[dim, inner])?;
  let limits_note = format!("{} derived tile {:?}", probe.label(), probe.tile());
  drop(probe);
  println!("{limits_note}; rows {rows}");
  println!("| tile | qkv GFLOPS | out GFLOPS | fc11 GFLOPS | fc12 GFLOPS | fc2 GFLOPS | layer GFLOPS | layer s |");
  println!("|---|---:|---:|---:|---:|---:|---:|---:|");
  for side in [8u32, 16, 32] {
    for bk4 in [2u32, 4, 8, 16] {
      let tile = Tile { bm: side * 4, bn: side * 4, bk4 };
      let mut gpu = match GpuGemm::open_with(&[dim, inner], Some(tile)) {
        Ok(gpu) => gpu,
        Err(reason) => {
          println!("| {}×{} bk4 {bk4} | refused: {reason} |", tile.bm, tile.bn);
          continue;
        }
      };
      for w in &weights {
        gpu.make_resident(w)?;
      }
      // Correctness on the ragged slice.
      let (k, n) = shapes[0];
      let mut out = vec![0.0f32; 37 * n];
      gpu.gemm(&x, k, &weights[0], n, 37, &mut out)?;
      let mut worst = 0.0f64;
      for r in 0..37 {
        for o in 0..n {
          let expect: f64 = (0..k).map(|d| x[r * k + d] as f64 * weights[0][o * k + d] as f64).sum();
          worst = worst.max((out[r * n + o] as f64 - expect).abs() / expect.abs().max(1.0));
        }
      }
      if worst > 1e-4 {
        println!("| {}×{} bk4 {bk4} | WRONG: max rel err {worst:.3e} |", tile.bm, tile.bn);
        continue;
      }
      let mut cells = Vec::new();
      let (mut layer_secs, mut layer_flops) = (0.0f64, 0.0f64);
      for (w, &(k, n)) in weights.iter().zip(&shapes) {
        gpu.dispatch_only(k, w, n, rows, 2)?;
        let secs = gpu.dispatch_only(k, w, n, rows, 20)? / 20.0;
        let flops = 2.0 * rows as f64 * k as f64 * n as f64;
        cells.push(format!("{:.0}", flops / secs / 1e9));
        layer_secs += secs;
        layer_flops += flops;
      }
      println!(
        "| {}×{} bk4 {bk4} | {} | {:.0} | {layer_secs:.4} |",
        tile.bm,
        tile.bn,
        cells.join(" | "),
        layer_flops / layer_secs / 1e9
      );
    }
  }
  Ok(())
}
