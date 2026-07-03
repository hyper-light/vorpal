//! Transitive closure (§11.5): correctness + push ≡ pull equivalence.

use vorpal_graph::closure::reachable_strategy;
use vorpal_graph::{Direction, EdgeLog, EdgeType, Graph, Strategy, reachable};

fn graph(node_count: u32, edges: &[(u32, u32)]) -> Graph {
  let mut log = EdgeLog::new();
  for &(a, b) in edges {
    log.push(a, b, EdgeType::CALLS);
  }
  Graph::compact(node_count, &log)
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
