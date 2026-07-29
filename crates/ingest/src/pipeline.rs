//! The bounded streaming orchestrator, decoupled from how files are parsed (§3.4), with a
//! two-pass linking step that resolves references into `calls`/`references` edges (§3.3).

use std::collections::HashMap;
use std::io;
use std::path::Path;

use vorpal_kg::{EdgeType, Kg, KgWriter, NodeId, SymbolKind};
use vorpal_resolve::{Reference, ResolveStats, Resolver, Symbol, SymbolTable, resolve_all};

/// Turns one file's source into KG nodes/edges via the writer, appending any references it finds
/// to `references` for later resolution. Implementors own their parse tree locally.
pub trait FileExtractor {
  fn extract_into(
    &self,
    path: &str,
    source: &str,
    writer: &mut KgWriter,
    references: &mut Vec<Reference>,
  );

  /// Whether this extractor handles `path` (default: all files). Directory ingestion skips files
  /// for which this is false, avoiding reads of unsupported types.
  fn handles(&self, _path: &str) -> bool {
    true
  }
}

/// Running totals for an ingest session.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct IngestStats {
  pub indexed: u64,
  pub skipped: u64,
  pub bytes: u64,
}

/// Per-file result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileOutcome {
  Indexed,
  Skipped,
}

/// A single-writer ingest sink: reads files, applies content-hash skip, drives extraction into a
/// [`KgWriter`], buffers references, and seals a queryable [`Kg`] (optionally after linking).
pub struct Ingestor<E: FileExtractor> {
  extractor: E,
  writer: KgWriter,
  references: Vec<Reference>,
  seen: HashMap<String, [u8; 32]>,
  stats: IngestStats,
}

impl<E: FileExtractor> Ingestor<E> {
  pub fn new(extractor: E) -> Self {
    Self {
      extractor,
      writer: KgWriter::new(),
      references: Vec::new(),
      seen: HashMap::new(),
      stats: IngestStats::default(),
    }
  }

  /// Ingest one in-memory source. Content-hash skip (§3.4): if `path` was last seen with the same
  /// bytes, it is not re-parsed.
  pub fn ingest_source(&mut self, path: &str, source: &str) -> FileOutcome {
    let hash = *blake3::hash(source.as_bytes()).as_bytes();
    self.stats.bytes += source.len() as u64;
    if self.seen.get(path) == Some(&hash) {
      self.stats.skipped += 1;
      return FileOutcome::Skipped;
    }
    self.seen.insert(path.to_owned(), hash);
    self
      .extractor
      .extract_into(path, source, &mut self.writer, &mut self.references);
    self.stats.indexed += 1;
    FileOutcome::Indexed
  }

  /// Read a file (bounded by its size — the only transient buffer) and ingest it.
  pub fn ingest_file(&mut self, path: &Path) -> io::Result<FileOutcome> {
    let source = std::fs::read_to_string(path)?;
    Ok(self.ingest_source(&path.to_string_lossy(), &source))
  }

  /// Ingest a pre-extracted [`FileProduct`] (freshly built or replayed from the incremental
  /// cache) — defines the file's entities and re-attributes its references by entity path.
  /// Takes the product by value: its reference strings are moved into the buffered
  /// [`Reference`]s, so the single-writer apply stage clones nothing.
  pub fn ingest_product(&mut self, path: &str, product: crate::FileProduct) {
    apply_product(path, product, &mut self.writer, &mut self.references);
  }

  /// Recursively ingest a directory, respecting `.gitignore`, skipping files the extractor does
  /// not handle. Bounded: one file is read at a time. Per-file read errors (e.g. non-UTF-8) are
  /// skipped so a stray file cannot abort the walk.
  pub fn ingest_dir(&mut self, root: &Path) -> io::Result<()> {
    for entry in ignore::Walk::new(root) {
      let entry = entry.map_err(io::Error::other)?;
      if !entry.file_type().is_some_and(|t| t.is_file()) {
        continue;
      }
      let path = entry.path();
      if !self.extractor.handles(path.to_string_lossy().as_ref()) {
        continue;
      }
      let _ = self.ingest_file(path);
    }
    Ok(())
  }

  pub fn stats(&self) -> IngestStats {
    self.stats
  }

  /// Distinct entities interned so far.
  pub fn node_count(&self) -> usize {
    self.writer.node_count()
  }

  /// References buffered so far, awaiting resolution.
  pub fn pending_references(&self) -> usize {
    self.references.len()
  }

  /// Seal definitions + containment only (buffered references are dropped unresolved).
  pub fn seal(self) -> Kg {
    self.writer.seal()
  }

  /// Two-pass link + seal (§3.3): build the symbol table from interned definitions, resolve every
  /// buffered reference, inject the resolved edges, then seal. Returns the graph and resolution
  /// stats. Unresolvable references produce no edge — they are counted, never faked.
  pub fn link_and_seal(self, resolver: &Resolver) -> (Kg, ResolveStats) {
    link_writer(self.writer, self.references, resolver)
  }
}

