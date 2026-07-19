//! Assembles a knowledge graph from extraction output (§3.1→§3.3).
//!
//! Two-phase capable: [`KgWriter::define`] interns a node (returning its `NodeId`) and
//! [`KgWriter::add_edge`] links two ids, so a caller can ingest definitions first, then resolve
//! references and inject `calls`/`references` edges before [`KgWriter::seal`] (§3.3 linking).

use std::hash::{Hash, Hasher};
use std::ops::Range;

use vorpal_canonical::{CanonicalIndex, CanonicalKey};
use vorpal_graph::{EdgeLog, EdgeType, Graph};
use vorpal_outline::model::OutlineItem;
use vorpal_segment::{NodeId, Segment, SegmentBuilder, SegmentDirectory};

use crate::kg::Kg;
use crate::model::SymbolKind;

/// One node's attributes for [`KgWriter::define`]. `entity_path` is the identity within the file
/// (e.g. `Owner.method`); `name` is the display name.
pub struct NodeDef<'a> {
  pub kind: SymbolKind,
  pub name: &'a str,
  pub entity_path: &'a str,
  pub path: &'a str,
  pub signature: &'a str,
  pub exported: bool,
  pub content_hash: u64,
}

/// Accumulates interned nodes (SoA columns + string heap) and edges, then seals a queryable
/// [`Kg`]. Ids are dense and assignment-ordered, so a column row index equals its id.
#[derive(Default)]
pub struct KgWriter {
  canonical: CanonicalIndex,
  edges: EdgeLog,
  heap: Vec<u8>,
  kind: Vec<u8>,
  name_off: Vec<u32>,
  name_len: Vec<u32>,
  path_off: Vec<u32>,
  path_len: Vec<u32>,
  sig_off: Vec<u32>,
  sig_len: Vec<u32>,
  content_hash: Vec<u64>,
  flags: Vec<u8>,
}

impl KgWriter {
  pub fn new() -> Self {
    Self::default()
  }

  /// Intern an entity; if new, append its column row. Returns the dense node id. Re-defining the
  /// same identity returns the existing id (dedup, §9.2) without appending.
  pub fn define(&mut self, def: NodeDef<'_>) -> NodeId {
    let key = CanonicalKey::of(def.path, def.entity_path);
    let assignment = self.canonical.get_or_assign(key, def.content_hash);
    let id = assignment.node_id();
    if assignment.is_new() {
      debug_assert_eq!(
        id.raw() as usize,
        self.kind.len(),
        "dense assignment-ordered rows"
      );
      let (name_off, name_len) = self.push_str(def.name);
      let (path_off, path_len) = self.push_str(def.path);
      let (sig_off, sig_len) = self.push_str(def.signature);
      self.kind.push(def.kind.tag());
      self.name_off.push(name_off);
      self.name_len.push(name_len);
      self.path_off.push(path_off);
      self.path_len.push(path_len);
      self.sig_off.push(sig_off);
      self.sig_len.push(sig_len);
      self.content_hash.push(def.content_hash);
      self.flags.push(u8::from(def.exported));
    }
    id
  }

  /// Link two existing nodes with an edge (containment during ingest, resolved calls/refs after).
  pub fn add_edge(&mut self, from: NodeId, to: NodeId, edge: EdgeType) {
    self.edges.push(from.raw() as u32, to.raw() as u32, edge);
  }

