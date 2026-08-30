//! Community detection over the `calls` graph — the `communities.bin` sidecar (VCOM v1).
//!
//! Deterministic Louvain: nodes are visited in ascending id order, modularity gains are
//! compared in exact integer arithmetic (edge weights are multiplicities, so no float
//! epsilon decides a tie — the best gain wins, ties stay put or take the lowest community
//! id), and every level aggregates in id order, so the assignment is a pure function of
//! the graph and the cap. Only nodes with at least one `calls` edge take part; everything
//! else is its own singleton. Omnipresent nodes — calls degree above √(2m), the modularity
//! resolution scale, where a single node would absorb ~1/k of the graph into its community
//! (`kfree`, `memcpy`, `printk` at kernel scale) — are held out of the optimization and
//! reported as singletons: they belong to every cluster, so they name none.
//!
//! The reported community is a size-bounded cut of the Louvain dendrogram: each node's
//! coarsest ancestor with at most `cap` members, never finer than the first level. On a
//! large call graph the top level is a handful of hub basins (46k members on the kernel)
//! over a dust of pairs, while the lower levels hold the working groups; the cap keeps the
//! answer at that scale. `cap = 0` reports the top level (classic Louvain). The cap is
//! written into the sidecar header, so changing it re-keys the file.
//!
//! A warm-time sidecar like the ANN tier — never part of the generation content id, stamped
//! with the node-segment hash so a stale file reads as absent (queries answer `null`, the
//! architecture summary says "not built"), rebuilt by the next warm.

use std::io::{self, Write};
use std::path::Path;

use rayon::prelude::*;
use vorpal_graph::{EdgeType, Graph};

use crate::Kg;

const MAGIC: &[u8; 4] = b"VCOM";
const VERSION: u32 = 1;
/// magic, version, stamp, count, cap.
const HEADER_LEN: usize = 24;
/// Local-moving sweeps per level before aggregating (each sweep is O(E)).
const MAX_SWEEPS: usize = 12;
/// Aggregation levels.
const MAX_LEVELS: usize = 8;
/// Participants per parallel fill chunk when flattening the level-0 adjacency.
const FILL_CHUNK: usize = 1 << 16;
/// Floor for the omnipresent-node degree threshold, so tiny graphs hold nothing out.
const MIN_HUB_DEGREE: u64 = 32;
/// Default member cap for the dendrogram cut (`VORPAL_COMMUNITY_CAP`; 0 disables).
pub const DEFAULT_CAP: u32 = 512;
const NONE: u32 = u32::MAX;

/// One Louvain level: flat undirected weighted adjacency (CSR).
struct Level {
  offsets: Vec<usize>,
  targets: Vec<u32>,
  /// Per-edge weight; empty means every edge weighs 1 (level 0, where multiplicity is
  /// carried by duplicate targets rather than merged weights).
  weights: Vec<u64>,
  /// Weight folded into each node from inside its own community (both directions), so
  /// an aggregated node's degree stays the sum of its members' degrees.
  self_loop: Vec<u64>,
}

impl Level {
  fn node_count(&self) -> usize {
    self.self_loop.len()
  }

  fn edges(&self, u: usize) -> impl Iterator<Item = (u32, u64)> + '_ {
    let range = self.offsets[u]..self.offsets[u + 1];
    let weights = &self.weights;
    let start = range.start;
    self.targets[range].iter().enumerate().map(move |(i, &v)| {
      let w = if weights.is_empty() { 1 } else { weights[start + i] };
      (v, w)
    })
  }
}

/// Level 0 over the participating nodes plus the original-node → participant map.
struct Participants {
  level: Level,
  /// Original node → participant index, `NONE` for singletons (no kept `calls` edge).
  part_of: Vec<u32>,
}