/// The global linking tail shared by [`Ingestor::link_and_seal`] and the sharded commit path:
/// symbol table from the writer's definitions, resolve every reference, inject the resolved
/// edges, seal. Takes the references by value so the ~hundreds of MB of reference strings —
/// and the symbol table — are freed **before** seal allocates the segment buffer: at kernel
/// scale those two corpses pinned ~1 GB through the peak-memory moment for nothing.
/// Where the (rebased) reference stream goes during a streamed apply: RAM for callers that
/// want the vector, or the disk spill for bulk builds — at kernel scale the in-RAM vector
/// was ~220 MB of peak footprint that resolution only ever reads once, sequentially.
enum RefSink<'a> {
  Ram(&'a mut Vec<Reference>),
  Spill(&'a mut vorpal_resolve::RefSpillWriter),
}

impl RefSink<'_> {
  fn consume(&mut self, shard_references: Vec<Reference>, id_base: u64) -> io::Result<()> {
    let rebased = shard_references.into_iter().map(|mut reference| {
      reference.from = NodeId::new(reference.from.raw() + id_base);
      reference
    });
    match self {
      RefSink::Ram(references) => {
        references.extend(rebased);
        Ok(())
      }
      RefSink::Spill(writer) => {
        for reference in rebased {
          writer.push(&reference)?;
        }
        Ok(())
      }
    }
  }
}

/// Merge one completed shard into the global writer, rebasing its buffered references by
/// the id base the absorb assigns — the single absorption step both the rolling path and
/// the leftover tail share, so their outputs are identical by construction.
fn absorb_shard(
  writer: &mut KgWriter,
  sink: &mut RefSink<'_>,
  shard_writer: KgWriter,
  shard_references: Vec<Reference>,
) -> io::Result<()> {
  let id_base = writer.absorb(shard_writer);
  sink.consume(shard_references, id_base)
}

/// Emit a phase stamp to stderr when `VORPAL_PHASE_TRACE` is set — for correlating RSS
/// timelines with pipeline phases during memory profiling.
pub fn phase_trace(label: &str) {
  vorpal_kg::phase_stamp(label);
}

/// Hand freed-but-retained allocator pages back to the OS at a phase boundary. On macOS the
/// default malloc keeps freed pages dirty in per-thread magazines, so a build's peak
/// footprint reads as (largest phase) + (every earlier phase's retained garbage) even when
/// the live set shrank between phases. One `malloc_zone_pressure_relief` sweep at each seam
/// makes the footprint track the live set instead. Elsewhere this is a no-op — the peak
/// figures we publish are honest live-set peaks, not allocator accidents.
pub fn release_freed_pages() {
  #[cfg(target_os = "macos")]
  {
    unsafe extern "C" {
      fn malloc_zone_pressure_relief(zone: *mut std::ffi::c_void, goal: usize) -> usize;
    }
    unsafe {
      malloc_zone_pressure_relief(std::ptr::null_mut(), 0);
    }
  }
}

pub fn link_writer(
  mut writer: KgWriter,
  references: Vec<Reference>,
  resolver: &Resolver,
) -> (Kg, ResolveStats) {
  phase_trace("link: table build start");
  let table = build_symbol_table(&writer);
  // The table build's transients (per-shard pair vectors, the finalize sort buffer) just
  // died — return their pages before resolution allocates the edge lists.
  release_freed_pages();
  phase_trace("link: resolve start");
  let (edges, stats) = resolve_all(&table, &references, resolver);
  phase_trace("link: resolve done");
  drop(table);
  drop(references);
  // The largest transient of the run (references + table) just died — return its pages
  // before compaction and seal allocate theirs.
  release_freed_pages();
  for edge in &edges {
    writer.add_edge(
      edge.from,
      edge.to,
      edge.edge.with_confidence(edge.confidence),
    );
  }
  drop(edges);
  phase_trace("link: seal start");
  let kg = writer.seal();
  release_freed_pages();
  phase_trace("link: seal done");
  (kg, stats)
}

/// [`link_writer`] over a spilled reference stream: the same table build, resolution, and
/// seal, with references streamed off disk in bounded chunks (identical output — chunking is
/// invisible to per-reference resolution) and the spill deleted once resolved. Also returns
/// the per-edge evidence rows (span, resolver reason, confidence, candidate count) resolution
/// produced — the caller persists them as the generation's `evidence.bin` sidecar (§5), so
/// every persisted relation can answer "why does this exist?".
pub fn link_writer_spilled(
  mut writer: KgWriter,
  spill: vorpal_resolve::RefSpill,
  resolver: &Resolver,
) -> io::Result<(Kg, ResolveStats, Vec<vorpal_kg::EvidenceRow>)> {
  phase_trace("link: table build start");
  let table = build_symbol_table(&writer);
  release_freed_pages();
  phase_trace("link: resolve start");
  // Edges stream straight into the writer's edge log, in resolution order — the collected
  // edge vector was ~90 MB alive under the seal at kernel scale. Evidence rows are collected
  // alongside (24 bytes per emitted edge; they must all exist before the canonical sort that
  // makes the sidecar deterministic, so streaming them out is not an option).
  let mut evidence: Vec<vorpal_kg::EvidenceRow> = Vec::new();
  let stats = {
    let writer = &mut writer;
    let evidence = &mut evidence;
    vorpal_resolve::resolve_all_spilled_into(&table, &spill, resolver, |edge| {
      writer.add_edge(
        edge.from,
        edge.to,
        edge.edge.with_confidence(edge.confidence),
      );
      evidence.push(vorpal_kg::EvidenceRow {
        from: edge.from.raw() as u32,
        to: edge.to.raw() as u32,
        etype: edge.edge.base().0,
        reason: edge.reason as u8,
        confidence: edge.confidence,
        candidates: edge.candidates,
        span_start: edge.span.0,
        span_end: edge.span.1,
      });
    })?
  };
  phase_trace("link: resolve done");
  drop(table);
  let _ = spill.remove();
  // The link transients (spill chunks + table) just died — return their pages before
  // compaction and seal allocate theirs.
  release_freed_pages();
  phase_trace("link: seal start");
  let kg = writer.seal();
  release_freed_pages();
  phase_trace("link: seal done");
  Ok((kg, stats, evidence))
}

