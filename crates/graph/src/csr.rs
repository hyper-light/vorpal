//! Directed CSR with an aligned edge-type payload column (read form, §9.3).
//!
//! Built by GVEL-style counting-scatter (degree histogram → prefix sum → scatter, §11.2): one
//! pass to count, one to place, keeping the `etype` column in step with `targets`.

use vorpal_mem::{PodColumn, prefetch_read};

#[derive(Debug, Clone, Default)]
pub(crate) struct DirectedCsr {
  /// `row_offsets[u]..row_offsets[u + 1]` bounds node `u`'s slice; length `node_count + 1`.
  row_offsets: PodColumn<u64>,
  /// The other endpoint per edge (dst for out-CSR, src for in-CSC).
  targets: PodColumn<u32>,
  /// Edge label per edge, aligned 1:1 with `targets`.
  etypes: PodColumn<u16>,
}

impl DirectedCsr {
  /// Build from parallel `(key, other, etype)` columns, where `key` is the row (src for out,
  /// dst for in). All three slices have equal length.
  pub(crate) fn build(node_count: u32, keys: &[u32], others: &[u32], etypes: &[u16]) -> Self {
    let n = node_count as usize;
    let m = keys.len();
    let mut row_offsets = vec![0u64; n + 1];
    for &k in keys {
      row_offsets[k as usize + 1] += 1;
    }
    for i in 1..=n {
      row_offsets[i] += row_offsets[i - 1];
    }
    let mut targets = vec![0u32; m];
    let mut out_etypes = vec![0u16; m];
    let mut cursor = row_offsets[..n].to_vec();
    for i in 0..m {
      let k = keys[i] as usize;
      let slot = cursor[k] as usize;
      targets[slot] = others[i];
      out_etypes[slot] = etypes[i];
      cursor[k] += 1;
    }
    Self {
      row_offsets: PodColumn::from_vec(row_offsets),
      targets: PodColumn::from_vec(targets),
      etypes: PodColumn::from_vec(out_etypes),
    }
  }

  /// The reverse CSR: every edge `u → v` of `self` becomes an entry `u` in row `v`, and each
  /// row lists its sources in ascending order, i.e. the same bytes as building the in-CSR
  /// from a src-major edge list — without materializing that list. Two passes over the
  /// out-CSR (degree histogram, then scatter); the scatter walks rows in ascending `u`, so
  /// every in-row fills in source order.
  pub(crate) fn transpose(&self, node_count: u32) -> Self {
    let n = node_count as usize;
    let m = self.targets.len();
    let mut row_offsets = vec![0u64; n + 1];
    for &t in self.targets.iter() {
      row_offsets[t as usize + 1] += 1;
    }
    for i in 1..=n {
      row_offsets[i] += row_offsets[i - 1];
    }
    let mut targets = vec![0u32; m];
    let mut out_etypes = vec![0u16; m];
    let mut cursor = row_offsets[..n].to_vec();
    let offsets = &self.row_offsets;
    for u in 0..n {
      let (a, b) = (offsets[u] as usize, offsets[u + 1] as usize);
      for (&v, &et) in self.targets[a..b].iter().zip(&self.etypes[a..b]) {
        let slot = cursor[v as usize] as usize;
        targets[slot] = u as u32;
        out_etypes[slot] = et;
        cursor[v as usize] += 1;
      }
    }
    Self {
      row_offsets: PodColumn::from_vec(row_offsets),
      targets: PodColumn::from_vec(targets),
      etypes: PodColumn::from_vec(out_etypes),
    }
  }

  /// Assemble from already-built columns (the mapped load path).
  pub(crate) fn from_columns(
    row_offsets: PodColumn<u64>,
    targets: PodColumn<u32>,
    etypes: PodColumn<u16>,
  ) -> Self {
    debug_assert_eq!(targets.len(), etypes.len());
    Self {
      row_offsets,
      targets,
      etypes,
    }
  }

