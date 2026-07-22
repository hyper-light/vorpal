//! The sealed, queryable knowledge graph (§3.3, §3.5, §11).

use std::fs;
use std::io;
use std::path::Path;

use vorpal_graph::{Direction, EdgeType, Graph, reachable};
use vorpal_mem::{CorpusProbe, ResourcePolicy};
use vorpal_segment::{NodeId, Segment, SegmentDirectory, SegmentError};

use crate::model::SymbolKind;

/// Write `name` under `dir` through a `.tmp` sibling, then atomically swap it in.
fn write_via_tmp(
  dir: &Path,
  name: &str,
  write: impl FnOnce(&mut std::io::BufWriter<fs::File>) -> io::Result<()>,
) -> io::Result<()> {
  use std::io::Write;
  let tmp = dir.join(format!("{name}.tmp"));
  let mut out = std::io::BufWriter::with_capacity(1 << 20, fs::File::create(&tmp)?);
  write(&mut out)?;
  out.flush()?;
  drop(out);
  replace_file(&tmp, &dir.join(name))
}

/// Atomic-replace rename (POSIX semantics; Windows needs the destination cleared first).
fn replace_file(tmp: &Path, dest: &Path) -> io::Result<()> {
  #[cfg(windows)]
  let _ = fs::remove_file(dest);
  fs::rename(tmp, dest)
}

/// A resolved node's attributes, borrowing the string heap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NodeView<'a> {
  pub kind: SymbolKind,
  pub name: &'a str,
  pub path: &'a str,
  pub signature: &'a str,
  pub content_hash: u64,
  pub exported: bool,
}

/// Directory positions of the node segment's columns, resolved once at construction so point
/// access (`kg.node` in every hot loop) is allocation-free: no name hashing, no per-field
/// directory scan (measured: 6 heap allocations per `node()` call before this cache).
struct NodeColumns {
  kind: usize,
  name_off: usize,
  name_len: usize,
  path_off: usize,
  path_len: usize,
  sig_off: usize,
  sig_len: usize,
  content_hash: usize,
  flags: usize,
}

impl NodeColumns {
  fn resolve(segment: &Segment) -> Option<Self> {
    Some(Self {
      kind: segment.column_index("kind")?,
      name_off: segment.column_index("name_off")?,
      name_len: segment.column_index("name_len")?,
      path_off: segment.column_index("path_off")?,
      path_len: segment.column_index("path_len")?,
      sig_off: segment.column_index("sig_off")?,
      sig_len: segment.column_index("sig_len")?,
      content_hash: segment.column_index("content_hash")?,
      flags: segment.column_index("flags")?,
    })
  }
}

/// A queryable knowledge graph: a node segment (SoA columns) + string heap + compacted graph.
pub struct Kg {
  nodes: Segment,
  cols: NodeColumns,
  heap: vorpal_mem::PodColumn<u8>,
  /// Where the heap bytes already live on disk, when they do (streamed commit or load) —
  /// lets `save` rename or skip instead of rewriting a file readers may have mapped.
  heap_file: Option<std::path::PathBuf>,
  graph: Graph,
  directory: SegmentDirectory,
}

impl Kg {
  pub(crate) fn new(
    nodes: Segment,
    heap: Vec<u8>,
    graph: Graph,
    directory: SegmentDirectory,
  ) -> Result<Self, SegmentError> {
    Self::with_heap_column(
      nodes,
      vorpal_mem::PodColumn::from_vec(heap),
      None,
      graph,
      directory,
    )
  }

  /// Construct over an already-built heap column — the streamed-commit and load paths, where
  /// the heap bytes live on disk (`heap_file`) and the column is a zero-copy map of them.
  pub(crate) fn with_heap_column(
    nodes: Segment,
    heap: vorpal_mem::PodColumn<u8>,
    heap_file: Option<std::path::PathBuf>,
    graph: Graph,
    directory: SegmentDirectory,
  ) -> Result<Self, SegmentError> {
    let cols = NodeColumns::resolve(&nodes).ok_or(SegmentError::Corrupt(
      "node segment missing a required column",
    ))?;
    Ok(Self {
      nodes,
      cols,
      heap,
      heap_file,
      graph,
      directory,
    })
  }

  pub fn node_count(&self) -> usize {
    self.nodes.row_count() as usize
  }

  pub fn is_empty(&self) -> bool {
    self.node_count() == 0
  }