/// Fewer files than this per shard and the fan-out overhead outweighs the win: small trees
/// take the single-writer path outright.
const MIN_FILES_PER_SHARD: usize = 16;

/// Upper bound on files per streaming shard (see `stream_apply_impl`'s sizing comment).
const SHARD_CAP_FILES: usize = 64;

/// §7.5 **sharded single-writer commit** over pre-extracted products: partition the
/// (path-sorted) product list into contiguous shards, apply each shard in its own private
/// [`KgWriter`] in parallel — one writer per shard, no locks — then absorb the shards in
/// order, rebasing node ids and buffered references. Contiguous slices + ordered absorption
/// reproduce the serial writer's id assignment exactly, so the sealed output is bit-identical
/// to a single-writer apply (pinned by test); cross-shard resolution still happens in the
/// global [`link_writer`] pass, which sees the merged table.
pub fn apply_products_sharded(
  products: Vec<(String, crate::FileProduct)>,
) -> (KgWriter, Vec<Reference>) {
  use rayon::prelude::*;

  let threads = rayon::current_num_threads().max(1);
  // ~2 shards per thread for balance, floored so tiny trees stay serial.
  let shard_size = products
    .len()
    .div_ceil(threads * 2)
    .max(MIN_FILES_PER_SHARD);

  if products.len() <= shard_size {
    let mut writer = KgWriter::new();
    let mut references = Vec::new();
    for (path, product) in products {
      apply_product(&path, product, &mut writer, &mut references);
    }
    return (writer, references);
  }

  let shards: Vec<(KgWriter, Vec<Reference>)> = products
    .into_par_iter()
    .chunks(shard_size)
    .map(|shard| {
      let mut writer = KgWriter::new();
      let mut references = Vec::new();
      for (path, product) in shard {
        apply_product(&path, product, &mut writer, &mut references);
      }
      (writer, references)
    })
    .collect();

  let mut writer = KgWriter::new();
  let mut references = Vec::new();
  for (shard_writer, shard_references) in shards {
    let id_base = writer.absorb(shard_writer);
    references.extend(shard_references.into_iter().map(|mut reference| {
      reference.from = NodeId::new(reference.from.raw() + id_base);
      reference
    }));
  }
  (writer, references)
}

/// Apply one file's product to the writer: ingest its items, then push its references with
/// `from` resolved through the writer's canonical identity (entity path → fresh `NodeId`).
/// Consumes the product so name/qualifier strings move instead of cloning — this stage is the
/// serial single-writer section of the pipeline, so every allocation here is on the critical
/// path at any corpus size.
pub(crate) fn apply_product(
  path: &str,
  product: crate::FileProduct,
  writer: &mut KgWriter,
  references: &mut Vec<Reference>,
) {
  let crate::FileProduct { items, refs, .. } = product;
  apply_parts(
    path,
    &items,
    refs.iter().map(|r| crate::product::RefView {
      from_entity_index: r.from_entity_index,
      name: &r.name,
      kind: r.kind,
      start: r.start,
      end: r.end,
      qualifier: r.qualifier.as_deref(),
      form: r.form,
    }),
    writer,
    references,
  );
}

/// Apply a pack-replayed product straight from its mapped bytes: decode to views, apply —
/// no owned strings anywhere on the path (the replay profile showed decode's per-string
/// allocations as a top cost).
pub(crate) fn apply_product_view(
  path: &str,
  view: &crate::ProductView<'_>,
  writer: &mut KgWriter,
  references: &mut Vec<Reference>,
) {
  apply_parts(
    path,
    &view.items,
    view.refs.iter().copied(),
    writer,
    references,
  );
}

/// The single application kernel both product forms share.
fn apply_parts<'a>(
  path: &str,
  items: &[vorpal_outline::model::OutlineItem<'_>],
  refs: impl Iterator<Item = crate::product::RefView<'a>>,
  writer: &mut KgWriter,
  references: &mut Vec<Reference>,
) {
  // Identity lookups below are scoped to this file's entities, and each path lands exactly
  // once (manifest invariant) — so the previous files' identity keys are dead weight.
  writer.forget_identity_scope();
  writer.ingest_file(path, items);
  // References carry entity *indices* into the file's local layout; resolve them through the
  // recomputed layout (an out-of-range index — a corrupt product — simply drops the ref).
  let (entities, _spans) = crate::outline_extractor::local_layout(items);
  // Intern the file's path once; every reference carries the 4-byte id.
  let path_id = vorpal_resolve::intern::intern(path);
  for r in refs {
    let Some(entity) = entities.get(r.from_entity_index as usize) else {
      continue;
    };
    if let Some(from) = writer.entity_id(path, entity) {
      references.push(
        Reference::with_interned_path(from, path_id, r.name, crate::product::tag_refkind(r.kind))
          .with_evidence(r.start, r.end)
          .with_qualifier_ref(r.qualifier)
          .with_form(crate::product::tag_refform(r.form)),
      );
    }
  }
}

/// Below this many definitions the table builds serially — fan-out costs more than it saves.
const MIN_DEFS_PER_SHARD: usize = 4096;

/// The owner id for members whose owner's name no reference ever interned: a reserved,
/// unparseable string (control character) that can never equal a real qualifier, preserving
/// "is a member" without admitting a match.
fn unmatchable_owner() -> vorpal_resolve::NameId {
  vorpal_resolve::intern::intern("\u{1}vorpal:unreferenced-owner")
}

