//! The recorded encoder-throughput sweep (ENCODER_RESEARCH §6/§8.2, Stage A):
//! effective GFLOPS of the owned CodeRankEmbed forward under each GEMM path, on
//! batches of real definition surfaces (~12 tokens) — plus the cross-path parity
//! (min cosine) and the rayon thread-stability verdict of the throughput path.
//! Dev-only target — compiles solely under `--features bench-internals`, never
//! into any production binary.
//!
//! ```text
//! VORPAL_CODERANK_DIR=<model-dir> cargo run --release -p vorpal-index \
//!   --features bench-internals --example sweep_encoder -- <index-dir> [batch ...]
//! ```
//!
//! Surfaces come from the index's own definitions (in-degree order — the
//! sidecar's coverage order), so token counts are the production distribution.
//! FLOPs per batch = 2 × non-embedding params × tokens (the per-token law the
//! research doc states; attention/LayerNorm terms < 1% at ~12 tokens). Each cell
//! is the median of REPS wall-clock runs; paths interleave so drift hits both.

use std::path::Path;

use vorpal_ann::encoder::{CodeEncoder, GemmPath};
use vorpal_kg::{Kg, NodeId};

const REPS: usize = 3;

fn median(samples: &mut [f64]) -> f64 {
  samples.sort_by(f64::total_cmp);
  samples.get(samples.len() / 2).copied().unwrap_or(0.0)
}