/// Compute a community id per node (dense `0..k`) with the given member cap.
pub fn compute(kg: &Kg, cap: u32) -> Vec<u32> {
  let graph = kg.graph();
  let n = graph.node_count();
  let Participants { level: level0, part_of } = calls_adjacency(graph);
  let participants = level0.node_count();
  // Dendrogram: `levels[k][node]` = the level-(k+1) node holding level-k node `node`.
  let mut levels: Vec<Vec<u32>> = Vec::new();
  let mut groups_per_level: Vec<usize> = Vec::new();
  let mut level = level0;
  for _ in 0..MAX_LEVELS {
    let assignment = local_moving(&level);
    let (compact, groups) = renumber(&assignment, level.node_count());
    if groups == level.node_count() {
      break; // nothing merged at this level: converged
    }
    let next = aggregate(&level, &compact, groups);
    levels.push(compact);
    groups_per_level.push(groups);
    level = next;
  }
  let (keys, key_space) = cut(&levels, &groups_per_level, cap, participants);
  // Final ids by first appearance in node-id order (stable across equal inputs).
  let mut dense = vec![NONE; key_space];
  let mut next = 0u32;
  let membership: Vec<u32> = (0..n)
    .map(|i| {
      let p = part_of[i];
      if p == NONE {
        let id = next;
        next += 1;
        return id;
      }
      let slot = &mut dense[keys[p as usize] as usize];
      if *slot == NONE {
        *slot = next;
        next += 1;
      }
      *slot
    })
    .collect();
  if crate::phase_trace_enabled() {
    let mut sizes = vec![0u32; next as usize];
    for &c in &membership {
      sizes[c as usize] += 1;
    }
    crate::phase_stamp(&format!("communities: final {}", describe(&sizes)));
  }
  membership
}

/// The reported community of each participant: the coarsest dendrogram ancestor with at
/// most `cap` members (level 0 is the floor; cap 0 = the top level). Returns keys that are
/// dense over the union of all levels' groups, and that key space.
fn cut(levels: &[Vec<u32>], groups: &[usize], cap: u32, participants: usize) -> (Vec<u32>, usize) {
  if levels.is_empty() {
    return ((0..participants as u32).collect(), participants);
  }
  // Members per group, per level, in participants.
  let mut sizes: Vec<Vec<u32>> = Vec::with_capacity(levels.len());
  for (k, compact) in levels.iter().enumerate() {
    let mut size = vec![0u32; groups[k]];
    for (node, &g) in compact.iter().enumerate() {
      size[g as usize] += if k == 0 { 1 } else { sizes[k - 1][node] };
    }
    if crate::phase_trace_enabled() {
      crate::phase_stamp(&format!("communities: level {k} {}", describe(&size)));
    }
    sizes.push(size);
  }
  let mut bases = Vec::with_capacity(levels.len());
  let mut total = 0usize;
  for &g in groups {
    bases.push(total as u32);
    total += g;
  }
  let keys = (0..participants)
    .map(|p| {
      let mut node = p;
      let mut chosen = (0usize, levels[0][p] as usize);
      for (k, compact) in levels.iter().enumerate() {
        let g = compact[node] as usize;
        if k > 0 && cap != 0 && sizes[k][g] > cap {
          break;
        }
        chosen = (k, g);
        node = g;
      }
      bases[chosen.0] + chosen.1 as u32
    })
    .collect();
  (keys, total)
}

/// Trace-only cluster statistics for one size table.
fn describe(sizes: &[u32]) -> String {
  let mut clusters: Vec<u32> = sizes.iter().copied().filter(|&m| m > 1).collect();
  clusters.sort_unstable();
  let count = clusters.len();
  let covered: u64 = clusters.iter().map(|&m| m as u64).sum();
  let pick = |q: f64| {
    clusters
      .get(((count as f64 * q) as usize).min(count.saturating_sub(1)))
      .copied()
      .unwrap_or(0)
  };
  format!(
    "{count} clusters of >=2 covering {covered} nodes; sizes p50 {} p90 {} p99 {} max {}",
    pick(0.5),
    pick(0.9),
    pick(0.99),
    clusters.last().copied().unwrap_or(0)
  )
}

fn is_call_to(u: u32, v: u32, edge_type: u16) -> bool {
  v != u && EdgeType(edge_type).base() == EdgeType::CALLS
}

