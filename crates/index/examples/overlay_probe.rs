//! Per-op cost probe for AnnOverlay at real corpus scale: load the committed kernel tier,
//! adopt it, and time inserts/deletes/searches. Scratch tooling for profiling sessions.
use vorpal_ann::{AnnIndex, AnnOverlay};

#[cfg(feature = "jemalloc")]
#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

fn vector_for(id: u64, dim: usize) -> Vec<f32> {
  let mut state = id.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0x5EED;
  let mut v: Vec<f32> = (0..dim)
    .map(|_| {
      state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
      let mut z = state;
      z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
      z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
      ((z ^ (z >> 31)) as f64 / u64::MAX as f64) as f32 - 0.5
    })
    .collect();
  let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
  for x in &mut v {
    *x /= norm.max(1e-9);
  }
  v
}

fn main() {
  let path = std::env::args().nth(1).expect("usage: overlay_probe <ann.bin>");
  let t0 = std::time::Instant::now();
  let base = AnnIndex::load(std::path::Path::new(&path)).expect("load tier");
  eprintln!("load: {:?} ({} rows)", t0.elapsed(), base.len());
  let t0 = std::time::Instant::now();
  let mut overlay = AnnOverlay::adopt(base).expect("vamana tier");
  eprintln!("adopt: {:?}", t0.elapsed());

  let dim = 256usize;
  let t0 = std::time::Instant::now();
  for i in 0..200u64 {
    overlay.insert(u64::MAX - i, &vector_for(i, dim));
  }
  let ins = t0.elapsed();
  eprintln!("200 inserts: {:?} ({:?}/op)", ins, ins / 200);

  let t0 = std::time::Instant::now();
  for i in 0..200u64 {
    overlay.delete(i * 9973);
  }
  let del = t0.elapsed();
  eprintln!("200 deletes: {:?} ({:?}/op)", del, del / 200);

  let t0 = std::time::Instant::now();
  let mut acc = 0usize;
  for i in 0..50u64 {
    acc += overlay.search_pool(&vector_for(7_000_000 + i, dim), 80).len();
  }
  let q = t0.elapsed();
  eprintln!("50 searches(l=80): {:?} ({:?}/op, pool sum {})", q, q / 50, acc);
  eprintln!("live {} dead_fraction {:.5}", overlay.live_len(), overlay.dead_fraction());
}
