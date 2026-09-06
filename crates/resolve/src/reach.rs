//! Include-visibility reachability (extraction-coverage campaign): the oracle that
//! lets macro candidates bind by INCLUSION rather than name-globality.
//!
//! C/C++ macros are textual: a reference in file F can only mean a macro defined in
//! F itself or in a file reachable through F's transitive include closure. That is
//! the most correct binding derivable from source alone (the true ceiling without
//! the build system's `-D`/include-path flags), and it is what turns same-named
//! per-arch/vendored duplicates from global ambiguity into unique, correct
//! resolutions — each includer reaches exactly the copy it includes.
//!
//! Construction: the file→file include edges (resolved path-form imports) condense
//! through iterative Tarjan SCC (include guards make cycles legal — members of a
//! cycle share visibility), then closures compute once per SCC in reverse
//! topological order as sorted, deduped `NameId` vectors — shared by every member
//! file, so memory is Σ|scc closure|, not Σ|file closure|. Everything is sorted
//! before use, so the build is deterministic for a given edge multiset regardless
//! of input order.

use std::collections::HashMap;

use crate::intern::NameId;

/// Per-file transitive include reachability. Immutable after construction; safe to
/// share across resolution shards (`&self` queries only).
pub struct IncludeReach<'i> {
  /// file path → its SCC index.
  scc_of: HashMap<NameId<'i>, u32>,
  /// SCC index → (start, len) into `closure_data` (CSR layout: one arena, one
  /// span per SCC — replaces the per-SCC `Vec<Vec>` whose growth-and-copy
  /// churn measured 2.1 GB single-threaded at kernel scale).
  spans: Vec<(u32, u32)>,
  /// Sorted, deduped closure members for every SCC, back to back.
  closure_data: Vec<NameId<'i>>,
}

