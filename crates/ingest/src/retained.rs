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

use std::collections::BTreeMap;
use std::io;
use std::path::Path;

use vorpal_kg::{FileBlock, Kg, KgWriter};
use vorpal_resolve::{RefStore, Resolver};

use crate::pipeline::{apply_product_view, build_symbol_table_over};

/// Retained pipeline state: everything an incremental re-link needs, minus the interner.
pub struct RetainedIndex {
  writer: KgWriter,
  store: RefStore,
  /// Path → footprint, iterated in path order — the canonical block order every link uses.
  files: BTreeMap<String, FileBlock>,
  /// Containment watermark: the edge-log length with NO resolution edges appended. Every
  /// apply and every link first truncates back to it, so block edge ranges stay valid and
  /// resolution edges never leak between links.
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
        self.files.remove(path);
        self.store.retract_file(interner.intern(path).to_bits());
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
      self.files.insert((*path).to_string(), block);
      self
        .store
        .append_file(interner.intern(path).to_bits(), references.iter())?;
    }
    Ok(())
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

    let mut evidence: Vec<vorpal_kg::EvidenceRow> =
      Vec::with_capacity(self.store.count() as usize);
    let stats = {
      let writer = &mut self.writer;
      let store = &mut self.store;
      let evidence = std::cell::RefCell::new(&mut evidence);
      vorpal_resolve::resolve_all_store_into(
        interner,
        &table,
        store,
        order.iter().copied(),
        resolver,
        |edge| {
          writer.add_edge(edge.from, edge.to, edge.edge.with_confidence(edge.confidence));
          let (alt_ids, alt_count) = edge.alternatives;
          evidence.borrow_mut().push(vorpal_kg::EvidenceRow {
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
          evidence.borrow_mut().push(vorpal_kg::EvidenceRow {
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

    let (kg, lut) = self.writer.seal_canonical(&blocks, self.watermark);
    // Evidence carries retained-writer ids; the sealed graph carries canonical ids. One
    // remap through the seal's own LUT keeps them in lockstep (NO_EDGE stays sentinel).
    for row in &mut evidence {
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
    }
    // Hygiene: drop this link's resolution edges so the next apply sees containment only.
    self.writer.truncate_edges(self.watermark);
    Ok((kg, stats, evidence))
  }
}
