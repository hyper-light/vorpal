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
  /// Byte range of the definition in `path` (`(0, 0)` when unknown, e.g. File nodes) —
  /// persisted so query surfaces can fetch the defining source verbatim.
  pub span: (u32, u32),
}

/// Where the writer's string heap lives. Shard writers and small trees build in RAM; the
/// merged writer of a streaming commit writes **through to disk** as shards absorb — the
/// concatenated heap (~140 MB at kernel scale) never occupies anonymous memory, and the
/// link pass reads it back through a zero-copy map. `Streaming` is write-only by
/// construction: every read happens after `finalize_streamed_heap` flips it to `Mapped`.
enum HeapStore {
  Ram(Vec<u8>),
  Streaming {
    out: std::io::BufWriter<std::fs::File>,
    len: u64,
    path: std::path::PathBuf,
  },
  Mapped {
    column: vorpal_mem::PodColumn<u8>,
    path: std::path::PathBuf,
  },
}

impl Default for HeapStore {
  fn default() -> Self {
    HeapStore::Ram(Vec::new())
  }
}

impl HeapStore {
  fn len(&self) -> u64 {
    match self {
      HeapStore::Ram(bytes) => bytes.len() as u64,
      HeapStore::Streaming { len, .. } => *len,
      HeapStore::Mapped { column, .. } => column.len() as u64,
    }
  }

  fn append(&mut self, bytes: &[u8]) {
    // Heap offsets are u32 columns; a >4 GiB heap needs a format change, not silent wrap.
    assert!(
      self.len() + bytes.len() as u64 <= u32::MAX as u64,
      "string heap exceeds the u32 offset space"
    );
    match self {
      HeapStore::Ram(heap) => heap.extend_from_slice(bytes),
      HeapStore::Streaming { out, len, .. } => {
        use std::io::Write;
        out.write_all(bytes).expect("streamed heap write failed");
        *len += bytes.len() as u64;
      }
      HeapStore::Mapped { .. } => panic!("append to a finalized writer heap"),
    }
  }

  fn bytes(&self) -> &[u8] {
    match self {
      HeapStore::Ram(heap) => heap,
      HeapStore::Mapped { column, .. } => column,
      HeapStore::Streaming { .. } => {
        panic!("read from a streaming writer heap before finalize_streamed_heap")
      }
    }
  }
}

impl std::fmt::Debug for HeapStore {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    write!(f, "HeapStore(len={})", self.len())
  }
}

/// Accumulates interned nodes (SoA columns + string heap) and edges, then seals a queryable
/// [`Kg`]. Ids are dense and assignment-ordered, so a column row index equals its id.
#[derive(Default)]
pub struct KgWriter {
  canonical: CanonicalIndex,
  /// The current file's path with its interned heap location — one heap copy per file, shared
  /// by every node of that file.
  shared_path: Option<(String, (u32, u32))>,
  edges: EdgeLog,
  heap: HeapStore,
  kind: Vec<u8>,
  name_off: Vec<u32>,
  name_len: Vec<u32>,
  path_off: Vec<u32>,
  path_len: Vec<u32>,
  sig_off: Vec<u32>,
  sig_len: Vec<u32>,
  content_hash: Vec<u64>,
  /// Durable external id (IMPROVEMENTS 07-29 §2): the first 128 bits of the node's canonical
  /// key (`blake3(path, entity_path)`), split into two u64 columns. A pure function of the
  /// symbol's logical identity, so it survives rebuilds and dense-id shifts; a move or rename
  /// is an explicit identity transition (new key), never a silent reuse.
  eid_lo: Vec<u64>,
  eid_hi: Vec<u64>,
  flags: Vec<u8>,
  span_start: Vec<u32>,
  span_end: Vec<u32>,
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
      let kb = key.as_bytes();
      self.eid_lo.push(u64::from_le_bytes(kb[0..8].try_into().unwrap()));
      self.eid_hi.push(u64::from_le_bytes(kb[8..16].try_into().unwrap()));
      self.flags.push(u8::from(def.exported));
      self.span_start.push(def.span.0);
      self.span_end.push(def.span.1);
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
    // Intern the path bytes once for this whole file: every node of the file shares one heap
    // copy via identical (offset, len) column entries — reader-compatible, and it removes the
    // dominant heap duplication (at kernel scale, ~130 MB of repeated path strings for
    // ~3.5 MB of unique paths).
    self.shared_path_for(path);
    let file_id = self.define(NodeDef {
      kind: SymbolKind::File,
      name: path,
      entity_path: "",
      path,
      signature: "",
      exported: true,
      content_hash: content_hash(&[path]),
      span: (0, 0),
    });
    // The file node is the outermost enclosing scope, so file-level references (e.g. imports)
    // attribute to it when no smaller item/member span contains them.
    spans.push((0..usize::MAX, file_id));

