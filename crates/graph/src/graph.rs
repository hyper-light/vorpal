//! The compacted graph: CSR (out) + CSC (in), the read form (§9.3).

use vorpal_mem::PodColumn;

use crate::csr::DirectedCsr;

/// "VGPH" — the graph section file's magic.
const GRAPH_MAGIC: u32 = 0x5647_5048;
const GRAPH_VERSION: u32 = 1;
use crate::edge::{EdgeLog, EdgeType};

/// A sealed, compacted directed graph over dense `u32` node ids, with both traversal directions
/// materialized so `callersOf` and `refsTo` each read exactly one direction (§9.3).
#[derive(Debug, Clone, Default)]
pub struct Graph {
  node_count: u32,
  out: DirectedCsr,
  inc: DirectedCsr,
}

impl Graph {
  /// An empty graph over `node_count` nodes.
  pub fn empty(node_count: u32) -> Self {
    Self::from_parts(node_count, &[], &[], &[])
  }

  /// Compact an [`EdgeLog`] into CSR + CSC (the write→read transform, §9.3).
  pub fn compact(node_count: u32, log: &EdgeLog) -> Self {
    Self::from_parts(node_count, log.srcs(), log.dsts(), log.etypes())
  }

  /// [`Graph::compact`] under the bucketed format's CSC law (P4.3): the CSC scatters over
  /// the SRC-MAJOR enumeration (each source's edges in log order, sources ascending)
  /// instead of the raw interleaved log. That enumeration is exactly what the per-bucket
  /// edge slabs store, so a graph rebuilt from slabs is bit-identical to the sealed one —
  /// per-destination adjacency order included. The out-CSR is unchanged (its per-src order
  /// IS the log's restriction either way).
  pub fn compact_src_major(node_count: u32, log: &EdgeLog) -> Self {
    let out = DirectedCsr::build(node_count, log.srcs(), log.dsts(), log.etypes());
    // The in-CSR is the out-CSR's transpose taken in ascending source order: the same bytes
    // as rebuilding it from the src-major edge list, without allocating that list (three
    // columns of every edge — ~160 MB and their first-touch faults on the kernel) and
    // without the extra pass. Measured at 0.17–0.20 s per kernel seal before this.
    let inc = out.transpose(node_count);
    Self {
      node_count,
      out,
      inc,
    }
  }

  /// Build both directions from parallel `(src, dst, etype)` columns. Public for the
  /// bucketed edge store (P4.3), whose slab concatenation IS the src-major enumeration —
  /// building from it here reproduces `compact_src_major`'s bytes exactly.
  pub fn from_parts(node_count: u32, srcs: &[u32], dsts: &[u32], etypes: &[u16]) -> Self {
    // out-CSR keys on src; in-CSC keys on dst (so its "targets" are the sources). The two
    // directions share no state — build them concurrently (each build is itself a
    // deterministic counting scatter, so the output bytes are unchanged).
    let (out, inc) = rayon::join(
      || DirectedCsr::build(node_count, srcs, dsts, etypes),
      || DirectedCsr::build(node_count, dsts, srcs, etypes),
    );
    Self {
      node_count,
      out,
      inc,
    }
  }

  pub fn node_count(&self) -> usize {
    self.node_count as usize
  }

  pub fn edge_count(&self) -> usize {
    self.out.total_edges()
  }

  /// Out-neighbors of `u` (`refsTo` / `calls` direction).
  pub fn out_targets(&self, u: u32) -> &[u32] {
    self.out.targets(u)
  }

  pub fn out_edge_types(&self, u: u32) -> &[u16] {
    self.out.edge_types(u)
  }

  /// Every out-edge's type tag, in CSR order — the bulk column for whole-graph statistics
  /// (each directed edge appears exactly once here).
  pub fn out_etypes_all(&self) -> &[u16] {
    self.out.raw_etypes()
  }

  pub fn out_degree(&self, u: u32) -> usize {
    self.out.degree(u)
  }

  /// In-neighbors of `u` (`callersOf` / who-references direction) — one CSC read, no fan-out.
  pub fn in_targets(&self, u: u32) -> &[u32] {
    self.inc.targets(u)
  }

  pub fn in_edge_types(&self, u: u32) -> &[u16] {
    self.inc.edge_types(u)
  }

  pub fn in_degree(&self, u: u32) -> usize {
    self.inc.degree(u)
  }

  /// Prefetching out-neighbor walk (§8.3): `f(dst, etype)` per edge, staging `distance` ahead.
  pub fn for_each_out_prefetched<F: FnMut(u32, EdgeType)>(
    &self,
    u: u32,
    distance: usize,
    mut f: F,
  ) {
    self
      .out
      .for_each_prefetched(u, distance, |dst, et| f(dst, EdgeType(et)));
  }