fn build_symbol_table(writer: &KgWriter) -> SymbolTable {
  // Derive each member's owner row from the containment edges (`Kg` for `Kg.load`) — the
  // target side of qualified-reference matching. Containment from a File node is not
  // ownership: top-level items match by module file instead. One cheap serial pass.
  let node_count = writer.node_count();
  let mut owner_of: Vec<Option<u32>> = vec![None; node_count];
  for (src, dst, etype) in writer.edge_log().iter() {
    let containment = etype.base() == EdgeType::DEFINES
      || etype.base() == EdgeType::HAS_METHOD
      || etype.base() == EdgeType::HAS_FIELD;
    if containment
      && writer
        .definition(src as usize)
        .map(|(_, _, _, kind, _)| kind)
        != Some(SymbolKind::File)
      && (dst as usize) < owner_of.len()
    {
      owner_of[dst as usize] = Some(src);
    }
  }

  // §7.5 sharded table build: contiguous row ranges each fill a private table on their own
  // thread, absorbed in row order — candidate lists end up in the exact order the serial
  // insertion produced (pinned by test). Small graphs build serially.
  let insert_range = |range: std::ops::Range<usize>| {
    let mut table = SymbolTable::new();
    table.reserve(range.len());
    for row in range {
      let (id, name, path, kind, exported) = writer.definition(row).expect("row < node_count");
      if kind == SymbolKind::File {
        // File nodes are the targets of path-form imports (`import "./util"`).
        table.insert_file(path, id);
      } else if kind != SymbolKind::Import {
        // Import/alias nodes are wiring, not definitions: offering them as resolution targets
        // let a `use foo` in one file steal call edges meant for the real `foo`.
        // Owners resolve by `peek`: an owner name no reference ever interned can never match
        // a qualifier, but member-ness must survive — the unmatchable sentinel keeps such
        // members out of the top-level (module-stem) matching path.
        let owner = owner_of[row]
          .and_then(|src| writer.definition(src as usize))
          .map(|(_, owner_name, _, _, _)| {
            vorpal_resolve::intern::peek(owner_name).unwrap_or_else(unmatchable_owner)
          });
        table.insert_if_referenced(
          name,
          Symbol {
            id,
            kind,
            path: vorpal_resolve::intern::intern(path),
            exported,
            owner,
          },
        );
      }
    }
    table
  };

  if node_count <= MIN_DEFS_PER_SHARD {
    let mut table = insert_range(0..node_count);
    table.finalize();
    return table;
  }
  use rayon::prelude::*;
  vorpal_kg::phase_stamp("table: owner pass done");
  let threads = rayon::current_num_threads().max(1);
  let shard_size = node_count.div_ceil(threads * 2).max(MIN_DEFS_PER_SHARD);
  let starts: Vec<usize> = (0..node_count).step_by(shard_size).collect();
  let shards: Vec<SymbolTable> = starts
    .par_iter()
    .map(|&start| insert_range(start..(start + shard_size).min(node_count)))
    .collect();
  vorpal_kg::phase_stamp("table: shards built");
  let table = SymbolTable::from_shards(shards);
  vorpal_kg::phase_stamp("table: finalized");
  table
}

#[cfg(test)]
mod sharded_table_tests {
  use super::*;
  use vorpal_kg::NodeDef;

  /// The serial specification: single-pass insertion via `for_each_definition`, exactly the
  /// pre-sharding algorithm. The sharded build must produce an equal table.
  fn serial_reference_table(writer: &KgWriter) -> SymbolTable {
    let mut names: Vec<String> = Vec::with_capacity(writer.node_count());
    let mut kinds: Vec<SymbolKind> = Vec::with_capacity(writer.node_count());
    writer.for_each_definition(|_, name, _, kind, _| {
      names.push(name.to_owned());
      kinds.push(kind);
    });
    let mut owner_of: Vec<Option<u32>> = vec![None; names.len()];
    for (src, dst, etype) in writer.edge_log().iter() {
      let containment = etype.base() == EdgeType::DEFINES
        || etype.base() == EdgeType::HAS_METHOD
        || etype.base() == EdgeType::HAS_FIELD;
      if containment
        && kinds.get(src as usize).copied() != Some(SymbolKind::File)
        && (dst as usize) < owner_of.len()
      {
        owner_of[dst as usize] = Some(src);
      }
    }
    let mut table = SymbolTable::new();
    writer.for_each_definition(|id, name, path, kind, exported| {
      if kind == SymbolKind::File {
        table.insert_file(path, id);
      } else if kind != SymbolKind::Import {
        let owner = owner_of[id.raw() as usize].map(|src| {
          vorpal_resolve::intern::peek(&names[src as usize]).unwrap_or_else(unmatchable_owner)
        });
        table.insert_if_referenced(
          name,
          Symbol {
            id,
            kind,
            path: vorpal_resolve::intern::intern(path),
            exported,
            owner,
          },
        );
      }
    });
    table.finalize();
    table
  }

