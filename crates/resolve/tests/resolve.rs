//! Cross-file resolution (§3.3): scope precedence, visibility, ambiguity, and a real-KG check.

use std::borrow::Cow;

use vorpal_kg::KgWriter;
use vorpal_outline::model::{
  EntryRole, OutlineEntry, OutlineItem, SourcePosition, SourceRange, SymbolType,
};
use vorpal_resolve::{
  Confidence, EdgeType, NodeId, RefKind, Reference, Resolver, Symbol, SymbolKind, SymbolTable,
  resolve_all,
};

fn sym(id: u64, kind: SymbolKind, path: &str, exported: bool) -> Symbol {
  Symbol {
    id: NodeId::new(id),
    kind,
    path: path.to_string(),
    exported,
  }
}

#[test]
fn resolves_local_definition_with_highest_confidence() {
  let mut table = SymbolTable::new();
  table.insert("foo", sym(1, SymbolKind::Function, "a.rs", false));
  let reference = Reference::new(NodeId::new(0), "a.rs", "foo", RefKind::Call);

  let res = Resolver::new().resolve(&table, &reference);
  assert_eq!(res.target, Some(NodeId::new(1)));
  assert_eq!(res.confidence, Confidence::LOCAL);
  assert_eq!(res.edge, EdgeType::CALLS);
}

#[test]
fn resolves_exported_cross_file_definition() {
  let mut table = SymbolTable::new();
  table.insert("bar", sym(2, SymbolKind::Function, "b.rs", true));
  let reference = Reference::new(NodeId::new(0), "a.rs", "bar", RefKind::Call);

  let res = Resolver::new().resolve(&table, &reference);
  assert_eq!(res.target, Some(NodeId::new(2)));
  assert_eq!(res.confidence, Confidence::CROSS_FILE);
}

#[test]
fn private_cross_file_definition_is_not_visible() {
  let mut table = SymbolTable::new();
  table.insert("secret", sym(3, SymbolKind::Function, "b.rs", false));
  let reference = Reference::new(NodeId::new(0), "a.rs", "secret", RefKind::Call);

  let res = Resolver::new().resolve(&table, &reference);
  assert_eq!(
    res.target, None,
    "private symbol in another file is invisible"
  );
  assert_eq!(res.confidence, Confidence::NONE);
  assert_eq!(
    res.candidates, 1,
    "it exists, just isn't reachable — reported, not faked"
  );
}

#[test]
fn local_definition_wins_over_exported_elsewhere() {
  let mut table = SymbolTable::new();
  table.insert("dup", sym(5, SymbolKind::Function, "b.rs", true));
  table.insert("dup", sym(6, SymbolKind::Function, "a.rs", false));
  let reference = Reference::new(NodeId::new(0), "a.rs", "dup", RefKind::Call);

  let res = Resolver::new().resolve(&table, &reference);
  assert_eq!(res.target, Some(NodeId::new(6)), "same-file binding wins");
  assert_eq!(res.confidence, Confidence::LOCAL);
}

#[test]
fn ambiguous_exported_is_labeled_and_deterministic() {
  let mut table = SymbolTable::new();
  table.insert("amb", sym(9, SymbolKind::Function, "c.rs", true));
  table.insert("amb", sym(4, SymbolKind::Function, "b.rs", true));
  let reference = Reference::new(NodeId::new(0), "a.rs", "amb", RefKind::Call);

  let res = Resolver::new().resolve(&table, &reference);
  assert_eq!(
    res.target,
    Some(NodeId::new(4)),
    "deterministic min-id target"
  );
  assert_eq!(res.confidence, Confidence::AMBIGUOUS);
  assert_eq!(res.candidates, 2);
}

#[test]
fn path_imports_resolve_to_file_nodes() {
  let mut table = SymbolTable::new();
  table.insert_file("src/util.ts", NodeId::new(7));

  // `./util` from a sibling file resolves via the importer's own extension.
  let reference = Reference::new(NodeId::new(0), "src/a.ts", "./util", RefKind::Import);
  let res = Resolver::new().resolve(&table, &reference);
  assert_eq!(res.target, Some(NodeId::new(7)));
  assert_eq!(res.confidence, Confidence::CROSS_FILE);

  // `../` navigation and explicit-extension forms also match exactly.
  let reference = Reference::new(
    NodeId::new(0),
    "src/deep/b.ts",
    "../util.ts",
    RefKind::Import,
  );
  assert_eq!(
    Resolver::new().resolve(&table, &reference).target,
    Some(NodeId::new(7))
  );

  // A path miss stays unresolved — exact matches only, never faked.
  let reference = Reference::new(NodeId::new(0), "src/a.ts", "./missing", RefKind::Import);
  assert_eq!(Resolver::new().resolve(&table, &reference).target, None);
}