/// Level 0: every `calls` edge in both directions (self-calls dropped), one entry per
/// occurrence, minus the omnipresent nodes, over the nodes that keep at least one edge.
/// Degrees are counted in parallel; the flat target array is filled in parallel over
/// disjoint participant-range slices.
fn calls_adjacency(graph: &Graph) -> Participants {
  let n = graph.node_count();
  let count_edges = |u: u32, keep: &dyn Fn(u32) -> bool| {
    let mut count = 0u32;
    for (targets, types) in [
      (graph.out_targets(u), graph.out_edge_types(u)),
      (graph.in_targets(u), graph.in_edge_types(u)),
    ] {
      for (&v, &et) in targets.iter().zip(types) {
        if is_call_to(u, v, et) && keep(v) {
          count += 1;
        }
      }
    }
    count
  };
  let raw: Vec<u32> = (0..n as u32)
    .into_par_iter()
    .map(|u| count_edges(u, &|_| true))
    .collect();
  let two_m: u64 = raw.iter().map(|&c| c as u64).sum();
  let threshold = two_m.isqrt().max(MIN_HUB_DEGREE);
  let hub: Vec<bool> = raw.iter().map(|&c| c as u64 > threshold).collect();
  drop(raw);
  let omnipresent = hub.iter().filter(|&&h| h).count();
  crate::phase_stamp(&format!(
    "communities: 2m={two_m} hub threshold {threshold} omnipresent {omnipresent}"
  ));
  let keep = |v: u32| !hub[v as usize];
  let counts: Vec<u32> = (0..n as u32)
    .into_par_iter()
    .map(|u| if hub[u as usize] { 0 } else { count_edges(u, &keep) })
    .collect();
  let mut part_of = vec![NONE; n];
  let mut nodes: Vec<u32> = Vec::new();
  let mut offsets = vec![0usize];
  for (u, &c) in counts.iter().enumerate() {
    if c > 0 {
      part_of[u] = nodes.len() as u32;
      nodes.push(u as u32);
      let last = offsets[offsets.len() - 1];
      offsets.push(last + c as usize);
    }
  }
  drop(counts);
  let participants = nodes.len();
  let mut targets = vec![0u32; offsets[participants]];
  let mut chunks: Vec<(usize, &mut [u32])> = Vec::with_capacity(participants / FILL_CHUNK + 1);
  let mut rest: &mut [u32] = &mut targets;
  let mut first = 0;
  while first < participants {
    let end = (first + FILL_CHUNK).min(participants);
    let (head, tail) = std::mem::take(&mut rest).split_at_mut(offsets[end] - offsets[first]);
    chunks.push((first, head));
    rest = tail;
    first = end;
  }
  chunks.into_par_iter().for_each(|(first, slice)| {
    let mut pos = 0;
    let end = (first + FILL_CHUNK).min(participants);
    for &u in &nodes[first..end] {
      for (targets, types) in [
        (graph.out_targets(u), graph.out_edge_types(u)),
        (graph.in_targets(u), graph.in_edge_types(u)),
      ] {
        for (&v, &et) in targets.iter().zip(types) {
          if is_call_to(u, v, et) && keep(v) {
            slice[pos] = part_of[v as usize];
            pos += 1;
          }
        }
      }
    }
  });
  Participants {
    level: Level {
      offsets,
      targets,
      weights: Vec::new(),
      self_loop: vec![0; participants],
    },
    part_of,
  }
}

/// Modularity gain of placing a node of degree `k_u` into community `c`, scaled by 2m so
/// it is an exact integer: `link_c·2m − k_u·tot_c`.
fn gain(link: &[u64], tot: &[u64], two_m: u64, k_u: u64, c: u32) -> i128 {
  link[c as usize] as i128 * two_m as i128 - k_u as i128 * tot[c as usize] as i128
}

/// One Louvain level: sweep nodes in id order, moving each to the neighbor community with
/// the best modularity gain (ties: stay if staying ties, else the lowest community id),
/// until a sweep moves nothing.
fn local_moving(level: &Level) -> Vec<u32> {
  let n = level.node_count();
  let degree: Vec<u64> = (0..n)
    .map(|u| level.edges(u).map(|(_, w)| w).sum::<u64>() + level.self_loop[u])
    .collect();
  let two_m: u64 = degree.iter().sum();
  let mut community: Vec<u32> = (0..n as u32).collect();
  if two_m == 0 {
    return community;
  }
  let mut tot = degree.clone();
  // Dense scratch: weight from the current node into each community, reset per node via
  // the touched list (all weights are ≥ 1, so zero means untouched).
  let mut link: Vec<u64> = vec![0; n];
  let mut touched: Vec<u32> = Vec::new();
  for _ in 0..MAX_SWEEPS {
    let mut moved = false;
    for u in 0..n {
      if level.offsets[u] == level.offsets[u + 1] {
        continue; // no neighbors: nothing to move toward
      }
      for (v, w) in level.edges(u) {
        let c = community[v as usize];
        if link[c as usize] == 0 {
          touched.push(c);
        }
        link[c as usize] += w;
      }
      let own = community[u];
      let k_u = degree[u];
      // Remove u from its community for the gain comparison.
      tot[own as usize] -= k_u;
      let mut best = own;
      let mut best_gain = gain(&link, &tot, two_m, k_u, own);
      for &c in &touched {
        if c == own {
          continue;
        }
        let g = gain(&link, &tot, two_m, k_u, c);
        if g > best_gain || (g == best_gain && best != own && c < best) {
          best = c;
          best_gain = g;
        }
      }
      tot[best as usize] += k_u;
      if best != own {
        community[u] = best;
        moved = true;
      }
      for &c in &touched {
        link[c as usize] = 0;
      }
      touched.clear();
    }
    if !moved {
      break;
    }
  }
  community
}

