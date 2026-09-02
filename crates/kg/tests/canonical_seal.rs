//! The retained-writer determinism theorem, executable (SUBSECOND.md Phase 3): a writer that
//! retracts a file by tombstone and re-ingests its replacement at the tail must
//! `seal_canonical` to artifacts **byte-identical** to a from-scratch writer over the same
//! live file set. This is the keystone the memory-primary daemon stands on — if these bytes
//! ever drift, retained rebuilds silently fork from canonical generations.

use std::borrow::Cow;
use std::fs;
use std::ops::Range;
use std::path::Path;

use vorpal_graph::EdgeType;
use vorpal_kg::{FileBlock, KgWriter};
use vorpal_outline::model::{
  EntryRole, OutlineEntry, OutlineItem, OutlineMember, SourcePosition, SourceRange, SymbolType,
};

fn range(bytes: Range<usize>) -> SourceRange {
  SourceRange {
    byte_offset: bytes,
    start: SourcePosition { line: 0, column: 0 },
    end: SourcePosition { line: 0, column: 0 },
  }
}

fn item<'a>(name: &'a str, signature: &'a str, members: Vec<OutlineMember<'a>>) -> OutlineItem<'a> {
  OutlineItem {
    entry: OutlineEntry {
      role: EntryRole::Item,
      symbol_type: SymbolType::Function,
      name: Cow::Borrowed(name),
      range: range(0..24),
      signature: Cow::Borrowed(signature),
      ast_kind: Cow::Borrowed("function_definition"),
    },
    is_import: false,
    is_exported: true,
    members,
  }
}

fn member<'a>(name: &'a str, signature: &'a str) -> OutlineMember<'a> {
  OutlineMember {
    entry: OutlineEntry {
      role: EntryRole::Member,
      symbol_type: SymbolType::Method,
      name: Cow::Borrowed(name),
      range: range(4..20),
      signature: Cow::Borrowed(signature),
      ast_kind: Cow::Borrowed("function_definition"),
    },
    is_public: true,
  }
}

/// Ingest one file into `writer`, returning its [`FileBlock`] footprint. Mirrors the
/// pipeline's bookkeeping: capture rows/heap/edges before and after, scope identity per file.
fn ingest(writer: &mut KgWriter, path: &str, items: &[OutlineItem<'_>]) -> FileBlock {
  writer.forget_identity_scope();
  let rows_start = writer.node_count() as u32;
  let heap_start = writer.heap_len();
  let edges_start = writer.edges_len() as u32;
  writer.ingest_file(path, items);
  FileBlock {
    rows: rows_start..writer.node_count() as u32,
    heap: heap_start..writer.heap_len(),
    edges: edges_start..writer.edges_len() as u32,
  }
}

fn x_items() -> Vec<OutlineItem<'static>> {
  vec![item("xf", "def xf()", vec![member("xm", "def xm(self)")])]
}

fn y_old_items() -> Vec<OutlineItem<'static>> {
  vec![item("yf_old", "def yf_old()", vec![])]
}

fn y_new_items() -> Vec<OutlineItem<'static>> {
  vec![
    item("yf", "def yf()", vec![member("ym", "def ym(self)")]),
    item("yg", "def yg()", vec![]),
  ]
}

fn z_items() -> Vec<OutlineItem<'static>> {
  vec![item("zf", "def zf()", vec![])]
}

fn artifact_bytes(dir: &Path) -> Vec<(String, Vec<u8>)> {
  let mut out: Vec<(String, Vec<u8>)> = fs::read_dir(dir)
    .expect("read dir")
    .flatten()
    .map(|e| {
      (
        e.file_name().to_string_lossy().into_owned(),
        fs::read(e.path()).expect("read artifact"),
      )
    })
    .collect();
  out.sort_by(|a, b| a.0.cmp(&b.0));
  out
}

