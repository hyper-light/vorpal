//! The recorded sweep behind the content-id fold's HASH_CHUNK constant (no-magic-constants
//! law): times `generation_content_id` over a REAL generation directory at every candidate
//! chunk size, best-of-N wall per size. The chunk size is a fold-protocol constant — the
//! sweep informs which value gets frozen, it never varies at runtime.
//!
//!   cargo run --release -p vorpal-index --example content_id_sweep -- <generation-dir> [reps]
//!
//! Run at two scales minimum (vorpal repo, linux kernel), flat and bucketed layouts; the
//! table lands in docs/wip/SUBSECOND.md §P4.1.

use std::path::Path;
use std::time::Instant;

fn main() {
  let mut args = std::env::args().skip(1);
  let dir = args.next().expect("usage: content_id_sweep <generation-dir> [reps]");
  let dir = Path::new(&dir);
  let reps: usize = args.next().and_then(|r| r.parse().ok()).unwrap_or(3);
  let total_bytes: u64 = std::fs::read_dir(dir)
    .map(|entries| {
      entries
        .flatten()
        .map(|e| {
          if e.path().is_dir() {
            std::fs::read_dir(e.path())
              .map(|inner| inner.flatten().filter_map(|f| f.metadata().ok()).map(|m| m.len()).sum())
              .unwrap_or(0)
          } else {
            e.metadata().map(|m| m.len()).unwrap_or(0)
          }
        })
        .sum()
    })
    .unwrap_or(0);
  println!("generation: {} ({:.1} MiB)", dir.display(), total_bytes as f64 / (1 << 20) as f64);
  println!("{:>10} {:>12} {:>12}", "chunk", "best", "throughput");
  for shift in [18u32, 19, 20, 21, 22, 23, 24, 25] {
    let chunk = 1u64 << shift;
    let mut best = f64::MAX;
    let mut id = String::new();
    for _ in 0..reps {
      let start = Instant::now();
      match vorpal_index::generation_content_id_folded(dir, chunk) {
        Ok(computed) => id = computed,
        Err(err) => {
          eprintln!("FAIL at chunk {chunk}: {err}");
          return;
        }
      }
      best = best.min(start.elapsed().as_secs_f64());
    }
    println!(
      "{:>7} KiB {:>9.2} ms {:>9.1} MB/s  id={}",
      chunk >> 10,
      best * 1e3,
      total_bytes as f64 / best / 1e6,
      &id[..8]
    );
  }
}
