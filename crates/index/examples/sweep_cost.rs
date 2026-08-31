//! Measures the daemon liveness-backstop sweep body at a given source root: the
//! `Manifest::scan` walk (stat every handled file) that dominates
//! `LiveOverlay::stat_changes` — the two-pointer diff against the retained manifest is a
//! linear in-RAM pass over the same entries. This is the recorded-sweep evidence behind
//! `BACKSTOP_OVERHEAD_INVERSE` in crates/mcp/src/server.rs: the backstop period is
//! 100× this cost, so the table this prints IS the period table, corpus by corpus.
//!
//!   cargo run --release -p vorpal-index --example sweep_cost -- <root> [rounds]

use std::time::Instant;

fn main() {
  let mut args = std::env::args().skip(1);
  let Some(root) = args.next() else {
    eprintln!("usage: sweep_cost <root> [rounds]");
    std::process::exit(2);
  };
  let rounds: usize = args
    .next()
    .and_then(|raw| raw.parse().ok())
    .unwrap_or(3);
  let extractor = match vorpal_ingest::OutlineExtractor::new() {
    Ok(extractor) => extractor,
    Err(err) => {
      eprintln!("extractor init failed: {err}");
      std::process::exit(1);
    }
  };
  let root = std::path::PathBuf::from(root);
  for round in 0..rounds {
    let started = Instant::now();
    match vorpal_ingest::Manifest::scan(&root, |path| extractor.handles(path)) {
      Ok(manifest) => {
        let cost = started.elapsed();
        println!(
          "round {} | files {} | scan {:?} | backstop period (100x) {:?}",
          round,
          manifest.entries().len(),
          cost,
          cost * 100,
        );
      }
      Err(err) => {
        eprintln!("scan failed: {err}");
        std::process::exit(1);
      }
    }
  }
}