  #[test]
  fn sharded_table_build_equals_the_serial_specification() {
    // Referenced-only inserts key off the interner: intern every name this corpus uses (as
    // reference construction would have during commit) so the oracle exercises real,
    // non-empty tables regardless of what other tests interned first.
    for j in 0..4usize {
      vorpal_resolve::intern::intern(&format!("Item{j}"));
      vorpal_resolve::intern::intern(&format!("member_{j}"));
    }
    // A writer big enough to force multiple shards (> MIN_DEFS_PER_SHARD definitions), with
    // files, items, members (owners), imports, duplicate names, and privates.
    let mut writer = KgWriter::new();
    for i in 0..800usize {
      let path = format!("src/file_{i:03}.rs");
      let file_id = writer.define(NodeDef {
        kind: SymbolKind::File,
        name: &path,
        entity_path: "",
        path: &path,
        signature: "",
        exported: true,
        content_hash: i as u64,
        span: (0, 0),
      });
      for j in 0..4usize {
        let item_name = format!("Item{j}");
        let item_id = writer.define(NodeDef {
          kind: if j % 2 == 0 {
            SymbolKind::Struct
          } else {
            SymbolKind::Function
          },
          name: &item_name,
          entity_path: &item_name,
          path: &path,
          signature: "sig",
          exported: j % 3 != 0,
          content_hash: (i * 10 + j) as u64,
          span: (0, 0),
        });
        writer.add_edge(file_id, item_id, EdgeType::DEFINES);
        let member_name = format!("member_{j}");
        let entity = format!("{item_name}.{member_name}");
        let member_id = writer.define(NodeDef {
          kind: SymbolKind::Method,
          name: &member_name,
          entity_path: &entity,
          path: &path,
          signature: "msig",
          exported: true,
          content_hash: (i * 100 + j) as u64,
          span: (0, 0),
        });
        writer.add_edge(item_id, member_id, EdgeType::HAS_METHOD);
      }
      let import_name = format!("imported_{i}");
      writer.define(NodeDef {
        kind: SymbolKind::Import,
        name: &import_name,
        entity_path: &import_name,
        path: &path,
        signature: "",
        exported: false,
        content_hash: i as u64 + 7,
        span: (0, 0),
      });
    }
    assert!(
      writer.node_count() > MIN_DEFS_PER_SHARD,
      "corpus must force the sharded path ({} defs)",
      writer.node_count()
    );

    assert_eq!(
      build_symbol_table(&writer),
      serial_reference_table(&writer),
      "sharded table diverged from the serial specification"
    );
  }
}

// --- §7.5 bounded streaming: byte-budget admission → MPMC stages → sharded commit ---------

/// In-flight byte budget (§7.5 byte-budget admission): discovery reserves a file's bytes
/// before it is read and the committer releases them once its product has been applied, so
/// peak transient memory is bounded by `capacity` regardless of corpus size. Reservation is a
/// CAS on a cache-padded atomic (the hot path); exhaustion parks on a condvar until a release
/// makes room. A single item larger than the whole budget reserves the full capacity instead
/// of deadlocking — progress over precision for the pathological case.
pub struct ByteBudget {
  capacity: u64,
  used: crossbeam_utils::CachePadded<std::sync::atomic::AtomicU64>,
  peak: crossbeam_utils::CachePadded<std::sync::atomic::AtomicU64>,
  gate: std::sync::Mutex<()>,
  room: std::sync::Condvar,
}

impl ByteBudget {
  pub fn new(capacity: u64) -> Self {
    Self {
      capacity: capacity.max(1),
      used: crossbeam_utils::CachePadded::new(std::sync::atomic::AtomicU64::new(0)),
      peak: crossbeam_utils::CachePadded::new(std::sync::atomic::AtomicU64::new(0)),
      gate: std::sync::Mutex::new(()),
      room: std::sync::Condvar::new(),
    }
  }

  /// Reserve `bytes` (clamped to capacity), blocking until they fit. Release with the SAME
  /// `bytes` value: both sides apply the identical clamp, so accounts always balance — even
  /// for a file larger than the whole budget.
  pub fn reserve(&self, bytes: u64) -> u64 {
    use std::sync::atomic::Ordering;
    let want = bytes.clamp(1, self.capacity);
    loop {
      let current = self.used.load(Ordering::Acquire);
      if current + want <= self.capacity {
        if self
          .used
          .compare_exchange(current, current + want, Ordering::AcqRel, Ordering::Acquire)
          .is_ok()
        {
          self.peak.fetch_max(current + want, Ordering::AcqRel);
          return want;
        }
        continue; // CAS race: retry immediately.
      }
      // No room: park until a release, then re-check.
      let guard = self.gate.lock().unwrap();
      if self.used.load(Ordering::Acquire) + want <= self.capacity {
        continue; // Released between the check and the lock.
      }
      let _guard = self.room.wait(guard).unwrap();
    }
  }

  pub fn release(&self, bytes: u64) {
    use std::sync::atomic::Ordering;
    let amount = bytes.clamp(1, self.capacity); // the mirror of reserve's clamp
    self.used.fetch_sub(amount, Ordering::AcqRel);
    let _guard = self.gate.lock().unwrap();
    self.room.notify_all();
  }

  /// High-water mark of concurrent reservations — the observable proof that admission
  /// actually bounded in-flight bytes (asserted by tests, useful as telemetry).
  pub fn peak(&self) -> u64 {
    self.peak.load(std::sync::atomic::Ordering::Acquire)
  }
}

/// Per-worker reusable buffers (§7.5 per-worker arenas, realized as scratch reuse): the two
/// dominant per-file allocations — the source read buffer and the product encode buffer —
/// amortize to zero across a worker's lifetime. Contents that must outlive a file (product
/// strings) are copied out exactly once, as everywhere else in the pipeline; the parse tree
/// itself lives in tree-sitter's allocator and is out of scratch's reach.
#[derive(Default)]
pub struct ExtractScratch {
  pub source: String,
  pub encode: Vec<u8>,
}

impl ExtractScratch {
  /// Retained-capacity bound per buffer. Reuse makes the common case allocation-free, but an
  /// unbounded buffer pins the *largest file the worker ever saw* for the rest of the run —
  /// across a pool of workers on a corpus with 10–20 MB generated headers, hundreds of MB of
  /// dead high-water. Oversized buffers are released after use; the next giant file simply
  /// reallocates (rare by construction — that's what makes the buffer *scratch*).
  const RETAIN_LIMIT: usize = 2 * 1024 * 1024;

