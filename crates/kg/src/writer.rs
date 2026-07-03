//! Assembles a knowledge graph from extraction output (§3.1→§3.3).

use std::hash::{Hash, Hasher};

use vorpal_canonical::{CanonicalIndex, CanonicalKey};
use vorpal_graph::{EdgeLog, EdgeType, Graph};
use vorpal_outline::model::OutlineItem;
use vorpal_segment::{Segment, SegmentBuilder, SegmentDirectory};

use crate::kg::Kg;
use crate::model::SymbolKind;

/// One node's attributes, staged for interning + column append.
struct PendingNode<'a> {
  kind: SymbolKind,
  name: &'a str,
  path: &'a str,
  signature: &'a str,
  exported: bool,
  hash: u64,
}

/// Accumulates interned nodes (SoA columns + string heap) and containment edges, then seals a
/// queryable [`Kg`]. Ids are dense and assignment-ordered, so a column row index equals its id.
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

  /// Ingest one file's extracted outline: create a `File` node, a node per top-level item, and a
  /// node per member, wired with `defines` / `has_method` / `has_field` containment edges.
  pub fn ingest_file(&mut self, path: &str, items: &[OutlineItem<'_>]) {
    let file_id = self.add_node(
      CanonicalKey::of(path, ""),
      PendingNode {
        kind: SymbolKind::File,
        name: path,
        path,
        signature: "",
        exported: true,
        hash: content_hash(&[path]),
      },
    );

    for item in items {
      let name = item.entry.name.as_ref();
      let signature = item.entry.signature.as_ref();
      let kind = SymbolKind::from_symbol_type(item.entry.symbol_type, item.is_import);
      let item_id = self.add_node(
        CanonicalKey::of(path, name),
        PendingNode {
          kind,
          name,
          path,
          signature,
          exported: item.is_exported,
          hash: content_hash(&[name, signature]),
        },
      );
      self.edges.push(file_id, item_id, EdgeType::DEFINES);

      for member in &item.members {
        let mname = member.entry.name.as_ref();
        let msig = member.entry.signature.as_ref();
        let mkind = SymbolKind::from_symbol_type(member.entry.symbol_type, false);
        let entity_path = qualified(name, mname);
        let member_id = self.add_node(
          CanonicalKey::of(path, &entity_path),
          PendingNode {
            kind: mkind,
            name: mname,
            path,
            signature: msig,
            exported: member.is_public,
            hash: content_hash(&[&entity_path, msig]),
          },
        );
        self
          .edges
          .push(item_id, member_id, mkind.containment_edge());
      }
    }
  }

  /// Intern an entity and, if new, append its column row. Returns the dense node id (as `u32`).
  fn add_node(&mut self, key: CanonicalKey, node: PendingNode<'_>) -> u32 {
    let assignment = self.canonical.get_or_assign(key, node.hash);
    let id = assignment.node_id().raw() as u32;
    if assignment.is_new() {
      debug_assert_eq!(
        id as usize,
        self.kind.len(),
        "dense assignment-ordered rows"
      );
      let (name_off, name_len) = self.push_str(node.name);
      let (path_off, path_len) = self.push_str(node.path);
      let (sig_off, sig_len) = self.push_str(node.signature);
      self.kind.push(node.kind.tag());
      self.name_off.push(name_off);
      self.name_len.push(name_len);
      self.path_off.push(path_off);
      self.path_len.push(path_len);
      self.sig_off.push(sig_off);
      self.sig_len.push(sig_len);
      self.content_hash.push(node.hash);
      self.flags.push(u8::from(node.exported));
    }
    id
  }

  fn push_str(&mut self, s: &str) -> (u32, u32) {
    let off = self.heap.len() as u32;
    self.heap.extend_from_slice(s.as_bytes());
    (off, s.len() as u32)
  }

  pub fn node_count(&self) -> usize {
    self.kind.len()
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
