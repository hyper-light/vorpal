//! L1→L3 assembly: outline extraction → interned nodes + containment graph, then queries.

use std::borrow::Cow;

use vorpal_kg::{EdgeType, Kg, KgWriter, NodeId, SymbolKind};
use vorpal_outline::model::{
  EntryRole, OutlineEntry, OutlineItem, OutlineMember, SourcePosition, SourceRange, SymbolType,
};

fn range() -> SourceRange {
  SourceRange {
    byte_offset: 0..1,
    start: SourcePosition { line: 0, column: 0 },
    end: SourcePosition { line: 0, column: 1 },
  }
}

fn entry(
  role: EntryRole,
  sym: SymbolType,
  name: &'static str,
  sig: &'static str,
) -> OutlineEntry<'static> {
  OutlineEntry {
    role,
    symbol_type: sym,
    name: Cow::Borrowed(name),
    range: range(),
    signature: Cow::Borrowed(sig),
    ast_kind: Cow::Borrowed(""),
  }
}

fn item(
  sym: SymbolType,
  name: &'static str,
  sig: &'static str,
  exported: bool,
  members: Vec<OutlineMember<'static>>,
) -> OutlineItem<'static> {
  OutlineItem {
    entry: entry(EntryRole::Item, sym, name, sig),
    is_import: false,
    is_exported: exported,
    members,
  }
}

fn member(sym: SymbolType, name: &'static str, sig: &'static str) -> OutlineMember<'static> {
  OutlineMember {
    entry: entry(EntryRole::Member, sym, name, sig),
    is_public: true,
  }
}

/// A class `Parser` with a method and a field, plus a free function.
fn parser_file() -> Vec<OutlineItem<'static>> {
  vec![
    item(
      SymbolType::Class,
      "Parser",
      "class Parser",
      true,
      vec![
        member(SymbolType::Method, "parse", "parse(input)"),
        member(SymbolType::Field, "pos", "pos: usize"),
      ],
    ),
    item(SymbolType::Function, "helper", "fn helper()", false, vec![]),
  ]
}

fn find(kg: &Kg, name: &str) -> NodeId {
  (0..kg.node_count() as u64)
    .map(NodeId::new)
    .find(|&id| kg.node(id).is_some_and(|v| v.name == name))
    .unwrap_or_else(|| panic!("node {name} not found"))
}

fn defines_names(kg: &Kg, id: NodeId) -> Vec<String> {
  let mut names: Vec<String> = kg
    .defines(id)
    .into_iter()
    .map(|n| kg.node(n).unwrap().name.to_string())
    .collect();
  names.sort();
  names
}

#[test]
fn assembles_nodes_and_containment_edges() {
  let mut writer = KgWriter::new();
  writer.ingest_file("src/parser.rs", &parser_file());
  let kg = writer.seal();

  // File + Parser + parse + pos + helper.
  assert_eq!(kg.node_count(), 5);

  let file = find(&kg, "src/parser.rs");
  let parser = find(&kg, "Parser");
  let parse = find(&kg, "parse");
  let pos = find(&kg, "pos");
  let helper = find(&kg, "helper");

  // Node attributes come back from the sealed columns + heap.
  let pv = kg.node(parser).unwrap();
  assert_eq!(pv.kind, SymbolKind::Class);
  assert_eq!(pv.signature, "class Parser");
  assert_eq!(pv.path, "src/parser.rs");
  assert!(pv.exported);
  assert_eq!(kg.node(file).unwrap().kind, SymbolKind::File);
  assert_eq!(kg.node(parse).unwrap().kind, SymbolKind::Method);
  assert_eq!(kg.node(pos).unwrap().kind, SymbolKind::Field);
  assert!(!kg.node(helper).unwrap().exported);

  // Containment forest: file defines the two top-level items.
  assert_eq!(defines_names(&kg, file), vec!["Parser", "helper"]);
  // Parser has a method and a field.
  assert_eq!(defines_names(&kg, parser), vec!["parse", "pos"]);

  // Edge kinds distinguish method vs field.
  let out: Vec<(NodeId, EdgeType)> = kg.out_neighbors(parser);
  assert!(out.contains(&(parse, EdgeType::HAS_METHOD)));
  assert!(out.contains(&(pos, EdgeType::HAS_FIELD)));

  // Reverse containment (`callersOf`-style CSC read).
  assert_eq!(kg.container_of(parse), Some(parser));
  assert_eq!(kg.container_of(parser), Some(file));
  assert_eq!(kg.container_of(file), None);
}

#[test]
fn dedups_repeated_ingest_of_the_same_file() {
  let mut writer = KgWriter::new();
  writer.ingest_file("a.rs", &parser_file());
  let first = writer.node_count();
  // Re-ingesting the identical file assigns no new ids (canonical dedup).
  writer.ingest_file("a.rs", &parser_file());
  assert_eq!(writer.node_count(), first);
}

#[test]
fn keeps_files_independent() {
  let mut writer = KgWriter::new();
  writer.ingest_file(
    "a.rs",
    &[item(SymbolType::Function, "f", "fn f()", true, vec![])],
  );
  writer.ingest_file(
    "b.rs",
    &[item(SymbolType::Function, "f", "fn f()", true, vec![])],
  );
  let kg = writer.seal();

  // Same symbol name in two files → two distinct nodes (path-qualified identity).
  assert_eq!(kg.node_count(), 4); // 2 files + 2 functions
  let a = find(&kg, "a.rs");
  let b = find(&kg, "b.rs");
  assert_eq!(defines_names(&kg, a), vec!["f"]);
  assert_eq!(defines_names(&kg, b), vec!["f"]);
  // The two `f` nodes have different containers.
  assert_ne!(kg.defines(a)[0], kg.defines(b)[0]);
}
