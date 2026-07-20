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
    link_writer(self.writer, &self.references, resolver)
  }
}

/// The global linking tail shared by [`Ingestor::link_and_seal`] and the sharded commit path:
/// symbol table from the writer's definitions, resolve every reference, inject the resolved
/// edges, seal.
pub fn link_writer(
  mut writer: KgWriter,
  references: &[Reference],
  resolver: &Resolver,
) -> (Kg, ResolveStats) {
  let table = build_symbol_table(&writer);
  let (edges, stats) = resolve_all(&table, references, resolver);
  for edge in &edges {
    writer.add_edge(edge.from, edge.to, edge.edge);
  }
  (writer.seal(), stats)
}

/// Fewer files than this per shard and the fan-out overhead outweighs the win: small trees
/// take the single-writer path outright.
const MIN_FILES_PER_SHARD: usize = 16;

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
  writer.ingest_file(path, &items);
  // References carry entity *indices* into the file's local layout; resolve them through the
  // recomputed layout (an out-of-range index — a corrupt product — simply drops the ref).
  let (entities, _spans) = crate::outline_extractor::local_layout(&items);
  // One shared path per file; every reference clones the Arc, not the string.
  let shared_path: std::sync::Arc<str> = std::sync::Arc::from(path);
  for r in refs {
    let Some(entity) = entities.get(r.from_entity_index as usize) else {
      continue;
    };
    if let Some(from) = writer.entity_id(path, entity) {
      references.push(
        Reference::new(
          from,
          std::sync::Arc::clone(&shared_path),
          r.name,
          crate::product::tag_refkind(r.kind),
        )
          .with_evidence(r.start, r.end)
          .with_qualifier(r.qualifier)
          .with_form(crate::product::tag_refform(r.form)),
      );
    }
  }
}

/// Below this many definitions the table builds serially — fan-out costs more than it saves.
const MIN_DEFS_PER_SHARD: usize = 4096;

fn build_symbol_table(writer: &KgWriter) -> SymbolTable {
  // Derive each member's owner row from the containment edges (`Kg` for `Kg.load`) — the
  // target side of qualified-reference matching. Containment from a File node is not
  // ownership: top-level items match by module file instead. One cheap serial pass.
  let node_count = writer.node_count();
  let mut owner_of: Vec<Option<u32>> = vec![None; node_count];
  for (src, dst, etype) in writer.edge_log().iter() {
    let containment =
      etype == EdgeType::DEFINES || etype == EdgeType::HAS_METHOD || etype == EdgeType::HAS_FIELD;
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
    for row in range {
      let (id, name, path, kind, exported) = writer.definition(row).expect("row < node_count");
      if kind == SymbolKind::File {
        // File nodes are the targets of path-form imports (`import "./util"`).
        table.insert_file(path, id);
      } else if kind != SymbolKind::Import {
        // Import/alias nodes are wiring, not definitions: offering them as resolution targets
        // let a `use foo` in one file steal call edges meant for the real `foo`.
        let owner = owner_of[row]
          .and_then(|src| writer.definition(src as usize))
          .map(|(_, owner_name, _, _, _)| owner_name.to_owned());
        table.insert(
          name,
          Symbol {
            id,
            kind,
            path: path.to_owned(),
            exported,
            owner,
          },
        );
      }
    }
    table
  };

  if node_count <= MIN_DEFS_PER_SHARD {
    return insert_range(0..node_count);
  }
  use rayon::prelude::*;
  let threads = rayon::current_num_threads().max(1);
  let shard_size = node_count.div_ceil(threads * 2).max(MIN_DEFS_PER_SHARD);
  let starts: Vec<usize> = (0..node_count).step_by(shard_size).collect();
  let shards: Vec<SymbolTable> = starts
    .par_iter()
    .map(|&start| insert_range(start..(start + shard_size).min(node_count)))
    .collect();
  let mut table = SymbolTable::new();
  for shard in shards {
    table.absorb(shard);
  }
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
      let containment =
        etype == EdgeType::DEFINES || etype == EdgeType::HAS_METHOD || etype == EdgeType::HAS_FIELD;
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
        let owner = owner_of[id.raw() as usize].map(|src| names[src as usize].clone());
        table.insert(
          name,
          Symbol {
            id,
            kind,
            path: path.to_owned(),
            exported,
            owner,
          },
        );
      }
    });
    table
  }

  #[test]
  fn sharded_table_build_equals_the_serial_specification() {
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
  /// Read `path` into the reused source buffer (replacing its contents), UTF-8-validated
  /// exactly like `fs::read_to_string`.
  pub fn read_source(&mut self, path: &Path) -> io::Result<&str> {
    use std::io::Read;
    self.source.clear();
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
/// absorbed in shard order at the end. Pinned byte-for-byte by test, including under a
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
  let threads = std::thread::available_parallelism()
    .map(|n| n.get())
    .unwrap_or(1);
  let shard_size = entries
    .len()
    .div_ceil((threads * 2).max(1))
    .max(MIN_FILES_PER_SHARD);

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
        StreamWork::Skipped => {}
      }
    }
    return Ok((
      writer,
      references,
      StreamStats {
        parsed,
        replayed,
        peak_in_flight_bytes: 0,
      },
    ));
  }

  let num_shards = entries.len().div_ceil(shard_size);
  let committers = num_shards.min((threads / 4).max(1));
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
                Slot::Skipped => {}
              }
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

    // Admission, on the calling thread: manifest order, budget-gated.
    for (sequence, entry) in entries.iter().enumerate() {
      if abort.load(std::sync::atomic::Ordering::Acquire) {
        break;
      }
      budget.reserve(entry.size.max(1));
      if work_tx.send((sequence, entry)).is_err() {
        break;
      }
    }
    drop(work_tx);

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

  // Ordered absorption, exactly as the batch path: shard 0..n, id and reference rebase.
  let mut by_shard: std::collections::BTreeMap<usize, (KgWriter, Vec<Reference>)> =
    std::collections::BTreeMap::new();
  let (mut parsed, mut replayed) = (0u64, 0u64);
  for (writers, shard_parsed, shard_replayed) in outputs {
    parsed += shard_parsed;
    replayed += shard_replayed;
    for (shard, writer_and_refs) in writers {
      by_shard.insert(shard, writer_and_refs);
    }
  }
  let mut writer = KgWriter::new();
  let mut references = Vec::new();
  for (_, (shard_writer, shard_references)) in by_shard {
    let id_base = writer.absorb(shard_writer);
    references.extend(shard_references.into_iter().map(|mut reference| {
      reference.from = NodeId::new(reference.from.raw() + id_base);
      reference
    }));
  }
  Ok((
    writer,
    references,
    StreamStats {
      parsed,
      replayed,
      peak_in_flight_bytes: budget.peak(),
    },
  ))
}
