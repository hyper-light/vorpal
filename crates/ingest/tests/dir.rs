//! Directory ingestion (§3.4) end-to-end: index a tree of real files, resolve cross-file calls,
//! answer `callers_of` — the query a CLI/MCP surface exposes.

use std::fs;

use vorpal_ingest::{Ingestor, OutlineExtractor, Resolver};

#[test]
fn ingests_a_directory_and_answers_callers() {
  let dir = std::env::temp_dir().join(format!("vorpal-ingest-dir-{}", std::process::id()));
  let _ = fs::remove_dir_all(&dir);
  fs::create_dir_all(&dir).unwrap();
  fs::write(dir.join("b.rs"), "pub fn target() -> i32 {\n    0\n}\n").unwrap();
  fs::write(
    dir.join("a.rs"),
    "pub fn caller() -> i32 {\n    target()\n}\n",
  )
  .unwrap();

  let mut ing = Ingestor::new(OutlineExtractor::new().unwrap());
  ing.ingest_dir(&dir).unwrap();
  assert_eq!(ing.stats().indexed, 2, "both source files indexed");

  let (kg, stats) = ing.link_and_seal(&Resolver::new());
  assert!(
    stats.resolved >= 1,
    "cross-file call should resolve; {stats:?}"
  );

  // callers_of("target") includes the cross-file caller (resolved from the AST reference).
  let caller_names: Vec<String> = kg
    .callers_of("target")
    .into_iter()
    .filter_map(|id| kg.node(id).map(|v| v.name.to_string()))
    .collect();
  assert!(
    caller_names.contains(&"caller".to_string()),
    "callers of target: {caller_names:?}"
  );

  let _ = fs::remove_dir_all(&dir);
}
