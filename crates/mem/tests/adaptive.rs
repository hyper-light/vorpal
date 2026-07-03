//! End-to-end: probe → policy → adaptive store + prefetching CSR walk + reset-per-batch arena.
//!
//! Demonstrates the "one code path, resource proportional to input" property (§8.1): the same
//! calls drive a few-file baseline here and would drive 10⁹ LOC unchanged — only the derived
//! policy differs.

use vorpal_mem::{
  AccessPattern, AnonStore, BatchArena, CorpusProbe, Csr, Hotness, ResourcePolicy, StoreKind,
};

#[test]
fn small_corpus_stays_on_the_cheap_baseline_path() {
  // A handful of files.
  let corpus = CorpusProbe::new(8_000, 5);
  let policy = ResourcePolicy::probe(corpus);

  // Baseline: no huge pages, no prefetch, no NUMA.
  let store_policy = policy.for_store(StoreKind::AnnAdjacency, AccessPattern::Random, Hotness::Hot);
  assert_eq!(store_policy.page, vorpal_mem::PagePolicy::Native);
  let distance = policy.prefetch_distance();
  assert_eq!(distance, 0);
  assert!(!policy.numa_enabled());

  // Adaptive anonymous store (native pages at this scale) — write/read a small region.
  let mut scratch = AnonStore::new(
    8192,
    StoreKind::NodesHot,
    AccessPattern::Random,
    Hotness::Hot,
    &policy,
  )
  .unwrap();
  scratch.as_mut_bytes()[0] = 42;
  assert_eq!(scratch.as_bytes()[0], 42);

  // Build a tiny graph and walk it with the policy-chosen prefetch distance.
  let graph = Csr::from_edges(4, &[(0, 1), (0, 2), (1, 3), (3, 0)]);
  let mut reachable_from_0 = Vec::new();
  graph.for_each_neighbor_prefetched(0, distance, |v| reachable_from_0.push(v));
  assert_eq!(reachable_from_0, vec![1, 2]);

  // Per-batch arena: allocate, then reset for reuse.
  let mut arena = BatchArena::from_policy(&policy, 0);
  let ids = arena.alloc_slice_copy(&[10u32, 20, 30]);
  assert_eq!(ids, &[10, 20, 30]);
  arena.reset();
  // Reset reuses the backing chunk rather than freeing it; the arena is usable again.
  let more = arena.alloc_slice_copy(&[99u32]);
  assert_eq!(more, &[99]);
}

#[test]
fn policy_escalation_is_purely_a_function_of_projected_size() {
  // Same machine, larger corpus → larger projected hot set and a non-zero prefetch distance.
  let tiny = ResourcePolicy::probe(CorpusProbe::new(4_000, 3));
  let large = ResourcePolicy::probe(CorpusProbe::new(50_000_000_000, 3_000_000));

  assert!(
    large.corpus().projected_hot_bytes(StoreKind::AnnAdjacency)
      > tiny.corpus().projected_hot_bytes(StoreKind::AnnAdjacency)
  );
  assert!(large.prefetch_distance() >= tiny.prefetch_distance());
}