    // Each entity's identity path is rendered into ONE reused buffer by the same
    // `write_entity_path_into` that backs `layout_entity_paths` — the conventions are shared
    // by construction (no lockstep to assert), and the per-file layout `Vec<String>` this
    // replaces was ~9 % of stream-phase allocation samples at kernel scale.
    let mut entity_buf = String::new();
    for item in items {
      let name = item.entry.name.as_ref();
      let signature = item.entry.signature.as_ref();
      let kind = SymbolKind::from_symbol_type(item.entry.symbol_type, item.is_import);
      write_entity_path_into(None, name, kind, signature, &mut entity_buf);
      let item_entity = entity_buf.as_str();
      let item_id = self.define(NodeDef {
        kind,
        name,
        entity_path: item_entity,
        path,
        signature,
        exported: item.is_exported,
        content_hash: content_hash(&[item_entity, signature]),
        span: clamp_span(&item.entry.range.byte_offset),
      });
      self.add_edge(file_id, item_id, EdgeType::DEFINES);
      spans.push((item.entry.range.byte_offset.clone(), item_id));

      for member in &item.members {
        let mname = member.entry.name.as_ref();
        let msig = member.entry.signature.as_ref();
        let mkind = SymbolKind::from_symbol_type(member.entry.symbol_type, false);
        write_entity_path_into(Some(name), mname, mkind, msig, &mut entity_buf);
        let member_entity = entity_buf.as_str();
        let member_id = self.define(NodeDef {
          kind: mkind,
          name: mname,
          entity_path: member_entity,
          path,
          signature: msig,
          exported: member.is_public,
          content_hash: content_hash(&[member_entity, msig]),
          span: clamp_span(&member.entry.range.byte_offset),
        });
        self.add_edge(item_id, member_id, mkind.containment_edge());
        spans.push((member.entry.range.byte_offset.clone(), member_id));
      }
    }
    spans
  }

  /// Release growth slack on every column, the string heap, and the edge log. Vec doubling
  /// leaves up to 2× capacity behind — ~150 MB of dead pages at kernel scale — and a writer
  /// that has absorbed its last shard keeps that slack alive through the whole link phase
  /// unless it is returned here.
  pub fn shrink_to_fit(&mut self) {
    self.kind.shrink_to_fit();
    self.name_off.shrink_to_fit();
    self.name_len.shrink_to_fit();
    self.path_off.shrink_to_fit();
    self.path_len.shrink_to_fit();
    self.sig_off.shrink_to_fit();
    self.sig_len.shrink_to_fit();
    self.content_hash.shrink_to_fit();
    self.flags.shrink_to_fit();
    self.span_start.shrink_to_fit();
    self.span_end.shrink_to_fit();
    if let HeapStore::Ram(heap) = &mut self.heap {
      heap.shrink_to_fit();
    }
    self.edges.shrink_to_fit();
  }

  /// Switch the (empty) heap to write-through mode: every appended string byte goes to
  /// `path` instead of anonymous memory. The streaming commit calls this on its merged
  /// writer before the first absorb; reads become possible after
  /// [`KgWriter::finalize_streamed_heap`].
  pub fn stream_heap_to(&mut self, path: &std::path::Path) -> std::io::Result<()> {
    assert_eq!(self.heap.len(), 0, "stream_heap_to on a non-empty heap");
    self.heap = HeapStore::Streaming {
      out: std::io::BufWriter::with_capacity(1 << 20, std::fs::File::create(path)?),
      len: 0,
      path: path.to_path_buf(),
    };
    Ok(())
  }

  /// Flush the streamed heap and reopen it as a zero-copy map, making reads (the link
  /// pass's `definition` lookups) legal again. No-op for RAM heaps.
  pub fn finalize_streamed_heap(&mut self) -> std::io::Result<()> {
    if let HeapStore::Streaming { out, len, path } = &mut self.heap {
      use std::io::Write;
      out.flush()?;
      let path = std::mem::take(path);
      let len = *len as usize;
      // Drop the writer handle before mapping.
      let placeholder = HeapStore::Ram(Vec::new());
      drop(std::mem::replace(&mut self.heap, placeholder));
      let store = std::sync::Arc::new(vorpal_mem::MappedStore::map_file(
        &path,
        vorpal_mem::StoreKind::VectorsFull,
        vorpal_mem::AccessPattern::Random,
        vorpal_mem::Hotness::Hot,
        &vorpal_mem::ResourcePolicy::probe(vorpal_mem::CorpusProbe::new(0, 0)),
      )?);
      let column = vorpal_mem::PodColumn::from_mapped_le(&store, 0, len, u8::from_le_bytes)?;
      self.heap = HeapStore::Mapped { column, path };
    }
    Ok(())
  }

  /// Forget canonical-identity keys accumulated so far, keeping the id counter (new
  /// definitions continue receiving fresh dense ids). Sound only when no already-applied
  /// `(path, entity)` will be defined or looked up again on this writer — the product-apply
  /// invariant (each path lands exactly once). Turns the identity map from corpus-sized into
  /// file-sized during bulk builds.
  pub fn forget_identity_scope(&mut self) {
    self.canonical.forget_keys();
  }
  /// The interned id for an entity (`entity_path` within `path`), if defined — the stable-key
  /// lookup replayed extraction products use to re-attribute their references.
  pub fn entity_id(&self, path: &str, entity_path: &str) -> Option<NodeId> {
    self.canonical.lookup(&CanonicalKey::of(path, entity_path))
  }

  /// The containment/link edges accumulated so far — lets a resolver derive member→owner
  /// relations (qualified-reference matching) before the graph is sealed.
  pub fn edge_log(&self) -> &EdgeLog {
    &self.edges
  }

  /// Absorb another writer's rows and edges after this writer's, rebasing node ids and heap
  /// offsets — the merge step of the §7.5 sharded single-writer commit. Returns the id base
  /// the absorbed writer's local ids were shifted by, so the caller can rebase anything else
  /// that carries them (buffered references).
  ///
  /// Because canonical identity is path-qualified and a file's product lands in exactly one
  /// shard, absorbed row sets are disjoint by construction — and absorbing shards in their
  /// original (path-sorted) order reproduces the exact id assignment a single serial writer
  /// would have produced, byte for byte.
  ///
  /// The absorbed writer's canonical index is discarded: a combined writer serves
  /// `for_each_definition` / `edge_log` / `add_edge` / `seal`, but **must not** be used for
  /// further `define` or `entity_id` calls (its canonical index no longer covers the absorbed
  /// rows; `define` would fire the dense-assignment debug assertion).
  pub fn absorb(&mut self, other: KgWriter) -> u64 {
    let id_base = self.kind.len() as u64;
    let heap_base = self.heap.len() as u32;
    self.heap.append(other.heap.bytes());
    self.kind.extend_from_slice(&other.kind);
    self
      .name_off
      .extend(other.name_off.iter().map(|off| off + heap_base));
    self.name_len.extend_from_slice(&other.name_len);
    self
      .path_off
      .extend(other.path_off.iter().map(|off| off + heap_base));
    self.path_len.extend_from_slice(&other.path_len);
    self
      .sig_off
      .extend(other.sig_off.iter().map(|off| off + heap_base));
    self.sig_len.extend_from_slice(&other.sig_len);
    self.content_hash.extend_from_slice(&other.content_hash);
    self.eid_lo.extend_from_slice(&other.eid_lo);
    self.eid_hi.extend_from_slice(&other.eid_hi);
    self.flags.extend_from_slice(&other.flags);
    self.span_start.extend_from_slice(&other.span_start);
    self.span_end.extend_from_slice(&other.span_end);
    for (src, dst, etype) in other.edges.iter() {
      self
        .edges
        .push(src + id_base as u32, dst + id_base as u32, etype);
    }
    id_base
  }

  /// Visit every interned definition — used to build a symbol table for reference resolution.
  pub fn for_each_definition<F: FnMut(NodeId, &str, &str, SymbolKind, bool)>(&self, mut visit: F) {
    for row in 0..self.kind.len() {
      let (id, name, path, kind, exported) = self.definition(row).expect("row < node_count");
      visit(id, name, path, kind, exported);
    }
  }

  /// Visit every File node with its path — the kind column gates the string decode, so a
  /// pass that relates files (co-change) touches 76k rows' strings at kernel scale, not 2.7M.
  pub fn for_each_file<F: FnMut(NodeId, &str)>(&self, mut visit: F) {
    let file_tag = SymbolKind::File.tag();
    for row in 0..self.kind.len() {
      if self.kind[row] == file_tag {
        visit(
          NodeId::new(row as u64),
          self.heap_str(self.path_off[row], self.path_len[row]),
        );
      }
    }
  }

  /// One interned definition by dense row (`row == id`): random access for sharded table
  /// builds, where contiguous row ranges are processed on independent threads (§7.5).
  pub fn definition(&self, row: usize) -> Option<(NodeId, &str, &str, SymbolKind, bool)> {
    if row >= self.kind.len() {
      return None;
    }
    Some((
      NodeId::new(row as u64),
      self.heap_str(self.name_off[row], self.name_len[row]),
      self.heap_str(self.path_off[row], self.path_len[row]),
      SymbolKind::from_tag(self.kind[row]),
      self.flags[row] & 1 != 0,
    ))
  }

  pub fn node_count(&self) -> usize {
    self.kind.len()
  }

  /// Current string-heap size in bytes. Names/paths/signatures are addressed with 32-bit
  /// offsets, so this is checked against the 4 GiB ceiling before the index is sealed.
  pub fn heap_len(&self) -> u64 {
    self.heap.len()
  }

  fn push_str(&mut self, s: &str) -> (u32, u32) {
    if let Some(&(off, len)) = self
      .shared_path
      .as_ref()
      .filter(|(text, _)| text == s)
      .map(|(_, at)| at)
    {
      return (off, len);
    }
    let off = self.heap.len() as u32;
    self.heap.append(s.as_bytes());
    (off, s.len() as u32)
  }

  /// Register `path` as the current file's shared heap string: subsequent `push_str` calls
  /// with the same text reuse one heap copy instead of appending a duplicate per node.
  fn shared_path_for(&mut self, path: &str) {
    let off = self.heap.len() as u32;
    self.heap.append(path.as_bytes());
    self.shared_path = Some((path.to_owned(), (off, path.len() as u32)));
  }

  fn heap_str(&self, off: u32, len: u32) -> &str {
    std::str::from_utf8(&self.heap.bytes()[off as usize..(off + len) as usize]).unwrap_or("")
  }

  /// Seal the accumulated nodes into a `.vseg` node segment + string heap and compact the edges
  /// into CSR/CSC (§9.3), returning a queryable graph.
  ///
  /// Columns stream into the segment one at a time — each is taken out of the writer, copied,
  /// and freed before the next is touched — so peak transient memory during seal is one
  /// column's worth, not a second full copy of every column at once. The edge log is likewise
  /// dropped as soon as the compacted graph exists.
  pub fn seal(mut self) -> Kg {
    crate::phase_stamp("seal: scc");
    let n = self.kind.len() as u32;
    // Strongly-connected-component sizes over calls edges (B6 v1.5): computed here from
    // the finished edge log — a DERIVED column, so shard absorption has nothing to merge
    // and the absorb/shrink column discipline does not apply to it.
    let scc = crate::scc::scc_sizes(n as usize, &self.edges);
    crate::phase_stamp("seal: columns");
    self.canonical.seal();
    drop(std::mem::take(&mut self.canonical));

    let mut builder = SegmentBuilder::new(0);
    let kind = std::mem::take(&mut self.kind);
    builder.add_u8("kind", &kind).unwrap();
    drop(kind);
    let name_off = std::mem::take(&mut self.name_off);
    builder.add_u32("name_off", &name_off).unwrap();
    drop(name_off);
    let name_len = std::mem::take(&mut self.name_len);
    builder.add_u32("name_len", &name_len).unwrap();
    drop(name_len);
    let path_off = std::mem::take(&mut self.path_off);
    builder.add_u32("path_off", &path_off).unwrap();
    drop(path_off);
    let path_len = std::mem::take(&mut self.path_len);
    builder.add_u32("path_len", &path_len).unwrap();
    drop(path_len);
    let sig_off = std::mem::take(&mut self.sig_off);
    builder.add_u32("sig_off", &sig_off).unwrap();
    drop(sig_off);
    let sig_len = std::mem::take(&mut self.sig_len);
    builder.add_u32("sig_len", &sig_len).unwrap();
    drop(sig_len);
    let content_hash = std::mem::take(&mut self.content_hash);
    builder.add_u64("content_hash", &content_hash).unwrap();
    drop(content_hash);
    let eid_lo = std::mem::take(&mut self.eid_lo);
    builder.add_u64("eid_lo", &eid_lo).unwrap();
    drop(eid_lo);
    let eid_hi = std::mem::take(&mut self.eid_hi);
    builder.add_u64("eid_hi", &eid_hi).unwrap();
    drop(eid_hi);
    let flags = std::mem::take(&mut self.flags);
    builder.add_u8("flags", &flags).unwrap();
    drop(flags);
    let span_start = std::mem::take(&mut self.span_start);
    builder.add_u32("span_start", &span_start).unwrap();
    drop(span_start);
    let span_end = std::mem::take(&mut self.span_end);
    builder.add_u32("span_end", &span_end).unwrap();
    drop(span_end);
    builder.add_u32("scc_size", &scc).unwrap();
    drop(scc);
    let nodes = Segment::open_owned(builder.build().unwrap()).unwrap();

    crate::phase_stamp("seal: compact");
    let edges = std::mem::take(&mut self.edges);
    let graph = Graph::compact(n, &edges);
    drop(edges);
    crate::phase_stamp("seal: kg assemble");

    let mut directory = SegmentDirectory::new();
    directory.insert(0, n as u64, 0);

    match self.heap {
      HeapStore::Ram(heap) => Kg::new(nodes, heap, graph, directory),
      HeapStore::Mapped { column, path } => {
        Kg::with_heap_column(nodes, column, Some(path), graph, directory)
      }
      HeapStore::Streaming { .. } => {
        unreachable!("finalize_streamed_heap runs before seal on the streaming path")
      }
    }
    .expect("sealed segment carries every column the builder just wrote")
  }
}

