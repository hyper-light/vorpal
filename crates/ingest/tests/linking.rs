//! Two-pass linking (§3.3): references resolve to real `calls` edges, queryable transitively.

use vorpal_ingest::{
  FileExtractor, Ingestor, KgWriter, NodeDef, NodeId, RefKind, Reference, Resolver, SymbolKind,
};
use vorpal_kg::EdgeType;

/// One shared session for the whole test binary — bounded vocabulary, no lifetime plumbing.
fn itn() -> &'static vorpal_ingest::Interner {
  static INTERNER: std::sync::OnceLock<vorpal_ingest::Interner> = std::sync::OnceLock::new();
  INTERNER.get_or_init(vorpal_ingest::Interner::default)
}


/// Stub: source `"name"` defines an exported function `name`; `"name->callee"` also emits a call
/// reference from `name` to `callee`. Exercises define + reference buffering in one pass.
struct DefRefStub;

impl FileExtractor for DefRefStub {
  fn extract_into<'i>(
    &self,
    interner: &'i vorpal_ingest::Interner,
    path: &str,
    source: &str,
    writer: &mut KgWriter,
    references: &mut Vec<Reference<'i>>,
  ) {
    let (name, callee) = match source.split_once("->") {
      Some((n, c)) => (n, Some(c)),
      None => (source, None),
    };
    let id = writer.define(NodeDef {
      kind: SymbolKind::Function,
      name,
      entity_path: name,
      path,
      signature: "",
      exported: true,
      content_hash: 0,
      span: (0, 0),
    });
    if let Some(callee) = callee {
      references.push(Reference::new(interner, id, path, callee, RefKind::Call));
    }
  }
}

fn find(kg: &vorpal_ingest::Kg, name: &str) -> NodeId {
  (0..kg.node_count() as u64)
    .map(NodeId::new)
    .find(|&id| kg.node(id).is_some_and(|v| v.name == name))
    .unwrap_or_else(|| panic!("node {name} not found"))
}

#[test]
fn resolves_and_links_cross_file_calls_into_the_graph() {
  let mut ing = Ingestor::new(itn(), DefRefStub);
  ing.ingest_source("b.rs", "target"); // exported fn `target`
  ing.ingest_source("a.rs", "caller->target"); // `caller` references `target`
  assert_eq!(ing.pending_references(), 1);

  let (kg, stats) = ing.link_and_seal(&Resolver::new());
  assert_eq!(stats.resolved, 1);
  assert_eq!(stats.unresolved(), 0);

  let caller = find(&kg, "caller");
  let target = find(&kg, "target");

  // A real `calls` edge now exists (resolved cross-file)...
  assert!(
    kg.out_neighbors(caller)
      .iter()
      .any(|&(to, e)| to == target && e.base() == EdgeType::CALLS),
    "expected caller --calls--> target"
  );
  // ...so the §11.5 transitive-callers closure over resolved edges includes `caller`.
  assert!(kg.reachable_in(target).contains(&caller));
}

#[test]
fn unresolved_reference_produces_no_edge() {
  let mut ing = Ingestor::new(itn(), DefRefStub);
  ing.ingest_source("a.rs", "caller->missing"); // `missing` is defined nowhere
  let (kg, stats) = ing.link_and_seal(&Resolver::new());

  assert_eq!(stats.unresolved(), 1);
  assert_eq!(stats.resolved, 0);
  let caller = find(&kg, "caller");
  assert!(
    kg.out_neighbors(caller).is_empty(),
    "no faked edge for an unresolved reference"
  );
}
