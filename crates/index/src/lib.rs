//! `vorpal-index` — a thin CLI over the ingest → resolve → persist → query pipeline (§3.6).
//!
//! `build_index` ingests a directory, resolves cross-file references into `calls` edges, and
//! persists the knowledge graph; the query verbs cold-open it and answer `callers`/`refs`/`node`.

use std::error::Error;
use std::fmt::Write as _;
use std::path::Path;

use vorpal_ingest::{Ingestor, Manifest, OutlineExtractor, Resolver};
use vorpal_kg::{Kg, NodeId};

/// Summary of an indexing run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexReport {
  /// The tree was unchanged since the last index — reused without re-parsing (§3.4).
  pub reused: bool,
  pub indexed: u64,
  pub skipped: u64,
  pub nodes: usize,
  pub resolved: u64,
  pub unresolved: u64,
}

/// Ingest `src`, resolve cross-file references, and persist the knowledge graph to `out`.
///
/// Near-instant re-index (§3.4): a persisted stat-manifest is compared first; if the tree is
/// unchanged, the existing index is reused without reading or parsing any file.
pub fn build_index(src: &Path, out: &Path) -> Result<IndexReport, Box<dyn Error>> {
  let extractor = OutlineExtractor::new()?;
  let manifest = Manifest::scan(src, |p| extractor.handles(p))?;
  let manifest_path = out.join("manifest.bin");

  if out.join("nodes.vseg").exists() {
    if let Ok(prior) = Manifest::load(&manifest_path) {
      if manifest.unchanged_since(&prior) {
        let kg = Kg::load(out)?;
        return Ok(IndexReport {
          reused: true,
          indexed: 0,
          skipped: manifest.len() as u64,
          nodes: kg.node_count(),
          resolved: 0,
          unresolved: 0,
        });
      }
    }
  }

  let mut ingestor = Ingestor::new(extractor);
  ingestor.ingest_dir(src)?;
  let ingest = ingestor.stats();
  let (kg, resolve) = ingestor.link_and_seal(&Resolver::new());
  kg.save(out)?;
  manifest.save(&manifest_path)?;
  Ok(IndexReport {
    reused: false,
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
