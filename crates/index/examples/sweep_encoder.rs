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
  println!(
    "model {} — non-embedding params {:.1}M; throughput path = {}; rayon threads {}",
    model_dir.display(),
    params as f64 / 1e6,
    GemmPath::Throughput.label(),
    rayon::current_num_threads(),
  );
  println!("| batch | tokens | tok/seq | fixed-order s | GFLOPS | throughput s | GFLOPS | speedup | min cosine | seq/s (throughput) |");
  println!("|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|");
  for &batch in &batches {
    let texts: Vec<&str> = pool[..batch].iter().map(String::as_str).collect();
    let tokens: usize = texts.iter().map(|t| encoder.sequence_len(t)).sum();
    let flops = 2.0 * params as f64 * tokens as f64;
    let mut fixed_s = Vec::with_capacity(REPS);
    let mut fast_s = Vec::with_capacity(REPS);
    let mut fixed_rows = Vec::new();
    let mut fast_rows = Vec::new();
    for _ in 0..REPS {
      let started = std::time::Instant::now();
      fixed_rows = encoder.embed_batch_with(&texts, GemmPath::FixedOrder)?;
      fixed_s.push(started.elapsed().as_secs_f64());
      let started = std::time::Instant::now();
      fast_rows = encoder.embed_batch_with(&texts, GemmPath::Throughput)?;
      fast_s.push(started.elapsed().as_secs_f64());
    }
    let min_cosine = fixed_rows
      .iter()
      .zip(&fast_rows)
      .map(|(a, b)| a.iter().zip(b).map(|(x, y)| *x as f64 * *y as f64).sum::<f64>())
      .fold(1.0f64, f64::min);
    let (fixed, fast) = (median(&mut fixed_s), median(&mut fast_s));
    println!(
      "| {batch} | {tokens} | {:.1} | {fixed:.3} | {:.1} | {fast:.3} | {:.1} | {:.2}× | {min_cosine:.6} | {:.0} |",
      tokens as f64 / batch as f64,
      flops / fixed / 1e9,
      flops / fast / 1e9,
      fixed / fast,
      batch as f64 / fast,
    );
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
