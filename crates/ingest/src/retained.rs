//! The retained ingest state behind the memory-primary daemon (SUBSECOND.md Phase 3).
//!
//! A [`RetainedIndex`] keeps the post-absorb, pre-link pipeline state alive between edits:
//! the long-lived [`KgWriter`] (rows, heap, containment edges), the per-file-retirable
//! [`RefStore`], and a canonical (path-sorted) registry of each file's [`FileBlock`]
//! footprint. An edit retracts one file (tombstone by registry/range removal) and re-applies
//! its product at the writer tail; a link then rebuilds ONLY the derived state — masked
//! symbol table over alive blocks, full resolution over alive references, canonical-order
//! seal — skipping the 72k-file product replay a fresh pipeline pays.
//!
//! Determinism: the sealed graph is byte-identical to a from-scratch build of the same live
//! tree (crates/kg/tests/canonical_seal.rs), the masked table inserts symbols in the exact
//! canonical order a scratch table does, and evidence rows are remapped through the same
//! id LUT as the edges — so answers, ids, and eids all match scratch, and the daemon's
//! background canonicalizer converges to the identical generation.
//!
//! Everything here is lifetime-free: the interner is BORROWED per call, never stored, so a
//! daemon owns `(Interner, RetainedIndex)` side by side without self-reference.

use std::collections::{BTreeMap, HashMap};
use std::io;
use std::path::Path;

use vorpal_kg::{EdgeType, FileBlock, Kg, KgWriter};
use vorpal_resolve::{Confidence, RefStore, Resolver};

use crate::pipeline::{apply_product_view, build_symbol_table_over};

/// One file's resolution outcome, in retained-writer id space: its emitted edges (in
/// emission order — the canonical-order feed makes that the from-scratch order), its
/// evidence rows, and its share of the stats. Bucketing by source file is what makes
/// scoped rederive possible: an edit re-resolves only the dirty buckets.
#[derive(Default)]
struct FileResolution {
  edges: Vec<(u32, u32, EdgeType)>,
  evidence: Vec<vorpal_kg::EvidenceRow>,
  stats: crate::ResolveStats,
}

/// One definition row's dirty-set summary. `owner_eid` is the owner's durable eid (None for
/// top-level rows): owner identity feeds qualified-reference resolution, so an entity moving
/// between owners must dirty its name even though its own eid, name, kind, and export bit
/// all survive.
struct RowSummary {
  eid: (u64, u64),
  name_bits: u32,
  kind_tag: u8,
  exported: bool,
  owner_eid: Option<(u64, u64)>,
  row: u32,
}

/// What the applies since the last link demand of the next one (a lattice: Clean ⊑ Scoped ⊑
/// Full — every apply can only move it up).
enum PendingScope {
  /// Nothing applied: assemble straight from the existing buckets.
  Clean,
  /// Re-resolve only the dirty files: the edited files themselves plus (via the postings)
  /// every file referencing a name whose candidate set changed.
  Scoped {
    dirty_names: std::collections::HashSet<u32>,
    dirty_files: std::collections::HashSet<u32>,
  },
  /// Something scoped reasoning does not cover (import wiring changed, or the state
  /// predates the first link): recompute every bucket.
  Full,
}

/// Retained pipeline state: everything an incremental re-link needs, minus the interner.
pub struct RetainedIndex {
  writer: KgWriter,
  store: RefStore,
  /// Path → footprint, iterated in path order — the canonical block order every link uses.
  files: BTreeMap<String, FileBlock>,
  /// Path bits → that file's resolution bucket (see [`FileResolution`]).
  resolution: HashMap<u32, FileResolution>,
  /// Name bits → files whose references mention that name (resolved or not). Stale entries
  /// are tolerated (they only overapproximate the dirty set); rebuilt by every full link's
  /// applies... entries accrue at apply time and reset with the overlay.
  postings: HashMap<u32, Vec<u32>>,
  /// Dead row → its eid-identical successor row. An edited file's unchanged entities keep
  /// their durable eid, so edges into their old rows heal by lookup instead of forcing the
  /// referencing file to re-resolve. Values may themselves die later — lookups chase.
  repair: HashMap<u32, u32>,
  pending: PendingScope,
  /// Containment watermark: the edge-log length with NO resolution edges appended. The edge
  /// log holds containment ONLY between links (resolution lives in the buckets), so this
  /// tracks the log length after the latest apply.
  watermark: usize,
}