impl<'i> IncludeReach<'i> {
  /// Build from directed include edges `(includer, included)`. Files never
  /// mentioned get no entry — `reaches` then answers only `from == def`.
  pub fn from_edges(edges: &[(NameId<'i>, NameId<'i>)]) -> Self {
    // Compact node ids, deterministically: sorted unique file list.
    let mut files: Vec<NameId<'i>> = edges.iter().flat_map(|&(a, b)| [a, b]).collect();
    files.sort_unstable();
    files.dedup();
    let index_of: HashMap<NameId<'i>, u32> = files
      .iter()
      .enumerate()
      .map(|(index, &file)| (file, index as u32))
      .collect();
    let n = files.len();
    let mut adjacency: Vec<Vec<u32>> = vec![Vec::new(); n];
    for &(from, to) in edges {
      adjacency[index_of[&from] as usize].push(index_of[&to]);
    }
    for list in &mut adjacency {
      list.sort_unstable();
      list.dedup();
    }

    // Iterative Tarjan SCC (no recursion — include chains can be deep).
    const UNVISITED: u32 = u32::MAX;
    let mut order = vec![UNVISITED; n]; // discovery index
    let mut low = vec![0u32; n];
    let mut on_stack = vec![false; n];
    let mut stack: Vec<u32> = Vec::new();
    let mut scc_id = vec![UNVISITED; n];
    let mut scc_count = 0u32;
    let mut next_order = 0u32;
    // Explicit DFS frames: (node, next child position).
    let mut frames: Vec<(u32, usize)> = Vec::new();
    for root in 0..n as u32 {
      if order[root as usize] != UNVISITED {
        continue;
      }
      frames.push((root, 0));
      while let Some(&mut (node, ref mut child_at)) = frames.last_mut() {
        let node_us = node as usize;
        if *child_at == 0 {
          order[node_us] = next_order;
          low[node_us] = next_order;
          next_order += 1;
          stack.push(node);
          on_stack[node_us] = true;
        }
        if let Some(&child) = adjacency[node_us].get(*child_at) {
          *child_at += 1;
          let child_us = child as usize;
          if order[child_us] == UNVISITED {
            frames.push((child, 0));
          } else if on_stack[child_us] {
            low[node_us] = low[node_us].min(order[child_us]);
          }
          continue;
        }
        // Node exhausted: close its SCC if it is a root, then propagate low.
        if low[node_us] == order[node_us] {
          loop {
            let member = stack.pop().expect("tarjan stack underflow (invariant)");
            on_stack[member as usize] = false;
            scc_id[member as usize] = scc_count;
            if member == node {
              break;
            }
          }
          scc_count += 1;
        }
        frames.pop();
        if let Some(&mut (parent, _)) = frames.last_mut() {
          let parent_us = parent as usize;
          low[parent_us] = low[parent_us].min(low[node_us]);
        }
      }
    }

    // Tarjan emits SCCs in reverse topological order: successors of an SCC always
    // have SMALLER scc ids. Closures build LEVEL-PARALLEL over the condensation
    // DAG — measured serial at kernel scale this was 1.58 s on one thread (12 %
    // of the whole build): every SCC at the same depth merges its successors'
    // finished spans independently, and each closure is ONE exact-capacity
    // allocation merged then deduped in place (no growth-and-copy chains).
    let mut members: Vec<Vec<u32>> = vec![Vec::new(); scc_count as usize];
    for (node, &scc) in scc_id.iter().enumerate() {
      members[scc as usize].push(node as u32);
    }
    // Per-SCC successor lists over the condensation, deduped.
    let mut scc_succs: Vec<Vec<u32>> = vec![Vec::new(); scc_count as usize];
    for (node, &scc) in scc_id.iter().enumerate() {
      for &child in &adjacency[node] {
        let child_scc = scc_id[child as usize];
        if child_scc != scc {
          debug_assert!(child_scc < scc, "successor SCC must precede in Tarjan order");
          scc_succs[scc as usize].push(child_scc);
        }
      }
    }
    for succs in &mut scc_succs {
      succs.sort_unstable();
      succs.dedup();
    }
    // Depth levels: level(scc) = 1 + max level of its successors. Successor ids
    // are smaller, so one ascending pass computes every level.
    let mut level: Vec<u32> = vec![0; scc_count as usize];
    let mut max_level = 0u32;
    for scc in 0..scc_count as usize {
      let l = scc_succs[scc]
        .iter()
        .map(|&s| level[s as usize] + 1)
        .max()
        .unwrap_or(0);
      level[scc] = l;
      max_level = max_level.max(l);
    }
    let mut by_level: Vec<Vec<u32>> = vec![Vec::new(); max_level as usize + 1];
    for (scc, &l) in level.iter().enumerate() {
      by_level[l as usize].push(scc as u32);
    }

    let mut spans: Vec<(u32, u32)> = vec![(0, 0); scc_count as usize];
    let mut closure_data: Vec<NameId<'i>> = Vec::new();
    for level_sccs in &by_level {
      // Build every closure in this level in parallel; successors' spans are
      // finished (strictly lower levels). Deterministic: rayon's collect
      // preserves input order, and the serial append below walks that order.
      use rayon::prelude::*;
      let built: Vec<Vec<NameId<'i>>> = level_sccs
        .par_iter()
        .map(|&scc| {
          let scc = scc as usize;
          let cap = members[scc].len()
            + scc_succs[scc]
              .iter()
              .map(|&s| spans[s as usize].1 as usize)
              .sum::<usize>();
          let mut closure: Vec<NameId<'i>> = Vec::with_capacity(cap);
          for &member in &members[scc] {
            closure.push(files[member as usize]);
          }
          for &succ in &scc_succs[scc] {
            let (start, len) = spans[succ as usize];
            closure.extend_from_slice(&closure_data[start as usize..(start + len) as usize]);
          }
          closure.sort_unstable();
          closure.dedup();
          closure
        })
        .collect();
      for (&scc, closure) in level_sccs.iter().zip(&built) {
        let start = closure_data.len() as u32;
        spans[scc as usize] = (start, closure.len() as u32);
        closure_data.extend_from_slice(closure);
      }
    }

    let scc_of = files
      .iter()
      .map(|&file| (file, scc_id[index_of[&file] as usize]))
      .collect();
    IncludeReach {
      scc_of,
      spans,
      closure_data,
    }
  }

  /// Whether a definition in `def` is include-visible from `from`: the same file,
  /// or a member of `from`'s transitive include closure.
  pub fn reaches(&self, from: NameId<'i>, def: NameId<'i>) -> bool {
    if from == def {
      return true;
    }
    self.scc_of.get(&from).is_some_and(|&scc| {
      let (start, len) = self.spans[scc as usize];
      self.closure_data[start as usize..(start + len) as usize]
        .binary_search(&def)
        .is_ok()
    })
  }

