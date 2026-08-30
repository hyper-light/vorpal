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
  /// The persistent symbol table (SUBSECOND.md Phase 3 — the ~69ms-per-link full rebuild
  /// becomes per-name maintenance). Interner brand erased for storage; rebound per link.
  table: Option<vorpal_resolve::RetainedSymbolTable>,
  /// Names (bits) whose candidate runs must rebuild before the next resolve — every name
  /// defined by an edited file (their row ids moved even when nothing semantic changed).
  table_dirty_names: std::collections::HashSet<u32>,
  /// Edited paths (bits) whose file-node entries must repoint; deleted paths to drop.
  table_dirty_files: std::collections::HashSet<u32>,
  /// Latch: rebuild the table from scratch this link (first-ever-referenced name appeared —
  /// the admission flip — or a change diffing could not cover, or garbage crossed the line).
  table_full: bool,
  /// Garbage candidate slots accumulated by `replace_candidates` repointing.
  table_garbage: usize,
  /// Path bits → path string, for canonical ordering and file-node repoints.
  bits_to_path: HashMap<u32, String>,
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
      table: None,
      table_dirty_names: std::collections::HashSet::new(),
      table_dirty_files: std::collections::HashSet::new(),
      table_full: false,
      table_garbage: 0,
      bits_to_path: HashMap::new(),
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
      table: None,
      table_dirty_names: std::collections::HashSet::new(),
      table_dirty_files: std::collections::HashSet::new(),
      table_full: false,
      table_garbage: 0,
      bits_to_path: HashMap::new(),
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
          self.table_dirty_names.extend(names.iter().copied());
          self.table_dirty_files.insert(bits);
          if had_imports {
            self.escalate_full();
            self.table_full = true;
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
      // Table maintenance inputs: every name this file defines (old and new — the rows'
      // ids moved regardless), its def-postings, and its file-node repoint.
      self.bits_to_path.insert(bits, (*path).to_string());
      self.table_dirty_files.insert(bits);
      for row in &new_rows {
        self.table_dirty_names.insert(row.name_bits);
      }
      if let Some(old_rows) = &old_rows {
        for row in old_rows {
          self.table_dirty_names.insert(row.name_bits);
        }
      }
      match old_rows {
        Some(old_rows) => match self.diff_blocks(&old_rows, &new_rows) {
          Some(dirty) => self.escalate_scoped(dirty, bits),
          None => {
            self.escalate_full();
            self.table_full = true;
          }
        },
        None => {
          // A brand-new file: nobody holds edges into it yet, so only names it defines can
          // change candidate sets — unless it wires imports (aliases affect its own refs,
          // which are re-resolved anyway; conservative on re-exports: escalate).
          let import_tag = crate::SymbolKind::Import.tag();
          if new_rows.iter().any(|row| row.kind_tag == import_tag) {
            self.escalate_full();
            self.table_full = true;
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
  /// [`RetainedIndex::link`] without materializing the evidence sidecar — the daemon's
  /// serve path drops evidence on the floor (generation-bound tools read the committed
  /// sidecar), and cloning + remapping ~7M rows costs ~100ms at kernel scale. The buckets
  /// keep their rows, so nothing is lost for a later persist.
  pub fn link_for_serving(
    &mut self,
    interner: &vorpal_resolve::Interner,
    resolver: &Resolver,
  ) -> io::Result<(Kg, crate::ResolveStats)> {
    let (kg, stats, _) = self.link_inner(interner, resolver, false)?;
    Ok((kg, stats))
  }

  pub fn link(
    &mut self,
    interner: &vorpal_resolve::Interner,
    resolver: &Resolver,
  ) -> io::Result<(Kg, crate::ResolveStats, Vec<vorpal_kg::EvidenceRow>)> {
    self.link_inner(interner, resolver, true)
  }

  fn link_inner(
    &mut self,
    interner: &vorpal_resolve::Interner,
    resolver: &Resolver,
    want_evidence: bool,
  ) -> io::Result<(Kg, crate::ResolveStats, Vec<vorpal_kg::EvidenceRow>)> {
    self.writer.truncate_edges(self.watermark);
    let blocks: Vec<FileBlock> = self.files.values().cloned().collect();
    // Canonical file order for every order-sensitive consumer below: the same path-sorted
    // sequence a from-scratch build processes.
    let order: Vec<u32> = self
      .files
      .keys()
      .map(|path| interner.intern(path).to_bits())
      .collect();
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
    // Persistent-table lifecycle: maintain per-name candidate runs when the edit footprint
    // allows, rebuild from scratch when it does not (first link, admission flip, import
    // wiring, oversized per-name scans, garbage past half the store). Maintenance computes
    // every run and file repoint FIRST (immutable phase), then applies (mutable phase).
    let dirty_names: Vec<u32> = self.table_dirty_names.drain().collect();
    let dirty_files: Vec<u32> = self.table_dirty_files.drain().collect();
    let mut rebuild = self.table.is_none() || self.table_full;
    if !rebuild && !(dirty_names.is_empty() && dirty_files.is_empty()) {
      // Splice maintenance: a dirty name's run keeps every symbol from UNEDITED files
      // (their ids are stable — that is the whole retained-writer design) and swaps only
      // the edited files' contributions, collected from ONE scan of each edited block.
      // O(run length) copies per dirty name; definer counts are irrelevant, so hub names
      // (a static defined in thousands of files) cost a memcpy, not a corpus scan.
      let edited_bits: std::collections::HashSet<u32> = dirty_files.iter().copied().collect();
      // path string → per-name new contributions, canonical (BTreeMap) order.
      let mut contributions: BTreeMap<&str, HashMap<u32, Vec<vorpal_resolve::Symbol<'_>>>> =
        BTreeMap::new();
      for &bits in &dirty_files {
        let Some(path) = self.bits_to_path.get(&bits) else {
          continue;
        };
        let Some(block) = self.files.get(path.as_str()) else {
          continue; // deleted: contributes nothing, its old entries filter out below
        };
        let mut owner_of: HashMap<u32, u32> = HashMap::new();
        let log = self.writer.edge_log();
        for i in block.edges.start as usize..(block.edges.end as usize).min(log.len()) {
          let (src, dst, _) = log.triple(i);
          if self.writer.node_kind(src as usize) != Some(crate::SymbolKind::File) {
            owner_of.insert(dst, src);
          }
        }
        let mut per_name: HashMap<u32, Vec<vorpal_resolve::Symbol<'_>>> = HashMap::new();
        for row in block.rows.clone() {
          let Some((id, name, row_path, kind, exported)) =
            self.writer.definition(row as usize)
          else {
            continue;
          };
          if kind == crate::SymbolKind::File || kind == crate::SymbolKind::Import {
            continue;
          }
          let owner = owner_of.get(&row).and_then(|&src| {
            self.writer.definition(src as usize).map(|(_, owner_name, _, _, _)| {
              interner
                .peek(owner_name)
                .unwrap_or_else(|| crate::pipeline::unmatchable_owner(interner))
            })
          });
          per_name
            .entry(interner.intern(name).to_bits())
            .or_default()
            .push(vorpal_resolve::Symbol {
              id,
              kind,
              path: interner.intern(row_path),
              exported,
              owner,
            });
        }
        contributions.insert(path.as_str(), per_name);
      }
      let repoints: Vec<(u32, Option<vorpal_kg::NodeId>)> = dirty_files
        .iter()
        .map(|&bits| {
          let id = self
            .bits_to_path
            .get(&bits)
            .and_then(|path| self.files.get(path))
            .map(|block| {
              debug_assert_eq!(
                self.writer.node_kind(block.rows.start as usize),
                Some(crate::SymbolKind::File),
                "a block's first row is its file node"
              );
              vorpal_kg::NodeId::new(block.rows.start as u64)
            });
          (bits, id)
        })
        .collect();
      let garbage = &mut self.table_garbage;
      let table = self
        .table
        .as_mut()
        .expect("checked above")
        .borrow_mut(interner);
      for &bits in &dirty_names {
        let Some(name) = interner.id_from_bits(bits) else {
          continue;
        };
        let old_run = table.candidates(name);
        *garbage += old_run.len();
        // Keep unedited files' symbols (canonical order preserved), then insert each edited
        // file's new entries at its canonical position (runs are path-major by invariant).
        let mut merged: Vec<vorpal_resolve::Symbol<'_>> = old_run
          .iter()
          .filter(|sym| !edited_bits.contains(&sym.path.to_bits()))
          .copied()
          .collect();
        for (path, per_name) in &contributions {
          let Some(rows) = per_name.get(&bits) else {
            continue;
          };
          let at = merged.partition_point(|sym| interner.text_of(sym.path) < *path);
          merged.splice(at..at, rows.iter().copied());
        }
        table.replace_candidates(name, &merged);
      }
      for (bits, id) in repoints {
        let Some(path_id) = interner.id_from_bits(bits) else {
          continue;
        };
        match id {
          Some(id) => table.update_file(path_id, id),
          None => table.remove_file(path_id),
        }
      }
      if *garbage * 2 > table.grouped_len() {
        rebuild = true; // tombstone debt: a fresh dense build is cheaper than the waste
      }
    }
    if rebuild {
      let row_ranges: Vec<std::ops::Range<usize>> = blocks
        .iter()
        .map(|b| b.rows.start as usize..b.rows.end as usize)
        .collect();
      vorpal_kg::phase_stamp("retained: table rebuild");
      let fresh = build_symbol_table_over(interner, &self.writer, &row_ranges);
      self.table = Some(vorpal_resolve::RetainedSymbolTable::erase(fresh));
      self.table_full = false;
      self.table_garbage = 0;
    } else {
      vorpal_kg::phase_stamp("retained: table maintained");
    }
    let table = self
      .table
      .as_mut()
      .expect("built or maintained above")
      .borrow_mut(interner);
    let qualified = self.store.qualified_imports(interner, order.iter().copied());
    vorpal_resolve::seed_import_bindings(interner, table, &qualified, resolver);
    let _pump_stats = {
      let resolution = std::cell::RefCell::new(&mut self.resolution);
      let store = &mut self.store;
      vorpal_resolve::resolve_all_store_into(
        interner,
        table,
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
    self.assemble(&blocks, &order, want_evidence)
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
    let repair = &self.repair;
    let chase = |mut row: u32| -> Option<u32> {
      let mut hops = 0;
      while !is_alive[row as usize] {
        row = *repair.get(&row)?;
        hops += 1;
        if hops > 64 {
          return None; // structurally impossible chain — treat as a miss, never spin
        }
      }
      Some(row)
    };
    // Buckets heal independently — fan the scan out (it walks every retained edge; serial
    // it was ~10-15ms of the serve path at kernel scale).
    use rayon::prelude::*;
    let mut untouched: Vec<&mut FileResolution> = self
      .resolution
      .iter_mut()
      .filter_map(|(bits, bucket)| (!dirty.contains(bits)).then_some(bucket))
      .collect();
    let ok = untouched
      .par_iter_mut()
      .map(|bucket| {
        for (from, to, _) in &mut bucket.edges {
          debug_assert!(is_alive[*from as usize], "untouched bucket sources stay alive");
          if !is_alive[*to as usize] {
            match chase(*to) {
              Some(next) => *to = next,
              None => return false,
            }
          }
        }
        for row in &mut bucket.evidence {
          if row.to != vorpal_kg::NO_EDGE && !is_alive[row.to as usize] {
            match chase(row.to) {
              Some(next) => row.to = next,
              None => return false,
            }
          }
          for alt in &mut row.alternatives {
            if !is_alive[*alt as usize] {
              match chase(*alt) {
                Some(next) => *alt = next,
                None => return false,
              }
            }
          }
        }
        true
      })
      .reduce(|| true, |a, b| a && b);
    if ok { Ok(()) } else { Err(()) }
  }

  /// Assemble the sealed graph from the containment blocks + the resolution buckets chained
  /// in canonical file order — the exact emission order a from-scratch resolve produces —
  /// and remap evidence copies through the seal's id LUT. Buckets stay in writer-id space
  /// (they outlive this link once scoped rederive lands).
  fn assemble(
    &mut self,
    blocks: &[FileBlock],
    order: &[u32],
    want_evidence: bool,
  ) -> io::Result<(Kg, crate::ResolveStats, Vec<vorpal_kg::EvidenceRow>)> {
    let mut stats = crate::ResolveStats::default();
    for bits in order {
      if let Some(bucket) = self.resolution.get(bits) {
        stats += bucket.stats;
      }
    }
    let resolution_edges = order.iter().filter_map(|bits| self.resolution.get(bits)).flat_map(|bucket| bucket.edges.iter().copied());
    let (kg, lut) = self.writer.seal_canonical_with(blocks, resolution_edges);
    if !want_evidence {
      return Ok((kg, stats, Vec::new()));
    }
    // Materialize sealed-id evidence copies in parallel per bucket (the saver's canonical
    // total-order sort makes concatenation order irrelevant): ~7M row clones were ~100ms
    // serial — the reason the serve path used to skip evidence entirely.
    use rayon::prelude::*;
    let lut = &lut;
    let per_bucket: Vec<Vec<vorpal_kg::EvidenceRow>> = order
      .par_iter()
      .filter_map(|bits| self.resolution.get(bits))
      .map(|bucket| {
        bucket
          .evidence
          .iter()
          .map(|row| {
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
            row
          })
          .collect()
      })
      .collect();
    let mut evidence: Vec<vorpal_kg::EvidenceRow> =
      Vec::with_capacity(per_bucket.iter().map(Vec::len).sum());
    for mut bucket in per_bucket {
      evidence.append(&mut bucket);
    }
    Ok((kg, stats, evidence))
  }
}
