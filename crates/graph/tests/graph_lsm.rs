//! Edge LSM (§9.3) + compaction-time locality relabel (§9.8), end to end.

use vorpal_graph::{
  EdgeLog, EdgeType, ForwardingTable, Graph, GraphStore, avg_edge_id_span, bfs_locality_order,
};

/// A path whose ids are scattered: 0-4-1-5-2-6-3-7. BFS locality order should compact the spans.
const SCATTERED: [(u32, u32); 7] = [(0, 4), (4, 1), (1, 5), (5, 2), (2, 6), (6, 3), (3, 7)];

fn scattered_graph() -> Graph {
  let mut log = EdgeLog::new();
  for &(a, b) in &SCATTERED {
    log.push(a, b, EdgeType::CALLS);
  }
  Graph::compact(8, &log)
}

fn sorted(mut v: Vec<u32>) -> Vec<u32> {
  v.sort_unstable();
  v
}

#[test]
fn compact_builds_both_directions() {
  let mut log = EdgeLog::new();
  log.push(0, 1, EdgeType::CALLS);
  log.push(0, 2, EdgeType::REFERENCES);
  log.push(3, 1, EdgeType::CALLS);
  let g = Graph::compact(4, &log);

  assert_eq!(g.node_count(), 4);
  assert_eq!(g.edge_count(), 3);
  // refsTo / calls-out: one direction.
  assert_eq!(g.out_targets(0), &[1, 2]);
  assert_eq!(
    g.out_edge_types(0),
    &[EdgeType::CALLS.0, EdgeType::REFERENCES.0]
  );
  // callersOf(1) = in-edges of 1 = {0, 3} — the other direction, no fan-out.
  assert_eq!(sorted(g.in_targets(1).to_vec()), vec![0, 3]);
  assert_eq!(g.in_degree(1), 2);
  assert_eq!(g.out_degree(1), 0);
}

#[test]
fn store_merges_delta_then_compacts() {
  let mut s = GraphStore::new(4);
  s.append(0, 1, EdgeType::CALLS);
  s.append(0, 2, EdgeType::REFERENCES);
  assert_eq!(s.pending(), 2);

  // Reads see the delta before any flush.
  let mut before = s.out_neighbors(0);
  before.sort_unstable();
  assert_eq!(
    before,
    vec![(1, EdgeType::CALLS), (2, EdgeType::REFERENCES)]
  );

  s.flush();
  assert_eq!(s.pending(), 0);
  let mut after = s.out_neighbors(0);
  after.sort_unstable();
  assert_eq!(after, vec![(1, EdgeType::CALLS), (2, EdgeType::REFERENCES)]);

  // New writes land in the delta; reads union compacted ∪ delta.
  s.append(0, 3, EdgeType::IMPORTS);
  let mut merged = s.out_neighbors(0);
  merged.sort_unstable();
  assert_eq!(
    merged,
    vec![
      (1, EdgeType::CALLS),
      (2, EdgeType::REFERENCES),
      (3, EdgeType::IMPORTS)
    ]
  );
  // callersOf via the delta too.
  assert_eq!(s.in_neighbors(3), vec![(0, EdgeType::IMPORTS)]);
}

#[test]
fn relabel_preserves_neighbors_and_improves_locality() {
  let g = scattered_graph();
  let before = avg_edge_id_span(&g);

  let order = bfs_locality_order(&g);
  let fwd = ForwardingTable::from_order(&order);
  let relabeled = g.relabel(&fwd);
  let after = avg_edge_id_span(&relabeled);

  assert!(
    after < before,
    "relabel should reduce id-span: after {after} vs before {before}"
  );

  // Correctness: neighbors are exactly the old neighbors, forwarded.
  for u in 0..g.node_count() as u32 {
    let nu = fwd.translate(u);
    let got = sorted(relabeled.out_targets(nu).to_vec());
    let want = sorted(g.out_targets(u).iter().map(|&v| fwd.translate(v)).collect());
    assert_eq!(got, want, "out-neighbors of old node {u}");
  }
}

#[test]
fn forwarding_tables_compose_without_chaining() {
  let g = scattered_graph();
  let fwd1 = ForwardingTable::from_order(&bfs_locality_order(&g));
  let g1 = g.relabel(&fwd1);
  let fwd2 = ForwardingTable::from_order(&bfs_locality_order(&g1));
  let g2 = g1.relabel(&fwd2);

  let composed = fwd2.compose(&fwd1);
  assert_eq!(composed.len(), g.node_count());

  for u in 0..g.node_count() as u32 {
    // Composition equals chaining the two lookups.
    assert_eq!(composed.translate(u), fwd2.translate(fwd1.translate(u)));
    // And a stale old id resolves straight into the twice-relabeled graph.
    let nu = composed.translate(u);
    let got = sorted(g2.out_targets(nu).to_vec());
    let want = sorted(
      g.out_targets(u)
        .iter()
        .map(|&v| composed.translate(v))
        .collect(),
    );
    assert_eq!(got, want, "composed neighbors of old node {u}");
  }
}

#[test]
fn identity_forwarding_is_a_noop() {
  let g = scattered_graph();
  let id = ForwardingTable::identity(g.node_count() as u32);
  let same = g.relabel(&id);
  for u in 0..g.node_count() as u32 {
    assert_eq!(same.out_targets(u), g.out_targets(u));
  }
}

#[test]
fn prefetched_walk_matches_plain_walk() {
  let g = scattered_graph();
  for u in 0..g.node_count() as u32 {
    let mut got = Vec::new();
    g.for_each_out_prefetched(u, 4, |d, e| got.push((d, e)));
    let want: Vec<(u32, EdgeType)> = g
      .out_targets(u)
      .iter()
      .zip(g.out_edge_types(u))
      .map(|(&d, &e)| (d, EdgeType(e)))
      .collect();
    assert_eq!(got, want, "node {u}");
  }
}

#[test]
fn store_relabel_for_locality_reindexes_in_new_space() {
  let mut s = GraphStore::new(8);
  for &(a, b) in &SCATTERED {
    s.append(a, b, EdgeType::CALLS);
  }
  let fwd = s.relabel_for_locality();
  assert_eq!(s.pending(), 0);

  // Querying by translated id returns the old neighborhood, forwarded.
  for u in 0..8u32 {
    let nu = fwd.translate(u);
    let got = sorted(s.out_neighbors(nu).iter().map(|&(d, _)| d).collect());
    let want = sorted(
      SCATTERED
        .iter()
        .filter(|&&(a, _)| a == u)
        .map(|&(_, b)| fwd.translate(b))
        .collect(),
    );
    assert_eq!(got, want, "old node {u}");
  }
}