  /// Number of files with include edges (diagnostics/phase stamps).
  pub fn file_count(&self) -> usize {
    self.scc_of.len()
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::intern::Interner;

  fn ids<'i>(interner: &'i Interner, names: &[&str]) -> Vec<NameId<'i>> {
    names.iter().map(|n| interner.intern(n)).collect()
  }

  #[test]
  fn direct_transitive_and_diamond_reach() {
    let interner = Interner::default();
    let f = ids(&interner, &["a.c", "b.h", "c.h", "d.h"]);
    // a.c -> b.h -> d.h ; a.c -> c.h -> d.h (diamond)
    let reach = IncludeReach::from_edges(&[
      (f[0], f[1]),
      (f[1], f[3]),
      (f[0], f[2]),
      (f[2], f[3]),
    ]);
    assert!(reach.reaches(f[0], f[1]));
    assert!(reach.reaches(f[0], f[3]), "transitive through either arm");
    assert!(reach.reaches(f[1], f[3]));
    assert!(!reach.reaches(f[1], f[2]), "siblings do not see each other");
    assert!(!reach.reaches(f[3], f[0]), "reachability is directed");
  }

  #[test]
  fn cycles_share_visibility_and_self_always_reaches() {
    let interner = Interner::default();
    let f = ids(&interner, &["x.h", "y.h", "z.h", "lone.c"]);
    // x <-> y (guarded mutual include), y -> z.
    let reach = IncludeReach::from_edges(&[(f[0], f[1]), (f[1], f[0]), (f[1], f[2])]);
    assert!(reach.reaches(f[0], f[1]) && reach.reaches(f[1], f[0]));
    assert!(reach.reaches(f[0], f[2]), "cycle members share successors");
    assert!(!reach.reaches(f[2], f[0]));
    assert!(reach.reaches(f[3], f[3]), "self-reach holds for unlisted files");
    assert!(!reach.reaches(f[3], f[0]));
  }

  #[test]
  fn build_is_order_invariant() {
    let interner = Interner::default();
    let f = ids(&interner, &["a.c", "b.h", "c.h", "d.h", "e.h"]);
    let edges = [
      (f[0], f[1]),
      (f[1], f[2]),
      (f[2], f[3]),
      (f[3], f[1]), // back edge: b/c/d cycle
      (f[0], f[4]),
    ];
    let forward = IncludeReach::from_edges(&edges);
    let mut reversed = edges;
    reversed.reverse();
    let backward = IncludeReach::from_edges(&reversed);
    for &from in &f {
      for &def in &f {
        assert_eq!(
          forward.reaches(from, def),
          backward.reaches(from, def),
          "edge order must not change reachability"
        );
      }
    }
  }
}

// ---------------------------------------------------------------------------
// Persisted include-edge graph (`reach.bin`) — the merge-era compose contract.
//
// Full builds resolve every path-form import into (includer, included) edges
// and seal this graph beside the generation. Scoped composes cannot afford —
// and do not need — the full closure rebuild: resolution queries reach ONLY
// from a reference's own file (`reaches(reference.from_path, …)`), so a
// compose re-resolving session files needs exactly the forward-reachable
// subgraph from those files. [`IncludeReach::for_roots`] walks it: session
// files contribute their FRESH first-hop edges (an import-editing compose is
// therefore correct with no decline), everything beyond first hop comes from
// this persisted graph (unchanged by definition — only session files changed).
//
// Layout (canonical: paths sorted, per-row targets sorted by target index):
// `VRCH` + version u32 + path_count u32 + edge_count u64 + path table
// (u32 len + UTF-8 bytes, sorted ascending) + row_starts ((path_count+1) ×
// u32) + targets (edge_count × u32 path indices).
// ---------------------------------------------------------------------------

/// File name of the persisted include-edge graph inside a generation.
pub const REACH_GRAPH_FILE: &str = "reach.bin";
const REACH_MAGIC: &[u8; 4] = b"VRCH";
/// Bumped when the layout or the edge-derivation semantics change.
pub const REACH_GRAPH_VERSION: u32 = 2;

