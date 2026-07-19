//! The bounded streaming orchestrator, decoupled from how files are parsed (§3.4), with a
//! two-pass linking step that resolves references into `calls`/`references` edges (§3.3).

use std::collections::HashMap;
use std::io;
use std::path::Path;

use vorpal_kg::{Kg, KgWriter, SymbolKind};
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
  pub fn ingest_product(&mut self, path: &str, product: &crate::FileProduct) {
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
  pub fn link_and_seal(mut self, resolver: &Resolver) -> (Kg, ResolveStats) {
    let table = build_symbol_table(&self.writer);
    let (edges, stats) = resolve_all(&table, &self.references, resolver);
    for edge in &edges {
      self.writer.add_edge(edge.from, edge.to, edge.edge);
    }
    (self.writer.seal(), stats)
  }
}

/// Apply one file's product to the writer: ingest its items, then push its references with
/// `from` resolved through the writer's canonical identity (entity path → fresh `NodeId`).
pub(crate) fn apply_product(
  path: &str,
  product: &crate::FileProduct,
  writer: &mut KgWriter,
  references: &mut Vec<Reference>,
) {
  writer.ingest_file(path, &product.items);
  for r in &product.refs {
    if let Some(from) = writer.entity_id(path, &r.from_entity) {
      references.push(
        Reference::new(
          from,
          path,
          r.name.clone(),
          crate::product::tag_refkind(r.kind),
        )
        .with_evidence(r.start, r.end),
      );
    }
  }
}

fn build_symbol_table(writer: &KgWriter) -> SymbolTable {
  let mut table = SymbolTable::new();
  writer.for_each_definition(|id, name, path, kind, exported| {
    if kind == SymbolKind::File {
      // File nodes are the targets of path-form imports (`import "./util"`).
      table.insert_file(path, id);
    } else {
      table.insert(
        name,
        Symbol {
          id,
          kind,
          path: path.to_owned(),
          exported,
        },
      );
    }
  });
  table
}
