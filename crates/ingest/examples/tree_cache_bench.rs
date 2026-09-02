//! Repeat-edit bench: the long-lived-process scenario the tree cache exists for.
//! Usage: tree_cache_bench <file> [rounds]
use std::time::Instant;

fn main() {
  let path = std::env::args().nth(1).expect("file");
  let rounds: usize = std::env::args().nth(2).and_then(|v| v.parse().ok()).unwrap_or(5);
  let source = std::fs::read_to_string(&path).expect("read");
  let extractor = vorpal_ingest::OutlineExtractor::new().expect("rules");

  let t = Instant::now();
  let prime = extractor.extract_product("bench.c", &source).expect("prime");
  println!("prime (cold whole parse): {} ms", t.elapsed().as_millis());

  let mut reference_bytes = Vec::new();
  vorpal_ingest::encode_product_into(&prime, &mut reference_bytes);

  let mut current = source.clone();
  for round in 1..=rounds {
    // Realistic editing: each save derives from the PREVIOUS state — a line near the
    // top on even rounds, near the middle on odd ones.
    let edited = if round % 2 == 0 {
      format!("// save {round}\n{current}")
    } else {
      let mid = current.len() / 2;
      let cut = current[..mid].rfind('\n').map_or(mid, |i| i + 1);
      format!("{}// mid save {round}\n{}", &current[..cut], &current[cut..])
    };
    current = edited.clone();
    let t = Instant::now();
    let product = extractor.extract_product("bench.c", &edited).expect("edited");
    let ms = t.elapsed().as_millis();
    // Byte-truth every round against a fresh parse of the same edited text.
    let fresh = extractor.extract_product("fresh.c", &edited).expect("fresh");
    let (mut a, mut b) = (Vec::new(), Vec::new());
    vorpal_ingest::encode_product_into(&product, &mut a);
    vorpal_ingest::encode_product_into(&fresh, &mut b);
    assert_eq!(a, b, "round {round}: divergence");
    println!("round {round}: {} ms (byte-verified)", ms);
  }
}
