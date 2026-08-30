//! Replay-gate attribution: for a committed generation, check every condition the
//! incremental stream's pack-replay gate applies, against the generation's OWN manifest
//! and against a fresh stat of the tree. Prints the first failing condition per sampled
//! file plus aggregate counts — the tool that answers "why did 0 of 72k files replay?".
//!
//!   cargo run --release -p vorpal-index --example replay_probe -- <generation-dir>

use std::path::Path;

use vorpal_ingest::{
  Manifest, PackReader, extraction_identity_for_path, peek_product_digest,
  peek_product_grammar_digest, peek_product_stamps, validate_product,
};

fn main() {
  let gen_dir = std::env::args().nth(1).expect("usage: replay_probe <generation-dir>");
  let gen_dir = Path::new(&gen_dir);
  let manifest = Manifest::load(&gen_dir.join("manifest.bin")).expect("manifest load");
  let pack = PackReader::open(gen_dir);
  let Some(pack) = pack else {
    println!("FAIL: PackReader::open returned None — no pack at {}", gen_dir.display());
    return;
  };
  let rules_digest = vorpal_ingest::OutlineExtractor::new()
    .expect("extractor")
    .rules_digest();

  let mut missing = 0u64;
  let mut stamp_manifest = 0u64;
  let mut stamp_fresh = 0u64;
  let mut grammar = 0u64;
  let mut digest_absent = 0u64;
  let mut invalid = 0u64;
  let mut ok = 0u64;
  let mut printed = 0;
  for entry in manifest.entries() {
    let Some(bytes) = pack.get(&entry.path) else {
      missing += 1;
      if printed < 5 {
        println!("MISSING from pack: {}", entry.path);
        printed += 1;
      }
      continue;
    };
    let stamps = peek_product_stamps(bytes);
    if stamps != Some((entry.size, entry.mtime_ns)) {
      stamp_manifest += 1;
      if printed < 5 {
        println!(
          "STAMP vs manifest: {} pack={stamps:?} manifest=({}, {})",
          entry.path, entry.size, entry.mtime_ns
        );
        printed += 1;
      }
      continue;
    }
    let fresh = std::fs::metadata(&entry.path).ok().map(|m| {
      use std::os::unix::fs::MetadataExt;
      (m.size(), m.mtime() as u64 * 1_000_000_000 + m.mtime_nsec() as u64)
    });
    if fresh != Some((entry.size, entry.mtime_ns)) {
      stamp_fresh += 1;
      if printed < 5 {
        println!(
          "STAMP vs fresh stat: {} manifest=({}, {}) fresh={fresh:?}",
          entry.path, entry.size, entry.mtime_ns
        );
        printed += 1;
      }
      continue;
    }
    if peek_product_grammar_digest(bytes) != extraction_identity_for_path(&entry.path, rules_digest)
    {
      grammar += 1;
      if printed < 5 {
        println!("GRAMMAR digest mismatch: {}", entry.path);
        printed += 1;
      }
      continue;
    }
    if peek_product_digest(bytes).is_none() {
      digest_absent += 1;
      if printed < 5 {
        println!("SOURCE digest absent: {}", entry.path);
        printed += 1;
      }
      continue;
    }
    if !validate_product(bytes) {
      invalid += 1;
      if printed < 5 {
        println!("INVALID product: {}", entry.path);
        printed += 1;
      }
      continue;
    }
    ok += 1;
  }
  println!(
    "total={} ok={ok} missing={missing} stamp_vs_manifest={stamp_manifest} \
     stamp_vs_fresh={stamp_fresh} grammar={grammar} digest_absent={digest_absent} \
     invalid={invalid}",
    manifest.entries().len(),
  );
}