/// Encode `(includer, included)` path edges canonically, followed (version 2) by the
/// include-root support the link learned (`SymbolTable::include_root_support`).
/// Determinism: the path table is the sorted, deduped path set; rows sort by (from, to)
/// index; support rows sort by root.
pub fn encode_reach_graph(edges: &[(&str, &str)], support: &[(&str, u32)]) -> Vec<u8> {
  let mut paths: Vec<&str> = edges.iter().flat_map(|&(a, b)| [a, b]).collect();
  paths.sort_unstable();
  paths.dedup();
  let index_of: HashMap<&str, u32> = paths
    .iter()
    .enumerate()
    .map(|(i, &p)| (p, i as u32))
    .collect();
  let mut pairs: Vec<(u32, u32)> = edges
    .iter()
    .map(|&(a, b)| (index_of[a], index_of[b]))
    .collect();
  pairs.sort_unstable();
  pairs.dedup();
  let mut out = Vec::new();
  out.extend_from_slice(REACH_MAGIC);
  out.extend_from_slice(&REACH_GRAPH_VERSION.to_le_bytes());
  out.extend_from_slice(&(paths.len() as u32).to_le_bytes());
  out.extend_from_slice(&(pairs.len() as u64).to_le_bytes());
  for path in &paths {
    out.extend_from_slice(&(path.len() as u32).to_le_bytes());
    out.extend_from_slice(path.as_bytes());
  }
  let mut starts: Vec<u32> = Vec::with_capacity(paths.len() + 1);
  let mut at = 0u32;
  let mut cursor = 0usize;
  for row in 0..paths.len() as u32 {
    starts.push(at);
    while cursor < pairs.len() && pairs[cursor].0 == row {
      cursor += 1;
      at += 1;
    }
  }
  starts.push(at);
  for start in &starts {
    out.extend_from_slice(&start.to_le_bytes());
  }
  for &(_, to) in &pairs {
    out.extend_from_slice(&to.to_le_bytes());
  }
  let mut support: Vec<(&str, u32)> = support.to_vec();
  support.sort_unstable();
  support.dedup();
  out.extend_from_slice(&(support.len() as u32).to_le_bytes());
  for (root, count) in support {
    out.extend_from_slice(&(root.len() as u32).to_le_bytes());
    out.extend_from_slice(root.as_bytes());
    out.extend_from_slice(&count.to_le_bytes());
  }
  out
}

/// Decoded persisted include-edge graph. Foreign or corrupt bytes decode to
/// `None` — the caller treats the family as absent (composes decline).
pub struct ReachGraph {
  paths: Vec<String>,
  row_starts: Vec<u32>,
  targets: Vec<u32>,
  /// Version 2 generations carry the link's learned include-root support; a version 1
  /// graph decodes with `None`, and composes that need it decline.
  support: Option<Vec<(String, u32)>>,
}

impl ReachGraph {
  pub fn decode(bytes: &[u8]) -> Option<Self> {
    let header = 4 + 4 + 4 + 8;
    if bytes.len() < header || &bytes[0..4] != REACH_MAGIC {
      return None;
    }
    let version = u32::from_le_bytes(bytes[4..8].try_into().ok()?);
    if version != 1 && version != REACH_GRAPH_VERSION {
      return None;
    }
    let path_count = u32::from_le_bytes(bytes[8..12].try_into().ok()?) as usize;
    let edge_count = u64::from_le_bytes(bytes[12..20].try_into().ok()?) as usize;
    let mut at = header;
    let mut paths = Vec::with_capacity(path_count);
    for _ in 0..path_count {
      let len = u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?) as usize;
      at += 4;
      let path = std::str::from_utf8(bytes.get(at..at + len)?).ok()?;
      at += len;
      paths.push(path.to_string());
    }
    let mut row_starts = Vec::with_capacity(path_count + 1);
    for _ in 0..=path_count {
      row_starts.push(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?));
      at += 4;
    }
    let mut targets = Vec::with_capacity(edge_count);
    for _ in 0..edge_count {
      targets.push(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?));
      at += 4;
    }
    let support = if version >= 2 {
      let count = u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?) as usize;
      at += 4;
      let mut support = Vec::with_capacity(count);
      for _ in 0..count {
        let len = u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?) as usize;
        at += 4;
        let root = std::str::from_utf8(bytes.get(at..at + len)?).ok()?;
        at += len;
        let n = u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?);
        at += 4;
        support.push((root.to_string(), n));
      }
      Some(support)
    } else {
      None
    };
    (at == bytes.len()).then_some(Self {
      paths,
      row_starts,
      targets,
      support,
    })
  }

  /// The include-root support persisted with this graph (`None` for a version 1 graph).
  pub fn include_root_support(&self) -> Option<&[(String, u32)]> {
    self.support.as_deref()
  }

  fn row_of(&self, path: &str) -> Option<usize> {
    self.paths.binary_search_by(|p| p.as_str().cmp(path)).ok()
  }

  fn out_of(&self, row: usize) -> &[u32] {
    let start = self.row_starts[row] as usize;
    let end = self.row_starts[row + 1] as usize;
    &self.targets[start..end]
  }
}

