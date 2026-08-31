//! The sealed, queryable knowledge graph (§3.3, §3.5, §11).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use vorpal_graph::{Direction, EdgeType, Graph, reachable};
use vorpal_mem::{AccessPattern, CorpusProbe, Hotness, MappedStore, ResourcePolicy, StoreKind};
use vorpal_segment::{NodeId, Segment, SegmentDirectory, SegmentError};

use crate::model::SymbolKind;

/// One stable way to say *which* symbol a query means — shared by the library, CLI, MCP,
/// and bindings. Display names are ergonomics, not identity: a bare-name selector that
/// matches several definitions is **ambiguous**, and query surfaces present the candidates
/// instead of silently merging their neighborhoods.
#[derive(Debug, Clone, Copy, Default)]
pub struct SymbolSelector<'a> {
  /// Dense node id — exact identity within one index generation.
  pub id: Option<u64>,
  /// Display name (used via the persisted name index).
  pub name: Option<&'a str>,
  /// Path filter: the node's file path must end with this suffix.
  pub path_suffix: Option<&'a str>,
  /// Symbol kind filter.
  pub kind: Option<crate::SymbolKind>,
  /// Durable external id (128-bit, hex on the wire) — the cross-generation bookmark form.
  pub external_id: Option<u128>,
}

/// "VNI1" — the persisted name index's magic.
const NAMES_MAGIC: u32 = 0x564E_4931;

/// Map `names.idx` under `dir` if present and shaped for exactly `node_count` rows; `None`
/// (scan fallback) otherwise — an older index dir simply lacks the sidecar.
/// Resolve an index root to the directory actually holding its artifacts (IMPROVEMENTS §4).
///
/// A generation-layout root carries a small `CURRENT` file naming the live, immutable,
/// content-addressed generation (`gen/<id>`); artifacts live inside it and a rebuild swaps
/// `CURRENT` atomically, so a reader always sees one complete generation — never a mixture.
/// A legacy/flat root (no `CURRENT`) resolves to itself, so pre-generation indexes and
/// unit tests that write artifacts directly keep working unchanged.
///
/// Idempotent: a resolved generation directory contains no `CURRENT`, so re-resolving is a
/// no-op — callers may resolve defensively at any boundary. A `CURRENT` that names a missing
/// or escaping path is ignored (treated as no index) rather than followed.
pub fn resolve_index_dir(root: &Path) -> PathBuf {
  let pointer = root.join("CURRENT");
  let Ok(named) = std::fs::read_to_string(&pointer) else {
    return root.to_path_buf();
  };
  let named = named.trim();
  // The pointer must name a simple relative subpath (e.g. "gen/<id>"): no absolute paths, no
  // parent escapes — a corrupt or hostile pointer must not redirect reads outside the root.
  let ok_shape = !named.is_empty()
    && !named.starts_with('/')
    && std::path::Path::new(named)
      .components()
      .all(|c| matches!(c, std::path::Component::Normal(_)));
  if !ok_shape {
    return root.to_path_buf();
  }
  let target = root.join(named);
  if target.is_dir() {
    target
  } else {
    root.to_path_buf()
  }
}

fn open_names_index(
  dir: &Path,
  policy: &ResourcePolicy,
  node_count: usize,
) -> Option<(vorpal_mem::PodColumn<u64>, vorpal_mem::PodColumn<u64>)> {
  let store = std::sync::Arc::new(
    MappedStore::map_file(
      &dir.join("names.idx"),
      StoreKind::Canonical,
      AccessPattern::Random,
      Hotness::Hot,
      policy,
    )
    .ok()?,
  );
  let bytes = store.as_bytes();
  if bytes.len() < 8 || bytes[0..4] != NAMES_MAGIC.to_le_bytes() {
    return None;
  }
  let count = u32::from_le_bytes(bytes[4..8].try_into().ok()?) as usize;
  if count != node_count || bytes.len() != 8 + count * 16 {
    return None; // foreign generation or torn write: scan fallback stays correct
  }
  let hashes =
    vorpal_mem::PodColumn::from_mapped_le(&store, 8, count * 8, u64::from_le_bytes).ok()?;
  let ids =
    vorpal_mem::PodColumn::from_mapped_le(&store, 8 + count * 8, count * 8, u64::from_le_bytes)
      .ok()?;
  Some((hashes, ids))
}

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
  /// Durable external id: first 128 bits of `blake3(path, entity_path)` — stable across
  /// rebuilds and dense-id shifts for the same logical symbol; a move/rename mints a new id
  /// (an explicit identity transition). `None` on segments written before the columns existed.
  pub external_id: Option<u128>,
  pub exported: bool,
  /// Definition byte range in `path`; `(0, 0)` when unknown (File nodes, pre-span segments).
  pub span: (u32, u32),
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
  /// Definition byte spans — `None` on segments written before the columns existed
  /// (spans then read as `(0, 0)`: "unknown").
  span_start: Option<usize>,
  span_end: Option<usize>,
  /// Durable external id halves — `None` on pre-eid segments (external ids then read `None`).
  eid_lo: Option<usize>,
  eid_hi: Option<usize>,
  /// Calls-SCC size (B6 v1.5) — `None` on segments sealed before the column existed.
  scc_size: Option<usize>,
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
      span_start: segment.column_index("span_start"),
      span_end: segment.column_index("span_end"),
      eid_lo: segment.column_index("eid_lo"),
      eid_hi: segment.column_index("eid_hi"),
      scc_size: segment.column_index("scc_size"),
    })
  }
}

