//! The sealed, queryable knowledge graph (§3.3, §3.5, §11).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use vorpal_graph::{Direction, EdgeType, Graph, reachable};
use vorpal_mem::{AccessPattern, CorpusProbe, Hotness, MappedStore, ResourcePolicy, StoreKind};
use vorpal_segment::{NodeId, Segment, SegmentBuilder, SegmentDirectory, SegmentError};

/// The bucketed node store's directory and TOC (P4.2), relative to the generation dir.
pub const NODES_DIR: &str = "nodes";
pub const NODES_TOC: &str = "nodes/toc.bin";
const NODES_TOC_FILE: &str = "toc.bin";
const NODES_TOC_MAGIC: &[u8; 4] = b"VNTC";
/// Version counter for the bucketed node-store TOC (`VNTC`). v2 (P4.3) appends the FILE
/// TABLE — per file `{file_key u64, dense row_start u64, rows u32}` in dense order — the
/// one map every `(file_key, ordinal)`-coded family (evidence, edges) densifies through.
const NODES_VERSION: u32 = 2;
/// TOC header: magic + version + bucket count u32 + total rows u64.
const NODES_TOC_HEADER: usize = 20;
/// One per-bucket TOC row: rows u32 + vseg len/digest u64 + heap len/digest u64.
const NODES_TOC_ROW: usize = 36;
/// One file-table row: file_key u64 + dense row_start u64 + rows u32.
const NODES_FILE_ROW: usize = 20;

/// Which node-store layout [`Kg::save_with`] publishes. Readers sniff; only writers choose.
#[derive(Debug, Clone)]
pub enum SegmentLayout {
  Flat,
  Bucketed {
    /// Canonical tree root — bucket membership hashes tree-relative spellings.
    tree_root: String,
    /// Prior generation dir, when one exists: unchanged buckets hard-link from it.
    prior: Option<PathBuf>,
    /// The generation's LIVE FILE COUNT (manifest entries) — the bucket law's one input,
    /// pinned by the caller so every artifact family buckets identically. Deriving it
    /// from File nodes would drift under parse-health exclusion (excluded files keep
    /// manifest entries and products but grow no nodes).
    live_files: usize,
  },
}

/// Whether `name` (a generation-relative artifact name) belongs to the bucketed node
/// store: the TOC or a `nodes/<k>.vseg` / `nodes/<k>.heap` slab.
pub fn is_nodes_member(name: &str) -> bool {
  if name == NODES_TOC {
    return true;
  }
  name
    .strip_prefix("nodes/")
    .and_then(|f| f.strip_suffix(".vseg").or_else(|| f.strip_suffix(".heap")))
    .is_some_and(|k| !k.is_empty() && k.len() <= 5 && k.bytes().all(|b| b.is_ascii_digit()))
}

/// Test fixture: a minimal `nodes/toc.bin` whose prefix sums equal `bases` — lets the
/// evidence unit tests exercise the bucketed store without building a full generation.
#[cfg(test)]
pub(crate) fn write_node_bases_fixture(dir: &Path, bases: &[u64]) -> io::Result<()> {
  use std::io::Write as _;
  fs::create_dir_all(dir.join(NODES_DIR))?;
  let mut out = fs::File::create(dir.join(NODES_TOC))?;
  out.write_all(NODES_TOC_MAGIC)?;
  out.write_all(&NODES_VERSION.to_le_bytes())?;
  out.write_all(&((bases.len() - 1) as u32).to_le_bytes())?;
  out.write_all(&bases[bases.len() - 1].to_le_bytes())?;
  for pair in bases.windows(2) {
    out.write_all(&((pair[1] - pair[0]) as u32).to_le_bytes())?;
    out.write_all(&[0u8; 32])?; // vseg/heap lens + digests: unread by the bases derivation
  }
  // Synthetic file table: one file per non-empty bucket, key = 0x1000 + bucket index.
  let files: Vec<(u64, u64, u32)> = bases
    .windows(2)
    .enumerate()
    .filter(|(_, pair)| pair[1] > pair[0])
    .map(|(k, pair)| (0x1000 + k as u64, pair[0], (pair[1] - pair[0]) as u32))
    .collect();
  out.write_all(&(files.len() as u64).to_le_bytes())?;
  for (key, start, rows) in files {
    out.write_all(&key.to_le_bytes())?;
    out.write_all(&start.to_le_bytes())?;
    out.write_all(&rows.to_le_bytes())?;
  }
  Ok(())
}

/// Dense-id point lookup over contiguous per-slab column stripes: binary search on the
/// stripe bases (one stripe — the flat/sealed case — resolves in a single probe).
pub struct Striped<'a, T> {
  stripes: Vec<(u64, &'a [T])>,
}

