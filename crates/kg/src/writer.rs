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
    // Entity paths in layout order (index 0 = file), disambiguated so overloads and same-name
    // different-kind siblings stay distinct. `ingest_file_with_spans` and `local_layout` both
    // source identity from this one function, so their conventions can never drift apart.
    let entity_paths = layout_entity_paths(items);
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

    let mut next = 1usize; // walks `entity_paths` in lockstep with the item/member traversal
    for item in items {
      let name = item.entry.name.as_ref();
      let signature = item.entry.signature.as_ref();
      let kind = SymbolKind::from_symbol_type(item.entry.symbol_type, item.is_import);
      let item_entity = entity_paths[next].as_str();
      next += 1;
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
        let member_entity = entity_paths[next].as_str();
        next += 1;
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

  /// A row's durable external id — the halves of `blake3(path, entity_path)` (§2). Stable
  /// across rebuilds and dense-id shifts, which is exactly what the retained daemon's
  /// edge-repair pass keys on: an edited file's unchanged entities keep their eid, so edges
  /// into them heal by eid lookup instead of forcing a re-resolve.
  pub fn node_eid(&self, row: usize) -> Option<(u64, u64)> {
    Some((*self.eid_lo.get(row)?, *self.eid_hi.get(row)?))
  }

  /// A row's kind alone — one column byte, no heap-string reads. The owner pass over the
  /// containment edge log needs only this, and paying `definition`'s three heap reads per
  /// edge (~2.75M random reads into the mapped heap at kernel scale) to look at one byte was
  /// the bulk of that pass.
  pub fn node_kind(&self, row: usize) -> Option<SymbolKind> {
    self.kind.get(row).map(|&tag| SymbolKind::from_tag(tag))
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
    crate::phase_stamp("seal: columns");
    let n = self.kind.len() as u32;
    self.canonical.seal();
    drop(std::mem::take(&mut self.canonical));

    // The builder borrows the writer's columns directly — zero copies into the builder (the
    // previous owned form copied every column once into the builder and again into the output
    // buffer). The only materialization is the output buffer itself; the columns are freed
    // immediately after, BEFORE edge compaction, so peak transient memory during seal is
    // strictly lower than the old one-column-at-a-time streaming dance.
    let nodes = {
      let mut builder = SegmentBuilder::new(0);
      builder.add_u8("kind", &self.kind).unwrap();
      builder.add_u32("name_off", &self.name_off).unwrap();
      builder.add_u32("name_len", &self.name_len).unwrap();
      builder.add_u32("path_off", &self.path_off).unwrap();
      builder.add_u32("path_len", &self.path_len).unwrap();
      builder.add_u32("sig_off", &self.sig_off).unwrap();
      builder.add_u32("sig_len", &self.sig_len).unwrap();
      builder.add_u64("content_hash", &self.content_hash).unwrap();
      builder.add_u64("eid_lo", &self.eid_lo).unwrap();
      builder.add_u64("eid_hi", &self.eid_hi).unwrap();
      builder.add_u8("flags", &self.flags).unwrap();
      builder.add_u32("span_start", &self.span_start).unwrap();
      builder.add_u32("span_end", &self.span_end).unwrap();
      Segment::open_owned(builder.build().unwrap()).unwrap()
    };
    drop(std::mem::take(&mut self.kind));
    drop(std::mem::take(&mut self.name_off));
    drop(std::mem::take(&mut self.name_len));
    drop(std::mem::take(&mut self.path_off));
    drop(std::mem::take(&mut self.path_len));
    drop(std::mem::take(&mut self.sig_off));
    drop(std::mem::take(&mut self.sig_len));
    drop(std::mem::take(&mut self.content_hash));
    drop(std::mem::take(&mut self.eid_lo));
    drop(std::mem::take(&mut self.eid_hi));
    drop(std::mem::take(&mut self.flags));
    drop(std::mem::take(&mut self.span_start));
    drop(std::mem::take(&mut self.span_end));

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

/// One retained file's footprint inside a long-lived writer — everything a canonical-order
/// seal needs to gather it back out: its contiguous row block, its heap slice, and its
/// containment edge range. All three are contiguous by construction: `absorb` (and direct
/// sequential ingest) appends a file's rows, heap bytes, and containment edges as one block.
#[derive(Debug, Clone)]
pub struct FileBlock {
  pub rows: Range<u32>,
  pub heap: Range<u64>,
  pub edges: Range<u32>,
}

impl KgWriter {
  /// Total edges recorded so far — the containment **watermark** a retained writer captures
  /// after its last ingest, so each re-link can roll resolution edges back with
  /// [`KgWriter::truncate_edges`].
  pub fn edges_len(&self) -> usize {
    self.edges.len()
  }

  /// Roll the edge log back to `len` (see [`KgWriter::edges_len`]).
  pub fn truncate_edges(&mut self, len: usize) {
    self.edges.truncate(len);
  }

  /// Seal in **canonical order**: gather only the rows/heap/edges of `blocks` (alive files,
  /// pre-sorted by path by the caller), renumbering ids densely in that order — the exact
  /// assignment a from-scratch build of the same live file set produces, so the sealed
  /// segment, heap, and graph are byte-identical to that build's.
  ///
  /// `resolution_edges_from` is the containment watermark: edges before it are per-file
  /// containment (gathered per block, both endpoints alive by construction); edges at or
  /// after it are this link's resolution edges (remapped through the same id LUT). The
  /// tombstoned rows left behind by retract-and-append edits are simply never gathered.
  /// Also returns the old→new id LUT (dead rows hold `u32::MAX`): a retained caller remaps
  /// its evidence rows through the same permutation the edges took, so sidecar ids and
  /// sealed ids can never disagree. Borrows the writer — a retained daemon seals a snapshot
  /// per link and keeps ingesting into the same writer afterward.
  pub fn seal_canonical(
    &self,
    blocks: &[FileBlock],
    resolution_edges_from: usize,
  ) -> (Kg, Vec<u32>) {
    let tail: Vec<(u32, u32, EdgeType)> = (resolution_edges_from..self.edges.len())
      .map(|i| self.edges.triple(i))
      .collect();
    self.seal_canonical_with(blocks, tail.into_iter())
  }

  /// The core canonical seal: containment edges gathered per block, this link's resolution
  /// edges supplied by the caller — already in the emission order a from-scratch resolve
  /// produces (the retained daemon keeps them in per-file buckets and chains them in
  /// canonical file order, which IS that order).
  pub fn seal_canonical_with(
    &self,
    blocks: &[FileBlock],
    resolution: impl Iterator<Item = (u32, u32, EdgeType)> + Send,
  ) -> (Kg, Vec<u32>) {
    crate::phase_stamp("seal-canonical: gather");

    let total_rows: usize = blocks.iter().map(|b| b.rows.len()).sum();
    // Old→new id LUT; dead rows keep u32::MAX and must never appear in a surviving edge.
    let mut lut = vec![u32::MAX; self.kind.len()];
    let mut next = 0u32;
    for block in blocks {
      for old in block.rows.clone() {
        lut[old as usize] = next;
        next += 1;
      }
    }

    fn gather<T: Copy>(col: &[T], blocks: &[FileBlock], total: usize) -> Vec<T> {
      let mut out = Vec::with_capacity(total);
      for block in blocks {
        out.extend_from_slice(&col[block.rows.start as usize..block.rows.end as usize]);
      }
      out
    }

    // Heap deltas first (serial prefix sums), then every gather — 13 columns and the heap
    // bytes — fans out in parallel: each is an independent pure copy, and the serial form
    // left this ~memcpy phase on one core (~45ms at kernel scale).
    let heap_total: usize = blocks.iter().map(|b| (b.heap.end - b.heap.start) as usize).sum();
    let mut heap_delta = Vec::with_capacity(blocks.len());
    let mut heap_cursor = 0i64;
    for block in blocks {
      heap_delta.push(heap_cursor - block.heap.start as i64);
      heap_cursor += (block.heap.end - block.heap.start) as i64;
    }
    let gather_off = |col: &[u32]| -> Vec<u32> {
      let mut out = Vec::with_capacity(total_rows);
      for (block, delta) in blocks.iter().zip(&heap_delta) {
        out.extend(
          col[block.rows.start as usize..block.rows.end as usize]
            .iter()
            .map(|&off| (off as i64 + delta) as u32),
        );
      }
      out
    };
    let gather_heap = || {
      let mut out = Vec::with_capacity(heap_total);
      for block in blocks {
        out.extend_from_slice(
          &self.heap.bytes()[block.heap.start as usize..block.heap.end as usize],
        );
      }
      out
    };

    let (
      (((kind, name_off), (name_len, path_off)), ((path_len, sig_off), (sig_len, content_hash))),
      (((eid_lo, eid_hi), (flags, span_start)), (span_end, new_heap)),
    ) = rayon::join(
      || {
        rayon::join(
          || {
            rayon::join(
              || rayon::join(|| gather(&self.kind, blocks, total_rows), || gather_off(&self.name_off)),
              || rayon::join(|| gather(&self.name_len, blocks, total_rows), || gather_off(&self.path_off)),
            )
          },
          || {
            rayon::join(
              || rayon::join(|| gather(&self.path_len, blocks, total_rows), || gather_off(&self.sig_off)),
              || {
                rayon::join(
                  || gather(&self.sig_len, blocks, total_rows),
                  || gather(&self.content_hash, blocks, total_rows),
                )
              },
            )
          },
        )
      },
      || {
        rayon::join(
          || {
            rayon::join(
              || rayon::join(|| gather(&self.eid_lo, blocks, total_rows), || gather(&self.eid_hi, blocks, total_rows)),
              || rayon::join(|| gather(&self.flags, blocks, total_rows), || gather(&self.span_start, blocks, total_rows)),
            )
          },
          || rayon::join(|| gather(&self.span_end, blocks, total_rows), gather_heap),
        )
      },
    );

    let n = total_rows as u32;
    crate::phase_stamp("seal-canonical: tracks");
    // Three independent tracks fan out: the node segment (zone maps + whole-segment digest),
    // the in-memory name index (hash + sort over the gathered name column — the very pairs
    // `build_names_index` would recompute from the sealed segment afterwards), and the graph
    // (edge remap through the LUT + CSR/CSC compaction). Serially these were four ~25ms
    // blocks on the serve path; the critical path is now the longest single track.
    let ((nodes, names), graph) = rayon::join(
      || {
        rayon::join(
          || {
            let mut builder = SegmentBuilder::new(0);
            builder.add_u8("kind", &kind).unwrap();
            builder.add_u32("name_off", &name_off).unwrap();
            builder.add_u32("name_len", &name_len).unwrap();
            builder.add_u32("path_off", &path_off).unwrap();
            builder.add_u32("path_len", &path_len).unwrap();
            builder.add_u32("sig_off", &sig_off).unwrap();
            builder.add_u32("sig_len", &sig_len).unwrap();
            builder.add_u64("content_hash", &content_hash).unwrap();
            builder.add_u64("eid_lo", &eid_lo).unwrap();
            builder.add_u64("eid_hi", &eid_hi).unwrap();
            builder.add_u8("flags", &flags).unwrap();
            builder.add_u32("span_start", &span_start).unwrap();
            builder.add_u32("span_end", &span_end).unwrap();
            Segment::open_owned(builder.build().unwrap()).unwrap()
          },
          || {
            use rayon::prelude::*;
            // Same pairs, same order as Kg::build_names_index over the sealed graph: every
            // row's (xxh3(name), id), sorted by (hash, id).
            let mut pairs: Vec<(u64, u64)> = (0..total_rows as u64)
              .into_par_iter()
              .filter_map(|i| {
                let off = name_off[i as usize] as usize;
                let len = name_len[i as usize] as usize;
                let name = std::str::from_utf8(new_heap.get(off..off + len)?).ok()?;
                Some((xxhash_rust::xxh3::xxh3_64(name.as_bytes()), i))
              })
              .collect();
            pairs.par_sort_unstable();
            let hashes: Vec<u64> = pairs.iter().map(|&(h, _)| h).collect();
            let ids: Vec<u64> = pairs.iter().map(|&(_, i)| i).collect();
            (hashes, ids)
          },
        )
      },
      || {
        // Containment edges per block (scratch order = per-file, path-major), then this
        // link's resolution edges. Dead-endpoint containment edges cannot exist (blocks
        // only cover alive rows and containment never crosses files); a dead endpoint in a
        // resolution edge is an upstream logic error — checked in debug, dropped
        // defensively in release.
        let mut new_edges = EdgeLog::new();
        for block in blocks {
          for i in block.edges.start as usize..block.edges.end as usize {
            let (src, dst, etype) = self.edges.triple(i);
            let (s, d) = (lut[src as usize], lut[dst as usize]);
            debug_assert!(
              s != u32::MAX && d != u32::MAX,
              "containment edge touches a dead row"
            );
            if s != u32::MAX && d != u32::MAX {
              new_edges.push(s, d, etype);
            }
          }
        }
        for (src, dst, etype) in resolution {
          let (s, d) = (lut[src as usize], lut[dst as usize]);
          debug_assert!(
            s != u32::MAX && d != u32::MAX,
            "resolution edge touches a dead row"
          );
          if s != u32::MAX && d != u32::MAX {
            new_edges.push(s, d, etype);
          }
        }
        Graph::compact(n, &new_edges)
      },
    );
    crate::phase_stamp("seal-canonical: assemble kg");

    let mut directory = SegmentDirectory::new();
    directory.insert(0, n as u64, 0);
    let mut kg = Kg::new(nodes, new_heap, graph, directory)
      .expect("sealed segment carries every column the builder just wrote");
    let (hashes, ids) = names;
    kg.set_names_index(hashes, ids);
    crate::phase_stamp("seal-canonical: done");
    (kg, lut)
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
  let base = match owner {
    Some(o) => format!("{o}.{name}"),
    None => name.to_string(),
  };
  if kind.is_overloadable() && !signature.is_empty() {
    format!("{base}\u{1f}{signature}")
  } else {
    base
  }
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
