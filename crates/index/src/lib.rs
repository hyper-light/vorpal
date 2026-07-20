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

use vorpal_ann::{AnnIndex, Embedder, LexicalEmbedder, tokenize};
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

/// Reciprocal Rank Fusion constant (the standard K=60): dampens the head of each list so no
/// single signal dominates, while rank-1 placements still carry the most weight.
const RRF_K: f32 = 60.0;

/// Fuse ranked candidate lists by RRF: `score(d) = Σ 1/(K + rank_in_list)`. Ties break by id for
/// determinism. Lists may overlap and have different lengths; absence from a list adds nothing.
fn rrf_fuse(lists: &[Vec<u64>], k: usize) -> Vec<(u64, f32)> {
  let mut scores: std::collections::HashMap<u64, f32> = std::collections::HashMap::new();
  for list in lists {
    for (rank, &id) in list.iter().enumerate() {
      *scores.entry(id).or_insert(0.0) += 1.0 / (RRF_K + rank as f32);
    }
  }
  let mut ranked: Vec<(u64, f32)> = scores.into_iter().collect();
  ranked.sort_by(|a, b| {
    b.1
      .partial_cmp(&a.1)
      .unwrap_or(std::cmp::Ordering::Equal)
      .then(a.0.cmp(&b.0))
  });
  ranked.truncate(k);
  ranked
}

/// Hybrid search (§3.5): three ranked lists fused by Reciprocal Rank Fusion —
/// 1. **name**: exact-name matches, then token-identical names, then names containing every
///    query token (shorter names first) — so querying a symbol by name always surfaces it;
/// 2. **semantic**: the ANN tier's lexical-embedding ranking — descriptive queries;
/// 3. **graph**: the candidates from (1)+(2) ranked by in-degree — heavily called/referenced
///    symbols outrank dead-weight lookalikes.
pub fn search_index(index_dir: &Path, query: &str, k: usize) -> Result<String, Box<dyn Error>> {
  let kg = Kg::load(index_dir)?;
  let ann = AnnIndex::load(&index_dir.join("ann.bin"))?;
  let embedder = LexicalEmbedder::default();
  let pool = (k * 4).max(50);

  let semantic: Vec<u64> = ann
    .search(&embedder.embed(query), pool)
    .into_iter()
    .map(|(id, _)| id)
    .collect();

  let query_tokens = tokenize(query);
  let mut named: Vec<(u64, (u8, usize))> = Vec::new();
  for i in 0..kg.node_count() as u64 {
    let Some(view) = kg.node(NodeId::new(i)) else {
      continue;
    };
    let name_tokens = tokenize(view.name);
    let tier = if view.name == query {
      0
    } else if !query_tokens.is_empty() && name_tokens == query_tokens {
      1
    } else if !query_tokens.is_empty() && query_tokens.iter().all(|t| name_tokens.contains(t)) {
      2
    } else {
      continue;
    };
    named.push((i, (tier, view.name.len())));
  }
  named.sort_by_key(|&(id, key)| (key, id));
  named.truncate(pool);
  let named: Vec<u64> = named.into_iter().map(|(id, _)| id).collect();

  // In-degree is a *disambiguator among name-matched candidates* (three `seal` methods → the
  // most-called one first), never a global popularity prior — a union with the semantic pool
  // let popular-but-irrelevant symbols outrank the semantically-best hit on descriptive queries
  // (caught by dogfood: a heavily-referenced `stdin` field beat `Manifest`).
  let mut by_degree: Vec<u64> = named.clone();
  by_degree.sort_by_key(|&id| {
    (
      std::cmp::Reverse(kg.in_neighbors(NodeId::new(id)).len()),
      id,
    )
  });

  let ranked = rrf_fuse(&[named, semantic, by_degree], k);
  let mut out = String::new();
  for (row, score) in ranked {
    if let Some(view) = kg.node(NodeId::new(row)) {
      let _ = writeln!(
        out,
        "{score:.4}  {} [{:?}] {}",
        view.name, view.kind, view.path
      );
    }
  }
  Ok(out)
}

/// Run one graph query verb against a persisted index and render the results — the shared
/// implementation behind the `vorpal-index` binary and the `vorpal graph` subcommand.
pub fn graph_query(index_dir: &Path, verb: &str, name: &str) -> Result<String, Box<dyn Error>> {
  let kg = Kg::load(index_dir)?;
  let ids = match verb {
    "callers" => kg.callers_of(name),
    "refs" | "references" => kg.references_to(name),
    "importers" => kg.importers_of(name),
    "implementors" => kg.implementors_of(name),
    "typeusers" => kg.users_of_type(name),
    "node" => kg.nodes_named(name),
    other => return Err(format!("unknown graph verb '{other}'").into()),
  };
  Ok(if ids.is_empty() {
    format!("(no results for '{name}')\n")
  } else {
    format_nodes(&kg, &ids)
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

#[cfg(test)]
mod tests {
  use super::rrf_fuse;

  #[test]
  fn rrf_fuses_and_breaks_ties_deterministically() {
    // Doc 7 is rank 0 in two lists; doc 3 is rank 0 in one and absent elsewhere.
    let ranked = rrf_fuse(&[vec![7, 3], vec![7, 9], vec![3, 7]], 10);
    assert_eq!(ranked[0].0, 7, "{ranked:?}");
    assert_eq!(ranked[1].0, 3, "{ranked:?}");
    assert_eq!(ranked[2].0, 9, "{ranked:?}");

    // A third-list (graph) placement tips otherwise-mirrored candidates.
    let ranked = rrf_fuse(&[vec![1, 2], vec![2, 1], vec![2, 1]], 10);
    assert_eq!(ranked[0].0, 2, "graph list breaks the tie: {ranked:?}");

    // Exact ties break by id.
    let ranked = rrf_fuse(&[vec![5], vec![4]], 10);
    assert_eq!(ranked[0].0, 4, "{ranked:?}");
  }
}