#[test]
fn tombstone_and_append_seals_byte_identical_to_scratch() {
  // Retained writer: x, y_old, z ingested; y retracted (by omission from the blocks);
  // y_new appended at the tail.
  let mut retained = KgWriter::new();
  let bx = ingest(&mut retained, "x.py", &x_items());
  let _by_old = ingest(&mut retained, "y.py", &y_old_items());
  let bz = ingest(&mut retained, "z.py", &z_items());
  let by_new = ingest(&mut retained, "y.py", &y_new_items());
  // A resolution edge across files, added AFTER the containment watermark in retained-writer
  // id space: x.py's xf (row 1 of block x) calls y.py's yf (row 1 of block y_new).
  let x_fn = bx.rows.start + 1;
  let y_fn = by_new.rows.start + 1;
  let resolution_from = by_new.edges.end as usize;
  retained.add_edge(
    vorpal_kg::NodeId::new(x_fn as u64),
    vorpal_kg::NodeId::new(y_fn as u64),
    EdgeType::CALLS,
  );
  // Canonical order: path-sorted alive blocks (x.py, y.py→new block, z.py).
  let blocks = [bx.clone(), by_new.clone(), bz.clone()];
  let (live, _lut) = retained.seal_canonical(&blocks, resolution_from);

  // Scratch writer over the same live set, same logical resolution edge.
  let mut scratch = KgWriter::new();
  let sx = ingest(&mut scratch, "x.py", &x_items());
  let sy = ingest(&mut scratch, "y.py", &y_new_items());
  let _sz = ingest(&mut scratch, "z.py", &z_items());
  scratch.add_edge(
    vorpal_kg::NodeId::new((sx.rows.start + 1) as u64),
    vorpal_kg::NodeId::new((sy.rows.start + 1) as u64),
    EdgeType::CALLS,
  );
  let reference = scratch.seal();

  let root = std::env::temp_dir().join(format!("vorpal-canonical-seal-{}", std::process::id()));
  let _ = fs::remove_dir_all(&root);
  let live_dir = root.join("live");
  let ref_dir = root.join("reference");
  live.save(&live_dir).expect("save live");
  reference.save(&ref_dir).expect("save reference");

  let live_artifacts = artifact_bytes(&live_dir);
  let ref_artifacts = artifact_bytes(&ref_dir);
  let live_names: Vec<&String> = live_artifacts.iter().map(|(n, _)| n).collect();
  let ref_names: Vec<&String> = ref_artifacts.iter().map(|(n, _)| n).collect();
  assert_eq!(live_names, ref_names, "artifact sets must match");
  for ((name, live_bytes), (_, ref_bytes)) in live_artifacts.iter().zip(&ref_artifacts) {
    assert_eq!(
      live_bytes, ref_bytes,
      "{name} bytes diverged between retained seal_canonical and scratch seal"
    );
  }
  let _ = fs::remove_dir_all(&root);
}

#[test]
fn seal_canonical_of_untouched_writer_matches_plain_seal() {
  let mut a = KgWriter::new();
  let ba = [
    ingest(&mut a, "x.py", &x_items()),
    ingest(&mut a, "y.py", &y_new_items()),
    ingest(&mut a, "z.py", &z_items()),
  ];
  let watermark = a.edges_len();
  let (live, _lut) = a.seal_canonical(&ba, watermark);

  let mut b = KgWriter::new();
  ingest(&mut b, "x.py", &x_items());
  ingest(&mut b, "y.py", &y_new_items());
  ingest(&mut b, "z.py", &z_items());
  let reference = b.seal();

  let root = std::env::temp_dir().join(format!("vorpal-canonical-noop-{}", std::process::id()));
  let _ = fs::remove_dir_all(&root);
  live.save(&root.join("live")).expect("save live");
  reference.save(&root.join("reference")).expect("save ref");
  let live_artifacts = artifact_bytes(&root.join("live"));
  let ref_artifacts = artifact_bytes(&root.join("reference"));
  assert_eq!(live_artifacts.len(), ref_artifacts.len());
  for ((name, lb), (_, rb)) in live_artifacts.iter().zip(&ref_artifacts) {
    assert_eq!(lb, rb, "{name} diverged on the identity case");
  }
  let _ = fs::remove_dir_all(&root);
}
