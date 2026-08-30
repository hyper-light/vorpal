//! Runtime-trace ingestion (ADOPTION #26): folded stacks become `observed.bin` rows —
//! evidence of calls the static graph can never prove (function pointers, dynamic
//! dispatch). Conservative by construction: unknown/ambiguous frames break the chain and
//! are counted; a rebuild invalidates the sidecar instead of carrying stale node ids.

use std::fs;

use vorpal_index::build_index;
use vorpal_kg::{Kg, NodeId, SymbolKind};

fn node(kg: &Kg, name: &str) -> NodeId {
  (0..kg.node_count() as u64)
    .map(NodeId::new)
    .find(|&n| kg.node(n).is_some_and(|v| v.name == name && v.kind == SymbolKind::Function))
    .unwrap_or_else(|| panic!("{name}"))
}

#[test]
fn folded_stacks_become_observed_rows_with_static_flags() {
  let base = std::env::temp_dir().join(format!("vorpal-observed-{}", std::process::id()));
  let src = base.join("src");
  let out = base.join("index");
  let _ = fs::remove_dir_all(&base);
  fs::create_dir_all(&src).unwrap();
  // `dispatch` calls its handler through a function pointer: statically invisible.
  fs::write(
    src.join("a.rs"),
    "pub fn work() {}\n\
     pub fn handler() { work(); }\n\
     pub fn dispatch(f: fn()) { f(); }\n\
     pub fn main_loop() { dispatch(handler); }\n",
  )
  .unwrap();
  build_index(&src, &out).unwrap();

  // Two stacks: one clean, one with an unknown frame (profiler noise) that must break the
  // chain rather than fabricate a call across it. Decorated frames normalize.
  let folded = base.join("run.folded");
  fs::write(
    &folded,
    "main_loop;dispatch;handler;work+0x12 [bin] 42\n\
     main_loop;dispatch;__libc_noise;work 7\n",
  )
  .unwrap();
  let report = vorpal_index::traces::ingest_traces(&out, &folded).unwrap();
  assert_eq!(report.stacks, 2);
  assert_eq!(report.rows, 3, "{report:?}"); // main_loop→dispatch, dispatch→handler, handler→work
  assert_eq!(report.unknown_frames, 1);
  assert_eq!(report.ambiguous_frames, 0);
  assert!(report.samples.iter().any(|s| s.contains("__libc_noise")), "{report:?}");

  let kg = Kg::load(&out).unwrap();
  let gen_dir = vorpal_index::resolve_index_dir(&out);
  let target = |name: &str| vorpal_index::GraphTarget {
    name: name.into(),
    id: None,
    external_id: None,
    path_suffix: None,
    kind: None,
    merge_all: false,
    show_ids: false,
  };
  let (records, present) =
    vorpal_index::records::observed_records(&kg, &gen_dir, &target("dispatch")).unwrap();
  assert!(present);
  // Out: dispatch → handler, seen 42 times, NOT in the static graph (fn pointer).
  let out_row = records.iter().find(|r| r.direction == "out").unwrap();
  assert_eq!(out_row.counterpart_name, "handler");
  assert_eq!(out_row.count, 42);
  assert!(!out_row.in_static_graph, "{records:?}");
  // In: main_loop → dispatch, both stacks (42 + 7), and the static graph agrees.
  let in_row = records.iter().find(|r| r.direction == "in").unwrap();
  assert_eq!(in_row.counterpart_name, "main_loop");
  assert_eq!(in_row.count, 49);
  assert!(in_row.in_static_graph, "{records:?}");
  // The unknown frame never fabricated dispatch→work: handler→work carries only the clean
  // stack's count.
  let (work_records, _) =
    vorpal_index::records::observed_records(&kg, &gen_dir, &target("work")).unwrap();
  let into_work: Vec<_> = work_records.iter().filter(|r| r.direction == "in").collect();
  assert_eq!(into_work.len(), 1, "{work_records:?}");
  assert_eq!(into_work[0].counterpart_name, "handler");
  assert_eq!(into_work[0].count, 42);
  let _ = node(&kg, "dispatch"); // fixture sanity

  // A rebuild renumbers nodes: the sidecar must read as absent, never as stale ids.
  fs::write(src.join("b.rs"), "pub fn newcomer() {}\n").unwrap();
  build_index(&src, &out).unwrap();
  let kg = Kg::load(&out).unwrap();
  let gen_dir = vorpal_index::resolve_index_dir(&out);
  let (records, present) =
    vorpal_index::records::observed_records(&kg, &gen_dir, &target("dispatch")).unwrap();
  assert!(!present, "stale sidecar must read as absent");
  assert!(records.is_empty());

  let _ = fs::remove_dir_all(&base);
}
