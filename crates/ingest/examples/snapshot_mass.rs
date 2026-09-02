//! Probe: capture a walk snapshot for one giant C file and report its resident mass
//! against its source size — the numbers that size the tree-cache budget policy.
//! Usage: snapshot_mass <file.c> [file2.c ...]

fn main() {
  // Probe wants the snapshot RETAINED regardless of the default budget; env must be set
  // before the cache policy's first read. Single-threaded at this point.
  unsafe { std::env::set_var("VORPAL_TREE_CACHE_BUDGET", "8589934592") };
  for path in std::env::args().skip(1) {
    let Ok(source) = std::fs::read_to_string(&path) else {
      eprintln!("{path}: unreadable");
      continue;
    };
    let Some(report) = vorpal_ingest::snapshot_mass_probe(&path, &source) else {
      eprintln!("{path}: not extractable");
      continue;
    };
    println!("{path}: {report}");
  }
}
