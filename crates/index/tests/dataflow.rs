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
