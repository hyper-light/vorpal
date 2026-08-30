//! G-M3 end-to-end: traceable arguments at resolved calls become dataflow.bin rows and
//! DATA_FLOWS edges; absent sidecars degrade; determinism holds.

use std::fs;

use vorpal_index::build_index;
use vorpal_kg::{DataflowStore, EdgeType, Kg};

#[test]
fn flows_persist_and_answer() {
  let base = std::env::temp_dir().join(format!("vorpal-flow-{}", std::process::id()));
  let src = base.join("src");
  let out = base.join("index");
  let _ = fs::remove_dir_all(&base);
  fs::create_dir_all(&src).unwrap();
  fs::write(
    src.join("a.py"),
    "def sink(value, other=None):\n    return value\n\n\
     def source():\n    count = 1\n    cfg = make()\n    return sink(count, other=cfg.size)\n",
  )
  .unwrap();

  build_index(&src, &out).unwrap();
  let dir = vorpal_kg::resolve_index_dir(&out);
  let store = DataflowStore::load(&dir).expect("sidecar loads");
  assert!(!store.is_empty(), "traceable args produced rows");

  let kg = Kg::load(&out).unwrap();
  let source_id = kg
    .callers_of("sink")
    .into_iter()
    .find(|&id| kg.node(id).is_some_and(|v| v.name == "source"))
    .expect("source calls sink");
  let sink_id = kg
    .select(&vorpal_kg::SymbolSelector {
      name: Some("sink"),
      ..Default::default()
    })
    .into_iter()
    .next()
    .expect("sink node");

  // The DATA_FLOWS edge exists beside the calls edge, same endpoints.
  let has_flow_edge = kg
    .out_neighbors(source_id)
    .iter()
    .any(|&(to, e)| to == sink_id && e.base() == EdgeType::DATA_FLOWS);
  assert!(has_flow_edge, "DATA_FLOWS edge emitted");

  // Row detail: positional Var arg `count` at index 0; kwarg FieldAccess `cfg.size`.
  let rows = store.flows_between(source_id.raw() as u32, sink_id.raw() as u32);
  assert!(rows.len() >= 2, "{rows:?}");
  assert!(
    rows.iter().any(|r| r.arg_index == 0 && r.class == 0 && r.expr == Some("count")),
    "{rows:?}"
  );
  assert!(
    rows.iter().any(|r| r.class == 1 && r.expr == Some("cfg.size")),
    "{rows:?}"
  );

  // Trace rendering (G-M5): a reachable walk over data_flows annotates each hop with its
  // flowing arguments (`expr→#param`), joined from the sidecar.
  let rendered = vorpal_index::reachable_query_on(
    &kg,
    Some(&dir),
    &vorpal_index::GraphTarget {
      name: "source".into(),
      ..vorpal_index::GraphTarget::default()
    },
    vorpal_index::Direction::Out,
    &[EdgeType::CALLS, EdgeType::DATA_FLOWS],
    Some(3),
    0,
  )
  .unwrap();
  assert!(
    rendered.contains("[count→#0, cfg.size→#1]→ sink"),
    "hop annotated with flowing args:
{rendered}"
  );

  // Kwarg binding (G-M5): `other=` binds to sink's parameter POSITION 1 by name.
  assert!(
    rows.iter().any(|r| r.expr == Some("cfg.size") && r.param_index == 1),
    "kwarg bound by name: {rows:?}"
  );

  // Determinism: rebuild from scratch → identical generation id (dataflow.bin included in
  // the content fold).
  let out2 = base.join("index2");
  build_index(&src, &out2).unwrap();
  assert_eq!(
    fs::read_to_string(out.join("CURRENT")).unwrap(),
    fs::read_to_string(out2.join("CURRENT")).unwrap()
  );

  let _ = fs::remove_dir_all(&base);
}

/// G-M5: Python kwargs bind to the callee's parameter POSITION by name (reversed keyword
/// order proves it), method calls shift positionals past an explicit `self`, and a keyword
/// no parameter matches is the honest `?` sentinel — never a guessed position.
#[test]
fn python_kwargs_bind_by_name_with_self_offset() {
  let base = std::env::temp_dir().join(format!("vorpal-kwflow-{}", std::process::id()));
  let src = base.join("src");
  let out = base.join("index");
  let _ = fs::remove_dir_all(&base);
  fs::create_dir_all(&src).unwrap();
  fs::write(
    src.join("m.py"),
    "def blend(alpha, beta, **rest):\n    return alpha\n\n\
     class Painter:\n    def draw(self, x, y=0):\n        return x\n\n\
     def scrambled(a, b):\n    return blend(beta=a, alpha=b, gamma=a)\n\n\
     def sketch(k):\n    p = Painter()\n    return p.draw(k, y=k)\n",
  )
  .unwrap();

  build_index(&src, &out).unwrap();
  let dir = vorpal_kg::resolve_index_dir(&out);
  let store = DataflowStore::load(&dir).expect("sidecar loads");
  let kg = Kg::load(&out).unwrap();
  let id_of = |name: &str| {
    kg.select(&vorpal_kg::SymbolSelector {
      name: Some(name),
      ..Default::default()
    })
    .into_iter()
    .next()
    .unwrap_or_else(|| panic!("{name} node"))
    .raw() as u32
  };

  // Reversed keywords: beta=(arg#0)→param#1, alpha=(arg#1)→param#0; gamma matches no
  // parameter (**rest absorbs it) → the u16::MAX sentinel.
  let rows = store.flows_between(id_of("scrambled"), id_of("blend"));
  assert!(
    rows.iter().any(|r| r.arg_index == 0 && r.param_index == 1 && r.expr == Some("a")),
    "beta → param#1: {rows:?}"
  );
  assert!(
    rows.iter().any(|r| r.arg_index == 1 && r.param_index == 0 && r.expr == Some("b")),
    "alpha → param#0: {rows:?}"
  );
  assert!(
    rows.iter().any(|r| r.param_index == u16::MAX),
    "unmatched keyword is the sentinel: {rows:?}"
  );

  // Method call through a constructed receiver: positional x shifts past self to param#1;
  // kwarg y binds param#2 by name.
  let rows = store.flows_between(id_of("sketch"), id_of("draw"));
  assert!(
    rows.iter().any(|r| r.arg_index == 0 && r.param_index == 1 && r.expr == Some("k")),
    "positional past self: {rows:?}"
  );
  assert!(
    rows.iter().any(|r| r.param_index == 2),
    "kwarg y → param#2: {rows:?}"
  );

  let _ = fs::remove_dir_all(&base);
}
