//! Transitive closure (§11.5): correctness + push ≡ pull equivalence.

use vorpal_graph::closure::reachable_strategy;
use vorpal_graph::{
  Direction, EdgeLog, EdgeType, Graph, Strategy, reachable, reachable_typed,
  reachable_typed_paths,
};

fn graph(node_count: u32, edges: &[(u32, u32)]) -> Graph {
  let mut log = EdgeLog::new();
  for &(a, b) in edges {
    log.push(a, b, EdgeType::CALLS);
  }
  Graph::compact(node_count, &log)
}

#[test]
fn typed_traversal_does_not_cross_other_edge_types() {
  // A container `0` DEFINES callable `1`; `1` CALLS `2` CALLS `3`; and `2` also HAS_METHOD `4`.
  let mut log = EdgeLog::new();
  log.push(0, 1, EdgeType::DEFINES);
  log.push(1, 2, EdgeType::CALLS);
  log.push(2, 3, EdgeType::CALLS);
  log.push(2, 4, EdgeType::HAS_METHOD);
  let g = Graph::compact(5, &log);

  let via = |seed: u32, dir, ets: &[EdgeType], depth| {
    let mut v: Vec<u32> = reachable_typed(&g, &[seed], dir, ets, depth)
      .iter()
      .map(|i| i as u32)
      .collect();
    v.sort_unstable();
    v
  };

  // Transitive callees of 1 via CALLS: {2, 3} — never the DEFINES parent or the HAS_METHOD child.
  assert_eq!(via(1, Direction::Out, &[EdgeType::CALLS], None), vec![2, 3]);
  // The unfiltered closure, by contrast, leaks across HAS_METHOD to include 4.
  let mut unfiltered: Vec<u32> = reachable(&g, &[1], Direction::Out)
    .iter()
    .map(|i| i as u32)
    .collect();
  unfiltered.sort_unstable();
  assert_eq!(unfiltered, vec![2, 3, 4], "unfiltered closure crosses edge types");
  // Depth bound: one CALLS hop from 1 reaches only 2.
  assert_eq!(via(1, Direction::Out, &[EdgeType::CALLS], Some(1)), vec![2]);
  // Transitive callers of 3 via CALLS (direction in): {1, 2}, not the DEFINES root 0.
  assert_eq!(via(3, Direction::In, &[EdgeType::CALLS], None), vec![1, 2]);
  // A relation not present yields nothing.
  assert_eq!(
    via(1, Direction::Out, &[EdgeType::IMPORTS], None),
    Vec::<u32>::new()
  );
}

fn ids(g: &Graph, seed: u32, dir: Direction) -> Vec<u32> {
  let mut v: Vec<u32> = reachable(g, &[seed], dir)
    .iter()
    .map(|i| i as u32)
    .collect();
  v.sort_unstable();
  v
}

#[test]
fn transitive_reachability_on_a_chain_with_a_branch() {
  // 0 → 1 → 2 → 3 and 0 → 4
  let g = graph(5, &[(0, 1), (1, 2), (2, 3), (0, 4)]);

  assert_eq!(ids(&g, 0, Direction::Out), vec![1, 2, 3, 4]);
  assert_eq!(ids(&g, 1, Direction::Out), vec![2, 3]);
  assert_eq!(ids(&g, 3, Direction::Out), Vec::<u32>::new());

  // callersOf-transitive = follow In edges.
  assert_eq!(ids(&g, 3, Direction::In), vec![0, 1, 2]);
  assert_eq!(ids(&g, 4, Direction::In), vec![0]);
  assert_eq!(ids(&g, 0, Direction::In), Vec::<u32>::new());
}

#[test]
fn cycles_exclude_the_seed() {
  // 0 → 1 → 2 → 0
  let g = graph(3, &[(0, 1), (1, 2), (2, 0)]);
  // Every other node is reachable, but the seed itself is not in its own reachable set.
  assert_eq!(ids(&g, 0, Direction::Out), vec![1, 2]);
  assert_eq!(ids(&g, 1, Direction::Out), vec![0, 2]);
  assert_eq!(ids(&g, 0, Direction::In), vec![1, 2]);
}

#[test]
fn multiple_seeds_union_their_reachable_sets() {
  let g = graph(6, &[(0, 1), (2, 3), (3, 4)]);
  let mut got: Vec<u32> = reachable(&g, &[0, 2], Direction::Out)
    .iter()
    .map(|i| i as u32)
    .collect();
  got.sort_unstable();
  assert_eq!(got, vec![1, 3, 4]);
}

#[test]
fn push_and_pull_agree_everywhere() {
  // A denser graph so Auto exercises the pull path on large frontiers.
  let mut edges = Vec::new();
  for i in 0..20u32 {
    edges.push((i, (i + 1) % 20));
    edges.push((i, (i + 7) % 20));
  }
  let g = graph(20, &edges);

  for dir in [Direction::Out, Direction::In] {
    for seed in 0..20u32 {
      let push: Vec<u32> = reachable_strategy(&g, &[seed], dir, Strategy::Push)
        .iter()
        .map(|i| i as u32)
        .collect();
      let pull: Vec<u32> = reachable_strategy(&g, &[seed], dir, Strategy::Pull)
        .iter()
        .map(|i| i as u32)
        .collect();
      let auto: Vec<u32> = reachable_strategy(&g, &[seed], dir, Strategy::Auto)
        .iter()
        .map(|i| i as u32)
        .collect();
      assert_eq!(push, pull, "push≠pull dir={dir:?} seed={seed}");
      assert_eq!(push, auto, "push≠auto dir={dir:?} seed={seed}");
    }
  }
}

#[test]
fn typed_paths_record_parents_and_respect_confidence_floor() {
  // 0 -CALLS(100)→ 1 -CALLS(90)→ 2 -CALLS(40)→ 3, plus 1 -HAS_METHOD→ 4.
  let mut log = EdgeLog::new();
  log.push(0, 1, EdgeType::CALLS.with_confidence(100));
  log.push(1, 2, EdgeType::CALLS.with_confidence(90));
  log.push(2, 3, EdgeType::CALLS.with_confidence(40));
  log.push(1, 4, EdgeType::HAS_METHOD);
  let g = Graph::compact(5, &log);

  // No floor: the whole CALLS chain, each step carrying its BFS parent.
  let steps = reachable_typed_paths(&g, &[0], Direction::Out, &[EdgeType::CALLS], None, 0);
  let nodes: Vec<u32> = steps.iter().map(|s| s.node).collect();
  assert_eq!(nodes, vec![1, 2, 3]);
  assert_eq!(steps[0].via.0, 0);
  assert_eq!(steps[1].via.0, 1);
  assert_eq!(steps[2].via.0, 2);
  assert_eq!(steps[2].depth, 3);
  assert_eq!(steps[2].via.1.confidence(), 40, "edge confidence rides along");

  // Grade floor at constrained (90): traversal stops before the heuristic (40) hop.
  let steps = reachable_typed_paths(&g, &[0], Direction::Out, &[EdgeType::CALLS], None, 90);
  let nodes: Vec<u32> = steps.iter().map(|s| s.node).collect();
  assert_eq!(nodes, vec![1, 2], "the 40-confidence hop must not be crossed");

  // A positive floor also excludes structural edges (confidence 0).
  let steps = reachable_typed_paths(&g, &[1], Direction::Out, &[EdgeType::HAS_METHOD], None, 1);
  assert!(steps.is_empty(), "structural edges sit below any grade floor");
}