  pub(crate) fn row_offsets(&self) -> &[u64] {
    &self.row_offsets
  }

  pub(crate) fn raw_targets(&self) -> &[u32] {
    &self.targets
  }

  pub(crate) fn raw_etypes(&self) -> &[u16] {
    &self.etypes
  }

  pub(crate) fn total_edges(&self) -> usize {
    self.targets.len()
  }

  #[inline]
  fn range(&self, u: u32) -> (usize, usize) {
    let start = self.row_offsets[u as usize] as usize;
    let end = self.row_offsets[u as usize + 1] as usize;
    (start, end)
  }

  pub(crate) fn targets(&self, u: u32) -> &[u32] {
    let (a, b) = self.range(u);
    &self.targets[a..b]
  }

  pub(crate) fn edge_types(&self, u: u32) -> &[u16] {
    let (a, b) = self.range(u);
    &self.etypes[a..b]
  }

  pub(crate) fn degree(&self, u: u32) -> usize {
    let (a, b) = self.range(u);
    b - a
  }

  /// Visit `u`'s neighbors, prefetching the adjacency of the neighbor `distance` hops ahead
  /// (§8.3 software pipeline). `distance == 0` is the plain scan.
  pub(crate) fn for_each_prefetched<F: FnMut(u32, u16)>(&self, u: u32, distance: usize, mut f: F) {
    let (a, b) = self.range(u);
    let ts = &self.targets[a..b];
    let es = &self.etypes[a..b];
    for i in 0..ts.len() {
      if distance != 0 {
        if let Some(&ahead) = ts.get(i + distance) {
          let off = self.row_offsets[ahead as usize] as usize;
          if off < self.targets.len() {
            prefetch_read(&self.targets[off] as *const u32);
          }
        }
      }
      f(ts[i], es[i]);
    }
  }
}

#[cfg(test)]
mod transpose_tests {
  use super::DirectedCsr;

  /// Deterministic LCG so the test needs no dev-dependency.
  fn edges(n: u32, m: usize, seed: u64) -> (Vec<u32>, Vec<u32>, Vec<u16>) {
    let mut x = seed;
    let mut next = move || {
      x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
      (x >> 33) as u32
    };
    let mut srcs = Vec::with_capacity(m);
    let mut dsts = Vec::with_capacity(m);
    let mut etypes = Vec::with_capacity(m);
    for _ in 0..m {
      srcs.push(next() % n);
      dsts.push(next() % n);
      etypes.push((next() % 7) as u16);
    }
    (srcs, dsts, etypes)
  }

  /// The transpose of the out-CSR must equal the in-CSR built from the src-major edge
  /// list (what `Graph::compact_src_major` used to materialize), column for column.
  #[test]
  fn transpose_matches_src_major_rebuild() {
    for (n, m, seed) in [(1u32, 0usize, 1u64), (5, 40, 2), (64, 1_000, 3), (1_000, 20_000, 4)] {
      let (srcs, dsts, etypes) = edges(n, m, seed);
      let out = DirectedCsr::build(n, &srcs, &dsts, &etypes);
      let mut ms = Vec::with_capacity(m);
      let mut md = Vec::with_capacity(m);
      let mut me = Vec::with_capacity(m);
      for u in 0..n {
        for (&d, &e) in out.targets(u).iter().zip(out.edge_types(u)) {
          ms.push(u);
          md.push(d);
          me.push(e);
        }
      }
      let expected = DirectedCsr::build(n, &md, &ms, &me);
      let got = out.transpose(n);
      assert_eq!(got.row_offsets(), expected.row_offsets(), "offsets n={n} m={m}");
      assert_eq!(got.raw_targets(), expected.raw_targets(), "targets n={n} m={m}");
      assert_eq!(got.raw_etypes(), expected.raw_etypes(), "etypes n={n} m={m}");
    }
  }
}