/// The within-file entity paths for a file's outline, in **layout order**: index 0 is the file
/// (entity `""`), then each item immediately followed by its members — exactly the order
/// [`KgWriter::ingest_file_with_spans`] walks and `local_layout` reproduces for reference
/// attribution. Same-named siblings are disambiguated by their **signature**, so overloads
/// (`f(int)` vs `f(str)`, `T.m()` vs `T.m(x)`) never collapse onto one identity. Both the writer
/// (which keys node identity on the entity path) and reference attribution consume this, keeping
/// the two conventions in lockstep.
pub fn layout_entity_paths(items: &[OutlineItem<'_>]) -> Vec<String> {
  let mut out = Vec::with_capacity(1 + items.iter().map(|i| 1 + i.members.len()).sum::<usize>());
  out.push(String::new()); // the file node
  for item in items {
    let kind = SymbolKind::from_symbol_type(item.entry.symbol_type, item.is_import);
    out.push(disambiguated_entity_path(
      None,
      item.entry.name.as_ref(),
      kind,
      item.entry.signature.as_ref(),
    ));
    for member in &item.members {
      let mkind = SymbolKind::from_symbol_type(member.entry.symbol_type, false);
      out.push(disambiguated_entity_path(
        Some(item.entry.name.as_ref()),
        member.entry.name.as_ref(),
        mkind,
        member.entry.signature.as_ref(),
      ));
    }
  }
  out
}

/// One entity path: the owner-qualified name, plus a signature discriminator **only for
/// overloadable (callable) kinds with a non-empty signature**. This splits overloads (`f(int)`
/// vs `f(str)`, `T.m()` vs `T.m(x)`) while leaving non-callables — types, `impl` blocks, fields,
/// imports — on their bare name, so `impl Reader` still merges into `struct Reader` and a
/// re-opened type keeps one identity. `0x1f` (unit separator) cannot appear in a source
/// identifier or an extracted signature, so it can never shift a component boundary and collide
/// two distinct entities.
fn disambiguated_entity_path(
  owner: Option<&str>,
  name: &str,
  kind: SymbolKind,
  signature: &str,
) -> String {
  let mut out = String::new();
  write_entity_path_into(owner, name, kind, signature, &mut out);
  out
}

/// Render one entity path into a reused buffer (cleared first) — the SINGLE writer of the
/// identity convention: `[owner.]name`, plus the `\u{1f}` signature discriminator only for
/// overloadable kinds with a non-empty signature (see [`disambiguated_entity_path`] for the
/// why). Everything that names an entity — [`layout_entity_paths`], the ingest walk's inline
/// rendering, [`EntityIdentity`] — routes through here, so the conventions cannot drift.
fn write_entity_path_into(
  owner: Option<&str>,
  name: &str,
  kind: SymbolKind,
  signature: &str,
  out: &mut String,
) {
  out.clear();
  if let Some(o) = owner {
    out.push_str(o);
    out.push('.');
  }
  out.push_str(name);
  if kind.is_overloadable() && !signature.is_empty() {
    out.push('\u{1f}');
    out.push_str(signature);
  }
}

/// One layout position's identity, **borrowed** from the outline items — the same data
/// [`layout_entity_paths`] renders, without the `String`. Reference attribution needs only the
/// *owner segment* of an entity path (its first `.`-segment, for `self.`/`Self::` receiver
/// classification), so the extraction worker keeps a `Vec<EntityIdentity>` (one allocation per
/// file) instead of a rendered path per entity and reconstructs the segment on demand.
#[derive(Clone, Copy, Debug)]
pub struct EntityIdentity<'a> {
  owner: Option<&'a str>,
  name: &'a str,
  /// The signature **iff the discriminator would be appended** (overloadable kind, non-empty
  /// signature) — empty otherwise, so reconstruction never re-decides the condition.
  sig: &'a str,
}