impl<'a, T: Copy> Striped<'a, T> {
  fn new(stripes: Vec<(u64, &'a [T])>) -> Self {
    Self { stripes }
  }

  pub fn get(&self, row: u64) -> Option<T> {
    let at = self.stripes.partition_point(|&(base, _)| base <= row);
    let (base, stripe) = self.stripes.get(at.checked_sub(1)?)?;
    stripe.get((row - base) as usize).copied()
  }
}

/// Per-bucket row/heap boundaries of a sealed graph (row_starts has buckets+1 entries),
/// plus the per-file identity table (`(file_key, dense row_start, rows)` in dense order).
struct BucketBounds {
  buckets: u32,
  row_starts: Vec<u64>,
  files: Vec<(u64, u64, u32)>,
}

/// One bucket's freshly built slab, ready to land or be carried.
struct BuiltBucket {
  rows: u32,
  vseg: Vec<u8>,
  vseg_digest: u64,
  heap_range: (usize, usize),
  heap_len: usize,
  heap_digest: u64,
}

struct NodesTocRow {
  rows: u32,
  vseg_len: u64,
  vseg_digest: u64,
  heap_len: u64,
  heap_digest: u64,
}

struct NodesToc {
  total: u64,
  rows: Vec<NodesTocRow>,
  /// Per file: `(file_key, dense row_start, rows)` in dense (row_start) order.
  files: Vec<(u64, u64, u32)>,
}

impl NodesToc {
  /// Parse a node-store TOC. `None` on any structural inconsistency — the caller decides
  /// whether that is fatal (a load) or merely disables the carry (a save).
  fn load(path: &Path) -> Option<NodesToc> {
    let bytes = fs::read(path).ok()?;
    if bytes.len() < NODES_TOC_HEADER || &bytes[0..4] != NODES_TOC_MAGIC {
      return None;
    }
    if u32::from_le_bytes(bytes[4..8].try_into().ok()?) != NODES_VERSION {
      return None;
    }
    let buckets = u32::from_le_bytes(bytes[8..12].try_into().ok()?) as usize;
    if buckets == 0 || buckets > crate::identity::BUCKET_MAX as usize {
      return None;
    }
    let total = u64::from_le_bytes(bytes[12..20].try_into().ok()?);
    if bytes.len() < NODES_TOC_HEADER + buckets * NODES_TOC_ROW {
      return None;
    }
    let mut rows = Vec::with_capacity(buckets);
    for k in 0..buckets {
      let at = NODES_TOC_HEADER + k * NODES_TOC_ROW;
      let row = &bytes[at..at + NODES_TOC_ROW];
      rows.push(NodesTocRow {
        rows: u32::from_le_bytes(row[0..4].try_into().ok()?),
        vseg_len: u64::from_le_bytes(row[4..12].try_into().ok()?),
        vseg_digest: u64::from_le_bytes(row[12..20].try_into().ok()?),
        heap_len: u64::from_le_bytes(row[20..28].try_into().ok()?),
        heap_digest: u64::from_le_bytes(row[28..36].try_into().ok()?),
      });
    }
    if rows.iter().map(|r| u64::from(r.rows)).sum::<u64>() != total {
      return None;
    }
    let files_at = NODES_TOC_HEADER + buckets * NODES_TOC_ROW;
    let file_count =
      u64::from_le_bytes(bytes.get(files_at..files_at + 8)?.try_into().ok()?) as usize;
    let table_at = files_at + 8;
    if bytes.len() < table_at + file_count * NODES_FILE_ROW {
      return None;
    }
    let mut files = Vec::with_capacity(file_count);
    let mut prev_start = 0u64;
    for i in 0..file_count {
      let at = table_at + i * NODES_FILE_ROW;
      let row = &bytes[at..at + NODES_FILE_ROW];
      let key = u64::from_le_bytes(row[0..8].try_into().ok()?);
      let start = u64::from_le_bytes(row[8..16].try_into().ok()?);
      let rows_in_file = u32::from_le_bytes(row[16..20].try_into().ok()?);
      if start < prev_start || start + u64::from(rows_in_file) > total {
        return None; // not dense order or beyond the universe
      }
      prev_start = start;
      files.push((key, start, rows_in_file));
    }
    Some(NodesToc { total, rows, files })
  }
}

/// The dense-id ⇄ `(file_key, ordinal)` map of one bucketed generation — built from the
/// node-store TOC's file table, consumed by every family that stores durable coordinates
/// (evidence, edges). Also carries the per-bucket dense bases.
pub struct NodeIdMap {
  /// Per-bucket dense bases (`buckets + 1` prefix sums).
  bases: Vec<u64>,
  /// `(file_key, dense row_start, rows)` in dense order — locate by row_start.
  by_start: Vec<(u64, u64, u32)>,
  /// The same rows sorted by file_key — densify by key. Keys are collision-gated at build.
  by_key: Vec<(u64, u64, u32)>,
  /// Dense per-id `(file_key, ordinal)` table, built on first bulk use (the WRITE paths
  /// convert tens of millions of endpoints — a binary search each measured ~0.6 s per
  /// kernel edit; readers doing per-query point lookups never pay the table).
  dense: std::sync::OnceLock<Vec<(u64, u32)>>,
}

impl NodeIdMap {
  /// Load from a generation directory's node-store TOC.
  pub fn from_dir(dir: &Path) -> Option<NodeIdMap> {
    let toc = NodesToc::load(&dir.join(NODES_TOC))?;
    let mut bases = Vec::with_capacity(toc.rows.len() + 1);
    let mut base = 0u64;
    for row in &toc.rows {
      bases.push(base);
      base += u64::from(row.rows);
    }
    bases.push(base);
    Some(Self::from_parts(bases, toc.files))
  }

  pub(crate) fn from_parts(bases: Vec<u64>, by_start: Vec<(u64, u64, u32)>) -> NodeIdMap {
    let mut by_key = by_start.clone();
    by_key.sort_unstable_by_key(|&(key, _, _)| key);
    NodeIdMap {
      bases,
      by_start,
      by_key,
      dense: std::sync::OnceLock::new(),
    }
  }

  /// The dense table for bulk conversion (write paths): one O(nodes) fill, then O(1) per
  /// endpoint.
  fn dense_table(&self) -> &[(u64, u32)] {
    self.dense.get_or_init(|| {
      let total = self.bases.last().copied().unwrap_or(0) as usize;
      let mut table = vec![(u64::MAX, u32::MAX); total];
      for &(key, start, rows) in &self.by_start {
        for ordinal in 0..rows {
          table[(start + u64::from(ordinal)) as usize] = (key, ordinal);
        }
      }
      table
    })
  }

  /// [`NodeIdMap::locate`] through the dense table — the bulk-conversion form.
  pub fn locate_bulk(&self, id: u32) -> Option<(u64, u32)> {
    let entry = *self.dense_table().get(id as usize)?;
    (entry.0 != u64::MAX).then_some(entry)
  }

  pub fn bases(&self) -> &[u64] {
    &self.bases
  }

  /// The per-file identity rows `(file_key, dense row_start, rows)` in dense order.
  pub fn files(&self) -> &[(u64, u64, u32)] {
    &self.by_start
  }

  /// Dense id → `(file_key, ordinal)`. `None` outside the universe.
  pub fn locate(&self, id: u32) -> Option<(u64, u32)> {
    let raw = u64::from(id);
    let at = self
      .by_start
      .partition_point(|&(_, start, _)| start <= raw)
      .checked_sub(1)?;
    let (key, start, rows) = self.by_start[at];
    (raw < start + u64::from(rows)).then(|| (key, (raw - start) as u32))
  }