/// Renumber an assignment densely by first appearance; returns (compact ids, count).
/// Assignment values index a table of `key_space` entries.
fn renumber(assignment: &[u32], key_space: usize) -> (Vec<u32>, usize) {
  let mut dense: Vec<u32> = vec![NONE; key_space];
  let mut next = 0u32;
  let compact: Vec<u32> = assignment
    .iter()
    .map(|&c| {
      let slot = &mut dense[c as usize];
      if *slot == NONE {
        *slot = next;
        next += 1;
      }
      *slot
    })
    .collect();
  (compact, next as usize)
}

/// Collapse each community into one node: inter-community weights sum into edges (sorted
/// in parallel, merged in order), and every intra-community weight — both directions, plus
/// the members' own self-loops — becomes the super-node's self-loop.
fn aggregate(level: &Level, compact: &[u32], groups: usize) -> Level {
  let mut self_loop = vec![0u64; groups];
  let mut edges: Vec<(u32, u32, u64)> = Vec::new();
  for u in 0..level.node_count() {
    let cu = compact[u];
    self_loop[cu as usize] += level.self_loop[u];
    for (v, w) in level.edges(u) {
      let cv = compact[v as usize];
      if cu == cv {
        self_loop[cu as usize] += w; // each internal edge is seen from both ends: 2·in_C
      } else {
        edges.push((cu, cv, w));
      }
    }
  }
  edges.par_sort_unstable_by_key(|e| (e.0, e.1));
  let mut offsets = vec![0usize; groups + 1];
  let mut targets = Vec::new();
  let mut weights = Vec::new();
  let mut i = 0;
  while i < edges.len() {
    let (cu, cv, mut w) = edges[i];
    let mut j = i + 1;
    while j < edges.len() && edges[j].0 == cu && edges[j].1 == cv {
      w += edges[j].2;
      j += 1;
    }
    targets.push(cv);
    weights.push(w);
    offsets[cu as usize + 1] += 1;
    i = j;
  }
  for g in 0..groups {
    offsets[g + 1] += offsets[g];
  }
  Level {
    offsets,
    targets,
    weights,
    self_loop,
  }
}

/// Persist the assignment beside the generation: header (magic, version, stamp, count,
/// cap), then one u32 per node. Atomic tmp+rename.
pub fn save(dir: &Path, stamp: u64, cap: u32, membership: &[u32]) -> io::Result<()> {
  let mut buf = Vec::with_capacity(HEADER_LEN + membership.len() * 4);
  buf.extend_from_slice(MAGIC);
  buf.extend_from_slice(&VERSION.to_le_bytes());
  buf.extend_from_slice(&stamp.to_le_bytes());
  buf.extend_from_slice(&(membership.len() as u32).to_le_bytes());
  buf.extend_from_slice(&cap.to_le_bytes());
  for &c in membership {
    buf.extend_from_slice(&c.to_le_bytes());
  }
  let tmp = dir.join("communities.bin.tmp");
  {
    let mut file = std::fs::File::create(&tmp)?;
    file.write_all(&buf)?;
    file.sync_all()?;
  }
  std::fs::rename(&tmp, dir.join("communities.bin"))
}

/// Parsed header: (stamp, count, cap), if the bytes carry a current-version header.
fn header(bytes: &[u8]) -> Option<(u64, usize, u32)> {
  if bytes.len() < HEADER_LEN || &bytes[0..4] != MAGIC {
    return None;
  }
  if u32::from_le_bytes(bytes[4..8].try_into().ok()?) != VERSION {
    return None;
  }
  let stamp = u64::from_le_bytes(bytes[8..16].try_into().ok()?);
  let count = u32::from_le_bytes(bytes[16..20].try_into().ok()?) as usize;
  let cap = u32::from_le_bytes(bytes[20..24].try_into().ok()?);
  Some((stamp, count, cap))
}

/// The persisted assignment, if present and stamped for `stamp` with `node_count` rows
/// (whatever cap it was built with — the builder decides freshness, see [`is_fresh`]).
pub fn load(dir: &Path, stamp: u64, node_count: usize) -> Option<Vec<u32>> {
  let bytes = std::fs::read(dir.join("communities.bin")).ok()?;
  let (file_stamp, count, _) = header(&bytes)?;
  if file_stamp != stamp || count != node_count || bytes.len() != HEADER_LEN + count * 4 {
    return None;
  }
  Some(
    bytes[HEADER_LEN..]
      .chunks_exact(4)
      .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
      .collect(),
  )
}