  /// Read `path` into the reused source buffer (replacing its contents), UTF-8-validated
  /// exactly like `fs::read_to_string`.
  pub fn read_source(&mut self, path: &Path) -> io::Result<&str> {
    use std::io::Read;
    self.source.clear();
    if self.source.capacity() > Self::RETAIN_LIMIT {
      self.source.shrink_to(Self::RETAIN_LIMIT);
    }
    if self.encode.capacity() > Self::RETAIN_LIMIT {
      self.encode.clear();
      self.encode.shrink_to(Self::RETAIN_LIMIT);
    }
    std::fs::File::open(path)?.read_to_string(&mut self.source)?;
    Ok(&self.source)
  }
}

/// One entry's streaming outcome, produced by the caller's work closure.
pub enum StreamWork {
  /// Freshly parsed this run.
  Parsed(String, crate::FileProduct),
  /// Replayed from the incremental cache.
  Replayed(String, crate::FileProduct),
  /// Replayed from the products pack: only the path travels — the committer decodes views
  /// straight out of the mapped pack and applies without materializing a product. The
  /// producer must have validated the entry (stamps + a full view decode).
  ReplayedPacked(String),
  /// Not extractable (unreadable, unsupported) — skipped, exactly like the batch path.
  Skipped,
}

/// Counters and telemetry from a streaming run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamStats {
  pub parsed: u64,
  pub replayed: u64,
  /// High-water mark of in-flight reserved bytes (≤ the configured budget).
  pub peak_in_flight_bytes: u64,
}

/// What a shard committer receives for one admitted entry: its global sequence number and
/// either a product to apply or a skip marker (skips still advance the in-shard order).
enum Slot {
  Product {
    path: String,
    product: crate::FileProduct,
    parsed: bool,
    reserved: u64,
  },
  Packed {
    path: String,
    reserved: u64,
  },
  Skipped,
}

/// §7.5 **bounded streaming ingest**: `discover → admit (byte budget) → extract (scoped
/// workers, per-worker scratch) → commit (single writer per shard, in-shard order)` joined by
/// bounded channels, so peak transient memory is O(budget + queue capacities), independent of
/// corpus size — a product exists in RAM only between extraction and application.
///
/// Ordering and therefore output are **identical to the batch path**: every admitted entry is
/// assigned its manifest-order sequence number up front; each shard's committer applies its
/// entries in sequence order (a reorder buffer absorbs out-of-order arrivals — bounded by the
/// byte budget, and the reason a straggler can never deadlock its shard); shard writers are
/// absorbed in shard order as they complete (rolling prefix absorption — the merged
/// writer grows during commit rather than doubling at the end). Pinned byte-for-byte by test, including under a
/// deliberately starved budget.
///
/// Workers are scoped threads borrowing `work` and the caller's state by `&` — no `'static`,
/// no `Arc` on the hot path (§7).
///
/// The first `Err` from `work` aborts the run and is returned; a partial graph is never
/// produced.
pub fn stream_apply<F>(
  entries: &[crate::FileStat],
  budget_bytes: u64,
  work: F,
) -> io::Result<(KgWriter, Vec<Reference>, StreamStats)>
where
  F: Fn(&crate::FileStat, &mut ExtractScratch) -> io::Result<StreamWork> + Sync,
{
  let mut references = Vec::new();
  let (writer, stats) = stream_apply_impl(
    entries,
    budget_bytes,
    work,
    RefSink::Ram(&mut references),
    None,
    None,
  )?;
  Ok((writer, references, stats))
}

/// [`stream_apply`] with the reference stream spilled to `spill_path` instead of buffered in
/// RAM — the bulk-build configuration. Resolve the result with
/// [`vorpal_resolve::resolve_all_spilled`] (or [`link_writer_spilled`]), which streams the
/// file back in bounded chunks and deletes it.
pub fn stream_apply_spilled<F>(
  entries: &[crate::FileStat],
  budget_bytes: u64,
  spill_path: &std::path::Path,
  heap_stream_path: Option<&std::path::Path>,
  pack: Option<&crate::PackReader>,
  work: F,
) -> io::Result<(KgWriter, vorpal_resolve::RefSpill, StreamStats)>
where
  F: Fn(&crate::FileStat, &mut ExtractScratch) -> io::Result<StreamWork> + Sync,
{
  let mut spill_writer = vorpal_resolve::RefSpillWriter::create(spill_path)?;
  let (writer, stats) = stream_apply_impl(
    entries,
    budget_bytes,
    work,
    RefSink::Spill(&mut spill_writer),
    heap_stream_path,
    pack,
  )?;
  Ok((writer, spill_writer.finish()?, stats))
}