  /// Resolve a node's attributes (§3.3). Reads HOT columns (`base + row·stride`) + the heap.
  pub fn node(&self, id: NodeId) -> Option<NodeView<'_>> {
    let (_segment, row) = self.directory.locate(id)?;
    let kind = SymbolKind::from_tag(self.nodes.column_at(self.cols.kind)?.get_u8(row)?);
    let content_hash = self.nodes.column_at(self.cols.content_hash)?.get_u64(row)?;
    let exported = self.nodes.column_at(self.cols.flags)?.get_u8(row)? & 1 != 0;
    Some(NodeView {
      kind,
      name: self.heap_str(self.cols.name_off, self.cols.name_len, row)?,
      path: self.heap_str(self.cols.path_off, self.cols.path_len, row)?,
      signature: self.heap_str(self.cols.sig_off, self.cols.sig_len, row)?,
      content_hash,
      exported,
    })
  }

  fn heap_str(&self, off_col: usize, len_col: usize, row: u64) -> Option<&str> {
    let off = self.nodes.column_at(off_col)?.get_u32(row)? as usize;
    let len = self.nodes.column_at(len_col)?.get_u32(row)? as usize;
    std::str::from_utf8(self.heap.get(off..off + len)?).ok()
  }

  /// Out-edges of `id` (`refsTo` / containment direction).
  pub fn out_neighbors(&self, id: NodeId) -> Vec<(NodeId, EdgeType)> {
    let u = id.raw() as u32;
    self
      .graph
      .out_targets(u)
      .iter()
      .zip(self.graph.out_edge_types(u))
      .map(|(&d, &e)| (NodeId::new(d as u64), EdgeType(e)))
      .collect()
  }

  /// In-edges of `id` (`callersOf` / container direction) — one CSC read (§9.3).
  pub fn in_neighbors(&self, id: NodeId) -> Vec<(NodeId, EdgeType)> {
    let u = id.raw() as u32;
    self
      .graph
      .in_targets(u)
      .iter()
      .zip(self.graph.in_edge_types(u))
      .map(|(&s, &e)| (NodeId::new(s as u64), EdgeType(e)))
      .collect()
  }

  /// Nodes that `id` contains/defines (`defines` / `has_method` / `has_field`).
  pub fn defines(&self, id: NodeId) -> Vec<NodeId> {
    self
      .out_neighbors(id)
      .into_iter()
      .filter(|(_, e)| is_containment(*e))
      .map(|(n, _)| n)
      .collect()
  }

  /// The container that defines `id`, if any (reverse containment).
  pub fn container_of(&self, id: NodeId) -> Option<NodeId> {
    self
      .in_neighbors(id)
      .into_iter()
      .find(|(_, e)| is_containment(*e))
      .map(|(n, _)| n)
  }

  /// Everything reachable from `id` by following out-edges transitively (masked-SpMV closure,
  /// §11.5). With today's containment-only edges this is the transitive `defines`/`has_*` set; the
  /// same kernel covers `calls`/`references` once those edges are produced.
  pub fn reachable_out(&self, id: NodeId) -> Vec<NodeId> {
    self.reachable(id, Direction::Out)
  }

  /// Everything that transitively reaches `id` via in-edges (its container chain today;
  /// transitive `callersOf` once call edges exist).
  pub fn reachable_in(&self, id: NodeId) -> Vec<NodeId> {
    self.reachable(id, Direction::In)
  }

  fn reachable(&self, id: NodeId, dir: Direction) -> Vec<NodeId> {
    reachable(&self.graph, &[id.raw() as u32], dir)
      .iter()
      .map(|u| NodeId::new(u as u64))
      .collect()
  }

  /// All nodes whose display name equals `name` (a linear scan; the resident name/FTS index is
  /// §3.2's job). The query surface a CLI/MCP exposes builds on this.
  pub fn nodes_named(&self, name: &str) -> Vec<NodeId> {
    use rayon::prelude::*;
    // Parallel scan over the node rows; the indexed collect keeps ascending-id order, so the
    // result is identical to the serial scan.
    (0..self.node_count() as u64)
      .into_par_iter()
      .map(NodeId::new)
      .filter(|&id| self.node(id).is_some_and(|view| view.name == name))
      .collect()
  }

  /// Direct callers of any node named `name` (incoming `calls` edges).
  pub fn callers_of(&self, name: &str) -> Vec<NodeId> {
    self.incoming_named(name, EdgeType::CALLS)
  }

  /// Direct referrers of any node named `name` (incoming `references` edges).
  pub fn references_to(&self, name: &str) -> Vec<NodeId> {
    self.incoming_named(name, EdgeType::REFERENCES)
  }

  /// Files that import any node named `name` (incoming `imports` edges).
  pub fn importers_of(&self, name: &str) -> Vec<NodeId> {
    self.incoming_named(name, EdgeType::IMPORTS)
  }

  /// Types implementing/extending a trait, interface, or base type (incoming `implements`).
  pub fn implementors_of(&self, name: &str) -> Vec<NodeId> {
    self.incoming_named(name, EdgeType::IMPLEMENTS)
  }

  /// Definitions using a type — fields, params, returns, annotations (incoming `of_type`).
  pub fn users_of_type(&self, name: &str) -> Vec<NodeId> {
    self.incoming_named(name, EdgeType::OF_TYPE)
  }

  fn incoming_named(&self, name: &str, edge: EdgeType) -> Vec<NodeId> {
    let mut found = Vec::new();
    for target in self.nodes_named(name) {
      for (from, kind) in self.in_neighbors(target) {
        if kind == edge && !found.contains(&from) {
          found.push(from);
        }
      }
    }
    found
  }

  /// Persist the graph to `dir`: the node `.vseg` segment, the string heap, and the edge list.
  /// Sealed segments are immutable, so this is a plain write (§9.7).
  pub fn save(&self, dir: &Path) -> io::Result<()> {
    use std::io::Write;
    crate::phase_stamp("kg save: segments");
    fs::create_dir_all(dir)?;
    // Every artifact lands via tmp + rename: a rebuild must never truncate a file a live
    // reader — this process's daemon, or another process — still has mapped (truncating a
    // mapped file makes later reads fault). Rename swaps the directory entry; the old inode
    // survives until its last map goes away.
    write_via_tmp(dir, "nodes.vseg", |out| out.write_all(self.nodes.bytes()))?;
    let heap_final = dir.join("strings.heap");
    match &self.heap_file {
      // Streamed commit: the bytes are already in the tmp file — publish it.
      Some(existing) if *existing == dir.join("strings.heap.tmp") => {
        replace_file(existing, &heap_final)?;
      }
      // Loaded from this very directory: identical bytes are already in place.
      Some(existing) if *existing == heap_final => {}
      _ => write_via_tmp(dir, "strings.heap", |out| out.write_all(&self.heap[..]))?,
    }

    crate::phase_stamp("kg save: graph");
    // Both CSR directions persist as one aligned section file the load path maps zero-copy —
    // the edge-list form forced every open to re-run compaction (~64 ms at kernel scale).
    write_via_tmp(dir, "graph.bin", |out| self.graph.write_to(out))?;
    crate::phase_stamp("kg save: done");
    Ok(())
  }

  /// Cold-open a persisted graph: **mmap** the node segment (§9.1 — no heap load of the columns),
  /// read the string heap, and rebuild the CSR/CSC from the edge list.
  pub fn load(dir: &Path) -> Result<Self, SegmentError> {
    crate::phase_stamp("kg load: nodes");
    let nodes_path = dir.join("nodes.vseg");
    let size = fs::metadata(&nodes_path)?.len();
    let policy = ResourcePolicy::probe(CorpusProbe::new(size, 1));
    let nodes = Segment::open_file(&nodes_path, &policy)?;
    crate::phase_stamp("kg load: map heap + graph");
    let heap_store = std::sync::Arc::new(
      vorpal_mem::MappedStore::map_file(
        &dir.join("strings.heap"),
        vorpal_mem::StoreKind::VectorsFull,
        vorpal_mem::AccessPattern::Random,
        vorpal_mem::Hotness::Hot,
        &policy,
      )
      .map_err(SegmentError::from)?,
    );
    let heap_len = heap_store.as_bytes().len();
    let heap = vorpal_mem::PodColumn::from_mapped_le(&heap_store, 0, heap_len, u8::from_le_bytes)
      .map_err(SegmentError::from)?;
    let graph_store = std::sync::Arc::new(
      vorpal_mem::MappedStore::map_file(
        &dir.join("graph.bin"),
        vorpal_mem::StoreKind::EdgesCsr,
        vorpal_mem::AccessPattern::Random,
        vorpal_mem::Hotness::Hot,
        &policy,
      )
      .map_err(SegmentError::from)?,
    );
    let graph = Graph::open_mapped(graph_store).map_err(SegmentError::from)?;
    crate::phase_stamp("kg load: done");
    let row_count = nodes.row_count();
    let mut directory = SegmentDirectory::new();
    directory.insert(0, row_count, 0);
    Self::with_heap_column(
      nodes,
      heap,
      Some(dir.join("strings.heap")),
      graph,
      directory,
    )
  }

  /// The node count of a persisted index, from the segment header alone — no string heap read,
  /// no edge-list read, no CSR rebuild. This is all the whole-tree-unchanged fast path needs,
  /// so a no-change re-index does not pay a graph load to report a number.
  pub fn peek_node_count(dir: &Path) -> Result<usize, SegmentError> {
    let nodes_path = dir.join("nodes.vseg");
    let size = fs::metadata(&nodes_path)?.len();
    let policy = ResourcePolicy::probe(CorpusProbe::new(size, 1));
    Ok(Segment::open_file(&nodes_path, &policy)?.row_count() as usize)
  }
}

fn is_containment(e: EdgeType) -> bool {
  matches!(
    e,
    EdgeType::DEFINES | EdgeType::HAS_METHOD | EdgeType::HAS_FIELD
  )
}