impl RetainedIndex {
  /// Build from an iterator of `(path, encoded product bytes)` in ANY order (the registry
  /// path-sorts). The daemon feeds the committed generation's pack entries; cost is one
  /// replay, paid once in the background — never on a query.
  pub fn build<'a>(
    interner: &vorpal_resolve::Interner,
    store_path: &Path,
    products: impl Iterator<Item = (&'a str, &'a [u8])>,
  ) -> io::Result<Self> {
    let mut retained = Self {
      writer: KgWriter::new(),
      store: RefStore::create(store_path)?,
      files: BTreeMap::new(),
      resolution: HashMap::new(),
      postings: HashMap::new(),
      repair: HashMap::new(),
      pending: PendingScope::Full,
      watermark: 0,
    };
    for (path, bytes) in products {
      retained.apply_product_bytes(interner, path, bytes)?;
    }
    Ok(retained)
  }

  /// An empty retained state — the daemon's builder applies the generation's products one
  /// by one (pack or loose, in manifest order) through [`RetainedIndex::apply_file`].
  pub fn empty(store_path: &Path) -> io::Result<Self> {
    Ok(Self {
      writer: KgWriter::new(),
      store: RefStore::create(store_path)?,
      files: BTreeMap::new(),
      resolution: HashMap::new(),
      postings: HashMap::new(),
      repair: HashMap::new(),
      pending: PendingScope::Full,
      watermark: 0,
    })
  }

  /// Whether `path` currently has a live footprint.
  pub fn contains(&self, path: &str) -> bool {
    self.files.contains_key(path)
  }

  /// Apply one edit: `Some(bytes)` (re)ingests the file's encoded product at the writer
  /// tail after retiring its previous footprint; `None` retires it outright (delete).
  pub fn apply_file(
    &mut self,
    interner: &vorpal_resolve::Interner,
    path: &str,
    product_bytes: Option<&[u8]>,
  ) -> io::Result<()> {
    match product_bytes {
      Some(bytes) => self.apply_product_bytes(interner, path, bytes),
      None => {
        let bits = interner.intern(path).to_bits();
        if let Some(block) = self.files.get(path).cloned() {
          // Every name this file defined loses a candidate; no successors exist, so edges
          // into these rows are unrepairable — exactly why their referencers re-resolve.
          let names: Vec<u32> = self
            .block_rows(interner, &block)
            .iter()
            .map(|row| row.name_bits)
            .collect();
          let import_tag = crate::SymbolKind::Import.tag();
          let had_imports = block
            .rows
            .clone()
            .any(|row| self.writer.node_kind(row as usize).map(crate::SymbolKind::tag) == Some(import_tag));
          if had_imports {
            self.escalate_full();
          } else {
            self.escalate_scoped(names, bits);
          }
        }
        self.files.remove(path);
        self.store.retract_file(bits);
        self.resolution.remove(&bits);
        Ok(())
      }
    }
  }

  /// Apply a batch of files with PARALLEL decode+ingest (per-file writers) and a serial,
  /// batch-order absorb — the overlay builder's path (72k products would take minutes
  /// serially; decode dominates and parallelizes perfectly). Byte-equivalent to serial
  /// [`RetainedIndex::apply_file`] calls in the same order: `absorb` reproduces the exact
  /// id assignment a single serial writer produces.
  pub fn apply_files_parallel(
    &mut self,
    interner: &vorpal_resolve::Interner,
    batch: &[(&str, &[u8])],
  ) -> io::Result<()> {
    use rayon::prelude::*;
    let parts: Vec<io::Result<(KgWriter, Vec<vorpal_resolve::Reference<'_>>)>> = batch
      .par_iter()
      .map(|(path, bytes)| {
        let view = crate::product::decode_product_view(bytes)?;
        let mut writer = KgWriter::new();
        let mut references = Vec::with_capacity(view.refs.len());
        apply_product_view(interner, path, &view, &mut writer, &mut references);
        Ok((writer, references))
      })
      .collect();
    self.writer.truncate_edges(self.watermark);
    for ((path, _), part) in batch.iter().zip(parts) {
      let (file_writer, mut references) = part?;
      let bits = interner.intern(path).to_bits();
      let old_rows = self
        .files
        .get(*path)
        .map(|block| self.block_rows(interner, &block.clone()));
      let rows_start = self.writer.node_count() as u32;
      let heap_start = self.writer.heap_len();
      let edges_start = self.writer.edges_len() as u32;
      let id_base = self.writer.absorb(file_writer);
      for reference in &mut references {
        reference.from = vorpal_kg::NodeId::new(reference.from.raw() + id_base);
      }
      let block = FileBlock {
        rows: rows_start..self.writer.node_count() as u32,
        heap: heap_start..self.writer.heap_len(),
        edges: edges_start..self.writer.edges_len() as u32,
      };
      self.watermark = self.writer.edges_len();
      // Dirty-set reasoning (scoped rederive): diff the definition rows by durable eid.
      let new_rows = self.block_rows(interner, &block);
      match old_rows {
        Some(old_rows) => match self.diff_blocks(&old_rows, &new_rows) {
          Some(dirty) => self.escalate_scoped(dirty, bits),
          None => self.escalate_full(),
        },
        None => {
          // A brand-new file: nobody holds edges into it yet, so only names it defines can
          // change candidate sets — unless it wires imports (aliases affect its own refs,
          // which are re-resolved anyway; conservative on re-exports: escalate).
          let import_tag = crate::SymbolKind::Import.tag();
          if new_rows.iter().any(|row| row.kind_tag == import_tag) {
            self.escalate_full();
          } else {
            let names: Vec<u32> = new_rows.iter().map(|row| row.name_bits).collect();
            self.escalate_scoped(names, bits);
          }
        }
      }
      // Reference postings for the dirty expansion; per-file dedup keeps rows bounded.
      let mut seen: std::collections::HashSet<u32> = std::collections::HashSet::new();
      for reference in &references {
        if seen.insert(reference.name.to_bits()) {
          self
            .postings
            .entry(reference.name.to_bits())
            .or_default()
            .push(bits);
        }
      }
      self.files.insert((*path).to_string(), block);
      self.resolution.remove(&bits);
      self.store.append_file(bits, references.iter())?;
    }
    Ok(())
  }

  fn block_rows(
    &self,
    interner: &vorpal_resolve::Interner,
    block: &FileBlock,
  ) -> Vec<RowSummary> {
    // Containment is per-block: owners come from this block's own edge range.
    let mut owner_of: HashMap<u32, u32> = HashMap::new();
    let log = self.writer.edge_log();
    for i in block.edges.start as usize..(block.edges.end as usize).min(log.len()) {
      let (src, dst, _) = log.triple(i);
      if self.writer.node_kind(src as usize) != Some(crate::SymbolKind::File) {
        owner_of.insert(dst, src);
      }
    }
    let mut out = Vec::with_capacity(block.rows.len());
    for row in block.rows.clone() {
      let Some((_, name, _, kind, exported)) = self.writer.definition(row as usize) else {
        continue;
      };
      let Some(eid) = self.writer.node_eid(row as usize) else {
        continue;
      };
      let owner_eid = owner_of
        .get(&row)
        .and_then(|&src| self.writer.node_eid(src as usize));
      out.push(RowSummary {
        eid,
        name_bits: interner.intern(name).to_bits(),
        kind_tag: kind.tag(),
        exported,
        owner_eid,
        row,
      });
    }
    out
  }

  fn escalate_full(&mut self) {
    self.pending = PendingScope::Full;
  }

  fn escalate_scoped(&mut self, names: impl IntoIterator<Item = u32>, file: u32) {
    match &mut self.pending {
      PendingScope::Full => {}
      PendingScope::Scoped {
        dirty_names,
        dirty_files,
      } => {
        dirty_names.extend(names);
        dirty_files.insert(file);
      }
      PendingScope::Clean => {
        self.pending = PendingScope::Scoped {
          dirty_names: names.into_iter().collect(),
          dirty_files: [file].into_iter().collect(),
        };
      }
    }
  }

  /// Diff an edited file's old and new definition rows: record eid-identical successors in
  /// the repair map, and return the names whose CANDIDATE SET changed (added, removed, or
  /// kind/export-flipped entities). `None` means scoped reasoning cannot cover this edit
  /// (import wiring changed) and the caller must escalate to Full.
  fn diff_blocks(&mut self, old_rows: &[RowSummary], new_rows: &[RowSummary]) -> Option<Vec<u32>> {
    let import_tag = crate::SymbolKind::Import.tag();
    let mut dirty = Vec::new();
    let mut new_by_eid: HashMap<(u64, u64), &RowSummary> = HashMap::with_capacity(new_rows.len());
    for row in new_rows {
      // Duplicate eids (re-defined identity in one file) are collapsed by the writer's
      // per-file dedup; last wins here to stay in lockstep.
      new_by_eid.insert(row.eid, row);
    }
    for old in old_rows {
      match new_by_eid.remove(&old.eid) {
        Some(new) => {
          // An import row surviving with identical shape only shifts ids (repairable);
          // any other import change moves this file's binding targets out of scoped reach.
          if (old.kind_tag == import_tag || new.kind_tag == import_tag)
            && (old.kind_tag != new.kind_tag || old.name_bits != new.name_bits)
          {
            return None;
          }
          self.repair.insert(old.row, new.row);
          if old.name_bits != new.name_bits
            || old.kind_tag != new.kind_tag
            || old.exported != new.exported
            || old.owner_eid != new.owner_eid
          {
            dirty.push(old.name_bits);
            dirty.push(new.name_bits);
          }
        }
        None => {
          if old.kind_tag == import_tag {
            return None;
          }
          dirty.push(old.name_bits); // entity removed: its candidate set shrank
        }
      }
    }
    for (_, new) in new_by_eid {
      if new.kind_tag == import_tag {
        return None; // a NEW import alias changes this file's binding targets
      }
      dirty.push(new.name_bits); // entity added: its candidate set grew
    }
    Some(dirty)
  }

  /// Every apply is absorb-based (a batch of one): the retained writer's own canonical
  /// index is NEVER advanced (absorb bypasses it by design), so defining directly into the
  /// retained writer would hand out node ids from 0 while pushing rows at the tail —
  /// silent id/row misalignment. One code path, one invariant.
  fn apply_product_bytes(
    &mut self,
    interner: &vorpal_resolve::Interner,
    path: &str,
    bytes: &[u8],
  ) -> io::Result<()> {
    self.apply_files_parallel(interner, &[(path, bytes)])
  }

  /// Alive files currently retained.
  pub fn file_count(&self) -> usize {
    self.files.len()
  }

  /// Dead fraction of the writer's rows — the caller's compaction (full-rebuild) trigger.
  pub fn dead_row_fraction(&self) -> f64 {
    let total = self.writer.node_count();
    if total == 0 {
      return 0.0;
    }
    let alive: usize = self.files.values().map(|b| b.rows.len()).sum();
    1.0 - alive as f64 / total as f64
  }

  /// Re-link the retained state and seal in canonical order: masked table build over alive
  /// blocks, import-binding pre-pass from the store's alive qualified imports, full
  /// resolution over alive references, canonical-order seal. Returns the sealed graph
  /// (byte-identical to a scratch build of the live tree), resolution stats, and evidence
  /// rows already remapped into sealed-id space.
  pub fn link(
    &mut self,
    interner: &vorpal_resolve::Interner,
    resolver: &Resolver,
  ) -> io::Result<(Kg, crate::ResolveStats, Vec<vorpal_kg::EvidenceRow>)> {
    self.writer.truncate_edges(self.watermark);
    let blocks: Vec<FileBlock> = self.files.values().cloned().collect();
    let row_ranges: Vec<std::ops::Range<usize>> = blocks
      .iter()
      .map(|b| b.rows.start as usize..b.rows.end as usize)
      .collect();
    let mut table = build_symbol_table_over(interner, &self.writer, &row_ranges);
    // Canonical file order for every order-sensitive consumer below: the same path-sorted
    // sequence a from-scratch build processes.
    let order: Vec<u32> = self
      .files
      .keys()
      .map(|path| interner.intern(path).to_bits())
      .collect();
    let qualified = self.store.qualified_imports(interner, order.iter().copied());
    vorpal_resolve::seed_import_bindings(interner, &mut table, &qualified, resolver);

    // Scope decision (SUBSECOND.md dirty-bucket rederive): the applies since the last link
    // recorded which candidate sets changed; expand through the reference postings to every
    // file whose resolution could differ, and re-resolve ONLY those buckets. Everything
    // else keeps its bucket, healed by the eid repair map where the edited file's unchanged
    // entities shifted ids. Past ~a quarter of the corpus the full streaming feed's
    // efficiency beats per-file scoping (measured shape, not a tuned constant: scoped pays
    // a repair scan over every retained edge either way) — and any repair miss (a dirty
    // set the reasoning under-approximated) falls back to a full re-resolve LOUDLY.
    let pending = std::mem::replace(&mut self.pending, PendingScope::Clean);
    let alive: std::collections::HashSet<u32> = order.iter().copied().collect();
    let mut scope: Option<std::collections::HashSet<u32>> = match pending {
      PendingScope::Clean => Some(std::collections::HashSet::new()),
      PendingScope::Full => None,
      PendingScope::Scoped {
        dirty_names,
        dirty_files,
      } => {
        let mut files = dirty_files;
        for name in &dirty_names {
          if let Some(referencers) = self.postings.get(name) {
            files.extend(referencers.iter().copied());
          }
        }
        files.retain(|bits| alive.contains(bits));
        if files.len() > 64 && files.len() * 4 > self.files.len() {
          None
        } else {
          Some(files)
        }
      }
    };
    if let Some(files) = &scope
      && !files.is_empty()
      && self.repair_buckets(&blocks, files).is_err()
    {
      // A bucket held an edge into a dead row with no successor outside the dirty set:
      // the scoped reasoning missed a dependency. Recompute everything — correctness is
      // never negotiable, scoping only ever an optimization.
      vorpal_kg::phase_stamp("retained: repair miss — full re-resolve");
      scope = None;
    }
    match &scope {
      None => {
        vorpal_kg::phase_stamp("retained: full link");
        self.resolution.clear();
      }
      Some(files) => {
        vorpal_kg::phase_stamp(&format!("retained: scoped link ({} dirty files)", files.len()));
        for bits in files {
          self.resolution.remove(bits);
        }
      }
    }
    let feed: Vec<u32> = match &scope {
      None => order.clone(),
      Some(files) => order.iter().copied().filter(|bits| files.contains(bits)).collect(),
    };
    if scope.is_none() {
      self.repair.clear();
    }
    let _pump_stats = {
      let resolution = std::cell::RefCell::new(&mut self.resolution);
      let store = &mut self.store;
      vorpal_resolve::resolve_all_store_into(
        interner,
        &table,
        store,
        feed.iter().copied(),
        resolver,
        |edge| {
          let mut resolution = resolution.borrow_mut();
          let bucket = resolution.entry(edge.from_path_bits).or_default();
          bucket
            .edges
            .push((edge.from.raw() as u32, edge.to.raw() as u32, edge.edge.with_confidence(edge.confidence)));
          if Confidence(edge.confidence) <= Confidence::AMBIGUOUS {
            bucket.stats.ambiguous += 1;
          } else {
            bucket.stats.resolved += 1;
          }
          let (alt_ids, alt_count) = edge.alternatives;
          bucket.evidence.push(vorpal_kg::EvidenceRow {
            from: edge.from.raw() as u32,
            to: edge.to.raw() as u32,
            name_hash: edge.name_hash,
            etype: edge.edge.base().0,
            reason: edge.reason as u8,
            confidence: edge.confidence,
            outcome: vorpal_kg::EvidenceOutcome::Edge,
            candidates: edge.candidates,
            span_start: edge.span.0,
            span_end: edge.span.1,
            alternatives: alt_ids[..alt_count as usize].to_vec(),
          });
        },
        |unresolved| {
          let mut resolution = resolution.borrow_mut();
          let bucket = resolution.entry(unresolved.from_path_bits).or_default();
          if unresolved.external {
            bucket.stats.external += 1;
          } else {
            bucket.stats.masked += 1;
          }
          bucket.evidence.push(vorpal_kg::EvidenceRow {
            from: unresolved.from.raw() as u32,
            to: vorpal_kg::NO_EDGE,
            name_hash: unresolved.name_hash,
            etype: unresolved.etype.base().0,
            reason: 0,
            confidence: 0,
            outcome: if unresolved.external {
              vorpal_kg::EvidenceOutcome::External
            } else {
              vorpal_kg::EvidenceOutcome::Masked
            },
            candidates: unresolved.candidates,
            span_start: unresolved.span.0,
            span_end: unresolved.span.1,
            alternatives: Vec::new(),
          });
        },
      )?
    };
    drop(table);
    self.assemble(&blocks, &order)
  }

  /// Heal untouched buckets in place after an edit: edges (and evidence targets) pointing
  /// at rows the edit retired follow the eid repair map to their successors. A dead target
  /// with no successor is only legal in a bucket the dirty set will re-resolve — anywhere
  /// else it means the dirty reasoning under-approximated, and the caller must recompute in
  /// full. `dirty` lists the buckets about to be re-resolved (skipped here).
  fn repair_buckets(
    &mut self,
    blocks: &[FileBlock],
    dirty: &std::collections::HashSet<u32>,
  ) -> Result<(), ()> {
    let mut is_alive = vec![false; self.writer.node_count()];
    for block in blocks {
      for row in block.rows.clone() {
        is_alive[row as usize] = true;
      }
    }
    let chase = |mut row: u32| -> Option<u32> {
      let mut hops = 0;
      while !is_alive[row as usize] {
        row = *self.repair.get(&row)?;
        hops += 1;
        if hops > 64 {
          return None; // structurally impossible chain — treat as a miss, never spin
        }
      }
      Some(row)
    };
    for (bits, bucket) in &mut self.resolution {
      if dirty.contains(bits) {
        continue;
      }
      for (from, to, _) in &mut bucket.edges {
        debug_assert!(is_alive[*from as usize], "untouched bucket sources stay alive");
        if !is_alive[*to as usize] {
          match chase(*to) {
            Some(next) => *to = next,
            None => return Err(()),
          }
        }
      }
      for row in &mut bucket.evidence {
        if row.to != vorpal_kg::NO_EDGE && !is_alive[row.to as usize] {
          match chase(row.to) {
            Some(next) => row.to = next,
            None => return Err(()),
          }
        }
        for alt in &mut row.alternatives {
          if !is_alive[*alt as usize] {
            match chase(*alt) {
              Some(next) => *alt = next,
              None => return Err(()),
            }
          }
        }
      }
    }
    Ok(())
  }

  /// Assemble the sealed graph from the containment blocks + the resolution buckets chained
  /// in canonical file order — the exact emission order a from-scratch resolve produces —
  /// and remap evidence copies through the seal's id LUT. Buckets stay in writer-id space
  /// (they outlive this link once scoped rederive lands).
  fn assemble(
    &mut self,
    blocks: &[FileBlock],
    order: &[u32],
  ) -> io::Result<(Kg, crate::ResolveStats, Vec<vorpal_kg::EvidenceRow>)> {
    let mut stats = crate::ResolveStats::default();
    for bits in order {
      if let Some(bucket) = self.resolution.get(bits) {
        stats += bucket.stats;
      }
    }
    let resolution_edges = order.iter().filter_map(|bits| self.resolution.get(bits)).flat_map(|bucket| bucket.edges.iter().copied());
    let (kg, lut) = self.writer.seal_canonical_with(blocks, resolution_edges);
    let mut evidence: Vec<vorpal_kg::EvidenceRow> =
      Vec::with_capacity(order.iter().filter_map(|bits| self.resolution.get(bits)).map(|b| b.evidence.len()).sum());
    for bits in order {
      let Some(bucket) = self.resolution.get(bits) else {
        continue;
      };
      for row in &bucket.evidence {
        let mut row = row.clone();
        debug_assert_ne!(lut[row.from as usize], u32::MAX, "evidence from a dead row");
        row.from = lut[row.from as usize];
        if row.to != vorpal_kg::NO_EDGE {
          debug_assert_ne!(lut[row.to as usize], u32::MAX, "evidence to a dead row");
          row.to = lut[row.to as usize];
        }
        for alt in &mut row.alternatives {
          debug_assert_ne!(lut[*alt as usize], u32::MAX, "alternative is a dead row");
          *alt = lut[*alt as usize];
        }
        evidence.push(row);
      }
    }
    Ok((kg, stats, evidence))
  }
}
