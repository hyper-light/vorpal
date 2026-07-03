//! `vorpal-index` — a thin CLI over the ingest → resolve → persist → query pipeline (§3.6).
//!
//! `build_index` ingests a directory, resolves cross-file references into `calls` edges, and
//! persists the knowledge graph; the query verbs cold-open it and answer `callers`/`refs`/`node`.

use std::error::Error;
use std::fmt::Write as _;
use std::path::Path;

use vorpal_ingest::{Ingestor, OutlineExtractor, Resolver};
use vorpal_kg::{Kg, NodeId};

/// Summary of an indexing run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexReport {
  pub indexed: u64,
  pub skipped: u64,
  pub nodes: usize,
  pub resolved: u64,
  pub unresolved: u64,
}

/// Ingest `src`, resolve cross-file references, and persist the knowledge graph to `out`.
pub fn build_index(src: &Path, out: &Path) -> Result<IndexReport, Box<dyn Error>> {
  let mut ingestor = Ingestor::new(OutlineExtractor::new()?);
  ingestor.ingest_dir(src)?;
  let ingest = ingestor.stats();
  let (kg, resolve) = ingestor.link_and_seal(&Resolver::new());
  kg.save(out)?;
  Ok(IndexReport {
    indexed: ingest.indexed,
    skipped: ingest.skipped,
    nodes: kg.node_count(),
    resolved: resolve.resolved + resolve.ambiguous,
    unresolved: resolve.unresolved,
  })
}

/// Render nodes as `name [Kind] path` lines.
pub fn format_nodes(kg: &Kg, ids: &[NodeId]) -> String {
  let mut out = String::new();
  for &id in ids {
    if let Some(view) = kg.node(id) {
      let _ = writeln!(out, "{} [{:?}] {}", view.name, view.kind, view.path);
    }
  }
  out
}
