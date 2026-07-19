//! `vorpal-index` — a thin CLI over the ingest → resolve → persist → query pipeline (§3.6).
//!
//! `build_index` is incremental (§3.4): a stat manifest decides per file whether its cached
//! extraction product can be replayed or the file must be re-parsed; the graph is always
//! re-linked from the complete product set (so removals/renames cannot leave stale nodes), and
//! an entirely unchanged tree short-circuits to reusing the persisted index outright.

use std::collections::HashSet;
use std::error::Error;
use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs;
use std::path::Path;

use vorpal_ann::{AnnIndex, Embedder, LexicalEmbedder};
use vorpal_ingest::{
  FileProduct, Ingestor, Manifest, OutlineExtractor, Resolver, cache_file_name, load_product,
  save_product,
};
use vorpal_kg::{Kg, NodeId};

/// Summary of an indexing run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexReport {
  /// The tree was unchanged since the last index — reused without re-parsing (§3.4).
  pub reused: bool,
  /// Files re-parsed this run (changed, new, or cache-missing).
  pub indexed: u64,
  /// Files whose cached extraction product was replayed without a parse.
  pub skipped: u64,
  pub nodes: usize,
  pub resolved: u64,
  pub unresolved: u64,
}

/// Ingest `src`, resolve cross-file references, and persist the knowledge graph to `out`.
pub fn build_index(src: &Path, out: &Path) -> Result<IndexReport, Box<dyn Error>> {
  let extractor = OutlineExtractor::new()?;
  let manifest = Manifest::scan(src, |p| extractor.handles(p))?;
  let manifest_path = out.join("manifest.bin");

  // Whole-tree fast path: nothing changed → reuse the persisted index without touching a file.
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

  // Incremental path: replay cached products for stat-unchanged files, re-parse the rest.
  let products_dir = out.join("products");
  fs::create_dir_all(&products_dir)?;
  let prior = Manifest::load(&manifest_path).unwrap_or_default();

  let mut reparsed = 0u64;
  let mut replayed = 0u64;
  let mut products: Vec<(String, FileProduct)> = Vec::new();
  for entry in manifest.entries() {
    let cache = products_dir.join(cache_file_name(&entry.path));
    let cached = if prior.contains(entry) {
      load_product(&cache).ok()
    } else {
      None
    };
    let product = match cached {
      Some(product) => {
        replayed += 1;
        product
      }
      None => {
        // Changed, new, or cache-missing: re-parse (unreadable files are skipped, not fatal).
        let Ok(source) = fs::read_to_string(&entry.path) else {
          continue;
        };
        let Some(product) = extractor.extract_product(&entry.path, &source) else {
          continue;
        };
        save_product(&cache, &product)?;
        reparsed += 1;
        product
      }
    };
    products.push((entry.path.clone(), product));
  }

  // Cache hygiene: drop products of files no longer in the tree.
  let expected: HashSet<OsString> = manifest
    .entries()
    .iter()
    .map(|e| OsString::from(cache_file_name(&e.path)))
    .collect();
  if let Ok(dir) = fs::read_dir(&products_dir) {
    for file in dir.flatten() {
      if !expected.contains(&file.file_name()) {
        let _ = fs::remove_file(file.path());
      }
    }
  }

  // Full re-link from the complete product set: identity, resolution, and edges are recomputed
  // from scratch, so stale state is structurally impossible.
  let mut ingestor = Ingestor::new(extractor);
  for (path, product) in &products {
    ingestor.ingest_product(path, product);
  }
  let (kg, resolve) = ingestor.link_and_seal(&Resolver::new());
  kg.save(out)?;
  build_ann(&kg, out)?;
  manifest.save(&manifest_path)?;
  Ok(IndexReport {
    reused: false,
    indexed: reparsed,
    skipped: replayed,
    nodes: kg.node_count(),
    resolved: resolve.resolved + resolve.ambiguous,
    unresolved: resolve.unresolved,
  })
}

/// Build the semantic tier over every KG node: each definition embeds its name (double-weighted),
/// signature, and file *basename* — never the full path, whose directory tokens are shared junk
/// that drowns the signal — through the pluggable embedder (default: the deterministic lexical
/// hasher) into the adaptive ANN index persisted beside the graph.
fn build_ann(kg: &Kg, out: &Path) -> Result<(), Box<dyn Error>> {
  let embedder = LexicalEmbedder::default();
  let mut rows = Vec::with_capacity(kg.node_count());
  for i in 0..kg.node_count() as u64 {
    let id = NodeId::new(i);
    if let Some(view) = kg.node(id) {
      let basename = view.path.rsplit('/').next().unwrap_or(view.path);
      let text = format!(
        "{} {} {} {}",
        view.name, view.name, view.signature, basename
      );
      rows.push((i, embedder.embed(&text)));
    }
  }
  AnnIndex::build(embedder.dim(), rows, None).save(&out.join("ann.bin"))?;
  Ok(())
}

/// Semantic search over a persisted index: embed the query, search the ANN tier, render the
/// matching nodes with their cosine similarity.
pub fn search_index(index_dir: &Path, query: &str, k: usize) -> Result<String, Box<dyn Error>> {
  let kg = Kg::load(index_dir)?;
  let ann = AnnIndex::load(&index_dir.join("ann.bin"))?;
  let embedder = LexicalEmbedder::default();
  let hits = ann.search(&embedder.embed(query), k);
  let mut out = String::new();
  for (row, dist_sq) in hits {
    if let Some(view) = kg.node(NodeId::new(row)) {
      // On unit vectors: cosine = 1 - d²/2.
      let similarity = 1.0 - dist_sq / 2.0;
      let _ = writeln!(
        out,
        "{similarity:.3}  {} [{:?}] {}",
        view.name, view.kind, view.path
      );
    }
  }
  Ok(out)
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
