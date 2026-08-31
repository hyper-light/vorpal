//! Cross-file resolution (§3.3): scope precedence, visibility, ambiguity, and a real-KG check.

use std::borrow::Cow;

use vorpal_kg::KgWriter;
use vorpal_outline::model::{
  EntryRole, OutlineEntry, OutlineItem, SourcePosition, SourceRange, SymbolType,
};
use vorpal_resolve::intern::Interner;
use vorpal_resolve::{
  Confidence, EdgeType, NodeId, RefKind, Reference, Resolver, Symbol, SymbolKind, SymbolTable,
  resolve_all,
};

/// One shared session for the whole test binary: tests only ever intern a bounded
/// vocabulary, and `'static` ids keep the assertions free of lifetime plumbing.
fn itn() -> &'static Interner {
  static INTERNER: std::sync::OnceLock<Interner> = std::sync::OnceLock::new();
  INTERNER.get_or_init(Interner::new)
}

fn sym(id: u64, kind: SymbolKind, path: &str, exported: bool) -> Symbol<'static> {
  Symbol {
    id: NodeId::new(id),
    kind,
    path: itn().intern(path),
    exported,
    owner: None,
  }
}

#[test]
fn resolves_local_definition_with_highest_confidence() {
  let mut table = SymbolTable::new();
  table.insert(itn(), "foo", sym(1, SymbolKind::Function, "a.rs", false));
  let reference = Reference::new(itn(), NodeId::new(0), "a.rs", "foo", RefKind::Call);

  table.finalize();
  let res = Resolver::new().resolve(itn(), &table, &reference, None);
  assert_eq!(res.target, Some(NodeId::new(1)));
  assert_eq!(res.confidence, Confidence::LOCAL);
  assert_eq!(res.edge, EdgeType::CALLS);
}

#[test]
fn resolves_exported_cross_file_definition() {
  let mut table = SymbolTable::new();
  table.insert(itn(), "bar", sym(2, SymbolKind::Function, "b.rs", true));
  let reference = Reference::new(itn(), NodeId::new(0), "a.rs", "bar", RefKind::Call);

  table.finalize();
  let res = Resolver::new().resolve(itn(), &table, &reference, None);
  assert_eq!(res.target, Some(NodeId::new(2)));
  assert_eq!(res.confidence, Confidence::CROSS_FILE);
}

#[test]
fn private_cross_file_definition_is_not_visible() {
  let mut table = SymbolTable::new();
  table.insert(itn(), "secret", sym(3, SymbolKind::Function, "b.rs", false));
  let reference = Reference::new(itn(), NodeId::new(0), "a.rs", "secret", RefKind::Call);

  table.finalize();
  let res = Resolver::new().resolve(itn(), &table, &reference, None);
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
  table.insert(itn(), "dup", sym(5, SymbolKind::Function, "b.rs", true));
  table.insert(itn(), "dup", sym(6, SymbolKind::Function, "a.rs", false));
  let reference = Reference::new(itn(), NodeId::new(0), "a.rs", "dup", RefKind::Call);

  table.finalize();
  let res = Resolver::new().resolve(itn(), &table, &reference, None);
  assert_eq!(res.target, Some(NodeId::new(6)), "same-file binding wins");
  assert_eq!(res.confidence, Confidence::LOCAL);
}

#[test]
fn ambiguous_exported_is_labeled_and_deterministic() {
  let mut table = SymbolTable::new();
  table.insert(itn(), "amb", sym(9, SymbolKind::Function, "c.rs", true));
  table.insert(itn(), "amb", sym(4, SymbolKind::Function, "b.rs", true));
  let reference = Reference::new(itn(), NodeId::new(0), "a.rs", "amb", RefKind::Call);

  table.finalize();
  let res = Resolver::new().resolve(itn(), &table, &reference, None);
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
  table.insert_file(itn(), "src/util.ts", NodeId::new(7));

  // `./util` from a sibling file resolves via the importer's own extension.
  let reference = Reference::new(itn(), NodeId::new(0), "src/a.ts", "./util", RefKind::Import);
  table.finalize();
  let res = Resolver::new().resolve(itn(), &table, &reference, None);
  assert_eq!(res.target, Some(NodeId::new(7)));
  assert_eq!(res.confidence, Confidence::CROSS_FILE);

  // `../` navigation and explicit-extension forms also match exactly.
  let reference = Reference::new(
    itn(),
    NodeId::new(0),
    "src/deep/b.ts",
    "../util.ts",
    RefKind::Import,
  );
  assert_eq!(
    Resolver::new().resolve(itn(), &table, &reference, None).target,
    Some(NodeId::new(7))
  );

  // A path miss stays unresolved — exact matches only, never faked.
  let reference = Reference::new(itn(), NodeId::new(0), "src/a.ts", "./missing", RefKind::Import);
  assert_eq!(Resolver::new().resolve(itn(), &table, &reference, None).target, None);
}

