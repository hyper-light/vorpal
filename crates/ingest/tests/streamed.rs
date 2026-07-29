//! §7.5 bounded streaming: `stream_apply` must produce output byte-identical to the batch
//! sharded apply — under generous AND deliberately starved byte budgets — while actually
//! bounding in-flight bytes, propagating errors, and preserving order across skips.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};

use vorpal_ingest::{
  FileProduct, FileStat, OutlineExtractor, Resolver, StreamWork, apply_products_sharded,
  link_writer, link_writer_spilled, stream_apply, stream_apply_spilled,
};

fn corpus() -> Vec<(String, String)> {
  let mut files = Vec::new();
  for i in 0..120 {
    let path = format!("src/mod_{i:03}.rs");
    let next = (i + 1) % 120;
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

fn extracted(files: &[(String, String)]) -> Vec<(String, FileProduct)> {
  let extractor = OutlineExtractor::new().expect("rules compile");
  files
    .iter()
    .filter_map(|(path, source)| Some((path.clone(), extractor.extract_product(path, source)?)))
    .collect()
}

fn entries_for(files: &[(String, String)]) -> Vec<FileStat> {
  files
    .iter()
    .enumerate()
    .map(|(i, (path, source))| FileStat {
      path: path.clone(),
      size: source.len() as u64,
      mtime_ns: i as u64,
    })
    .collect()
}

fn assert_identical_to_batch(budget: u64, tag: &str) {
  let files = corpus();
  let products = extracted(&files);
  let by_path: HashMap<String, FileProduct> = products.iter().cloned().collect();
  let entries = entries_for(&files);

  let (writer, references, stats) = stream_apply(&entries, budget, |entry, _scratch| {
    Ok(StreamWork::Parsed(
      entry.path.clone(),
      by_path[&entry.path].clone(),
    ))
  })
  .expect("stream succeeds");
  assert_eq!(stats.parsed, 120);
  assert_eq!(stats.replayed, 0);
  assert!(
    stats.peak_in_flight_bytes <= budget.max(1),
    "budget must bound in-flight bytes: peak {} > budget {budget} ({tag})",
    stats.peak_in_flight_bytes
  );
  let (streamed_kg, streamed_stats) = link_writer(writer, references, &Resolver::new());

  let (writer, references) = apply_products_sharded(products);
  let (batch_kg, batch_stats) = link_writer(writer, references, &Resolver::new());

  assert_eq!(streamed_stats, batch_stats, "{tag}");
  let base = std::env::temp_dir().join(format!("vorpal-streamed-eq-{tag}-{}", std::process::id()));
  let (a, b) = (base.join("stream"), base.join("batch"));
  let _ = fs::remove_dir_all(&base);
  streamed_kg.save(&a).unwrap();
  batch_kg.save(&b).unwrap();
  for file in ["nodes.vseg", "strings.heap", "graph.bin"] {
    assert_eq!(
      fs::read(a.join(file)).unwrap(),
      fs::read(b.join(file)).unwrap(),
      "{file} diverged ({tag})"
    );
  }
  let _ = fs::remove_dir_all(&base);
}

#[test]
fn streamed_apply_is_byte_identical_to_batch_with_a_generous_budget() {
  assert_identical_to_batch(64 * 1024 * 1024, "generous");
}

#[test]
fn streamed_apply_is_byte_identical_under_a_starved_budget() {
  // Roughly two files' worth: admission constantly blocks, reorder windows stay tiny, and
  // heavy backpressure must not perturb ordering or output.
  assert_identical_to_batch(700, "starved");
}

#[test]
fn skips_advance_order_without_perturbing_the_rest() {
  let files = corpus();
  let products = extracted(&files);
  let by_path: HashMap<String, FileProduct> = products.iter().cloned().collect();
  let entries = entries_for(&files);

  // Stream: every third file is skipped (unreadable/unsupported in real runs).
  let (writer, references, stats) = stream_apply(&entries, 64 * 1024, |entry, _| {
    let index: usize = entry.path[8..11].parse().unwrap();
    if index % 3 == 0 {
      return Ok(StreamWork::Skipped);
    }
    Ok(StreamWork::Replayed(
      entry.path.clone(),
      by_path[&entry.path].clone(),
    ))
  })
  .expect("stream succeeds");
  assert_eq!(stats.replayed, 80);
  let (streamed_kg, streamed_stats) = link_writer(writer, references, &Resolver::new());

  // Batch over exactly the non-skipped products.
  let kept: Vec<_> = products
    .into_iter()
    .enumerate()
    .filter(|(i, _)| i % 3 != 0)
    .map(|(_, product)| product)
    .collect();
  let (writer, references) = apply_products_sharded(kept);
  let (batch_kg, batch_stats) = link_writer(writer, references, &Resolver::new());

  assert_eq!(streamed_stats, batch_stats);
  assert_eq!(streamed_kg.node_count(), batch_kg.node_count());
}

#[test]
fn first_error_aborts_the_stream_without_hanging() {
  let files = corpus();
  let by_path: HashMap<String, FileProduct> = extracted(&files).into_iter().collect();
  let entries = entries_for(&files);
  let attempts = AtomicU64::new(0);

  let result = stream_apply(&entries, 8 * 1024, |entry, _| {
    let n = attempts.fetch_add(1, Ordering::Relaxed);
    if n == 57 {
      return Err(io::Error::other("injected cache-write failure"));
    }
    Ok(StreamWork::Parsed(
      entry.path.clone(),
      by_path[&entry.path].clone(),
    ))
  });
  let err = match result {
    Err(err) => err,
    Ok(_) => panic!("injected failure must surface"),
  };
  assert!(err.to_string().contains("injected"), "{err}");
}

/// The spilled configuration (references on disk between commit and resolve) must be
/// byte-identical to the in-RAM path: same graph bytes, same stats, spill file gone after.
#[test]
fn spilled_references_are_byte_identical_to_the_ram_path() {
  let files = corpus();
  let products = extracted(&files);
  let by_path: HashMap<String, FileProduct> = products.iter().cloned().collect();
  let entries = entries_for(&files);

  let base = std::env::temp_dir().join(format!("vorpal-spill-eq-{}", std::process::id()));
  let _ = fs::remove_dir_all(&base);
  fs::create_dir_all(&base).unwrap();
  let spill_path = base.join(".refs.spill");

  // Exercise the disk-streamed heap too: the merged writer's strings must round-trip
  // identically through write-through + map-back.
  let heap_stream = base.join("strings.heap.tmp");
  let (writer, spill, stats) = stream_apply_spilled(
    &entries,
    64 * 1024 * 1024,
    &spill_path,
    Some(&heap_stream),
    None,
    |entry, _scratch| {
      Ok(StreamWork::Parsed(
        entry.path.clone(),
        by_path[&entry.path].clone(),
      ))
    },
  )
  .expect("spilled stream succeeds");
  assert_eq!(stats.parsed, 120);
  assert!(spill.count() > 0, "corpus produces references");
  let (spilled_kg, spilled_stats, _evidence) =
    link_writer_spilled(writer, spill, &Resolver::new()).expect("spilled link succeeds");
  assert!(
    !spill_path.exists(),
    "spill is deleted once resolution has streamed it"
  );

  let (writer, references, _) = stream_apply(&entries, 64 * 1024 * 1024, |entry, _scratch| {
    Ok(StreamWork::Parsed(
      entry.path.clone(),
      by_path[&entry.path].clone(),
    ))
  })
  .expect("stream succeeds");
  let (ram_kg, ram_stats) = link_writer(writer, references, &Resolver::new());

  assert_eq!(spilled_stats, ram_stats);
  let (a, b) = (base.join("spilled"), base.join("ram"));
  spilled_kg.save(&a).unwrap();
  ram_kg.save(&b).unwrap();
  for file in ["nodes.vseg", "strings.heap", "graph.bin"] {
    assert_eq!(
      fs::read(a.join(file)).unwrap(),
      fs::read(b.join(file)).unwrap(),
      "{file} diverged between spilled and RAM reference paths"
    );
  }
  let _ = fs::remove_dir_all(&base);
}