/// A queryable knowledge graph: a node segment (SoA columns) + string heap + compacted graph.
pub struct Kg {
  nodes: Segment,
  cols: NodeColumns,
  heap: vorpal_mem::PodColumn<u8>,
  /// Persisted name index (`names.idx`): `(xxh3(name), id)` pairs sorted by `(hash, id)`,
  /// mapped zero-copy. `None` for index dirs written before the sidecar existed — lookups
  /// fall back to the parallel scan.
  names: Option<(vorpal_mem::PodColumn<u64>, vorpal_mem::PodColumn<u64>)>,
  /// Where the heap bytes already live on disk, when they do (streamed commit or load) —
  /// lets `save` rename or skip instead of rewriting a file readers may have mapped.
  heap_file: Option<std::path::PathBuf>,
  /// Per-edge evidence sidecar (`evidence.bin`), mapped zero-copy. `None` for in-RAM graphs
  /// and generations written before the sidecar existed — queries answer "no evidence
  /// recorded", never an error.
  evidence: Option<crate::evidence::EvidenceStore>,
  graph: Graph,
  directory: SegmentDirectory,
  /// The community sidecar (`communities.bin`), loaded on first use and validated against
  /// the node-segment stamp — absent or stale reads as `None` for every node.
  communities: std::sync::OnceLock<Option<Vec<u32>>>,
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
      names: None,
      evidence: None,
      graph,
      directory,
      communities: std::sync::OnceLock::new(),
    })
  }

  /// This node's `calls`-graph community (dense id), from the warm-time sidecar. `None`
  /// until a warm has built it for this generation — "unknown", never "alone".
  pub fn community(&self, id: NodeId) -> Option<u32> {
    let table = self.communities.get_or_init(|| {
      let dir = self.heap_file.as_ref()?.parent()?;
      let stamp = xxhash_rust::xxh3::xxh3_64(self.node_segment_bytes());
      crate::communities::load(dir, stamp, self.node_count())
    });
    table.as_ref()?.get(id.raw() as usize).copied()
  }

  /// The whole community table, when built (for summaries that walk every node once).
  pub fn communities(&self) -> Option<&[u32]> {
    self.community(NodeId::new(0));
    self.communities.get().and_then(|t| t.as_deref())
  }

  pub fn node_count(&self) -> usize {
    self.nodes.row_count() as usize
  }

  /// This node's calls-cycle component size: 1 outside any recursion, the knot's node
  /// count inside one. `None` on segments sealed before the column existed — absence is
  /// "unknown", never "acyclic".
  pub fn scc_size(&self, id: NodeId) -> Option<u32> {
    let (_segment, row) = self.directory.locate(id)?;
    self.nodes.column_at(self.cols.scc_size?)?.get_u32(row)
  }

  /// Total directed edges (each stored edge counted once).
  pub fn edge_count(&self) -> u64 {
    self.graph.edge_count() as u64
  }

  /// The underlying CSR/CSC graph — read-only access for traversal engines that need
  /// allocation-free adjacency walks with their own budgets (vorpal-query's bounded BFS),
  /// which `out_neighbors`/`in_neighbors` (allocating) and the fixed-shape `reachable_*`
  /// wrappers don't serve.
  pub fn graph(&self) -> &vorpal_graph::Graph {
    &self.graph
  }

  /// Incoming-edge count for one node — two mapped offset reads, no allocation (unlike
  /// [`Kg::in_neighbors`], which materializes the row).
  pub fn in_degree(&self, id: NodeId) -> usize {
    self.graph.in_degree(id.raw() as u32)
  }

  /// In-degree over REFERENTIAL edges only — the "how used" signal: resolution-emitted
  /// reference families plus request/notify usage, EXCLUDING derived and topological
  /// families (`data_flows` duplicates its call pair; `similar_to` and `changes_with` are
  /// similarity/history topology, not usage) and structural containment. The flow-era
  /// merge added those families, silently inflating every raw-degree "popularity" reading;
  /// rank signals must consume THIS, never the raw CSC degree.
  pub fn in_degree_referential(&self, id: NodeId) -> usize {
    self
      .in_edge_types_of(id)
      .iter()
      .filter(|&&tag| {
        matches!(
          EdgeType(tag).base(),
          EdgeType::CALLS
            | EdgeType::REFERENCES
            | EdgeType::IMPORTS
            | EdgeType::IMPLEMENTS
            | EdgeType::OF_TYPE
            | EdgeType::OVERRIDES
            | EdgeType::REQUESTS
            | EdgeType::NOTIFIES
        )
      })
      .count()
  }

  /// The incoming edges' packed type tags for one node, zero-copy (confidence in the high
  /// byte — compare through [`EdgeType::base`]). The allocation-free spine of whole-graph
  /// liveness scans.
  pub fn in_edge_types_of(&self, id: NodeId) -> &[u16] {
    self.graph.in_edge_types(id.raw() as u32)
  }

  /// Visit the referenced-name hash of every retained evidence occurrence (all outcomes).
  /// Returns `false` when the generation carries no sidecar — callers must then treat
  /// name-based suppression as unavailable, not as "nothing was referenced".
  pub fn for_each_evidence_name_hash(&self, f: impl FnMut(u32)) -> bool {
    match &self.evidence {
      Some(store) => {
        store.for_each_name_hash(f);
        true
      }
      None => false,
    }
  }

  /// Outgoing-edge count for one node — two mapped offset reads, no allocation.
  pub fn out_degree(&self, id: NodeId) -> usize {
    self.graph.out_degree(id.raw() as u32)
  }

  /// Node counts bucketed by symbol kind: one pass over the mapped kind column (u8/node).
  /// Ascending tag order, zero buckets omitted — deterministic by construction.
  pub fn node_count_by_kind(&self) -> Vec<(SymbolKind, u64)> {
    let mut buckets = [0u64; 256];
    if let Some(column) = self.nodes.column_at(self.cols.kind) {
      if let Some(tags) = column.as_slice::<u8>() {
        for &tag in tags {
          buckets[tag as usize] += 1;
        }
      } else {
        for row in 0..self.nodes.row_count() {
          if let Some(tag) = column.get_u8(row) {
            buckets[tag as usize] += 1;
          }
        }
      }
    }
    buckets
      .iter()
      .enumerate()
      .filter(|&(_, &count)| count > 0)
      .map(|(tag, &count)| (SymbolKind::from_tag(tag as u8), count))
      .collect()
  }

  /// Directed edge counts bucketed by base relation: one parallel pass over the out-CSR
  /// type column (u16/edge, confidence byte stripped; per-chunk buckets merged — counts
  /// commute, so the result is order-independent). Ascending tag order, zero buckets omitted.
  pub fn edge_count_by_type(&self) -> Vec<(EdgeType, u64)> {
    use rayon::prelude::*;
    let etypes = self.graph.out_etypes_all();
    let buckets = etypes
      .par_chunks(1 << 20)
      .fold(
        || [0u64; 256],
        |mut buckets, chunk| {
          for &packed in chunk {
            buckets[EdgeType(packed).base().0 as usize & 0xff] += 1;
          }
          buckets
        },
      )
      .reduce(
        || [0u64; 256],
        |mut a, b| {
          for (a, b) in a.iter_mut().zip(b) {
            *a += b;
          }
          a
        },
      );
    buckets
      .iter()
      .enumerate()
      .filter(|&(_, &count)| count > 0)
      .map(|(tag, &count)| (EdgeType(tag as u16), count))
      .collect()
  }

  /// Every retained evidence occurrence for edges `from → to` (all edge types): the source
  /// span of each referencing token, the resolver branch that bound it, its confidence, and
  /// the candidate count — "why does this relation exist?" (§5). Empty when the generation
  /// carries no sidecar or the pair has none.
  pub fn edge_evidence(&self, from: NodeId, to: NodeId) -> Vec<crate::evidence::EvidenceRow> {
    self
      .evidence
      .as_ref()
      .map(|store| store.edges_between(from.raw() as u32, to.raw() as u32))
      .unwrap_or_default()
  }

  /// The complete evidence row set — every edge occurrence resolution emitted, in canonical
  /// order. Empty when the generation carries no sidecar. This is the population a
  /// precision/recall evaluation measures over.
  pub fn all_evidence(&self) -> Vec<crate::evidence::EvidenceRow> {
    self
      .evidence
      .as_ref()
      .map(|store| store.rows().collect())
      .unwrap_or_default()
  }

  /// The no-edge occurrences at `from` whose referenced-name hash matches (low 32 bits of
  /// xxh3 of the name) — "why is there no edge from here to anything named X?".
  pub fn evidence_absences(&self, from: NodeId, name_hash: u32) -> Vec<crate::evidence::EvidenceRow> {
    self
      .evidence
      .as_ref()
      .map(|store| store.absences_from(from.raw() as u32, name_hash))
      .unwrap_or_default()
  }

  /// Every retained evidence occurrence originating at `from` — the one-sided form.
  pub fn evidence_from(&self, from: NodeId) -> Vec<crate::evidence::EvidenceRow> {
    self
      .evidence
      .as_ref()
      .map(|store| store.edges_from(from.raw() as u32))
      .unwrap_or_default()
  }

  /// The sealed node segment's raw bytes. The ANN freshness stamp is `xxh3` of exactly these
  /// bytes; hashing the *loaded* mapping (instead of re-reading `nodes.vseg`) pins the stamp
  /// to the generation actually being served — no load/hash race with a concurrent rebuild.
  pub fn node_segment_bytes(&self) -> &[u8] {
    self.nodes.bytes()
  }

  pub fn is_empty(&self) -> bool {
    self.node_count() == 0
  }

  /// Resolve a node's attributes (§3.3). Reads HOT columns (`base + row·stride`) + the heap.
  /// Just the node's name, zero-copy — the whole-graph name-scan primitive (pattern
  /// matching, dedup passes) where materializing the full [`NodeView`] would read three
  /// heap strings per row to use one.
  pub fn node_name(&self, id: NodeId) -> Option<&str> {
    let (_segment, row) = self.directory.locate(id)?;
    self.heap_str(self.cols.name_off, self.cols.name_len, row)
  }

  /// Just the node's kind — one u8 column read. Whole-graph scans gate on this before
  /// touching any heap string.
  pub fn node_kind(&self, id: NodeId) -> Option<SymbolKind> {
    let (_segment, row) = self.directory.locate(id)?;
    Some(SymbolKind::from_tag(
      self.nodes.column_at(self.cols.kind)?.get_u8(row)?,
    ))
  }

  /// The raw kind-tag column as one contiguous stripe (row index == dense `NodeId`) — the
  /// whole-graph scan fast path: no per-row directory lookup, no bounds ceremony. `None`
  /// when the segment stores kinds some other way; callers fall back to [`Kg::node_kind`].
  pub fn kind_tags(&self) -> Option<&[u8]> {
    self.nodes.column_at(self.cols.kind)?.as_slice::<u8>()
  }

  /// The raw content-hash column as one stripe (row index == dense `NodeId`) — per-file
  /// digesting and cross-generation alignment without per-row lookups.
  pub fn content_hashes(&self) -> Option<&[u64]> {
    self.nodes.column_at(self.cols.content_hash)?.as_slice::<u64>()
  }

  /// Just the node's defining path, zero-copy — scan passes that need file identity without
  /// the full three-string view.
  pub fn node_path(&self, id: NodeId) -> Option<&str> {
    let (_segment, row) = self.directory.locate(id)?;
    self.heap_str(self.cols.path_off, self.cols.path_len, row)
  }

  /// The node's durable identity pair `(external_id, content_hash)` — no heap-string reads.
  /// Cross-generation alignment (diffs) walks entire runs with this; `None` eid on pre-eid
  /// segments.
  pub fn node_identity(&self, id: NodeId) -> Option<(Option<u128>, u64)> {
    let (_segment, row) = self.directory.locate(id)?;
    let content_hash = self.nodes.column_at(self.cols.content_hash)?.get_u64(row)?;
    let external_id = match (self.cols.eid_lo, self.cols.eid_hi) {
      (Some(lo_col), Some(hi_col)) => {
        let lo = self.nodes.column_at(lo_col)?.get_u64(row)? as u128;
        let hi = self.nodes.column_at(hi_col)?.get_u64(row)? as u128;
        Some((hi << 64) | lo)
      }
      _ => None,
    };
    Some((external_id, content_hash))
  }

  pub fn node(&self, id: NodeId) -> Option<NodeView<'_>> {
    let (_segment, row) = self.directory.locate(id)?;
    let kind = SymbolKind::from_tag(self.nodes.column_at(self.cols.kind)?.get_u8(row)?);
    let content_hash = self.nodes.column_at(self.cols.content_hash)?.get_u64(row)?;
    let exported = self.nodes.column_at(self.cols.flags)?.get_u8(row)? & 1 != 0;
    let span = match (self.cols.span_start, self.cols.span_end) {
      (Some(start_col), Some(end_col)) => (
        self.nodes.column_at(start_col)?.get_u32(row)?,
        self.nodes.column_at(end_col)?.get_u32(row)?,
      ),
      _ => (0, 0),
    };
    let external_id = match (self.cols.eid_lo, self.cols.eid_hi) {
      (Some(lo_col), Some(hi_col)) => {
        let lo = self.nodes.column_at(lo_col)?.get_u64(row)? as u128;
        let hi = self.nodes.column_at(hi_col)?.get_u64(row)? as u128;
        Some((hi << 64) | lo)
      }
      _ => None,
    };
    Some(NodeView {
      kind,
      name: self.heap_str(self.cols.name_off, self.cols.name_len, row)?,
      path: self.heap_str(self.cols.path_off, self.cols.path_len, row)?,
      signature: self.heap_str(self.cols.sig_off, self.cols.sig_len, row)?,
      content_hash,
      external_id,
      exported,
      span,
    })
  }

  /// Nodes whose durable external id equals `eid` — how a client re-resolves a bookmarked
  /// symbol in a later generation. Normally 0 or 1 hits (the id is `blake3(path, entity)`);
  /// a linear column scan today, indexed if measurement ever warrants it. Empty on pre-eid
  /// segments.
  pub fn nodes_with_external_id(&self, eid: u128) -> Vec<NodeId> {
    let (Some(lo_col), Some(hi_col)) = (self.cols.eid_lo, self.cols.eid_hi) else {
      return Vec::new();
    };
    let (want_lo, want_hi) = (eid as u64, (eid >> 64) as u64);
    let mut hits = Vec::new();
    for row in 0..self.node_count() as u64 {
      let matches = self
        .nodes
        .column_at(lo_col)
        .and_then(|c| c.get_u64(row))
        .is_some_and(|lo| lo == want_lo)
        && self
          .nodes
          .column_at(hi_col)
          .and_then(|c| c.get_u64(row))
          .is_some_and(|hi| hi == want_hi);
      if matches {
        hits.push(NodeId::new(row));
      }
    }
    hits
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

  /// [`Kg::reachable_via`] with **paths and a grade floor**: each reached node carries its
  /// BFS-tree parent edge (chain to reconstruct one shortest compliant path) and traversal
  /// follows only edges at `min_confidence` or better — a positive floor excludes structural
  /// containment (confidence 0) and every resolution edge below the grade.
  pub fn reachable_via_paths(
    &self,
    id: NodeId,
    dir: Direction,
    edge_types: &[EdgeType],
    max_depth: Option<u32>,
    min_confidence: u8,
  ) -> Vec<vorpal_graph::ReachStep> {
    self.reachable_via_paths_multi(&[id], dir, edge_types, max_depth, min_confidence)
  }

  /// Multi-seed form: ONE BFS over the whole seed set, so each reached node's depth is its
  /// minimum hop distance from ANY seed — the impact-analysis semantics (seeds excluded).
  pub fn reachable_via_paths_multi(
    &self,
    seeds: &[NodeId],
    dir: Direction,
    edge_types: &[EdgeType],
    max_depth: Option<u32>,
    min_confidence: u8,
  ) -> Vec<vorpal_graph::ReachStep> {
    let raw: Vec<u32> = seeds.iter().map(|id| id.raw() as u32).collect();
    vorpal_graph::reachable_typed_paths(
      &self.graph,
      &raw,
      dir,
      edge_types,
      max_depth,
      min_confidence,
    )
  }

  /// Nodes reachable from `id` **restricted to the given edge types**, up to `max_depth` hops
  /// (`None` = unbounded), following out-edges (what `id` reaches) or in-edges (what reaches
  /// `id`). This is the relation-specific traversal: e.g. transitive callers are
  /// `reachable_via(id, In, &[EdgeType::CALLS], depth)` and can never cross a containment or
  /// import edge, unlike the unfiltered [`Kg::reachable_out`]/[`Kg::reachable_in`].
  pub fn reachable_via(
    &self,
    id: NodeId,
    dir: Direction,
    edge_types: &[EdgeType],
    max_depth: Option<u32>,
  ) -> Vec<NodeId> {
    vorpal_graph::reachable_typed(&self.graph, &[id.raw() as u32], dir, edge_types, max_depth)
      .iter()
      .map(|u| NodeId::new(u as u64))
      .collect()
  }

  /// All nodes whose display name equals `name`, ascending by id. Served by the persisted
  /// `names.idx` when the index dir carries one (two binary searches + per-hit string
  /// verification against hash collisions); the parallel scan fallback returns the identical
  /// list for older dirs.
  /// The node-segment identity stamp additive sidecars are keyed by (ANN tier,
  /// `communities.bin`, `observed.bin`): any regeneration changes it, so stale sidecars
  /// read as absent instead of answering with renumbered ids.
  pub fn node_segment_stamp(&self) -> u64 {
    xxhash_rust::xxh3::xxh3_64(self.node_segment_bytes())
  }

  pub fn nodes_named(&self, name: &str) -> Vec<NodeId> {
    if let Some((hashes, ids)) = &self.names {
      let hash = xxhash_rust::xxh3::xxh3_64(name.as_bytes());
      let lo = hashes.partition_point(|&h| h < hash);
      let hi = hashes.partition_point(|&h| h <= hash);
      // Pairs were sorted by (hash, id): ids within one hash's run are ascending.
      return ids[lo..hi]
        .iter()
        .map(|&i| NodeId::new(i))
        .filter(|&id| self.node(id).is_some_and(|view| view.name == name))
        .collect();
    }
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

  /// Nodes matching `selector`, ascending by id. `id` short-circuits (validated in range);
  /// `name` uses the persisted name index; `path_suffix` and `kind` filter either way. A
  /// selector with no fields matches nothing (never "everything" by accident).
  pub fn select(&self, selector: &SymbolSelector<'_>) -> Vec<NodeId> {
    if let Some(id) = selector.id {
      return match self.node(NodeId::new(id)) {
        Some(view)
          if selector.name.is_none_or(|n| view.name == n)
            && selector.kind.is_none_or(|k| view.kind == k)
            && selector.path_suffix.is_none_or(|p| view.path.ends_with(p))
            && selector.external_id.is_none_or(|e| view.external_id == Some(e)) =>
        {
          vec![NodeId::new(id)]
        }
        _ => Vec::new(),
      };
    }
    // The durable bookmark form: resolve by external id, refined by any other facets.
    if let Some(eid) = selector.external_id {
      return self
        .nodes_with_external_id(eid)
        .into_iter()
        .filter(|&id| {
          self.node(id).is_some_and(|view| {
            selector.name.is_none_or(|n| view.name == n)
              && selector.kind.is_none_or(|k| view.kind == k)
              && selector.path_suffix.is_none_or(|p| view.path.ends_with(p))
          })
        })
        .collect();
    }
    let Some(name) = selector.name else {
      return Vec::new();
    };
    self
      .nodes_named(name)
      .into_iter()
      .filter(|&id| {
        self.node(id).is_some_and(|view| {
          selector.kind.is_none_or(|k| view.kind == k)
            && selector.path_suffix.is_none_or(|p| view.path.ends_with(p))
        })
      })
      .collect()
  }

  /// In-neighbors of `id` over one edge type, ascending, deduplicated — the id-precise form
  /// of the `callers`/`references`/… verbs (name-based forms union this over namesakes).
  /// [`Kg::incoming_of`] carrying each in-edge's packed resolution confidence — what query
  /// surfaces render as evidence labels (IMPROVEMENTS §5: confidence queryable per edge).
  pub fn incoming_with_confidence(&self, id: NodeId, edge: EdgeType) -> Vec<(NodeId, u8)> {
    let mut found: Vec<(NodeId, u8)> = self
      .in_neighbors(id)
      .into_iter()
      .filter(|&(_, kind)| kind.base() == edge.base())
      .map(|(from, kind)| (from, kind.confidence()))
      .collect();
    found.sort_unstable_by_key(|&(n, _)| n.raw());
    found.dedup();
    found
  }

  pub fn incoming_of(&self, id: NodeId, edge: EdgeType) -> Vec<NodeId> {
    let mut found: Vec<NodeId> = self
      .in_neighbors(id)
      .into_iter()
      .filter(|&(_, kind)| kind.base() == edge.base())
      .map(|(from, _)| from)
      .collect();
    found.sort_unstable_by_key(|n| n.raw());
    found.dedup();
    found
  }

  fn incoming_named(&self, name: &str, edge: EdgeType) -> Vec<NodeId> {
    let mut found = Vec::new();
    for target in self.nodes_named(name) {
      for (from, kind) in self.in_neighbors(target) {
        if kind.base() == edge.base() && !found.contains(&from) {
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
    crate::phase_stamp("kg save: start");
    fs::create_dir_all(dir)?;
    // Every artifact lands via tmp + rename: a rebuild must never truncate a file a live
    // reader — this process's daemon, or another process — still has mapped (truncating a
    // mapped file makes later reads fault). Rename swaps the directory entry; the old inode
    // survives until its last map goes away.
    //
    // The four artifacts are independent files; writing them serially left the save's wall
    // time as their SUM (the largest single chunk of the post-stream tail). A scope writes
    // them concurrently — wall becomes the max — and each write is still tmp+rename atomic.
    let (nodes_result, names_result, graph_result) = std::thread::scope(|scope| {
      let nodes_task = scope.spawn(|| {
        write_via_tmp(dir, "nodes.vseg", |out| out.write_all(self.nodes.bytes()))?;
        let heap_final = dir.join("strings.heap");
        match &self.heap_file {
          // Streamed commit: the bytes are already in the tmp file — publish it.
          Some(existing) if *existing == dir.join("strings.heap.tmp") => {
            replace_file(existing, &heap_final)
          }
          // Loaded from this very directory: identical bytes are already in place.
          Some(existing) if *existing == heap_final => Ok(()),
          _ => write_via_tmp(dir, "strings.heap", |out| out.write_all(&self.heap[..])),
        }
      });
      let names_task = scope.spawn(|| self.write_names_index(dir));
      // Both CSR directions persist as one aligned section file the load path maps
      // zero-copy — the edge-list form forced every open to re-run compaction (~64 ms at
      // kernel scale).
      let graph_result = write_via_tmp(dir, "graph.bin", |out| self.graph.write_to(out));
      (
        nodes_task.join().expect("nodes/heap saver panicked"),
        names_task.join().expect("names saver panicked"),
        graph_result,
      )
    });
    nodes_result?;
    names_result?;
    graph_result?;
    crate::phase_stamp("kg save: done");
    Ok(())
  }

  /// Persist the name index sidecar (`names.idx`): `(xxh3(name), id)` pairs sorted by
  /// `(hash, id)` — exact-name lookup becomes two binary searches over a mapped column
  /// instead of a full node scan. Bytes are a pure function of the node table (sorted,
  /// fixed-width): bit-identical across rebuilds. Also used to backfill dirs written before
  /// the sidecar existed.
  /// Install a prebuilt in-memory name index (sorted `(xxh3(name), id)` pairs split into
  /// columns) — the canonical seal computes it in parallel with the segment build instead
  /// of paying a post-assembly scan.
  pub(crate) fn set_names_index(&mut self, hashes: Vec<u64>, ids: Vec<u64>) {
    debug_assert_eq!(hashes.len(), ids.len());
    self.names = Some((
      vorpal_mem::PodColumn::from_vec(hashes),
      vorpal_mem::PodColumn::from_vec(ids),
    ));
  }

  /// Build the name→id index **in memory** — for a daemon serving a freshly sealed graph
  /// that never touched disk. Same pairs, same `(hash, id)` order as the persisted
  /// `names.idx`, so lookups behave identically to a loaded generation; without it every
  /// name lookup on a live graph pays the full parallel scan (~20ms at kernel scale, on
  /// EVERY named query).
  pub fn build_names_index(&mut self) {
    use rayon::prelude::*;
    crate::phase_stamp("names index: start");
    let mut pairs: Vec<(u64, u64)> = (0..self.node_count() as u64)
      .into_par_iter()
      .filter_map(|i| {
        self
          .node_name(NodeId::new(i))
          .map(|name| (xxhash_rust::xxh3::xxh3_64(name.as_bytes()), i))
      })
      .collect();
    pairs.par_sort_unstable();
    let hashes: Vec<u64> = pairs.iter().map(|&(h, _)| h).collect();
    let ids: Vec<u64> = pairs.iter().map(|&(_, i)| i).collect();
    self.names = Some((
      vorpal_mem::PodColumn::from_vec(hashes),
      vorpal_mem::PodColumn::from_vec(ids),
    ));
    crate::phase_stamp("names index: done");
  }

  pub fn write_names_index(&self, dir: &Path) -> io::Result<()> {
    use std::io::Write;
    write_via_tmp(dir, "names.idx", |out| {
      use rayon::prelude::*;
      // Name-only extraction in parallel: `node_name` reads one heap string, where the full
      // `NodeView` materialized three per row — and the scan itself fans out (rayon's ordered
      // collect keeps row order, so the sorted result is unchanged).
      let mut pairs: Vec<(u64, u64)> = (0..self.node_count() as u64)
        .into_par_iter()
        .filter_map(|i| {
          self
            .node_name(NodeId::new(i))
            .map(|name| (xxhash_rust::xxh3::xxh3_64(name.as_bytes()), i))
        })
        .collect();
      pairs.par_sort_unstable();
      out.write_all(&NAMES_MAGIC.to_le_bytes())?;
      out.write_all(&(pairs.len() as u32).to_le_bytes())?;
      // The on-disk format is little-endian. Split the sorted pairs into the two columns and
      // write each as one bulk slice on LE hosts (byte-identical to the per-element
      // `to_le_bytes` loop it replaces — that loop did 2×n eight-byte `write_all`s); the
      // per-element form remains the portable path for big-endian targets.
      let hashes: Vec<u64> = pairs.iter().map(|&(h, _)| h).collect();
      let ids: Vec<u64> = pairs.iter().map(|&(_, i)| i).collect();
      if cfg!(target_endian = "little") {
        out.write_all(bytemuck::cast_slice(&hashes))?;
        out.write_all(bytemuck::cast_slice(&ids))?;
      } else {
        for &hash in &hashes {
          out.write_all(&hash.to_le_bytes())?;
        }
        for &id in &ids {
          out.write_all(&id.to_le_bytes())?;
        }
      }
      Ok(())
    })
  }

  /// Cold-open a persisted graph: **mmap** the node segment (§9.1 — no heap load of the columns),
  /// read the string heap, and rebuild the CSR/CSC from the edge list.
  pub fn load(dir: &Path) -> Result<Self, SegmentError> {
    let dir = &resolve_index_dir(dir);
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
    let mut kg = Self::with_heap_column(
      nodes,
      heap,
      Some(dir.join("strings.heap")),
      graph,
      directory,
    )?;
    // Cross-segment coherence gate: the graph and the node segment must describe the same node
    // universe. A mismatch means the mapped files come from different index generations — a
    // reader that opened while a rebuild was mid-rename. Refuse to serve rather than return
    // out-of-bounds neighbors or cross-generation nodes; the caller treats it as "no index" and
    // rebuilds. (The name index already self-validates its count and falls back to a scan.)
    if kg.graph.node_count() != kg.node_count() {
      return Err(SegmentError::Corrupt(
        "graph and node segment describe different node universes (mixed index generation)",
      ));
    }
    kg.names = open_names_index(dir, &policy, kg.node_count());
    kg.evidence = crate::evidence::EvidenceStore::open(dir);
    Ok(kg)
  }

  /// The node count of a persisted index, from the segment header alone — no string heap read,
  /// no edge-list read, no CSR rebuild. This is all the whole-tree-unchanged fast path needs,
  /// so a no-change re-index does not pay a graph load to report a number.
  pub fn peek_node_count(dir: &Path) -> Result<usize, SegmentError> {
    let dir = &resolve_index_dir(dir);
    let nodes_path = dir.join("nodes.vseg");
    let size = fs::metadata(&nodes_path)?.len();
    let policy = ResourcePolicy::probe(CorpusProbe::new(size, 1));
    Ok(Segment::open_file(&nodes_path, &policy)?.row_count() as usize)
  }
}

fn is_containment(e: EdgeType) -> bool {
  matches!(
    e.base(),
    EdgeType::DEFINES | EdgeType::HAS_METHOD | EdgeType::HAS_FIELD
  )
}
