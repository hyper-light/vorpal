//! The sealed, queryable knowledge graph (§3.3, §3.5, §11).

use std::fs;
use std::io;
use std::path::Path;

use vorpal_graph::{Direction, EdgeLog, EdgeType, Graph, reachable};
use vorpal_mem::{CorpusProbe, ResourcePolicy};
use vorpal_segment::{NodeId, Segment, SegmentDirectory, SegmentError};

use crate::model::SymbolKind;

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

/// A queryable knowledge graph: a node segment (SoA columns) + string heap + compacted graph.
pub struct Kg {
  nodes: Segment,
  heap: Vec<u8>,
  graph: Graph,
  directory: SegmentDirectory,
}

impl Kg {
  pub(crate) fn new(
    nodes: Segment,
    heap: Vec<u8>,
    graph: Graph,
    directory: SegmentDirectory,
  ) -> Self {
    Self {
      nodes,
      heap,
      graph,
      directory,
    }
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
    let kind = SymbolKind::from_tag(self.nodes.column("kind")?.get_u8(row)?);
    let content_hash = self.nodes.column("content_hash")?.get_u64(row)?;
    let exported = self.nodes.column("flags")?.get_u8(row)? & 1 != 0;
    Some(NodeView {
      kind,
      name: self.heap_str("name", row)?,
      path: self.heap_str("path", row)?,
      signature: self.heap_str("sig", row)?,
      content_hash,
      exported,
    })
  }

  fn heap_str(&self, field: &str, row: u64) -> Option<&str> {
    let off = self.nodes.column(&format!("{field}_off"))?.get_u32(row)? as usize;
    let len = self.nodes.column(&format!("{field}_len"))?.get_u32(row)? as usize;
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
    (0..self.node_count() as u64)
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
    fs::create_dir_all(dir)?;
    fs::write(dir.join("nodes.vseg"), self.nodes.bytes())?;
    fs::write(dir.join("strings.heap"), &self.heap)?;

    let edges: Vec<(u32, u32, u16)> = self.graph.out_edges().collect();
    let mut buf = Vec::with_capacity(8 + edges.len() * 10);
    buf.extend_from_slice(&(edges.len() as u64).to_le_bytes());
    for (src, dst, etype) in edges {
      buf.extend_from_slice(&src.to_le_bytes());
      buf.extend_from_slice(&dst.to_le_bytes());
      buf.extend_from_slice(&etype.to_le_bytes());
    }
    fs::write(dir.join("edges.bin"), &buf)?;
    Ok(())
  }

  /// Cold-open a persisted graph: **mmap** the node segment (§9.1 — no heap load of the columns),
  /// read the string heap, and rebuild the CSR/CSC from the edge list.
  pub fn load(dir: &Path) -> Result<Self, SegmentError> {
    let nodes_path = dir.join("nodes.vseg");
    let size = fs::metadata(&nodes_path)?.len();
    let policy = ResourcePolicy::probe(CorpusProbe::new(size, 1));
    let nodes = Segment::open_file(&nodes_path, &policy)?;
    let heap = fs::read(dir.join("strings.heap"))?;
    let edge_bytes = fs::read(dir.join("edges.bin"))?;

    let row_count = nodes.row_count();
    let graph = rebuild_graph(row_count as u32, &edge_bytes);
    let mut directory = SegmentDirectory::new();
    directory.insert(0, row_count, 0);
    Ok(Self::new(nodes, heap, graph, directory))
  }
}

fn rebuild_graph(node_count: u32, bytes: &[u8]) -> Graph {
  let mut log = EdgeLog::new();
  if bytes.len() >= 8 {
    let count = u64::from_le_bytes(bytes[0..8].try_into().unwrap());
    let mut off = 8usize;
    for _ in 0..count {
      if off + 10 > bytes.len() {
        break;
      }
      let src = u32::from_le_bytes(bytes[off..off + 4].try_into().unwrap());
      let dst = u32::from_le_bytes(bytes[off + 4..off + 8].try_into().unwrap());
      let etype = u16::from_le_bytes(bytes[off + 8..off + 10].try_into().unwrap());
      log.push(src, dst, EdgeType(etype));
      off += 10;
    }
  }
  Graph::compact(node_count, &log)
}

fn is_containment(e: EdgeType) -> bool {
  matches!(
    e,
    EdgeType::DEFINES | EdgeType::HAS_METHOD | EdgeType::HAS_FIELD
  )
}