/// Is the sidecar present, stamped for `stamp`, and built with `cap`? (Header-only check.)
pub fn is_fresh(dir: &Path, stamp: u64, cap: u32) -> bool {
  let Ok(bytes) = std::fs::read(dir.join("communities.bin")) else {
    return false;
  };
  matches!(header(&bytes), Some((file_stamp, _, file_cap)) if file_stamp == stamp && file_cap == cap)
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::{KgWriter, NodeDef, SymbolKind};

  fn def<'a>(name: &'a str) -> NodeDef<'a> {
    NodeDef {
      kind: SymbolKind::Function,
      name,
      entity_path: name,
      path: "src/x.rs",
      signature: "",
      exported: true,
      content_hash: 1,
      span: (0, 0),
    }
  }

  #[test]
  fn two_cliques_bridged_by_one_edge_split_and_singletons_stay_alone() {
    let mut w = KgWriter::new();
    let ids: Vec<_> = (0..9)
      .map(|i| w.define(def(Box::leak(format!("n{i}").into_boxed_str()))))
      .collect();
    let calls = EdgeType::CALLS.with_confidence(100);
    // Clique A: 0,1,2,3 ; clique B: 4,5,6,7 ; bridge 3→4 ; node 8 isolated.
    for group in [[0usize, 1, 2, 3], [4, 5, 6, 7]] {
      for &a in &group {
        for &b in &group {
          if a < b {
            w.add_edge(ids[a], ids[b], calls);
          }
        }
      }
    }
    w.add_edge(ids[3], ids[4], calls);
    let kg = w.seal();
    let c = compute(&kg, DEFAULT_CAP);
    assert_eq!(c.len(), 9);
    assert!(c[0] == c[1] && c[1] == c[2] && c[2] == c[3], "{c:?}");
    assert!(c[4] == c[5] && c[5] == c[6] && c[6] == c[7], "{c:?}");
    assert_ne!(c[0], c[4], "{c:?}");
    assert!(c[8] != c[0] && c[8] != c[4], "isolated node is its own community: {c:?}");
    // Dense renumbering by first appearance: clique A is community 0.
    assert_eq!(c[0], 0);
    // Deterministic, and the cap does not touch a graph whose clusters fit under it.
    assert_eq!(compute(&kg, DEFAULT_CAP), c);
    assert_eq!(compute(&kg, 0), c);

    // Round trip through the sidecar; a different cap reads as stale.
    let dir = std::env::temp_dir().join(format!("vorpal-vcom-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    save(&dir, 77, DEFAULT_CAP, &c).unwrap();
    assert!(is_fresh(&dir, 77, DEFAULT_CAP));
    assert!(!is_fresh(&dir, 78, DEFAULT_CAP));
    assert!(!is_fresh(&dir, 77, 0));
    assert_eq!(load(&dir, 77, 9).unwrap(), c);
    assert!(load(&dir, 77, 8).is_none(), "row-count mismatch reads as absent");
    let _ = std::fs::remove_dir_all(&dir);
  }

  #[test]
  fn dendrogram_cut_reports_the_coarsest_ancestor_under_the_cap() {
    // 8 participants: level 0 pairs them (4 groups), level 1 pairs the pairs (2 groups),
    // level 2 merges everything (1 group).
    let levels = vec![
      vec![0, 0, 1, 1, 2, 2, 3, 3],
      vec![0, 0, 1, 1],
      vec![0, 0],
    ];
    let groups = [4, 2, 1];
    let communities = |cap: u32| cut(&levels, &groups, cap, 8).0;
    // cap 0: the top level — one community; cap 8: the top level fits.
    for cap in [0, 8] {
      let c = communities(cap);
      assert!(c.iter().all(|&k| k == c[0]), "cap {cap}: {c:?}");
    }
    // cap 4: two communities of four.
    let c = communities(4);
    assert!(c[0] == c[1] && c[1] == c[2] && c[2] == c[3], "{c:?}");
    assert!(c[4] == c[5] && c[5] == c[6] && c[6] == c[7], "{c:?}");
    assert_ne!(c[0], c[4]);
    // cap 2: the four pairs.
    let c = communities(2);
    assert!(c[0] == c[1] && c[2] == c[3] && c[4] == c[5] && c[6] == c[7], "{c:?}");
    assert!(c[0] != c[2] && c[2] != c[4] && c[4] != c[6], "{c:?}");
    // cap 1: level 0 is the floor — still the four pairs, never singletons.
    assert_eq!(communities(1), communities(2));
    // No merges at all: every participant keeps its own key.
    assert_eq!(cut(&[], &[], 4, 3).0, vec![0, 1, 2]);
  }
}
