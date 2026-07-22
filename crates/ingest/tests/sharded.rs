//! §7.5 sharded single-writer commit: the sharded apply must produce output byte-identical to
//! the serial single-writer apply — same node ids, same heap layout, same edges, same
//! resolution — because shards are contiguous slices absorbed in order.

use std::fs;

use vorpal_ingest::{Ingestor, OutlineExtractor, Resolver, apply_products_sharded, link_writer};

/// A synthetic corpus big enough to guarantee multiple shards on any machine (shard size is
/// capped at max(len/(2·threads), 16), so ≥100 files always splits at least twice), with
/// cross-file calls, methods, generics, and imports to exercise the whole apply surface.
fn corpus() -> Vec<(String, String)> {
  let mut files = Vec::new();
  for i in 0..100 {
    let path = format!("src/mod_{i:03}.rs");
    let next = (i + 1) % 100;
    let source = format!(
      "use crate::mod_{next:03}::helper_{next};\n\n\
       pub struct Widget{i} {{\n  pub field: u32,\n}}\n\n\
       impl Widget{i} {{\n  pub fn method_{i}(&self) -> u32 {{\n    self.field\n  }}\n}}\n\n\
       pub fn helper_{i}<T: Clone>(value: T) -> T {{\n  helper_{next}(shared());\n  value\n}}\n\n\
       fn shared() -> u32 {{\n  0\n}}\n"
    );
    files.push((path, source));
  }
  files
}

#[test]
fn sharded_apply_is_byte_identical_to_serial_apply() {
  let extractor = OutlineExtractor::new().expect("rules compile");
  let files = corpus();
  let products: Vec<_> = files
    .iter()
    .filter_map(|(path, source)| Some((path.clone(), extractor.extract_product(path, source)?)))
    .collect();
  assert_eq!(products.len(), 100);

  // Serial: the classic single-writer Ingestor path.
  let mut serial = Ingestor::new(OutlineExtractor::new().unwrap());
  for (path, product) in products.clone() {
    serial.ingest_product(&path, product);
  }
  let (serial_kg, serial_stats) = serial.link_and_seal(&Resolver::new());

  // Sharded: parallel per-shard writers, ordered absorption, same global link.
  let (writer, references) = apply_products_sharded(products);
  let (sharded_kg, sharded_stats) = link_writer(writer, references, &Resolver::new());

  assert_eq!(
    serial_stats, sharded_stats,
    "resolution outcomes must match"
  );
  assert_eq!(serial_kg.node_count(), sharded_kg.node_count());

  // Byte-level equality of everything that persists.
  let base = std::env::temp_dir().join(format!("vorpal-sharded-eq-{}", std::process::id()));
  let (a, b) = (base.join("serial"), base.join("sharded"));
  let _ = fs::remove_dir_all(&base);
  serial_kg.save(&a).unwrap();
  sharded_kg.save(&b).unwrap();
  for file in ["nodes.vseg", "strings.heap", "graph.bin"] {
    let serial_bytes = fs::read(a.join(file)).unwrap();
    let sharded_bytes = fs::read(b.join(file)).unwrap();
    assert_eq!(serial_bytes, sharded_bytes, "{file} diverged");
  }
  let _ = fs::remove_dir_all(&base);
}

#[test]
fn sharded_apply_handles_tiny_and_empty_inputs() {
  let extractor = OutlineExtractor::new().expect("rules compile");

  let (writer, references) = apply_products_sharded(Vec::new());
  assert_eq!(writer.node_count(), 0);
  assert!(references.is_empty());

  // Below the shard floor: the serial fallback path, same output shape.
  let product = extractor
    .extract_product("one.rs", "pub fn lone() -> i32 {\n    1\n}\n")
    .unwrap();
  let (writer, references) = apply_products_sharded(vec![("one.rs".into(), product)]);
  assert_eq!(writer.node_count(), 2, "file node + fn node");
  assert!(references.is_empty());
}