impl<'i> IncludeReach<'i> {
  /// The compose-side reach: forward-reachable subgraph from `roots`, with the
  /// session files' FRESH first-hop edges (`fresh`, replacing whatever the
  /// persisted graph holds for them) and the persisted graph beyond. The
  /// closure over the traversed edge set answers `reaches(root, …)` exactly
  /// as the full build's closure does — the same `from_edges` oracle over the
  /// same reachable edges.
  pub fn for_roots(
    interner: &'i crate::Interner,
    graph: &ReachGraph,
    fresh: &[(NameId<'i>, NameId<'i>)],
    roots: &[NameId<'i>],
  ) -> IncludeReach<'i> {
    use std::collections::HashSet;
    // Every root is session-fresh: its first hop comes ONLY from `fresh`
    // (covering import deletions — a root with no fresh rows has no edges).
    let session: HashSet<NameId<'i>> = roots.iter().copied().collect();
    let mut edges: Vec<(NameId<'i>, NameId<'i>)> = Vec::new();
    let mut visited: HashSet<NameId<'i>> = HashSet::new();
    let mut queue: Vec<NameId<'i>> = Vec::new();
    for &root in roots {
      if visited.insert(root) {
        queue.push(root);
      }
    }
    while let Some(file) = queue.pop() {
      if session.contains(&file) {
        for &(from, to) in fresh.iter().filter(|&&(from, _)| from == file) {
          edges.push((from, to));
          if visited.insert(to) {
            queue.push(to);
          }
        }
        continue;
      }
      let Some(row) = graph.row_of(interner.text_of(file)) else {
        continue;
      };
      for &target in graph.out_of(row) {
        let to = interner.intern(&graph.paths[target as usize]);
        edges.push((file, to));
        if visited.insert(to) {
          queue.push(to);
        }
      }
    }
    IncludeReach::from_edges(&edges)
  }
}

/// Whether every root's FRESH out-edge target set equals the persisted graph's
/// row for it — the compose guard: composes hard-link `reach.bin`, so a session
/// whose imports changed must decline rather than resolve against a stale graph.
/// (Stored rows are target-index-ascending and the path table is sorted, so
/// index order IS lexicographic path order; the fresh side sorts to match.)
pub fn reach_rows_match<'i>(
  interner: &'i crate::Interner,
  graph: &ReachGraph,
  roots: &[NameId<'i>],
  fresh: &[(NameId<'i>, NameId<'i>)],
) -> bool {
  reach_rows_divergence(interner, graph, roots, fresh).is_none()
}

/// Why [`reach_rows_match`] would say no: the first root whose fresh first-hop targets
/// differ from the persisted row, with what the session resolved that the graph lacks and
/// what the graph holds that the session did not resolve. `None` when every row matches.
/// The decline message carries this so a compose that falls back to the full pipeline says
/// which include moved — or, when nothing moved in the source, which side failed to resolve.
pub fn reach_rows_divergence<'i>(
  interner: &'i crate::Interner,
  graph: &ReachGraph,
  roots: &[NameId<'i>],
  fresh: &[(NameId<'i>, NameId<'i>)],
) -> Option<String> {
  for &root in roots {
    let mut fresh_targets: Vec<&str> = fresh
      .iter()
      .filter(|&&(from, _)| from == root)
      .map(|&(_, to)| interner.text_of(to))
      .collect();
    fresh_targets.sort_unstable();
    fresh_targets.dedup();
    let stored: Vec<&str> = match graph.row_of(interner.text_of(root)) {
      Some(row) => graph
        .out_of(row)
        .iter()
        .map(|&t| graph.paths[t as usize].as_str())
        .collect(),
      None => Vec::new(),
    };
    if fresh_targets != stored {
      let missing: Vec<&str> =
        stored.iter().copied().filter(|t| fresh_targets.binary_search(t).is_err()).collect();
      let extra: Vec<&str> =
        fresh_targets.iter().copied().filter(|t| stored.binary_search(t).is_err()).collect();
      let show = |v: &[&str]| -> String {
        let head: Vec<&str> = v.iter().copied().take(4).collect();
        let more = v.len().saturating_sub(4);
        if more > 0 { format!("{} (+{more} more)", head.join(", ")) } else { head.join(", ") }
      };
      return Some(format!(
        "{}: session resolved {} first-hop include(s), the carried graph holds {}; \
         in the graph but not resolved now: [{}]; resolved now but not in the graph: [{}]",
        interner.text_of(root),
        fresh_targets.len(),
        stored.len(),
        show(&missing),
        show(&extra)
      ));
    }
  }
  None
}