#[test]
fn unknown_name_is_unresolved() {
  let table = SymbolTable::new();
  let reference = Reference::new(NodeId::new(0), "a.rs", "nope", RefKind::Call);
  let res = Resolver::new().resolve(&table, &reference);
  assert_eq!(res.target, None);
  assert_eq!(res.candidates, 0);
}

#[test]
fn ref_kinds_map_to_edge_types() {
  assert_eq!(RefKind::Call.edge(), EdgeType::CALLS);
  assert_eq!(RefKind::Type.edge(), EdgeType::OF_TYPE);
  assert_eq!(RefKind::Import.edge(), EdgeType::IMPORTS);
  assert_eq!(RefKind::Implements.edge(), EdgeType::IMPLEMENTS);
  assert_eq!(RefKind::Use.edge(), EdgeType::REFERENCES);
}

#[test]
fn resolve_all_reports_stats_and_labeled_edges() {
  let mut table = SymbolTable::new();
  table.insert("known", sym(1, SymbolKind::Function, "b.rs", true));
  table.insert("amb", sym(2, SymbolKind::Function, "b.rs", true));
  table.insert("amb", sym(3, SymbolKind::Function, "c.rs", true));
  let refs = vec![
    Reference::new(NodeId::new(0), "a.rs", "known", RefKind::Call),
    Reference::new(NodeId::new(0), "a.rs", "amb", RefKind::Call),
    Reference::new(NodeId::new(0), "a.rs", "missing", RefKind::Call),
  ];

  let (edges, stats) = resolve_all(&table, &refs, &Resolver::new());
  assert_eq!(stats.resolved, 1);
  assert_eq!(stats.ambiguous, 1);
  assert_eq!(stats.unresolved, 1);
  assert_eq!(
    edges.len(),
    2,
    "resolved + ambiguous both emit labeled edges"
  );
  assert!(
    edges
      .iter()
      .any(|e| e.to == NodeId::new(1) && e.confidence == 90)
  );
}

// --- integration against a real KG -------------------------------------------------------

fn item(
  sym: SymbolType,
  name: &'static str,
  sig: &'static str,
  exported: bool,
) -> OutlineItem<'static> {
  OutlineItem {
    entry: OutlineEntry {
      role: EntryRole::Item,
      symbol_type: sym,
      name: Cow::Borrowed(name),
      range: SourceRange {
        byte_offset: 0..1,
        start: SourcePosition { line: 0, column: 0 },
        end: SourcePosition { line: 0, column: 1 },
      },
      signature: Cow::Borrowed(sig),
      ast_kind: Cow::Borrowed(""),
    },
    is_import: false,
    is_exported: exported,
    members: vec![],
  }
}

#[test]
fn resolves_a_call_across_files_in_a_real_kg() {
  let mut writer = KgWriter::new();
  writer.ingest_file(
    "b.rs",
    &[item(SymbolType::Function, "target", "fn target()", true)],
  );
  writer.ingest_file(
    "a.rs",
    &[item(SymbolType::Function, "caller", "fn caller()", true)],
  );
  let kg = writer.seal();
  let table = SymbolTable::from_kg(&kg);

  let find = |name: &str| {
    (0..kg.node_count() as u64)
      .map(NodeId::new)
      .find(|&id| kg.node(id).is_some_and(|v| v.name == name))
      .unwrap()
  };
  let caller = find("caller");
  let target = find("target");

  let reference = Reference::new(caller, "a.rs", "target", RefKind::Call);
  let res = Resolver::new().resolve(&table, &reference);
  assert_eq!(
    res.target,
    Some(target),
    "cross-file call resolves to the exported def"
  );
  assert_eq!(res.confidence, Confidence::CROSS_FILE);

  // File nodes are not resolution targets.
  assert_eq!(table.candidates("a.rs").len(), 0);
}
