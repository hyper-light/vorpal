//! The one CSC law: a sealed graph's in-coming adjacency is scattered over the SRC-MAJOR
//! enumeration, never the raw edge-log order — the order every other producer (the bucketed
//! slab loader, the compose lanes) already used. Before 2026-09-05 the writer sealed in log
//! order unless a flag nothing set asked otherwise, so a cold generation's derived
//! `graph.bin` differed from an incremental one's for the same tree (718 incoming rows on
//! ast-grep): same edges, different per-destination order, observable by bounded traversals.

use std::borrow::Cow;
use std::ops::Range;

use vorpal_graph::{EdgeType, Graph};
use vorpal_kg::{KgWriter, NodeId};
use vorpal_outline::model::{
  EntryRole, OutlineEntry, OutlineItem, SourcePosition, SourceRange, SymbolType,
};

fn range(bytes: Range<usize>) -> SourceRange {
  SourceRange {
    byte_offset: bytes,
    start: SourcePosition { line: 0, column: 0 },
    end: SourcePosition { line: 0, column: 0 },
  }
}

fn function<'a>(name: &'a str) -> OutlineItem<'a> {
  OutlineItem {
    entry: OutlineEntry {
      role: EntryRole::Item,
      symbol_type: SymbolType::Function,
      name: Cow::Borrowed(name),
      range: range(0..24),
      signature: Cow::Borrowed(name),
      ast_kind: Cow::Borrowed("function_definition"),
    },
    is_import: false,
    is_exported: true,
    members: vec![],
  }
}

/// Edges appended in an order that is NOT source-major (a later source's call lands before
/// an earlier source's): the sealed CSC must still list a destination's sources ascending.
#[test]
fn sealed_incoming_adjacency_is_source_major_not_log_order() {
  let mut writer = KgWriter::new();
  writer.ingest_file("a.py", &[function("f0"), function("f1"), function("f2"), function("f3")]);
  // Node 0 is the File node; the functions are 1..=4.
  let (f1, f2, f3, f4) = (NodeId::new(1), NodeId::new(2), NodeId::new(3), NodeId::new(4));
  let log: Vec<(NodeId, NodeId, EdgeType)> = vec![
    (f3, f1, EdgeType::CALLS),
    (f2, f1, EdgeType::CALLS),
    (f4, f1, EdgeType::CALLS),
    (f3, f2, EdgeType::REFERENCES),
    (f2, f2, EdgeType::CALLS),
    (f4, f2, EdgeType::CALLS),
  ];
  for &(from, to, edge) in &log {
    writer.add_edge(from, to, edge);
  }
  let kg = writer.seal();

  // In-neighbours of f1 in source order: the File node's containment edge (source 0), then
  // the calls from 2, 3, 4 — the log said 3, 2, 4.
  let into_f1: Vec<u64> = kg.in_neighbors(f1).into_iter().map(|(from, _)| from.raw()).collect();
  assert_eq!(into_f1, vec![0, 2, 3, 4], "in-neighbours must be source-major");
  // f2 keeps duplicate multiplicity and payloads, in source order: containment from 0, then
  // (2, calls), (3, refs), (4, calls).
  let into_f2: Vec<(u64, EdgeType)> = kg
    .in_neighbors(f2)
    .into_iter()
    .map(|(from, edge)| (from.raw(), edge))
    .collect();
  assert_eq!(into_f2[0].0, 0, "containment edge first: {into_f2:?}");
  assert_eq!(
    into_f2[1..].to_vec(),
    vec![(2, EdgeType::CALLS), (3, EdgeType::REFERENCES), (4, EdgeType::CALLS)]
  );
  // Out-adjacency is the log's per-source restriction, unchanged by the law.
  let out_f3: Vec<u64> = kg.out_neighbors(f3).into_iter().map(|(to, _)| to.raw()).collect();
  assert_eq!(out_f3, vec![1, 2]);
}

/// The law at the graph level: `compact_src_major` over an interleaved log equals
/// `from_parts` over the src-major enumeration byte for byte, and differs from the raw
/// `compact` exactly where the log interleaves sources.
#[test]
fn compact_src_major_equals_from_parts_over_the_src_major_enumeration() {
  let mut log = vorpal_graph::EdgeLog::default();
  for &(s, d, e) in &[(3u32, 1u32, EdgeType::CALLS), (2, 1, EdgeType::CALLS), (4, 1, EdgeType::CALLS), (2, 2, EdgeType::CALLS)] {
    log.push(s, d, e);
  }
  let law = Graph::compact_src_major(5, &log);
  let raw = Graph::compact(5, &log);
  // Src-major enumeration of the same multiset.
  let srcs = [2u32, 2, 3, 4];
  let dsts = [1u32, 2, 1, 1];
  let etypes: Vec<u16> = std::iter::repeat_n(EdgeType::CALLS.0, 4).collect();
  let parts = Graph::from_parts(5, &srcs, &dsts, &etypes);
  let mut law_bytes = Vec::new();
  law.write_to(&mut law_bytes).unwrap();
  let mut parts_bytes = Vec::new();
  parts.write_to(&mut parts_bytes).unwrap();
  let mut raw_bytes = Vec::new();
  raw.write_to(&mut raw_bytes).unwrap();
  assert_eq!(law_bytes, parts_bytes, "the writer's law and the slab loader's order agree");
  assert_ne!(law_bytes, raw_bytes, "the raw log order is a different adjacency order");
}