#[test]
fn macros_bind_by_inclusion_not_name_globality() {
  use vorpal_resolve::IncludeReach;
  // Two same-named macro definitions (the vendored-parser.h shape) plus one local.
  let mut table = SymbolTable::new();
  table.insert(itn(), "STATE", sym(10, SymbolKind::Macro, "g1/parser.h", true));
  table.insert(itn(), "STATE", sym(11, SymbolKind::Macro, "g2/parser.h", true));
  table.insert(itn(), "LOCAL_M", sym(12, SymbolKind::Macro, "g1/parser.c", true));
  table.finalize();

  // g1/parser.c includes g1/parser.h; g2/parser.c includes g2/parser.h.
  let reach = IncludeReach::from_edges(&[
    (itn().intern("g1/parser.c"), itn().intern("g1/parser.h")),
    (itn().intern("g2/parser.c"), itn().intern("g2/parser.h")),
  ]);

  // Include-reachable: each parser.c binds to ITS OWN grammar's macro — unique, correct.
  let use_g1 = Reference::new(itn(), NodeId::new(0), "g1/parser.c", "STATE", RefKind::Call);
  let res = Resolver::new().resolve(itn(), &table, &use_g1, Some(&reach));
  assert_eq!(res.target, Some(NodeId::new(10)), "g1 reaches only g1's copy");
  assert_eq!(res.confidence, Confidence::CROSS_FILE);
  assert_eq!(res.candidates, 1, "the gate removed the other grammar's copy");

  let use_g2 = Reference::new(itn(), NodeId::new(0), "g2/parser.c", "STATE", RefKind::Call);
  let res = Resolver::new().resolve(itn(), &table, &use_g2, Some(&reach));
  assert_eq!(res.target, Some(NodeId::new(11)), "g2 reaches only g2's copy");

  // Same file needs no include edge at all.
  let use_local = Reference::new(itn(), NodeId::new(0), "g1/parser.c", "LOCAL_M", RefKind::Call);
  let res = Resolver::new().resolve(itn(), &table, &use_local, Some(&reach));
  assert_eq!(res.target, Some(NodeId::new(12)));
  assert_eq!(res.confidence, Confidence::LOCAL);

  // Not include-visible anywhere: masked — reported, never faked (even though the
  // definitions are exported; macros do not export, they include).
  let use_far = Reference::new(itn(), NodeId::new(0), "other/main.c", "STATE", RefKind::Call);
  let res = Resolver::new().resolve(itn(), &table, &use_far, Some(&reach));
  assert_eq!(res.target, None, "no include path ⇒ no binding");
  assert_eq!(res.confidence, Confidence::NONE);
  assert_eq!(res.candidates, 2, "both exist; neither is visible here");

  // No oracle at all (non-C pipelines that built no include graph): same-file still
  // binds, cross-file macro candidacy is withheld rather than guessed.
  let res = Resolver::new().resolve(itn(), &table, &use_local, None);
  assert_eq!(res.target, Some(NodeId::new(12)));
  let res = Resolver::new().resolve(itn(), &table, &use_g1, None);
  assert_eq!(res.target, None, "without reachability evidence, no cross-file macro guess");
}