fn stream_apply_impl<F>(
  entries: &[crate::FileStat],
  budget_bytes: u64,
  work: F,
  mut sink: RefSink<'_>,
  heap_stream_path: Option<&std::path::Path>,
  pack: Option<&crate::PackReader>,
) -> io::Result<(KgWriter, StreamStats)>
where
  F: Fn(&crate::FileStat, &mut ExtractScratch) -> io::Result<StreamWork> + Sync,
{
  let threads = std::thread::available_parallelism()
    .map(|n| n.get())
    .unwrap_or(1);
  // Shards are deliberately small: sequential admission keeps the in-flight window narrow,
  // and with a handful of huge shards only one or two committers were ever active — the
  // replay profile showed every worker blocked on a full committer channel while one
  // committer applied a 2,267-file shard serially. Capping shards at 64 files spreads the
  // active window across every committer; output bytes are shard-size-independent (pinned
  // by the streamed≡batch identity tests). Env-tunable for experiments.
  let shard_size = entries.len().div_ceil((threads * 2).max(1)).clamp(
    MIN_FILES_PER_SHARD,
    std::env::var("VORPAL_SHARD_CAP")
      .ok()
      .and_then(|v| v.parse().ok())
      .unwrap_or(SHARD_CAP_FILES),
  );

  // Small trees: one thread, one writer, zero fan-out — the same output by definition.
  if entries.len() <= shard_size {
    let mut scratch = ExtractScratch::default();
    let mut writer = KgWriter::new();
    let mut references = Vec::new();
    let (mut parsed, mut replayed) = (0u64, 0u64);
    for entry in entries {
      match work(entry, &mut scratch)? {
        StreamWork::Parsed(path, product) => {
          parsed += 1;
          apply_product(&path, product, &mut writer, &mut references);
        }
        StreamWork::Replayed(path, product) => {
          replayed += 1;
          apply_product(&path, product, &mut writer, &mut references);
        }
        StreamWork::ReplayedPacked(path) => {
          if let Some(view) = pack
            .and_then(|p| p.get(&path))
            .and_then(|bytes| crate::product::decode_product_view(bytes).ok())
          {
            apply_product_view(&path, &view, &mut writer, &mut references);
            replayed += 1;
          }
        }
        StreamWork::Skipped => {}
      }
    }
    sink.consume(references, 0)?;
    return Ok((
      writer,
      StreamStats {
        parsed,
        replayed,
        peak_in_flight_bytes: 0,
      },
    ));
  }

  let num_shards = entries.len().div_ceil(shard_size);
  // Half the workers commit: replay-heavy runs are apply-bound, and with threads/4 the
  // committers were the throughput ceiling once shards got small enough to keep them all fed.
  let committers = num_shards.min((threads / 2).max(1));
  let budget = ByteBudget::new(budget_bytes);
  let abort = std::sync::atomic::AtomicBool::new(false);
  let first_error: std::sync::Mutex<Option<io::Error>> = std::sync::Mutex::new(None);
  let fail = |err: io::Error| {
    abort.store(true, std::sync::atomic::Ordering::Release);
    first_error.lock().unwrap().get_or_insert(err);
  };

  // Admission → workers: a bounded MPMC of (sequence, entry); fixed capacity IS the
  // backpressure.
  let (work_tx, work_rx) = crossbeam_channel::bounded::<(usize, &crate::FileStat)>(threads * 2);
  // Workers → committers: one bounded channel per committer thread; shard k routes to
  // committer k % committers.
  let (slot_txs, slot_rxs): (Vec<_>, Vec<_>) = (0..committers)
    .map(|_| crossbeam_channel::bounded::<(usize, Slot)>(64))
    .unzip();

  let total_sequences = entries.len();
  let mut writer = KgWriter::new();
  if let Some(path) = heap_stream_path {
    // The merged heap writes through to disk as shards absorb (~140 MB at kernel scale that
    // never touches anonymous memory); the link pass reads it back through a zero-copy map.
    writer.stream_heap_to(path)?;
  }
  let mut holdback: std::collections::BTreeMap<usize, (KgWriter, Vec<Reference>)> =
    std::collections::BTreeMap::new();
  let mut next_absorb = 0usize;
  // First sink (spill IO) error: aborts absorption, surfaced after the scope joins.
  let mut sink_error: Option<io::Error> = None;
  // Committers → caller: completed shards, for rolling prefix absorption. Unbounded so a
  // committer never blocks handing off a finished shard.
  let (done_tx, done_rx) = crossbeam_channel::unbounded::<(usize, KgWriter, Vec<Reference>)>();

  let outputs = std::thread::scope(|scope| {
    // Committers: each owns its assigned shards' writers outright (single writer per shard)
    // and drains its channel unconditionally into per-shard reorder buffers — receiving never
    // blocks on applying, which is what keeps a full shard channel impossible and the
    // backpressure cycle broken.
    let committer_handles: Vec<_> = slot_rxs
      .into_iter()
      .enumerate()
      .map(|(committer_index, slot_rx)| {
        let budget = &budget;
        let done_tx = done_tx.clone();
        scope.spawn(move || {
          let owned_shards: Vec<usize> =
            (committer_index..num_shards).step_by(committers).collect();
          let mut writers: HashMap<usize, (KgWriter, Vec<Reference>)> = owned_shards
            .iter()
            .map(|&shard| (shard, (KgWriter::new(), Vec::new())))
            .collect();
          let mut pending: HashMap<usize, std::collections::BTreeMap<usize, Slot>> = owned_shards
            .iter()
            .map(|&shard| (shard, Default::default()))
            .collect();
          let mut next_expected: HashMap<usize, usize> = owned_shards
            .iter()
            .map(|&shard| (shard, shard * shard_size))
            .collect();
          let (mut parsed, mut replayed) = (0u64, 0u64);
          while let Ok((sequence, slot)) = slot_rx.recv() {
            let shard = sequence / shard_size;
            pending
              .get_mut(&shard)
              .expect("routed shard")
              .insert(sequence, slot);
            let expected = next_expected.get_mut(&shard).expect("routed shard");
            let queue = pending.get_mut(&shard).expect("routed shard");
            while let Some(slot) = queue.remove(expected) {
              *expected += 1;
              match slot {
                Slot::Product {
                  path,
                  product,
                  parsed: was_parsed,
                  reserved,
                } => {
                  let (writer, references) = writers.get_mut(&shard).expect("owned shard");
                  apply_product(&path, product, writer, references);
                  budget.release(reserved);
                  if was_parsed {
                    parsed += 1;
                  } else {
                    replayed += 1;
                  }
                }
                Slot::Packed { path, reserved } => {
                  // Decode views straight out of the mapped pack and apply — validated by
                  // the producer, so a failure here is disk rot; the file is then absent
                  // from this build rather than fatal.
                  if let Some(view) = pack
                    .and_then(|p| p.get(&path))
                    .and_then(|bytes| crate::product::decode_product_view(bytes).ok())
                  {
                    let (writer, references) = writers.get_mut(&shard).expect("owned shard");
                    apply_product_view(&path, &view, writer, references);
                    replayed += 1;
                  }
                  budget.release(reserved);
                }
                Slot::Skipped => {}
              }
            }
            // Shard complete? Hand it off for rolling prefix absorption — the merged
            // writer grows while commit continues, instead of every shard coexisting
            // with its merged copy in one final doubling spike.
            let shard_end = ((shard + 1) * shard_size).min(total_sequences);
            if *expected == shard_end
              && let Some((shard_writer, shard_references)) = writers.remove(&shard)
            {
              let _ = done_tx.send((shard, shard_writer, shard_references));
            }
          }
          (writers, parsed, replayed)
        })
      })
      .collect();

    // Extraction workers: scoped, borrowing `work` by reference; per-worker scratch reused
    // across every file the worker touches.
    let worker_handles: Vec<_> = (0..threads)
      .map(|_| {
        let work_rx = work_rx.clone();
        // Each worker owns clones of the committer senders; when the last worker exits, the
        // channels close and committers drain to completion.
        let slot_txs: Vec<crossbeam_channel::Sender<(usize, Slot)>> = slot_txs.clone();
        let work = &work;
        let budget = &budget;
        let abort = &abort;
        let fail = &fail;
        scope.spawn(move || {
          let mut scratch = ExtractScratch::default();
          while let Ok((sequence, entry)) = work_rx.recv() {
            if abort.load(std::sync::atomic::Ordering::Acquire) {
              budget.release(entry.size);
              continue;
            }
            let reserved = entry.size; // released with the same value; clamps match
            let slot = match work(entry, &mut scratch) {
              Ok(StreamWork::Parsed(path, product)) => Slot::Product {
                path,
                product,
                parsed: true,
                reserved,
              },
              Ok(StreamWork::Replayed(path, product)) => Slot::Product {
                path,
                product,
                parsed: false,
                reserved,
              },
              Ok(StreamWork::ReplayedPacked(path)) => Slot::Packed { path, reserved },
              Ok(StreamWork::Skipped) => {
                budget.release(reserved);
                Slot::Skipped
              }
              Err(err) => {
                budget.release(reserved);
                fail(err);
                continue;
              }
            };
            let shard = sequence / shard_size;
            if slot_txs[shard % committers].send((sequence, slot)).is_err() {
              break; // committer gone: only happens on abort/teardown
            }
          }
        })
      })
      .collect();
    drop(work_rx);
    drop(slot_txs);

    // Rolling prefix absorption, on the calling thread: shard k merges as soon as shards
    // 0..k have merged and k is complete — same order and rebases as absorb-at-the-end,
    // bit-identical output (pinned by test), but shard writers die while commit is still
    // running.
    let mut roll_in = |shard, shard_writer, shard_references| {
      holdback.insert(shard, (shard_writer, shard_references));
      while let Some((shard_writer, shard_references)) = holdback.remove(&next_absorb) {
        if let Err(err) = absorb_shard(&mut writer, &mut sink, shard_writer, shard_references)
          && sink_error.is_none()
        {
          sink_error = Some(err);
        }
        next_absorb += 1;
      }
    };

    // Admission, on the calling thread: manifest order, budget-gated.
    for (sequence, entry) in entries.iter().enumerate() {
      while let Ok((shard, shard_writer, shard_references)) = done_rx.try_recv() {
        roll_in(shard, shard_writer, shard_references);
      }
      if abort.load(std::sync::atomic::Ordering::Acquire) {
        break;
      }
      budget.reserve(entry.size.max(1));
      if work_tx.send((sequence, entry)).is_err() {
        break;
      }
    }
    drop(work_tx);
    // Drop the caller's sender so the drain below ends when the committers exit.
    drop(done_tx);
    phase_trace("stream: admission done, draining completions");
    while let Ok((shard, shard_writer, shard_references)) = done_rx.recv() {
      roll_in(shard, shard_writer, shard_references);
    }

    for handle in worker_handles {
      let _ = handle.join();
    }
    // Workers dropped their slot senders on exit; committers drain and finish.
    committer_handles
      .into_iter()
      .map(|handle| handle.join().expect("committer panicked"))
      .collect::<Vec<_>>()
  });

  if let Some(err) = first_error.into_inner().unwrap() {
    return Err(err);
  }

  let (mut parsed, mut replayed) = (0u64, 0u64);
  for (leftover, shard_parsed, shard_replayed) in outputs {
    parsed += shard_parsed;
    replayed += shard_replayed;
    // Leftovers exist only when admission aborted mid-run; fold them in anyway so the
    // (discarded) result is still built deterministically.
    for (shard, writer_and_refs) in leftover {
      holdback.insert(shard, writer_and_refs);
    }
  }
  phase_trace("stream: absorb tail");
  while let Some((shard_writer, shard_references)) = holdback.remove(&next_absorb) {
    if let Err(err) = absorb_shard(&mut writer, &mut sink, shard_writer, shard_references)
      && sink_error.is_none()
    {
      sink_error = Some(err);
    }
    next_absorb += 1;
  }
  if let Some(err) = sink_error {
    return Err(err);
  }
  // The writer has absorbed its last shard: return growth slack before link stacks the
  // table and edge transients on top, and reopen a streamed heap for the link pass's reads.
  writer.shrink_to_fit();
  writer.finalize_streamed_heap()?;
  Ok((
    writer,
    StreamStats {
      parsed,
      replayed,
      peak_in_flight_bytes: budget.peak(),
    },
  ))
}
