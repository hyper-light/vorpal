//! Regression: parser-swallow recovery through the real build path. The fixture is a
//! trimmed cpython `Objects/object.c`: bare statement-position macros wreck
//! `_PyObject_GetAttrId`'s body, tree-sitter loses its closing brace and parses every
//! later definition INSIDE that body — no top-level ERROR, the byte-ratio health policy
//! calls the file clean, and before the recovery walk the index held nothing after it
//! (measured: 65 of the real file's 142 functions; kernel `net/core/dev.c` lost 422).
//! The graph must carry the swallowed definitions with real spans, resolve calls INTO
//! them, and the health report must name the recovery.

use std::fs;

use vorpal_kg::{EdgeType, Kg, NodeId, SymbolKind};

fn nodes_named(kg: &Kg, name: &str, kind: SymbolKind) -> Vec<NodeId> {
  (0..kg.node_count() as u64)
    .map(NodeId::new)
    .filter(|&n| kg.node(n).is_some_and(|v| v.name == name && v.kind == kind))
    .collect()
}

fn node(kg: &Kg, name: &str, kind: SymbolKind) -> NodeId {
  let found = nodes_named(kg, name, kind);
  assert_eq!(found.len(), 1, "exactly one {kind:?} named {name:?}: {found:?}");
  found[0]
}

#[test]
fn swallowed_tail_definitions_reach_the_graph_and_resolve() {
  let base = std::env::temp_dir().join(format!("vorpal-swallow-shape-{}", std::process::id()));
  let _ = fs::remove_dir_all(&base);
  let src = base.join("src");
  fs::create_dir_all(&src).unwrap();
  let fixture = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/swallow-shape/object_tail.c"
  );
  fs::copy(fixture, src.join("object_tail.c")).unwrap();
  let source = fs::read_to_string(fixture).unwrap();
  let out = base.join("idx");
  let report = vorpal_index::build_index(&src, &out).expect("index build");
  assert_eq!(report.indexed, 1);
  assert_eq!(report.error_files, 1, "the fixture carries the parse damage it models");
  let kg = Kg::load(&out).unwrap();

  // Every definition after the swallow is a node — the label agent's dropped candidates
  // among them — with its real span.
  for name in [
    "PyObject_GetAttr",
    "PyObject_SetAttr",
    "PyObject_IsTrue",
    "PyObject_Not",
    "PyCallable_Check",
    "PyObject_GenericGetAttr",
  ] {
    let id = node(&kg, name, SymbolKind::Function);
    let view = kg.node(id).unwrap();
    let head = source.find(&format!("\n{name}(")).expect("definition head") + 1;
    let (start, end) = view.span;
    assert!(
      (start as usize) < head && head < end as usize,
      "{name}: span {start}..{end} must contain its head at {head}"
    );
  }
  assert_eq!(nodes_named(&kg, "_Py_NoneStruct", SymbolKind::Variable).len(), 1);
  assert_eq!(nodes_named(&kg, "none_as_number", SymbolKind::Variable).len(), 1);
  // Never minted: the swallower's locals, nor the fusion blob the parser named `if`.
  for name in ["result", "oname", "tp", "res"] {
    assert!(nodes_named(&kg, name, SymbolKind::Variable).is_empty(), "{name} leaked");
  }
  assert!(nodes_named(&kg, "if", SymbolKind::Function).is_empty());

  // The swallower keeps its identity but not the file: its span ends before the floor
  // (the first clean lifted definition).
  let swallower = node(&kg, "_PyObject_GetAttrId", SymbolKind::Function);
  let (_, end) = kg.node(swallower).unwrap().span;
  let floor = source.find("PyObject *\nPyObject_GetAttr(").unwrap();
  assert!((end as usize) <= floor, "swallower span {end} must end before the floor {floor}");

  // References attributed by span land on the lifted items: calls INTO a lifted
  // definition resolve, and a call FROM one is attributed to it, not to the swallower.
  let get_attr = node(&kg, "PyObject_GetAttr", SymbolKind::Function);
  let callers: Vec<NodeId> = kg
    .incoming_with_confidence(get_attr, EdgeType::CALLS)
    .into_iter()
    .map(|(from, _)| from)
    .collect();
  let callable_check = node(&kg, "PyCallable_Check", SymbolKind::Function);
  let generic = node(&kg, "PyObject_GenericGetAttr", SymbolKind::Function);
  assert!(callers.contains(&callable_check), "PyCallable_Check -> PyObject_GetAttr: {callers:?}");
  assert!(callers.contains(&generic), "PyObject_GenericGetAttr -> PyObject_GetAttr: {callers:?}");
  assert!(!callers.contains(&swallower), "the swallower must not absorb the lifted bodies' calls");
  let is_true = node(&kg, "PyObject_IsTrue", SymbolKind::Function);
  let not = node(&kg, "PyObject_Not", SymbolKind::Function);
  let is_true_callers: Vec<NodeId> = kg
    .incoming_with_confidence(is_true, EdgeType::CALLS)
    .into_iter()
    .map(|(from, _)| from)
    .collect();
  assert_eq!(is_true_callers, vec![not]);

  // The structural health signal names the recovery.
  let health = vorpal_index::parse_health_report(&out).expect("health report");
  // Eight: the six functions, the two globals. `_PyObject_SetAttributeErrorContext` is
  // fused into the parser's resync blob (its head sits inside an ERROR node) and stays
  // unrecoverable — the parser never produced a node for it.
  assert!(
    health.contains("swallowed tail recovered: 8 definitions lifted from `_PyObject_GetAttrId` (line 16)"),
    "{health}"
  );
  assert!(health.contains("parser-swallow recovery lifted 8 definitions in 1 files"), "{health}");
  let _ = fs::remove_dir_all(&base);
}