  /// Persist both directions as one aligned, little-endian section file (`graph.bin`):
  /// header, u64 offset sections, u32 target sections, u16 edge-type sections — in that
  /// order, so every section is naturally aligned for the zero-copy mapped open. The bytes
  /// are a pure function of the compacted graph, so persisted output stays bit-identical.
  pub fn write_to<W: std::io::Write>(&self, out: &mut W) -> std::io::Result<()> {
    let e = self.out.total_edges() as u64;
    out.write_all(&GRAPH_MAGIC.to_le_bytes())?;
    out.write_all(&GRAPH_VERSION.to_le_bytes())?;
    out.write_all(&self.node_count.to_le_bytes())?;
    out.write_all(&0u32.to_le_bytes())?; // pad: keeps the u64 sections 8-aligned
    out.write_all(&e.to_le_bytes())?;
    for column in [self.out.row_offsets(), self.inc.row_offsets()] {
      if cfg!(target_endian = "little") {
        out.write_all(bytemuck::cast_slice(column))?;
      } else {
        for &x in column {
          out.write_all(&x.to_le_bytes())?;
        }
      }
    }
    for column in [self.out.raw_targets(), self.inc.raw_targets()] {
      if cfg!(target_endian = "little") {
        out.write_all(bytemuck::cast_slice(column))?;
      } else {
        for &x in column {
          out.write_all(&x.to_le_bytes())?;
        }
      }
    }
    for column in [self.out.raw_etypes(), self.inc.raw_etypes()] {
      if cfg!(target_endian = "little") {
        out.write_all(bytemuck::cast_slice(column))?;
      } else {
        for &x in column {
          out.write_all(&x.to_le_bytes())?;
        }
      }
    }
    Ok(())
  }

  /// Zero-copy open of a [`Graph::write_to`] file: sections become mapped columns; pages
  /// fault in as queries touch them. The mapping stays alive for the graph's lifetime.
  pub fn open_mapped(store: std::sync::Arc<vorpal_mem::MappedStore>) -> std::io::Result<Self> {
    let bytes = store.as_bytes();
    let header = |at: usize| -> std::io::Result<u32> {
      bytes
        .get(at..at + 4)
        .map(|b| u32::from_le_bytes(b.try_into().expect("4 bytes")))
        .ok_or_else(|| std::io::Error::other("truncated graph file"))
    };
    if header(0)? != GRAPH_MAGIC {
      return Err(std::io::Error::other("bad graph file magic"));
    }
    if header(4)? != GRAPH_VERSION {
      return Err(std::io::Error::other("unsupported graph file version"));
    }
    let n = header(8)? as usize;
    let e = bytes
      .get(16..24)
      .map(|b| u64::from_le_bytes(b.try_into().expect("8 bytes")))
      .ok_or_else(|| std::io::Error::other("truncated graph file"))? as usize;

    let offsets_len = (n + 1) * 8;
    let targets_len = e * 4;
    let etypes_len = e * 2;
    let mut at = 24usize;
    let mut take = |len: usize| {
      let section = at;
      at += len;
      section
    };
    let out_offsets =
      PodColumn::from_mapped_le(&store, take(offsets_len), offsets_len, u64::from_le_bytes)?;
    let in_offsets =
      PodColumn::from_mapped_le(&store, take(offsets_len), offsets_len, u64::from_le_bytes)?;
    let out_targets =
      PodColumn::from_mapped_le(&store, take(targets_len), targets_len, u32::from_le_bytes)?;
    let in_targets =
      PodColumn::from_mapped_le(&store, take(targets_len), targets_len, u32::from_le_bytes)?;
    let out_etypes =
      PodColumn::from_mapped_le(&store, take(etypes_len), etypes_len, u16::from_le_bytes)?;
    let in_etypes =
      PodColumn::from_mapped_le(&store, take(etypes_len), etypes_len, u16::from_le_bytes)?;
    Ok(Self {
      node_count: n as u32,
      out: DirectedCsr::from_columns(out_offsets, out_targets, out_etypes),
      inc: DirectedCsr::from_columns(in_offsets, in_targets, in_etypes),
    })
  }

  /// All out-edges as `(src, dst, etype)` triples (for compaction / relabel rebuilds).
  pub fn out_edges(&self) -> impl Iterator<Item = (u32, u32, u16)> + '_ {
    (0..self.node_count).flat_map(move |u| {
      self
        .out
        .targets(u)
        .iter()
        .zip(self.out.edge_types(u))
        .map(move |(&dst, &et)| (u, dst, et))
    })
  }
}
