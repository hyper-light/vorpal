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