  /// Ingest one file's extracted outline (see [`KgWriter::ingest_file_with_spans`]), discarding
  /// the returned spans.
  pub fn ingest_file(&mut self, path: &str, items: &[OutlineItem<'_>]) {
    let _ = self.ingest_file_with_spans(path, items);
  }

  /// Ingest a file's outline — a `File` node, a node per top-level item, and a node per member,
  /// wired with `defines`/`has_method`/`has_field` edges — and return each item/member's
  /// `(byte range, id)` so a caller can attribute references to their enclosing definition (§3.3).
  pub fn ingest_file_with_spans(
    &mut self,
    path: &str,
    items: &[OutlineItem<'_>],
  ) -> Vec<(Range<usize>, NodeId)> {
    let mut spans = Vec::new();
    let file_id = self.define(NodeDef {
      kind: SymbolKind::File,
      name: path,
      entity_path: "",
      path,
      signature: "",
      exported: true,
      content_hash: content_hash(&[path]),
    });
    // The file node is the outermost enclosing scope, so file-level references (e.g. imports)
    // attribute to it when no smaller item/member span contains them.
    spans.push((0..usize::MAX, file_id));

    for item in items {
      let name = item.entry.name.as_ref();
      let signature = item.entry.signature.as_ref();
      let kind = SymbolKind::from_symbol_type(item.entry.symbol_type, item.is_import);
      let item_id = self.define(NodeDef {
        kind,
        name,
        entity_path: name,
        path,
        signature,
        exported: item.is_exported,
        content_hash: content_hash(&[name, signature]),
      });
      self.add_edge(file_id, item_id, EdgeType::DEFINES);
      spans.push((item.entry.range.byte_offset.clone(), item_id));

      for member in &item.members {
        let mname = member.entry.name.as_ref();
        let msig = member.entry.signature.as_ref();
        let mkind = SymbolKind::from_symbol_type(member.entry.symbol_type, false);
        let entity_path = qualified(name, mname);
        let member_id = self.define(NodeDef {
          kind: mkind,
          name: mname,
          entity_path: &entity_path,
          path,
          signature: msig,
          exported: member.is_public,
          content_hash: content_hash(&[&entity_path, msig]),
        });
        self.add_edge(item_id, member_id, mkind.containment_edge());
        spans.push((member.entry.range.byte_offset.clone(), member_id));
      }
    }
    spans
  }

  /// Visit every interned definition — used to build a symbol table for reference resolution.
  pub fn for_each_definition<F: FnMut(NodeId, &str, &str, SymbolKind, bool)>(&self, mut visit: F) {
    for row in 0..self.kind.len() {
      let name = self.heap_str(self.name_off[row], self.name_len[row]);
      let path = self.heap_str(self.path_off[row], self.path_len[row]);
      let kind = SymbolKind::from_tag(self.kind[row]);
      let exported = self.flags[row] & 1 != 0;
      visit(NodeId::new(row as u64), name, path, kind, exported);
    }
  }

  pub fn node_count(&self) -> usize {
    self.kind.len()
  }

  fn push_str(&mut self, s: &str) -> (u32, u32) {
    let off = self.heap.len() as u32;
    self.heap.extend_from_slice(s.as_bytes());
    (off, s.len() as u32)
  }

  fn heap_str(&self, off: u32, len: u32) -> &str {
    std::str::from_utf8(&self.heap[off as usize..(off + len) as usize]).unwrap_or("")
  }

  /// Seal the accumulated nodes into a `.vseg` node segment + string heap and compact the edges
  /// into CSR/CSC (§9.3), returning a queryable graph.
  pub fn seal(mut self) -> Kg {
    let n = self.kind.len() as u32;
    self.canonical.seal();

    let mut builder = SegmentBuilder::new(0);
    builder.add_u8("kind", &self.kind).unwrap();
    builder.add_u32("name_off", &self.name_off).unwrap();
    builder.add_u32("name_len", &self.name_len).unwrap();
    builder.add_u32("path_off", &self.path_off).unwrap();
    builder.add_u32("path_len", &self.path_len).unwrap();
    builder.add_u32("sig_off", &self.sig_off).unwrap();
    builder.add_u32("sig_len", &self.sig_len).unwrap();
    builder.add_u64("content_hash", &self.content_hash).unwrap();
    builder.add_u8("flags", &self.flags).unwrap();
    let nodes = Segment::open_owned(builder.build().unwrap()).unwrap();

    let graph = Graph::compact(n, &self.edges);

    let mut directory = SegmentDirectory::new();
    directory.insert(0, n as u64, 0);

    Kg::new(nodes, self.heap, graph, directory)
  }
}

fn qualified(owner: &str, member: &str) -> String {
  format!("{owner}.{member}")
}

fn content_hash(parts: &[&str]) -> u64 {
  let mut hasher = std::collections::hash_map::DefaultHasher::new();
  for part in parts {
    part.hash(&mut hasher);
  }
  hasher.finish()
}