#[cfg(test)]
mod graph_tests {
  use super::*;
  use crate::Interner;

  /// for_roots must answer `reaches(root, x)` exactly as the full closure —
  /// every root, every target, cycles included.
  #[test]
  fn for_roots_matches_full_closure() {
    let interner = Interner::new();
    let names: Vec<&str> = vec!["a", "b", "c", "d", "e", "f", "g"];
    let id = |s: &str| interner.intern(s);
    // Includes with a cycle (b<->c), a diamond, and an island (g).
    let edge_strs = [
      ("a", "b"),
      ("b", "c"),
      ("c", "b"),
      ("b", "d"),
      ("a", "e"),
      ("e", "d"),
      ("f", "a"),
    ];
    let edges: Vec<(NameId, NameId)> =
      edge_strs.iter().map(|&(x, y)| (id(x), id(y))).collect();
    let full = IncludeReach::from_edges(&edges);
    let encoded = encode_reach_graph(&edge_strs, &[]);
    let graph = ReachGraph::decode(&encoded).expect("round-trips");
    for root_name in &names {
      let root = id(root_name);
      // Fresh first hop == the graph's row (the unchanged-imports compose case).
      let fresh: Vec<(NameId, NameId)> =
        edges.iter().copied().filter(|&(from, _)| from == root).collect();
      let scoped = IncludeReach::for_roots(&interner, &graph, &fresh, &[root]);
      for target_name in &names {
        let target = id(target_name);
        assert_eq!(
          full.reaches(root, target),
          scoped.reaches(root, target),
          "root {root_name} target {target_name}"
        );
      }
    }
  }
}

#[cfg(test)]
mod format_tests {
  use super::*;

  #[test]
  fn support_round_trips_and_version_one_decodes_without_it() {
    let edges = [("a.c", "include/x.h"), ("b.c", "include/x.h"), ("b.c", "tools/include/x.h")];
    let support = [("tools/include/", 3u32), ("include/", 9u32)];
    let bytes = encode_reach_graph(&edges, &support);
    let graph = ReachGraph::decode(&bytes).expect("v2 decodes");
    assert_eq!(
      graph.include_root_support(),
      Some(&[("include/".to_string(), 9u32), ("tools/include/".to_string(), 3u32)][..]),
      "support rows come back sorted by root"
    );
    let row = graph.row_of("b.c").expect("b.c has a row");
    let targets: Vec<&str> = graph.out_of(row).iter().map(|&t| graph.paths[t as usize].as_str()).collect();
    assert_eq!(targets, ["include/x.h", "tools/include/x.h"]);
    // A version 1 graph is the same bytes without the support section and with the old
    // version stamp: it still decodes, and reports no support.
    let support_len = 4 + support.iter().map(|(r, _)| 4 + r.len() + 4).sum::<usize>();
    let mut v1 = bytes[..bytes.len() - support_len].to_vec();
    v1[4..8].copy_from_slice(&1u32.to_le_bytes());
    let old = ReachGraph::decode(&v1).expect("v1 decodes");
    assert!(old.include_root_support().is_none());
    assert_eq!(old.paths, graph.paths);
    // Truncated or foreign bytes decode to nothing.
    assert!(ReachGraph::decode(&bytes[..bytes.len() - 1]).is_none());
    assert!(ReachGraph::decode(b"VRCHxxxx").is_none());
  }
}
