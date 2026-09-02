//! Overlay recall probe vs build calibration, same tier: load a committed `ann.bin`,
//! adopt it into an [`vorpal_ann::AnnOverlay`], and run the runtime probe. Comparing the
//! printed value with the generation's `ann.calibration.json` separates probe-set
//! variance (different seeded probes, ±few/320) from any systematic beam difference.
//!
//!   cargo run --release -p vorpal-ann --example overlay_recall_probe -- <gen-dir>

fn main() {
  let dir = std::env::args().nth(1).expect("usage: overlay_recall_probe <generation-dir>");
  let index = vorpal_ann::AnnIndex::load(std::path::Path::new(&dir).join("ann.bin").as_path())
    .expect("ann.bin load");
  let rows = index.len();
  let overlay = vorpal_ann::AnnOverlay::adopt(index).expect("vamana tier");
  let start = std::time::Instant::now();
  let measured = overlay.pool_recall_probe().expect("measurable");
  println!(
    "overlay probe on {rows} rows: {measured:.4} ({} ms) — compare ann.calibration.json",
    start.elapsed().as_millis()
  );
}
