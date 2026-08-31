//! The recorded semantic-engine cost sweep: Vamana beam vs flat exact scan at a
//! geometric grid of fetch widths, on a WARM index. Dev-only target — it compiles
//! solely under `--features bench-internals` (see `required-features` in Cargo.toml),
//! never into any production binary. The `exhaustive_cutover` fit in
//! crates/index/src/lib.rs and the tables in docs/wip/BENCHMARKS.md come from here:
//!
//! ```text
//! cargo run --release -p vorpal-index --features bench-internals \
//!   --example sweep_semantic -- <index-dir> [take ...]
//! ```
//!
//! Engines are interleaved (beam, scan, beam, scan …) so machine drift hits both
//! equally; each cell is the median over 8 fixed queries × 3 reps.

use std::path::Path;

use vorpal_ann::AnnIndex;
use vorpal_index::bench;
use vorpal_kg::Kg;

const REPS: usize = 3;

/// Fixed descriptive queries: engine cost is driven by take and n, not query content —
/// a fixed set keeps reruns comparable across machines and sessions.
const QUERIES: [&str; 8] = [
  "socket buffer alloc",
  "page fault handler",
  "tcp congestion window",
  "mutex lock acquire",
  "inode lookup path",
  "interrupt request register",
  "dma coherent alloc",
  "scheduler pick next task",
];

fn median(samples: &mut [f64]) -> f64 {
  samples.sort_by(f64::total_cmp);
  samples.get(samples.len() / 2).copied().unwrap_or(0.0)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
  let args: Vec<String> = std::env::args().skip(1).collect();
  let Some(index) = args.first() else {
    return Err(
      "usage: sweep_semantic <index-dir> [take ...]  (default: geometric ×2 grid 400…51200)"
        .into(),
    );
  };
  let index = Path::new(index);
  let takes: Vec<usize> = if args.len() > 1 {
    args[1..]
      .iter()
      .map(|t| t.parse::<usize>())
      .collect::<Result<Vec<usize>, _>>()?
  } else {
    vec![400, 800, 1600, 3200, 6400, 12800, 25600, 51200]
  };

  if !bench::ann_tier_fresh(index)? {
    return Err("ann tier absent/stale — warm first: vorpal-index __warm-ann <index>".into());
  }
  let generation_dir = vorpal_kg::resolve_index_dir(index);
  let kg = Kg::load(&generation_dir)?;
  let ann = AnnIndex::load(&generation_dir.join("ann.bin"))?;
  let ids = bench::semantic_rows(&kg);
  let query_vecs: Vec<Vec<f32>> = QUERIES.iter().map(|q| bench::embed_query(q)).collect();
  println!(
    "n={} semantic rows; median over {} queries x {REPS} reps, interleaved",
    ids.len(),
    QUERIES.len()
  );
  println!("take\tbeam_ms\tscan_ms");
  for &take in &takes {
    let mut beam_samples = Vec::new();
    let mut scan_samples = Vec::new();
    for _ in 0..REPS {
      for query_vec in &query_vecs {
        let started = std::time::Instant::now();
        std::hint::black_box(ann.search(query_vec, take));
        beam_samples.push(started.elapsed().as_secs_f64() * 1e3);
        let started = std::time::Instant::now();
        std::hint::black_box(vorpal_ann::exhaustive_semantic(
          bench::embed_dim(),
          &ids,
          |i, row| bench::embed_row(&kg, ids[i], row),
          query_vec,
          take,
        ));
        scan_samples.push(started.elapsed().as_secs_f64() * 1e3);
      }
    }
    println!(
      "{take}\t{:.2}\t{:.2}",
      median(&mut beam_samples),
      median(&mut scan_samples)
    );
  }
  Ok(())
}
