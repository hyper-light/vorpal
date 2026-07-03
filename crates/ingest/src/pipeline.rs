//! The bounded streaming orchestrator, decoupled from how files are parsed (§3.4).

use std::collections::HashMap;
use std::io;
use std::path::Path;

use vorpal_kg::{Kg, KgWriter};

/// Turns one file's source into KG nodes/edges via the writer. Implementors own their parse tree
/// locally and ingest within the call, so nothing borrowed from the parse escapes.
pub trait FileExtractor {
  fn extract_into(&self, path: &str, source: &str, writer: &mut KgWriter);
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
/// [`KgWriter`], and seals a queryable [`Kg`].
pub struct Ingestor<E: FileExtractor> {
  extractor: E,
  writer: KgWriter,
  seen: HashMap<String, [u8; 32]>,
  stats: IngestStats,
}

impl<E: FileExtractor> Ingestor<E> {
  pub fn new(extractor: E) -> Self {
    Self {
      extractor,
      writer: KgWriter::new(),
      seen: HashMap::new(),
      stats: IngestStats::default(),
    }
  }

  /// Ingest one in-memory source. Content-hash skip (§3.4): if `path` was last seen with the same
  /// bytes, it is not re-parsed. Returns whether the file was indexed or skipped.
  pub fn ingest_source(&mut self, path: &str, source: &str) -> FileOutcome {
    let hash = *blake3::hash(source.as_bytes()).as_bytes();
    self.stats.bytes += source.len() as u64;
    if self.seen.get(path) == Some(&hash) {
      self.stats.skipped += 1;
      return FileOutcome::Skipped;
    }
    self.seen.insert(path.to_owned(), hash);
    self.extractor.extract_into(path, source, &mut self.writer);
    self.stats.indexed += 1;
    FileOutcome::Indexed
  }

  /// Read a file (bounded by its size — the only transient buffer) and ingest it.
  pub fn ingest_file(&mut self, path: &Path) -> io::Result<FileOutcome> {
    let source = std::fs::read_to_string(path)?;
    Ok(self.ingest_source(&path.to_string_lossy(), &source))
  }

  pub fn stats(&self) -> IngestStats {
    self.stats
  }

  /// Distinct entities interned so far.
  pub fn node_count(&self) -> usize {
    self.writer.node_count()
  }

  /// Seal all accumulated nodes/edges into a queryable knowledge graph.
  pub fn seal(self) -> Kg {
    self.writer.seal()
  }
}