/// Definition surfaces in the sidecar's coverage order (referential in-degree
/// descending, id ascending), in the rerank's exact surface recipe.
fn surfaces(kg: &Kg, take: usize) -> Vec<String> {
  let mut ids: Vec<(usize, u64)> = (0..kg.node_count() as u64)
    .filter(|&id| {
      kg.node(NodeId::new(id))
        .is_some_and(|view| view.kind != vorpal_kg::SymbolKind::Import)
    })
    .map(|id| (kg.in_degree_referential(NodeId::new(id)), id))
    .collect();
  ids.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
  ids
    .into_iter()
    .take(take)
    .filter_map(|(_, id)| {
      let view = kg.node(NodeId::new(id))?;
      let basename = view.path.rsplit('/').next().unwrap_or(view.path);
      Some(format!("{} {} {basename}", view.name, view.signature))
    })
    .collect()
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
  let args: Vec<String> = std::env::args().skip(1).collect();
  let Some(index) = args.first() else {
    return Err("usage: sweep_encoder <index-dir> [batch ...]  (default batches: 26 256 1024)".into());
  };
  // Dense-channel rank probe: `<index-dir> --dense-rank <query> <name> [<name> ...]`
  // (serving-style open: the encoder comes from the root/global selection).
  if args.get(1).map(String::as_str) == Some("--dense-rank") {
    let query = args.get(2).ok_or("usage: sweep_encoder <index-dir> --dense-rank <query> <name...>")?;
    let names: Vec<&str> = args[3..].iter().map(String::as_str).collect();
    let (ranks, head) = vorpal_index::bench::dense_ranks(Path::new(index), query, &names)?;
    println!("query {query:?}: dense top-10 = {head:?}");
    for (name, rank) in ranks {
      println!("  {name}: dense rank {:?}", rank.map(|r| r + 1));
    }
    return Ok(());
  }
  // GEMM shard sweep: `<index-dir> --shards <k> [batch]` — one 256-surface batch
  // (default) on the throughput path with the GEMMs split into k row-shards; prints
  // tokens/s. Run under `/usr/bin/time -l` for the cores-busy figure.
  if args.get(1).map(String::as_str) == Some("--shards") {
    let shards: usize = args.get(2).ok_or("usage: --shards <k> [batch]")?.parse()?;
    let batch: usize = args.get(3).map_or(Ok(256), |b| b.parse())?;
    let model_dir = std::env::var_os("VORPAL_CODERANK_DIR")
      .map(std::path::PathBuf::from)
      .ok_or("set VORPAL_CODERANK_DIR to the model directory")?;
    let encoder = CodeEncoder::open(&model_dir)?;
    let kg = Kg::load(&vorpal_kg::resolve_index_dir(Path::new(index)))?;
    let pool = surfaces(&kg, batch);
    let texts: Vec<&str> = pool.iter().map(String::as_str).collect();
    let tokens: usize = texts.iter().map(|t| encoder.sequence_len(t)).sum();
    vorpal_ann::encoder::set_throughput_shards(shards);
    // Warm-up (weights page-in), then the timed reps.
    encoder.embed_batch_with(&texts, GemmPath::Throughput)?;
    let started = std::time::Instant::now();
    for _ in 0..REPS {
      encoder.embed_batch_with(&texts, GemmPath::Throughput)?;
    }
    let secs = started.elapsed().as_secs_f64() / REPS as f64;
    println!(
      "shards {shards} (rayon threads {}): batch {batch} = {tokens} tokens, {:.3} s/batch, {:.0} tok/s, {:.0} GFLOPS",
      rayon::current_num_threads(),
      secs,
      tokens as f64 / secs,
      2.0 * encoder.non_embedding_params() as f64 * tokens as f64 / secs / 1e9,
    );
    return Ok(());
  }
  // GPU tile sweep: `<index-dir> --gpu-tiles [batch]` — one batch (default 256)
  // under every tile geometry the adapter's limits admit (workgroup side 8/16/32
  // × K stage 2/4/8/16 vec4); prints tok/s and GFLOPS per tile. The recorded
  // winner pins `Tile::derive`'s caps.
  if args.get(1).map(String::as_str) == Some("--gpu-tiles") {
    let batch: usize = args.get(2).map_or(Ok(256), |b| b.parse())?;
    let model_dir = std::env::var_os("VORPAL_CODERANK_DIR")
      .map(std::path::PathBuf::from)
      .ok_or("set VORPAL_CODERANK_DIR to the model directory")?;
    let encoder = CodeEncoder::open(&model_dir)?;
    let kg = Kg::load(&vorpal_kg::resolve_index_dir(Path::new(index)))?;
    let pool = surfaces(&kg, batch);
    let texts: Vec<&str> = pool.iter().map(String::as_str).collect();
    let tokens: usize = texts.iter().map(|t| encoder.sequence_len(t)).sum();
    let flops = 2.0 * encoder.non_embedding_params() as f64 * tokens as f64;
    println!("| tile (block, K stage vec4, workgroup) | s/batch | tok/s | GFLOPS | device s (compute+blit) | up+down s |");
    println!("|---|---:|---:|---:|---:|---:|");
    for side in [8u32, 16, 32] {
      for bk4 in [2u32, 4, 8, 16] {
        let tile = vorpal_ann::encoder::Tile { bm: side * 4, bn: side * 4, bk4 };
        let gpu = match encoder.open_gpu_with(Some(tile)) {
          Ok(gpu) => gpu,
          Err(reason) => {
            println!("| {}×{} bk4 {bk4} {side}×{side} | refused: {reason} | | | | |", tile.bm, tile.bn);
            continue;
          }
        };
        encoder.embed_batch_with(&texts, GemmPath::Gpu(&gpu))?;
        gpu.reset_transfer();
        let mut secs = Vec::with_capacity(REPS);
        for _ in 0..REPS {
          let started = std::time::Instant::now();
          encoder.embed_batch_with(&texts, GemmPath::Gpu(&gpu))?;
          secs.push(started.elapsed().as_secs_f64());
        }
        let report = gpu.transfer_report();
        let s = median(&mut secs);
        println!(
          "| {}×{} bk4 {bk4} {side}×{side} | {s:.3} | {:.0} | {:.0} | {:.3} | {:.3} |",
          tile.bm,
          tile.bn,
          tokens as f64 / s,
          flops / s / 1e9,
          report.device_secs / REPS as f64,
          (report.upload_secs + report.download_secs) / REPS as f64,
        );
      }
    }
    return Ok(());
  }
  let model_dir = std::env::var_os("VORPAL_CODERANK_DIR")
    .map(std::path::PathBuf::from)
    .ok_or("set VORPAL_CODERANK_DIR to the model directory")?;
  let batches: Vec<usize> = if args.len() > 1 {
    args[1..].iter().map(|a| a.parse()).collect::<Result<_, _>>()?
  } else {
    vec![26, 256, 1024]
  };
  let encoder = CodeEncoder::open(&model_dir)?;
  let kg = Kg::load(&vorpal_kg::resolve_index_dir(Path::new(index)))?;
  let largest = batches.iter().copied().max().unwrap_or(0);
  let pool = surfaces(&kg, largest);
  if pool.len() < largest {
    return Err(format!("index holds only {} surfaces, {largest} requested", pool.len()).into());
  }
  // `VORPAL_SWEEP_DUMP=<file>`: write the surfaces one per line, so an external
  // build (a pre-change checkout) can be timed on the identical inputs.
  if let Some(dump) = std::env::var_os("VORPAL_SWEEP_DUMP") {
    std::fs::write(dump, pool.join("\n"))?;
  }
  let params = encoder.non_embedding_params();
  // The GPU rung (the ladder's own choice for this model), or its stated refusal.
  let rung = encoder.doc_side_rung();
  println!(
    "model {} — non-embedding params {:.1}M; throughput path = {}; GPU rung = {}; rayon threads {}",
    model_dir.display(),
    params as f64 / 1e6,
    GemmPath::Throughput.label(),
    rung.label(),
    rayon::current_num_threads(),
  );
  if let Some(gpu) = rung.gpu() {
    println!("gpu tile {:?}, resident weights {:.1} MB", gpu.tile(), gpu.resident_bytes() as f64 / 1e6);
  }
  println!("| batch | tokens | tok/seq | fixed-order s | GFLOPS | throughput s | GFLOPS | speedup | min cosine | seq/s (throughput) | gpu s | GFLOPS | gpu vs fixed | gpu vs throughput | seq/s (gpu) | transfer share |");
  println!("|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|");
  let min_cosine = |a: &[Vec<f32>], b: &[Vec<f32>]| -> f64 {
    a.iter()
      .zip(b)
      .map(|(a, b)| a.iter().zip(b).map(|(x, y)| *x as f64 * *y as f64).sum::<f64>())
      .fold(1.0f64, f64::min)
  };
  // The int8 path's rows (rate + the derived retention bar — the gated test's
  // statistic on the production surface distribution), printed after the table.
  let mut int8_rows: Vec<String> = Vec::new();
  for &batch in &batches {
    let texts: Vec<&str> = pool[..batch].iter().map(String::as_str).collect();
    let tokens: usize = texts.iter().map(|t| encoder.sequence_len(t)).sum();
    let flops = 2.0 * params as f64 * tokens as f64;
    let mut fixed_s = Vec::with_capacity(REPS);
    let mut fast_s = Vec::with_capacity(REPS);
    let mut gpu_s = Vec::with_capacity(REPS);
    let mut fixed_rows = Vec::new();
    let mut fast_rows = Vec::new();
    let mut gpu_rows = Vec::new();
    if let Some(gpu) = rung.gpu() {
      // Warm-up (scratch buffers grow to this batch), then the ledger is reset
      // so the transfer share covers the timed reps alone.
      encoder.embed_batch_with(&texts, GemmPath::Gpu(gpu))?;
      gpu.reset_transfer();
    }
    let mut int8_s = Vec::with_capacity(REPS);
    let mut int8_rows_out = Vec::new();
    for _ in 0..REPS {
      let started = std::time::Instant::now();
      fixed_rows = encoder.embed_batch_with(&texts, GemmPath::FixedOrder)?;
      fixed_s.push(started.elapsed().as_secs_f64());
      let started = std::time::Instant::now();
      fast_rows = encoder.embed_batch_with(&texts, GemmPath::Throughput)?;
      fast_s.push(started.elapsed().as_secs_f64());
      if let Some(gpu) = rung.gpu() {
        let started = std::time::Instant::now();
        gpu_rows = encoder.embed_batch_with(&texts, GemmPath::Gpu(gpu))?;
        gpu_s.push(started.elapsed().as_secs_f64());
      }
      let started = std::time::Instant::now();
      int8_rows_out = encoder.embed_batch_with(&texts, GemmPath::Int8)?;
      int8_s.push(started.elapsed().as_secs_f64());
    }
    {
      // int8 vs fixed-order, and the output-int8 bar (the sidecar's own
      // quantizer dequantized) on the same fixed-order embeddings.
      let cos = |a: &[f32], b: &[f32]| a.iter().zip(b).map(|(x, y)| *x as f64 * *y as f64).sum::<f64>();
      let (mut min_int8, mut min_bar, mut dev_int8, mut dev_bar) = (1.0f64, 1.0f64, 0.0f64, 0.0f64);
      for (f, q) in fixed_rows.iter().zip(&int8_rows_out) {
        let c = cos(f, q);
        let mut codes = vec![0i8; f.len()];
        let scale = vorpal_ann::dense::quantize_row(f, &mut codes);
        let deq: Vec<f32> = codes.iter().map(|&c| c as f32 * scale).collect();
        let norm = deq.iter().map(|v| (*v as f64) * (*v as f64)).sum::<f64>().sqrt();
        let bar = if norm > 0.0 { cos(f, &deq) / norm } else { 0.0 };
        min_int8 = min_int8.min(c);
        min_bar = min_bar.min(bar);
        dev_int8 += 1.0 - c;
        dev_bar += 1.0 - bar;
      }
      let n = batch as f64;
      let int8 = median(&mut int8_s.clone());
      int8_rows.push(format!(
        "| {batch} | {tokens} | {int8:.3} | {:.1} | {:.2}× | {:.2}× | {min_int8:.6} | {:.2e} | {min_bar:.6} | {:.2e} | {} |",
        flops / int8 / 1e9,
        median(&mut fixed_s.clone()) / int8,
        median(&mut fast_s.clone()) / int8,
        dev_int8 / n,
        dev_bar / n,
        if dev_int8 <= dev_bar { "MEETS" } else { "FAILS" },
      ));
    }
    let (fixed, fast) = (median(&mut fixed_s), median(&mut fast_s));
    let gpu_cells = match rung.gpu() {
      Some(gpu) => {
        let report = gpu.transfer_report();
        let device = median(&mut gpu_s);
        let host_copies = (report.upload_secs + report.download_secs) / REPS as f64;
        format!(
          "{device:.3} | {:.1} | {:.6} | {:.6} | {:.0} | {:.1}% host copies ({:.0}+{:.0} MB/batch), device {:.3} s{}",
          flops / device / 1e9,
          min_cosine(&fixed_rows, &gpu_rows),
          min_cosine(&fast_rows, &gpu_rows),
          batch as f64 / device,
          host_copies * 100.0 / device,
          report.bytes_up as f64 / REPS as f64 / 1e6,
          report.bytes_down as f64 / REPS as f64 / 1e6,
          report.device_secs / REPS as f64,
          gpu.fault().map_or(String::new(), |f| format!(" RETIRED: {f}")),
        )
      }
      None => "— | — | — | — | — | —".to_string(),
    };
    println!(
      "| {batch} | {tokens} | {:.1} | {fixed:.3} | {:.1} | {fast:.3} | {:.1} | {:.2}× | {:.6} | {:.0} | {gpu_cells} |",
      tokens as f64 / batch as f64,
      flops / fixed / 1e9,
      flops / fast / 1e9,
      fixed / fast,
      min_cosine(&fixed_rows, &fast_rows),
      batch as f64 / fast,
    );
  }
  // Dispatch-only ceiling: the six shapes of one layer at the largest batch's
  // token count, no host copies — the GPU kernel's own rate, the analogue of
  // the raw `cblas_sgemm` ceiling column.
  if let Some(gpu) = rung.gpu() {
    let texts: Vec<&str> = pool[..largest].iter().map(String::as_str).collect();
    let rows: usize = texts.iter().map(|t| encoder.sequence_len(t)).sum();
    let reps = 20;
    let mut layer_secs = 0.0f64;
    let mut layer_flops = 0.0f64;
    for (dim_in, w, rows_out) in encoder.layer_gemm_shapes(0)? {
      gpu.dispatch_only(dim_in, w, rows_out, rows, 2)?;
      let secs = gpu.dispatch_only(dim_in, w, rows_out, rows, reps)? / reps as f64;
      let flops = 2.0 * rows as f64 * dim_in as f64 * rows_out as f64;
      println!(
        "dispatch-only {rows}×{dim_in}·({rows_out}×{dim_in})ᵀ: {:.4} s, {:.0} GFLOPS",
        secs,
        flops / secs / 1e9
      );
      layer_secs += secs;
      layer_flops += flops;
    }
    println!(
      "dispatch-only ceiling at {rows} tokens: {:.0} GFLOPS over the layer's GEMMs ({:.3} s × {} layers = {:.3} s of pure GEMM per batch)",
      layer_flops / layer_secs / 1e9,
      layer_secs,
      encoder.layers(),
      layer_secs * encoder.layers() as f64
    );
  }
  println!();
  println!(
    "int8 path = {} ({} MB of int8 weights); bar = output-int8 quantization of the same fixed-order embeddings (the published ≥ 97 % retention's perturbation); verdict MEETS ⇔ mean 1-cos(int8) ≤ mean 1-cos(bar)",
    GemmPath::Int8.label(),
    encoder.int8_bytes() / (1 << 20),
  );
  println!("| batch | tokens | int8 s | GOPS | vs fixed | vs throughput | min cos (int8) | mean 1-cos (int8) | min cos (bar) | mean 1-cos (bar) | verdict |");
  println!("|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|");
  for row in &int8_rows {
    println!("{row}");
  }
  // Thread-stability verdict of the throughput path (rayon 1 vs default pool),
  // on the smallest batch — the same statement the gated test prints.
  let texts: Vec<&str> = pool[..batches.iter().copied().min().unwrap_or(1)]
    .iter()
    .map(String::as_str)
    .collect();
  let default_rows = encoder.embed_batch_with(&texts, GemmPath::Throughput)?;
  let single_rows = rayon::ThreadPoolBuilder::new()
    .num_threads(1)
    .build()?
    .install(|| encoder.embed_batch_with(&texts, GemmPath::Throughput))?;
  let same = default_rows
    .iter()
    .flatten()
    .map(|v| v.to_bits())
    .eq(single_rows.iter().flatten().map(|v| v.to_bits()));
  println!(
    "throughput path rayon 1-thread vs default pool: {}",
    if same { "IDENTICAL bytes" } else { "DIFFERENT bytes" }
  );
  Ok(())
}