impl<'a> EntityIdentity<'a> {
  /// The file node's identity (layout index 0, entity path `""`).
  pub const FILE: EntityIdentity<'static> = EntityIdentity { owner: None, name: "", sig: "" };

  pub fn new(owner: Option<&'a str>, name: &'a str, kind: SymbolKind, signature: &'a str) -> Self {
    let sig = if kind.is_overloadable() && !signature.is_empty() {
      signature
    } else {
      ""
    };
    EntityIdentity { owner, name, sig }
  }

  /// Exactly `path.split('.').next()` filtered to non-empty — where `path` is what
  /// [`disambiguated_entity_path`] would render for this identity — computed without building
  /// the path (pinned against the rendered form by `entity_identity_tests`). Only the rare
  /// `name\u{1f}signature` composite (top-level overloadable, dot-free name) must allocate.
  pub fn owner_segment(&self) -> Option<String> {
    if let Some(owner) = self.owner {
      // `owner.name…`: the first `.`-segment ends inside (or exactly at the end of) `owner`.
      let segment = &owner[..owner.find('.').unwrap_or(owner.len())];
      return (!segment.is_empty()).then(|| segment.to_string());
    }
    if let Some(dot) = self.name.find('.') {
      let segment = &self.name[..dot];
      return (!segment.is_empty()).then(|| segment.to_string());
    }
    if !self.sig.is_empty() {
      // `name\u{1f}sig…`: the segment crosses the separator — the one case that must build.
      let sig_head = &self.sig[..self.sig.find('.').unwrap_or(self.sig.len())];
      let mut segment = String::with_capacity(self.name.len() + 1 + sig_head.len());
      segment.push_str(self.name);
      segment.push('\u{1f}');
      segment.push_str(sig_head);
      return Some(segment); // contains `\u{1f}`, so never empty
    }
    (!self.name.is_empty()).then(|| self.name.to_string())
  }
}

