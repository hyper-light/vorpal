//! Real AST reference extraction (§3.1): parse Rust, extract call sites, resolve, link, query.

use vorpal_ingest::{Ingestor, NodeId, OutlineExtractor, Resolver};
use vorpal_kg::EdgeType;

const SRC: &str = "\
pub fn helper() -> i32 {
    1
}

pub fn run() -> i32 {
    helper() + helper()
}
";

fn find(kg: &vorpal_ingest::Kg, name: &str) -> NodeId {
  (0..kg.node_count() as u64)
    .map(NodeId::new)
    .find(|&id| kg.node(id).is_some_and(|v| v.name == name))
    .unwrap_or_else(|| panic!("node {name} not found"))
}

#[test]
fn extracts_and_resolves_real_rust_calls() {
  let mut ing = Ingestor::new(OutlineExtractor::new().unwrap());
  ing.ingest_source("lib.rs", SRC);

  // `run` calls `helper` (twice) — real call sites, extracted from the AST.
  assert!(
    ing.pending_references() >= 1,
    "expected call sites, found {}",
    ing.pending_references()
  );

  let (kg, stats) = ing.link_and_seal(&Resolver::new());
  assert!(stats.resolved >= 1, "calls should resolve; stats {stats:?}");

  let run = find(&kg, "run");
  let helper = find(&kg, "helper");

  // A real `calls` edge from the AST-extracted, resolved reference.
  assert!(
    kg.out_neighbors(run)
      .iter()
      .any(|&(to, e)| to == helper && e == EdgeType::CALLS),
    "expected run --calls--> helper"
  );
  // Transitive callers of `helper` (§11.5) include `run`.
  assert!(kg.reachable_in(helper).contains(&run));
  // `helper` calls nothing.
  assert!(
    !kg
      .out_neighbors(helper)
      .iter()
      .any(|&(_, e)| e == EdgeType::CALLS)
  );
}