  /// `(file_key, ordinal)` → dense id. `None` for unknown keys or out-of-file ordinals.
  pub fn densify(&self, key: u64, ordinal: u32) -> Option<u32> {
    let at = self.by_key.partition_point(|&(k, _, _)| k < key);
    let &(k, start, rows) = self.by_key.get(at)?;
    (k == key && ordinal < rows).then(|| (start + u64::from(ordinal)) as u32)
  }
}

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
  /// Node slabs covering the dense id space in order — ONE for a sealed in-RAM graph or a
  /// flat (v1) generation, one per bucket for a bucketed (P4.2) generation. `cols` and
  /// `heaps` are parallel: slab k's heap-offset columns are LOCAL to `heaps[k]`, so a
  /// bucketed load rebases nothing.
  segments: Vec<Segment>,
  cols: Vec<NodeColumns>,
  heaps: Vec<vorpal_mem::PodColumn<u8>>,
  /// Cached total rows across slabs (`node_count` stays O(1)).
  total_rows: u64,
  /// Persisted name index (`names.idx`): `(xxh3(name), id)` pairs sorted by `(hash, id)`,
  /// mapped zero-copy. `None` for index dirs written before the sidecar existed — lookups
  /// fall back to the parallel scan.
  names: Option<(vorpal_mem::PodColumn<u64>, vorpal_mem::PodColumn<u64>)>,
  /// Where the (flat) heap bytes already live on disk, when they do (streamed commit or
  /// flat load) — lets `save` rename or skip instead of rewriting a file readers may have
  /// mapped. Always `None` for bucketed loads.
  heap_file: Option<std::path::PathBuf>,
  /// The generation directory this graph was loaded from (`None` for in-RAM seals) — the
  /// anchor for lazy sidecars (communities) under either layout.
  home_dir: Option<std::path::PathBuf>,
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
    Self::with_slabs(vec![nodes], vec![heap], heap_file, None, graph, directory)
  }

  /// The general constructor: parallel node/heap slabs covering the dense id space in
  /// directory order. Single-slab callers are the sealed/flat paths; the bucketed load
  /// hands one slab per bucket.
  pub(crate) fn with_slabs(
    segments: Vec<Segment>,
    heaps: Vec<vorpal_mem::PodColumn<u8>>,
    heap_file: Option<std::path::PathBuf>,
    home_dir: Option<std::path::PathBuf>,
    graph: Graph,
    directory: SegmentDirectory,
  ) -> Result<Self, SegmentError> {
    if segments.len() != heaps.len() {
      return Err(SegmentError::Corrupt("node slabs and heap slabs disagree"));
    }
    let cols = segments
      .iter()
      .map(NodeColumns::resolve)
      .collect::<Option<Vec<_>>>()
      .ok_or(SegmentError::Corrupt(
        "node segment missing a required column",
      ))?;
    let total_rows = segments.iter().map(Segment::row_count).sum();
    Ok(Self {
      segments,
      cols,
      heaps,
      total_rows,
      heap_file,
      home_dir,
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
      let dir = self
        .home_dir
        .as_deref()
        .or_else(|| self.heap_file.as_ref()?.parent())?;
      crate::communities::load(dir, self.node_segment_stamp(), self.node_count())
    });
    table.as_ref()?.get(id.raw() as usize).copied()
  }

  /// The whole community table, when built (for summaries that walk every node once).
  pub fn communities(&self) -> Option<&[u32]> {
    self.community(NodeId::new(0));
    self.communities.get().and_then(|t| t.as_deref())
  }

  pub fn node_count(&self) -> usize {
    self.total_rows as usize
  }

  /// This node's calls-cycle component size: 1 outside any recursion, the knot's node
  /// count inside one. `None` on segments sealed before the column existed — absence is
  /// "unknown", never "acyclic".
  pub fn scc_size(&self, id: NodeId) -> Option<u32> {
    let (seg, row) = self.directory.locate(id)?;
    let seg = seg as usize;
    self.segments[seg]
      .column_at(self.cols[seg].scc_size?)?
      .get_u32(row)
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
    for (segment, cols) in self.segments.iter().zip(&self.cols) {
      let Some(column) = segment.column_at(cols.kind) else {
        continue;
      };
      if let Some(tags) = column.as_slice::<u8>() {
        for &tag in tags {
          buckets[tag as usize] += 1;
        }
      } else {
        for row in 0..segment.row_count() {
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

  /// The sealed node slabs' raw bytes, in dense-id (slab) order — what the freshness stamp
  /// folds. Hashing the *loaded* mappings (instead of re-reading files) pins the stamp to
  /// the generation actually being served — no load/hash race with a concurrent rebuild.
  pub fn node_segment_slabs(&self) -> impl Iterator<Item = &[u8]> {
    self.segments.iter().map(Segment::bytes)
  }

  pub fn is_empty(&self) -> bool {
    self.node_count() == 0
  }

  /// Resolve a node's attributes (§3.3). Reads HOT columns (`base + row·stride`) + the heap.
  /// Just the node's name, zero-copy — the whole-graph name-scan primitive (pattern
  /// matching, dedup passes) where materializing the full [`NodeView`] would read three
  /// heap strings per row to use one.
  pub fn node_name(&self, id: NodeId) -> Option<&str> {
    let (seg, row) = self.directory.locate(id)?;
    let seg = seg as usize;
    self.heap_str(seg, self.cols[seg].name_off, self.cols[seg].name_len, row)
  }

  /// Just the node's kind — one u8 column read. Whole-graph scans gate on this before
  /// touching any heap string.
  pub fn node_kind(&self, id: NodeId) -> Option<SymbolKind> {
    let (seg, row) = self.directory.locate(id)?;
    let seg = seg as usize;
    Some(SymbolKind::from_tag(
      self.segments[seg].column_at(self.cols[seg].kind)?.get_u8(row)?,
    ))
  }

  /// The raw kind-tag column as contiguous per-slab stripes `(dense id base, tags)`, in
  /// id order — the whole-graph scan fast path: no per-row directory lookup inside a
  /// stripe. One stripe for flat/sealed graphs, one per bucket for bucketed generations.
  /// `None` when any slab stores kinds some other way; callers fall back to
  /// [`Kg::node_kind`].
  pub fn kind_tag_stripes(&self) -> Option<Vec<(u64, &[u8])>> {
    let mut out = Vec::with_capacity(self.segments.len());
    let mut base = 0u64;
    for (segment, cols) in self.segments.iter().zip(&self.cols) {
      out.push((base, segment.column_at(cols.kind)?.as_slice::<u8>()?));
      base += segment.row_count();
    }
    Some(out)
  }

  /// The raw content-hash column as contiguous per-slab stripes `(dense id base, hashes)`
  /// — per-file digesting and cross-generation alignment without per-row lookups.
  pub fn content_hash_stripes(&self) -> Option<Vec<(u64, &[u64])>> {
    let mut out = Vec::with_capacity(self.segments.len());
    let mut base = 0u64;
    for (segment, cols) in self.segments.iter().zip(&self.cols) {
      out.push((base, segment.column_at(cols.content_hash)?.as_slice::<u64>()?));
      base += segment.row_count();
    }
    Some(out)
  }

  /// [`Kg::kind_tag_stripes`] wrapped for dense-id point lookups (O(log slabs) — one probe
  /// for flat graphs). The surfaces that used to index one flat slice by id use this.
  pub fn kind_tag_lookup(&self) -> Option<Striped<'_, u8>> {
    self.kind_tag_stripes().map(Striped::new)
  }

  /// [`Kg::content_hash_stripes`] wrapped for dense-id point lookups.
  pub fn content_hash_lookup(&self) -> Option<Striped<'_, u64>> {
    self.content_hash_stripes().map(Striped::new)
  }

  /// Just the node's defining path, zero-copy — scan passes that need file identity without
  /// the full three-string view.
  pub fn node_path(&self, id: NodeId) -> Option<&str> {
    let (seg, row) = self.directory.locate(id)?;
    let seg = seg as usize;
    self.heap_str(seg, self.cols[seg].path_off, self.cols[seg].path_len, row)
  }

  /// The node's durable identity pair `(external_id, content_hash)` — no heap-string reads.
  /// Cross-generation alignment (diffs) walks entire runs with this; `None` eid on pre-eid
  /// segments.
  pub fn node_identity(&self, id: NodeId) -> Option<(Option<u128>, u64)> {
    let (seg, row) = self.directory.locate(id)?;
    let (segment, cols) = (&self.segments[seg as usize], &self.cols[seg as usize]);
    let content_hash = segment.column_at(cols.content_hash)?.get_u64(row)?;
    let external_id = match (cols.eid_lo, cols.eid_hi) {
      (Some(lo_col), Some(hi_col)) => {
        let lo = segment.column_at(lo_col)?.get_u64(row)? as u128;
        let hi = segment.column_at(hi_col)?.get_u64(row)? as u128;
        Some((hi << 64) | lo)
      }
      _ => None,
    };
    Some((external_id, content_hash))
  }

  pub fn node(&self, id: NodeId) -> Option<NodeView<'_>> {
    let (seg, row) = self.directory.locate(id)?;
    let seg = seg as usize;
    let (segment, cols) = (&self.segments[seg], &self.cols[seg]);
    let kind = SymbolKind::from_tag(segment.column_at(cols.kind)?.get_u8(row)?);
    let content_hash = segment.column_at(cols.content_hash)?.get_u64(row)?;
    let exported = segment.column_at(cols.flags)?.get_u8(row)? & 1 != 0;
    let span = match (cols.span_start, cols.span_end) {
      (Some(start_col), Some(end_col)) => (
        segment.column_at(start_col)?.get_u32(row)?,
        segment.column_at(end_col)?.get_u32(row)?,
      ),
      _ => (0, 0),
    };
    let external_id = match (cols.eid_lo, cols.eid_hi) {
      (Some(lo_col), Some(hi_col)) => {
        let lo = segment.column_at(lo_col)?.get_u64(row)? as u128;
        let hi = segment.column_at(hi_col)?.get_u64(row)? as u128;
        Some((hi << 64) | lo)
      }
      _ => None,
    };
    Some(NodeView {
      kind,
      name: self.heap_str(seg, cols.name_off, cols.name_len, row)?,
      path: self.heap_str(seg, cols.path_off, cols.path_len, row)?,
      signature: self.heap_str(seg, cols.sig_off, cols.sig_len, row)?,
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
    let (want_lo, want_hi) = (eid as u64, (eid >> 64) as u64);
    let mut hits = Vec::new();
    let mut base = 0u64;
    for (segment, cols) in self.segments.iter().zip(&self.cols) {
      let rows = segment.row_count();
      if let (Some(lo_col), Some(hi_col)) = (cols.eid_lo, cols.eid_hi) {
        for row in 0..rows {
          let matches = segment
            .column_at(lo_col)
            .and_then(|c| c.get_u64(row))
            .is_some_and(|lo| lo == want_lo)
            && segment
              .column_at(hi_col)
              .and_then(|c| c.get_u64(row))
              .is_some_and(|hi| hi == want_hi);
          if matches {
            hits.push(NodeId::new(base + row));
          }
        }
      }
      base += rows;
    }
    hits
  }

  fn heap_str(&self, seg: usize, off_col: usize, len_col: usize, row: u64) -> Option<&str> {
    let segment = &self.segments[seg];
    let off = segment.column_at(off_col)?.get_u32(row)? as usize;
    let len = segment.column_at(len_col)?.get_u32(row)? as usize;
    std::str::from_utf8(self.heaps[seg].get(off..off + len)?).ok()
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
    // Streaming fold over the slabs in dense-id order. For a single slab this is exactly
    // `xxh3_64(bytes)` — every stamp persisted by flat generations stays valid.
    let mut hasher = xxhash_rust::xxh3::Xxh3::new();
    for slab in self.node_segment_slabs() {
      hasher.update(slab);
    }
    hasher.digest()
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
    self.save_with(dir, &SegmentLayout::Flat)
  }

  /// Persist under an explicit node-store layout (P4.2). `Flat` is the v1 monolith
  /// (`nodes.vseg` + `strings.heap`, byte-identical to the historical writer);
  /// `Bucketed` writes `nodes/<k>.vseg` + `nodes/<k>.heap` + `nodes/toc.bin`, hard-linking
  /// every bucket whose bytes match the prior generation's TOC digests.
  pub fn save_with(&self, dir: &Path, layout: &SegmentLayout) -> io::Result<()> {
    crate::phase_stamp("kg save: start");
    fs::create_dir_all(dir)?;
    // Every artifact lands via tmp + rename: a rebuild must never truncate a file a live
    // reader — this process's daemon, or another process — still has mapped (truncating a
    // mapped file makes later reads fault). Rename swaps the directory entry; the old inode
    // survives until its last map goes away.
    //
    // The artifacts are independent; writing them serially left the save's wall time as
    // their SUM (the largest single chunk of the post-stream tail). A scope writes them
    // concurrently — wall becomes the max — and each write is still tmp+rename atomic.
    match layout {
      SegmentLayout::Flat => {
        let (nodes_result, names_result, graph_result) = std::thread::scope(|scope| {
          let nodes_task = scope.spawn(|| self.save_nodes_flat(dir));
          let names_task = scope.spawn(|| self.write_names_index(dir));
          // Both CSR directions persist as one aligned section file the load path maps
          // zero-copy — the edge-list form forced every open to re-run compaction (~64 ms
          // at kernel scale). Flat lane: graph.bin is the TRUTH and joins the identity.
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
      }
      SegmentLayout::Bucketed {
        tree_root,
        prior,
        live_files,
      } => {
        // Bucket boundaries once, shared by the node slabs and the edge slabs.
        let bounds = self.bucket_bounds(tree_root, *live_files)?;
        let id_map = NodeIdMap::from_parts(bounds.row_starts.clone(), bounds.files.clone());
        let (nodes_result, names_result, edges_result) = std::thread::scope(|scope| {
          let nodes_task =
            scope.spawn(|| self.save_nodes_bucketed(dir, &bounds, prior.as_deref()));
          // names.idx under the bucketed format is a WARM DERIVED artifact: written
          // eagerly for query UX, excluded from the generation's identity (its ids move
          // with every id shift, and queries fall back to the scan when it is absent).
          let names_task = scope.spawn(|| self.write_names_index(dir));
          // The graph's TRUTH is the per-source-bucket edge slabs (P4.3); the dense
          // CSR/CSC cache is written after the scope, once the TOC it is stamped
          // against exists.
          let edges_result =
            crate::edgestore::save(dir, &self.graph, &id_map, prior.as_deref());
          (
            nodes_task.join().expect("nodes/heap saver panicked"),
            names_task.join().expect("names saver panicked"),
            edges_result,
          )
        });
        let node_fold = nodes_result?;
        names_result?;
        edges_result?;
        // Derived CSR/CSC cache (`graph.bin` + `graph.stamp`): best-effort — never
        // load-bearing (the loader rebuilds from slabs on any mismatch), never identity.
        if let Some(stamp) = crate::edgestore::expected_stamp(dir, node_fold) {
          let _ = crate::edgestore::write_cache(dir, &self.graph, stamp);
        }
      }
    }
    crate::phase_stamp("kg save: done");
    Ok(())
  }

  /// The flat (v1) node store: one segment file + one heap file — byte-identical to the
  /// historical writer for single-slab graphs, which is every graph a build seals.
  fn save_nodes_flat(&self, dir: &Path) -> io::Result<()> {
    use std::io::Write;
    if self.segments.len() != 1 {
      // A bucketed-loaded graph re-persisting flat (format downgrade). No production path
      // does this today — a daemon re-seals before persisting — so the honest answer is a
      // loud error, not a silently synthesized monolith nothing certifies.
      return Err(io::Error::other(
        "flat save of a bucketed-loaded graph is unsupported; re-seal before persisting",
      ));
    }
    write_via_tmp(dir, "nodes.vseg", |out| out.write_all(self.segments[0].bytes()))?;
    let heap_final = dir.join("strings.heap");
    match &self.heap_file {
      // Streamed commit: the bytes are already in the tmp file — publish it.
      Some(existing) if *existing == dir.join("strings.heap.tmp") => {
        replace_file(existing, &heap_final)
      }
      // Loaded from this very directory: identical bytes are already in place.
      Some(existing) if *existing == heap_final => Ok(()),
      _ => write_via_tmp(dir, "strings.heap", |out| out.write_all(&self.heaps[0][..])),
    }?;
    // One truth per directory: a format downgrade must retire the bucketed layout, or the
    // sniff (bucketed first) would resurrect stale nodes.
    if dir.join(NODES_DIR).is_dir() {
      let _ = fs::remove_dir_all(dir.join(NODES_DIR));
    }
    Ok(())
  }

  /// The bucketed (P4.2) node store: per-bucket segment + heap slabs and a TOC, bucket
  /// membership by the shared identity law. Slab bytes are id-free and heap offsets are
  /// bucket-LOCAL, so an unchanged bucket's bytes are position-independent — the carry
  /// hard-links any bucket whose freshly built bytes match the prior TOC's digests
  /// (exactness by comparison: a cross-file `scc_size` change honestly rewrites the
  /// buckets it reached, and nothing else is assumed).
  /// Returns the fold over the slab bytes in bucket order — the node-store stamp a LOADED
  /// bucketed generation reports, which the graph cache is stamped against.
  fn save_nodes_bucketed(
    &self,
    dir: &Path,
    bounds: &BucketBounds,
    prior: Option<&Path>,
  ) -> io::Result<u64> {
    use rayon::prelude::*;
    use std::io::Write;
    let buckets = bounds.buckets;
    let nodes_dir = dir.join(NODES_DIR);
    fs::create_dir_all(&nodes_dir)?;
    let prior_toc = prior.and_then(|p| NodesToc::load(&p.join(NODES_DIR).join(NODES_TOC_FILE)));
    let prior_ok = prior_toc
      .as_ref()
      .is_some_and(|toc| toc.rows.len() as u32 == buckets);

    // Build every bucket's slab bytes in parallel (independent pure slices of the sealed
    // columns), then land them: identical-to-prior buckets hard-link, changed ones write.
    let built: io::Result<Vec<BuiltBucket>> = (0..buckets as usize)
      .into_par_iter()
      .map(|k| self.build_bucket_slab(bounds, k))
      .collect();
    let built = built?;
    for (k, bucket) in built.iter().enumerate() {
      let vseg_name = format!("{k:04}.vseg");
      let heap_name = format!("{k:04}.heap");
      let carried = prior_ok
        && prior_toc.as_ref().is_some_and(|toc| {
          let row = &toc.rows[k];
          row.rows == bucket.rows
            && row.vseg_len == bucket.vseg.len() as u64
            && row.vseg_digest == bucket.vseg_digest
            && row.heap_len == bucket.heap_len as u64
            && row.heap_digest == bucket.heap_digest
        });
      if carried {
        let prior_dir = prior.map(|p| p.join(NODES_DIR));
        let linked = prior_dir.as_ref().is_some_and(|pd| {
          let mut ok = true;
          for name in [&vseg_name, &heap_name] {
            let (from, to) = (pd.join(name), nodes_dir.join(name));
            if from == to {
              continue; // legacy same-directory publish: already in place
            }
            let _ = fs::remove_file(&to);
            if fs::hard_link(&from, &to).is_err() {
              ok = false;
              break;
            }
          }
          ok
        });
        if linked {
          continue;
        }
        // Link refused (cross-device, permissions): fall through to the write — same
        // bytes, full cost.
      }
      write_via_tmp(&nodes_dir, &vseg_name, |out| out.write_all(&bucket.vseg))?;
      write_via_tmp(&nodes_dir, &heap_name, |out| {
        let (start, end) = bucket.heap_range;
        out.write_all(&self.heaps[0][start..end])
      })?;
    }
    // TOC last — the publish's commit record for this artifact family.
    write_via_tmp(&nodes_dir, NODES_TOC_FILE, |out| {
      out.write_all(NODES_TOC_MAGIC)?;
      out.write_all(&NODES_VERSION.to_le_bytes())?;
      out.write_all(&buckets.to_le_bytes())?;
      out.write_all(&(self.total_rows).to_le_bytes())?;
      for bucket in &built {
        out.write_all(&bucket.rows.to_le_bytes())?;
        out.write_all(&(bucket.vseg.len() as u64).to_le_bytes())?;
        out.write_all(&bucket.vseg_digest.to_le_bytes())?;
        out.write_all(&(bucket.heap_len as u64).to_le_bytes())?;
        out.write_all(&bucket.heap_digest.to_le_bytes())?;
      }
      // The file table (TOC v2): the dense-id anchor for every (file_key, ordinal)-coded
      // family. Dense order; keys are collision-gated at build.
      out.write_all(&(bounds.files.len() as u64).to_le_bytes())?;
      for &(key, start, rows_in_file) in &bounds.files {
        out.write_all(&key.to_le_bytes())?;
        out.write_all(&start.to_le_bytes())?;
        out.write_all(&rows_in_file.to_le_bytes())?;
      }
      Ok(())
    })?;
    // One truth per directory: retire the flat pair (upgrade in a legacy same-dir publish)
    // and any bucket files beyond this publish's count.
    let _ = fs::remove_file(dir.join("nodes.vseg"));
    let _ = fs::remove_file(dir.join("strings.heap"));
    if let Ok(dirents) = fs::read_dir(&nodes_dir) {
      for entry in dirents.flatten() {
        if let Ok(name) = entry.file_name().into_string() {
          let stale = name
            .strip_suffix(".vseg")
            .or_else(|| name.strip_suffix(".heap"))
            .and_then(|k| k.parse::<u32>().ok())
            .is_some_and(|k| k >= buckets);
          if stale || name.ends_with(".tmp") {
            let _ = fs::remove_file(entry.path());
          }
        }
      }
    }
    let mut fold = xxhash_rust::xxh3::Xxh3::new();
    for bucket in &built {
      fold.update(&bucket.vseg);
    }
    Ok(fold.digest())
  }

  /// The dense-id ⇄ `(file_key, ordinal)` map this graph persists under `layout` — what
  /// the evidence and edge savers convert endpoints against. `None` for the flat layout.
  pub fn node_id_map(&self, layout: &SegmentLayout) -> io::Result<Option<NodeIdMap>> {
    match layout {
      SegmentLayout::Flat => Ok(None),
      SegmentLayout::Bucketed {
        tree_root,
        live_files,
        ..
      } => {
        let bounds = self.bucket_bounds(tree_root, *live_files)?;
        Ok(Some(NodeIdMap::from_parts(bounds.row_starts, bounds.files)))
      }
    }
  }

  /// Per-bucket row/heap boundaries of this sealed single-slab graph, derived from the
  /// columns themselves: File nodes open blocks (layout order — file node first), blocks
  /// must arrive bucket-major (the v2 canonical order), and a bucket's heap slab spans
  /// from its smallest referenced offset to the next bucket's. Offsets under zero-length
  /// strings are ignored (and canonicalized to 0 at slab build).
  fn bucket_bounds(&self, tree_root: &str, live_files: usize) -> io::Result<BucketBounds> {
    if self.segments.len() != 1 {
      return Err(io::Error::other(
        "bucketed save of an already-bucketed graph without a re-seal is unsupported",
      ));
    }
    let segment = &self.segments[0];
    let cols = &self.cols[0];
    let rows = segment.row_count();
    let kind = segment
      .column_at(cols.kind)
      .and_then(|c| c.as_slice::<u8>().map(<[u8]>::to_vec))
      .ok_or_else(|| io::Error::other("kind column unavailable for bucket bounds"))?;
    let file_tag = SymbolKind::File.tag();
    let mut file_rows: Vec<u64> = Vec::new();
    for (row, &tag) in kind.iter().enumerate() {
      if tag == file_tag {
        file_rows.push(row as u64);
      }
    }
    let buckets = crate::identity::bucket_count_for(live_files);
    let mut row_starts: Vec<u64> = vec![u64::MAX; buckets as usize + 1];
    let mut files: Vec<(u64, u64, u32)> = Vec::with_capacity(file_rows.len());
    let mut last_bucket: i64 = -1;
    for (i, &file_row) in file_rows.iter().enumerate() {
      let path = self
        .heap_str(0, cols.path_off, cols.path_len, file_row)
        .ok_or_else(|| io::Error::other("File node without a path"))?;
      let rel = crate::identity::tree_relative(path, tree_root);
      let bucket = crate::identity::bucket_of(rel, buckets);
      if i64::from(bucket) < last_bucket {
        return Err(io::Error::other(
          "graph is not sealed in bucket-major canonical order — bucketed save refused",
        ));
      }
      if i64::from(bucket) > last_bucket {
        for start in &mut row_starts[(last_bucket + 1) as usize..=bucket as usize] {
          *start = file_row;
        }
        last_bucket = i64::from(bucket);
      }
      let end = file_rows.get(i + 1).copied().unwrap_or(rows);
      files.push((
        crate::identity::FileKey::of(rel).0,
        file_row,
        (end - file_row) as u32,
      ));
    }
    for start in &mut row_starts[(last_bucket + 1) as usize..=buckets as usize] {
      *start = rows;
    }
    Ok(BucketBounds {
      buckets,
      row_starts,
      files,
    })
  }

  /// Build one bucket's slab: sliced columns with heap offsets rebased to bucket-local
  /// (zero-length strings canonicalize to offset 0), a fresh segment with no id base in
  /// its bytes, and the heap range + digests for the TOC.
  fn build_bucket_slab(&self, bounds: &BucketBounds, k: usize) -> io::Result<BuiltBucket> {
    let segment = &self.segments[0];
    let cols = &self.cols[0];
    let (start, end) = (bounds.row_starts[k] as usize, bounds.row_starts[k + 1] as usize);
    let rows = end - start;

    let slice_u8 = |col: usize| -> io::Result<Vec<u8>> {
      let view = segment
        .column_at(col)
        .ok_or_else(|| io::Error::other("column vanished mid-save"))?;
      let all = view
        .as_slice::<u8>()
        .ok_or_else(|| io::Error::other("column not sliceable"))?;
      Ok(all[start..end].to_vec())
    };
    let slice_u32 = |col: usize| -> io::Result<Vec<u32>> {
      let view = segment
        .column_at(col)
        .ok_or_else(|| io::Error::other("column vanished mid-save"))?;
      let all = view
        .as_slice::<u32>()
        .ok_or_else(|| io::Error::other("column not sliceable"))?;
      Ok(all[start..end].to_vec())
    };
    let slice_u64 = |col: usize| -> io::Result<Vec<u64>> {
      let view = segment
        .column_at(col)
        .ok_or_else(|| io::Error::other("column vanished mid-save"))?;
      let all = view
        .as_slice::<u64>()
        .ok_or_else(|| io::Error::other("column not sliceable"))?;
      Ok(all[start..end].to_vec())
    };
    let col_or = |col: Option<usize>, what: &str| -> io::Result<usize> {
      col.ok_or_else(|| io::Error::other(format!("sealed segment missing {what}")))
    };

    let kind = slice_u8(cols.kind)?;
    let mut name_off = slice_u32(cols.name_off)?;
    let name_len = slice_u32(cols.name_len)?;
    let mut path_off = slice_u32(cols.path_off)?;
    let path_len = slice_u32(cols.path_len)?;
    let mut sig_off = slice_u32(cols.sig_off)?;
    let sig_len = slice_u32(cols.sig_len)?;
    let content_hash = slice_u64(cols.content_hash)?;
    let eid_lo = slice_u64(col_or(cols.eid_lo, "eid_lo")?)?;
    let eid_hi = slice_u64(col_or(cols.eid_hi, "eid_hi")?)?;
    let flags = slice_u8(cols.flags)?;
    let span_start = slice_u32(col_or(cols.span_start, "span_start")?)?;
    let span_end = slice_u32(col_or(cols.span_end, "span_end")?)?;
    let scc_size = slice_u32(col_or(cols.scc_size, "scc_size")?)?;

    // The bucket's heap slab: smallest referenced offset .. next bucket's smallest (heap
    // bytes are gathered in block order, so referenced offsets are bucket-contiguous;
    // unreferenced residue between blocks rides with whichever slab precedes it — offsets
    // stay consistent either way).
    let heap_total = self.heaps[0].len();
    let mut heap_start = heap_total;
    for i in 0..rows {
      for (off, len) in [
        (name_off[i], name_len[i]),
        (path_off[i], path_len[i]),
        (sig_off[i], sig_len[i]),
      ] {
        if len > 0 {
          heap_start = heap_start.min(off as usize);
        }
      }
    }
    if rows == 0 {
      heap_start = 0;
    }
    // End = the next non-empty bucket's start; computed by the caller pass below would
    // need cross-bucket state, so derive it the same way: max referenced end.
    let mut heap_end = heap_start;
    for i in 0..rows {
      for (off, len) in [
        (name_off[i], name_len[i]),
        (path_off[i], path_len[i]),
        (sig_off[i], sig_len[i]),
      ] {
        if len > 0 {
          heap_end = heap_end.max(off as usize + len as usize);
        }
      }
    }
    let base = heap_start as u32;
    for i in 0..rows {
      name_off[i] = if name_len[i] > 0 { name_off[i] - base } else { 0 };
      path_off[i] = if path_len[i] > 0 { path_off[i] - base } else { 0 };
      sig_off[i] = if sig_len[i] > 0 { sig_off[i] - base } else { 0 };
    }

    let mut builder = SegmentBuilder::new(0);
    let build_err = |_| io::Error::other("bucket slab build failed");
    builder.add_u8("kind", &kind).map_err(build_err)?;
    builder.add_u32("name_off", &name_off).map_err(build_err)?;
    builder.add_u32("name_len", &name_len).map_err(build_err)?;
    builder.add_u32("path_off", &path_off).map_err(build_err)?;
    builder.add_u32("path_len", &path_len).map_err(build_err)?;
    builder.add_u32("sig_off", &sig_off).map_err(build_err)?;
    builder.add_u32("sig_len", &sig_len).map_err(build_err)?;
    builder.add_u64("content_hash", &content_hash).map_err(build_err)?;
    builder.add_u64("eid_lo", &eid_lo).map_err(build_err)?;
    builder.add_u64("eid_hi", &eid_hi).map_err(build_err)?;
    builder.add_u8("flags", &flags).map_err(build_err)?;
    builder.add_u32("span_start", &span_start).map_err(build_err)?;
    builder.add_u32("span_end", &span_end).map_err(build_err)?;
    builder.add_u32("scc_size", &scc_size).map_err(build_err)?;
    let vseg = builder.build().map_err(build_err)?;
    let vseg_digest = xxhash_rust::xxh3::xxh3_64(&vseg);
    let heap_digest = xxhash_rust::xxh3::xxh3_64(&self.heaps[0][heap_start..heap_end]);
    Ok(BuiltBucket {
      rows: rows as u32,
      vseg,
      vseg_digest,
      heap_range: (heap_start, heap_end),
      heap_len: heap_end - heap_start,
      heap_digest,
    })
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
    let bucketed = dir.join(NODES_TOC).is_file();
    let (segments, heaps, heap_file, directory) = if bucketed {
      Self::open_bucketed_slabs(dir)?
    } else {
      Self::open_flat_slabs(dir)?
    };
    crate::phase_stamp("kg load: map graph");
    let size = segments.iter().map(|s| s.bytes().len() as u64).sum();
    let policy = ResourcePolicy::probe(CorpusProbe::new(size, 1));
    let map_cached_graph = || -> Result<Graph, SegmentError> {
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
      Graph::open_mapped(graph_store).map_err(SegmentError::from)
    };
    let graph = if bucketed {
      // The truth is the per-source-bucket edge slabs; `graph.bin` is a stamped derived
      // cache. Serve the cache only when its stamp matches BOTH the loaded node slabs and
      // the edge TOC; otherwise rebuild from the slabs and re-cache best-effort (the lazy
      // sidecar posture ANN established for committed generations).
      let mut fold = xxhash_rust::xxh3::Xxh3::new();
      for segment in &segments {
        fold.update(segment.bytes());
      }
      let node_fold = fold.digest();
      let id_map = NodeIdMap::from_dir(dir).ok_or(SegmentError::Corrupt(
        "bucketed node store: unreadable TOC for the id map",
      ))?;
      let expected = crate::edgestore::expected_stamp(dir, node_fold);
      let fresh = expected.is_some() && expected == crate::edgestore::cache_stamp_of(dir);
      let cached = if fresh { map_cached_graph().ok() } else { None };
      match cached {
        Some(graph) => graph,
        // A stale, missing, or unreadable cache all land here — never an error.
        None => {
          let graph = crate::edgestore::load_graph(dir, &id_map).map_err(SegmentError::from)?;
          if let Some(stamp) = expected {
            let _ = crate::edgestore::write_cache(dir, &graph, stamp);
          }
          graph
        }
      }
    } else {
      map_cached_graph()?
    };
    crate::phase_stamp("kg load: done");
    let mut kg = Self::with_slabs(
      segments,
      heaps,
      heap_file,
      Some(dir.to_path_buf()),
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

  /// The node count of a persisted index, from the segment header (flat) or the node-store
  /// TOC (bucketed) alone — no string heap read, no edge-list read, no CSR rebuild. This is
  /// all the whole-tree-unchanged fast path needs, so a no-change re-index does not pay a
  /// graph load to report a number.
  pub fn peek_node_count(dir: &Path) -> Result<usize, SegmentError> {
    let dir = &resolve_index_dir(dir);
    if let Some(toc) = NodesToc::load(&dir.join(NODES_TOC)) {
      return Ok(toc.total as usize);
    }
    let nodes_path = dir.join("nodes.vseg");
    let size = fs::metadata(&nodes_path)?.len();
    let policy = ResourcePolicy::probe(CorpusProbe::new(size, 1));
    Ok(Segment::open_file(&nodes_path, &policy)?.row_count() as usize)
  }

  /// Open the flat (v1) node store: one segment + one heap file.
  #[allow(clippy::type_complexity)]
  fn open_flat_slabs(
    dir: &Path,
  ) -> Result<
    (Vec<Segment>, Vec<vorpal_mem::PodColumn<u8>>, Option<PathBuf>, SegmentDirectory),
    SegmentError,
  > {
    let nodes_path = dir.join("nodes.vseg");
    let size = fs::metadata(&nodes_path)?.len();
    let policy = ResourcePolicy::probe(CorpusProbe::new(size, 1));
    let nodes = Segment::open_file(&nodes_path, &policy)?;
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
    let mut directory = SegmentDirectory::new();
    directory.insert(0, nodes.row_count(), 0);
    Ok((
      vec![nodes],
      vec![heap],
      Some(dir.join("strings.heap")),
      directory,
    ))
  }

  /// Open the bucketed (P4.2) node store: every slab named by a consistent TOC, mapped
  /// zero-copy; dense id bases are TOC prefix sums (never slab bytes). Any inconsistency —
  /// bad TOC, missing slab, length mismatch — is a loud error: a generation directory is
  /// atomic, so a half-present node store means a mixed generation, the same refusal as
  /// the graph/node universe gate.
  #[allow(clippy::type_complexity)]
  fn open_bucketed_slabs(
    dir: &Path,
  ) -> Result<
    (Vec<Segment>, Vec<vorpal_mem::PodColumn<u8>>, Option<PathBuf>, SegmentDirectory),
    SegmentError,
  > {
    let toc = NodesToc::load(&dir.join(NODES_TOC))
      .ok_or(SegmentError::Corrupt("bucketed node store: unreadable TOC"))?;
    let policy = ResourcePolicy::probe(CorpusProbe::new(0, 1));
    let nodes_dir = dir.join(NODES_DIR);
    let mut segments = Vec::with_capacity(toc.rows.len());
    let mut heaps = Vec::with_capacity(toc.rows.len());
    let mut directory = SegmentDirectory::new();
    let mut base = 0u64;
    for (k, row) in toc.rows.iter().enumerate() {
      let vseg_path = nodes_dir.join(format!("{k:04}.vseg"));
      if fs::metadata(&vseg_path)?.len() != row.vseg_len {
        return Err(SegmentError::Corrupt(
          "bucketed node store: slab length disagrees with TOC (mixed generation)",
        ));
      }
      let segment = Segment::open_file(&vseg_path, &policy)?;
      if segment.row_count() != u64::from(row.rows) {
        return Err(SegmentError::Corrupt(
          "bucketed node store: slab row count disagrees with TOC",
        ));
      }
      let heap_path = nodes_dir.join(format!("{k:04}.heap"));
      let heap_meta_len = fs::metadata(&heap_path)?.len();
      if heap_meta_len != row.heap_len {
        return Err(SegmentError::Corrupt(
          "bucketed node store: heap slab length disagrees with TOC",
        ));
      }
      let heap = if heap_meta_len == 0 {
        vorpal_mem::PodColumn::from_vec(Vec::new())
      } else {
        let store = std::sync::Arc::new(
          vorpal_mem::MappedStore::map_file(
            &heap_path,
            vorpal_mem::StoreKind::VectorsFull,
            vorpal_mem::AccessPattern::Random,
            vorpal_mem::Hotness::Hot,
            &policy,
          )
          .map_err(SegmentError::from)?,
        );
        vorpal_mem::PodColumn::from_mapped_le(&store, 0, heap_meta_len as usize, u8::from_le_bytes)
          .map_err(SegmentError::from)?
      };
      if row.rows > 0 {
        directory.insert(base, u64::from(row.rows), k as u32);
      }
      base += u64::from(row.rows);
      segments.push(segment);
      heaps.push(heap);
    }
    Ok((segments, heaps, None, directory))
  }
}

fn is_containment(e: EdgeType) -> bool {
  matches!(
    e.base(),
    EdgeType::DEFINES | EdgeType::HAS_METHOD | EdgeType::HAS_FIELD
  )
}