/// [`layout_entity_paths`], borrowed: the same layout order (file, then each item immediately
/// followed by its members) carrying [`EntityIdentity`] views instead of rendered `String`s.
pub fn layout_entity_identities<'a>(items: &'a [OutlineItem<'_>]) -> Vec<EntityIdentity<'a>> {
  let mut out = Vec::with_capacity(1 + items.iter().map(|i| 1 + i.members.len()).sum::<usize>());
  out.push(EntityIdentity::FILE);
  for item in items {
    let kind = SymbolKind::from_symbol_type(item.entry.symbol_type, item.is_import);
    out.push(EntityIdentity::new(
      None,
      item.entry.name.as_ref(),
      kind,
      item.entry.signature.as_ref(),
    ));
    for member in &item.members {
      let mkind = SymbolKind::from_symbol_type(member.entry.symbol_type, false);
      out.push(EntityIdentity::new(
        Some(item.entry.name.as_ref()),
        member.entry.name.as_ref(),
        mkind,
        member.entry.signature.as_ref(),
      ));
    }
  }
  out
}

/// Clamp a byte range to the u32 column space (files past 4 GiB store a saturated span).
fn clamp_span(range: &Range<usize>) -> (u32, u32) {
  (
    range.start.min(u32::MAX as usize) as u32,
    range.end.min(u32::MAX as usize) as u32,
  )
}

fn content_hash(parts: &[&str]) -> u64 {
  let mut hasher = std::collections::hash_map::DefaultHasher::new();
  for part in parts {
    part.hash(&mut hasher);
  }
  hasher.finish()
}

#[cfg(test)]
mod entity_identity_tests {
  use super::*;

  /// `owner_segment` reconstructs `rendered_path.split('.').next()` (non-empty-filtered)
  /// without building the path. One case per branch of the reconstruction, expected segment
  /// written literally — the drift anchor between the borrowed identity and the rendered
  /// convention.
  #[test]
  fn owner_segment_matches_rendered_path_split() {
    // (owner, name, kind, signature, expected segment)
    let cases: &[(Option<&str>, &str, SymbolKind, &str, Option<&str>)] = &[
      // Layout index 0: the file node — empty path, no owner.
      (None, "", SymbolKind::File, "", None),
      // Member of a plain-named item: segment = the owner.
      (Some("Kg"), "load", SymbolKind::Method, "(x)", Some("Kg")),
      // Member of a dotted item name (Lua `function M.sub.f()` shapes): the owner's head.
      (Some("M.sub"), "f", SymbolKind::Method, "", Some("M")),
      // Top-level dotted name: the name's head.
      (None, "Foo.bar", SymbolKind::Function, "", Some("Foo")),
      // Non-overloadable kind: bare name, signature never appended.
      (None, "Reader", SymbolKind::Struct, "(ignored)", Some("Reader")),
      // Top-level overloadable with a signature: the segment crosses the `\u{1f}` separator
      // and swallows the signature up to ITS first dot — the one case that must build.
      (None, "f", SymbolKind::Function, "(int)", Some("f\u{1f}(int)")),
      (None, "f", SymbolKind::Function, "(a.b)", Some("f\u{1f}(a")),
    ];
    for &(owner, name, kind, sig, want) in cases {
      let path = disambiguated_entity_path(owner, name, kind, sig);
      let split = path.split('.').next().unwrap_or(path.as_str());
      let rendered = (!split.is_empty()).then(|| split.to_string());
      let got = EntityIdentity::new(owner, name, kind, sig).owner_segment();
      assert_eq!(got, rendered, "reconstruction drifted from the rendered path {path:?}");
      assert_eq!(got.as_deref(), want, "segment spec changed for path {path:?}");
    }
  }
}