#[test]
fn root_relative_imports_resolve_by_suffix_with_learned_roots() {
  // The kernel shape: `include/` is the real root, `tools/include/` a shadow copy.
  let mut table = SymbolTable::new();
  table.insert_file(itn(), "./include/linux/a.h", NodeId::new(1));
  table.insert_file(itn(), "./include/linux/b.h", NodeId::new(2));
  table.insert_file(itn(), "./tools/include/linux/a.h", NodeId::new(3));
  table.finalize();

  let import = |from: &str, name: &str| {
    Reference::new(itn(), NodeId::new(0), from, name, RefKind::Import)
  };
  // The corpus's own import stream: `linux/a.h` is satisfied by both roots,
  // `linux/b.h` only by `./include` — so `./include` earns more support.
  let stream = vec![
    import("./kernel/core.c", "linux/a.h"),
    import("./kernel/core.c", "linux/b.h"),
  ];
  table.learn_include_roots(itn(), &stream);

  // Prefix tie (both candidates share only "." with the importer) → support wins:
  // the main tree binds `./include`, not the tools shadow.
  let res = Resolver::new().resolve(itn(), &table, &import("./kernel/core.c", "linux/a.h"), None);
  assert_eq!(res.target, Some(NodeId::new(1)), "support breaks the prefix tie");
  assert_eq!(res.confidence, Confidence::CROSS_FILE);

  // Locality trumps popularity: a tools/ importer shares more prefix with the
  // tools copy and binds it despite `./include`'s greater support.
  let res = Resolver::new().resolve(
    itn(),
    &table,
    &import("./tools/perf/util.c", "linux/a.h"),
    None,
  );
  assert_eq!(res.target, Some(NodeId::new(3)), "nearest prefix wins first");

  // No structural evidence, no guess: a bare basename never suffix-matches…
  let mut bare = SymbolTable::new();
  bare.insert_file(itn(), "./deep/zlib.h", NodeId::new(4));
  bare.finalize();
  let res = Resolver::new().resolve(itn(), &bare, &import("./a.c", "zlib.h"), None);
  assert_eq!(res.target, None, "single-component names carry no path evidence");

  // …and a full tie (equal prefix, equal support) stays honestly unresolved.
  let mut tied = SymbolTable::new();
  tied.insert_file(itn(), "./east/linux/t.h", NodeId::new(5));
  tied.insert_file(itn(), "./west/linux/t.h", NodeId::new(6));
  tied.finalize();
  let stream = vec![import("./main.c", "linux/t.h")];
  tied.learn_include_roots(itn(), &stream);
  let res = Resolver::new().resolve(itn(), &tied, &import("./main.c", "linux/t.h"), None);
  assert_eq!(res.target, None, "a dead tie is reported, never guessed");
}

#[test]
fn unknown_name_is_unresolved() {
  let table = SymbolTable::new();
  let reference = Reference::new(itn(), NodeId::new(0), "a.rs", "nope", RefKind::Call);
  let res = Resolver::new().resolve(itn(), &table, &reference, None);
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
  table.insert(itn(), "known", sym(1, SymbolKind::Function, "b.rs", true));
  table.insert(itn(), "amb", sym(2, SymbolKind::Function, "b.rs", true));
  table.insert(itn(), "amb", sym(3, SymbolKind::Function, "c.rs", true));
  let refs = vec![
    Reference::new(itn(), NodeId::new(0), "a.rs", "known", RefKind::Call),
    Reference::new(itn(), NodeId::new(0), "a.rs", "amb", RefKind::Call),
    Reference::new(itn(), NodeId::new(0), "a.rs", "missing", RefKind::Call),
  ];

  table.finalize();
  let (edges, stats) = resolve_all(itn(), &table, &refs, &Resolver::new(), None);
  assert_eq!(stats.resolved, 1);
  assert_eq!(stats.ambiguous, 1);
  assert_eq!(stats.unresolved(), 1);
  assert_eq!(stats.external, 1, "no `missing` definition exists anywhere");
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
  let table = SymbolTable::from_kg(itn(), &kg);

  let find = |name: &str| {
    (0..kg.node_count() as u64)
      .map(NodeId::new)
      .find(|&id| kg.node(id).is_some_and(|v| v.name == name))
      .unwrap()
  };
  let caller = find("caller");
  let target = find("target");

  let reference = Reference::new(itn(), caller, "a.rs", "target", RefKind::Call);
  let res = Resolver::new().resolve(itn(), &table, &reference, None);
  assert_eq!(
    res.target,
    Some(target),
    "cross-file call resolves to the exported def"
  );
  assert_eq!(res.confidence, Confidence::CROSS_FILE);

  // File nodes are not resolution targets.
  assert_eq!(
    table
      .candidates(itn().intern("a.rs"))
      .len(),
    0
  );
}
