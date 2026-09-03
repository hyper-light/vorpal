//! Diagnostic: the product byte-identity oracle across two builds. Prints one line per
//! banked product — `xxh3(body) <tab> swallow-count <tab> lifted <tab> path` — where the
//! body is every product byte AFTER the identity header (magic, version, stamps, source
//! digest, extraction identity), so two builds under different rule digests compare on
//! extraction OUTPUT alone. Diff two runs: every differing path must carry a swallow
//! recovery on the new side (the diagnosis fired); any other difference is a regression.
//! Usage: product_hashes <index-dir>
use vorpal_ingest::PackReader;

fn main() {
  let dir = std::path::PathBuf::from(std::env::args().nth(1).expect("index dir"));
  let dir = vorpal_kg::resolve_index_dir(&dir);
  let pack = PackReader::open(&dir).expect("product pack in the generation");
  let mut rows: Vec<(String, u64, usize, u64)> = pack
    .entries()
    .map(|(path, bytes)| {
      // Canonical body across v19 (no swallow section) and v20: everything after the
      // identity header, with the swallow section (a v20 insertion right after the
      // error spans) cut out, so the two generations compare on extraction output alone.
      let version = bytes
        .get(4..8)
        .and_then(|b| b.try_into().ok())
        .map(u32::from_le_bytes)
        .unwrap_or(0);
      let u32_at = |at: usize| {
        bytes
          .get(at..at + 4)
          .and_then(|b| b.try_into().ok())
          .map(u32::from_le_bytes)
          .unwrap_or(0) as usize
      };
      // header 40 + error_nodes 4 + error_bytes 8 = 52, then the span count.
      let spans_end = 56 + u32_at(52) * 8;
      let mut h = xxhash_rust::xxh3::Xxh3::new();
      h.update(bytes.get(40..spans_end.min(bytes.len())).unwrap_or(&[]));
      let rest = if version >= 20 {
        spans_end + 4 + u32_at(spans_end) * 8
      } else {
        spans_end
      };
      h.update(bytes.get(rest..).unwrap_or(&[]));
      let swallows = vorpal_ingest::peek_product_swallows(bytes).unwrap_or_default();
      let lifted: u64 = swallows.iter().map(|s| u64::from(s.lifted)).sum();
      (path.to_string(), h.digest(), swallows.len(), lifted)
    })
    .collect();
  rows.sort();
  let out = std::io::stdout();
  let mut out = out.lock();
  use std::io::Write as _;
  for (path, hash, swallows, lifted) in rows {
    let _ = writeln!(out, "{hash:016x}\t{swallows}\t{lifted}\t{path}");
  }
}
