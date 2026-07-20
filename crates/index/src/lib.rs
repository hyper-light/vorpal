//! `vorpal-index` — a thin CLI over the ingest → resolve → persist → query pipeline (§3.6).
//!
//! `build_index` is incremental (§3.4): a stat manifest decides per file whether its cached
//! extraction product can be replayed or the file must be re-parsed; the graph is always
//! re-linked from the complete product set (so removals/renames cannot leave stale nodes), and
//! an entirely unchanged tree short-circuits to reusing the persisted index outright.
//!
//! Per-file work (read → parse → extract → cache write, or cache replay) fans out on rayon
//! (§7.5 work-stealing parse/extract): workers borrow the shared extractor immutably, and the
//! order-preserving collect keeps the product list — and therefore node-id assignment — exactly
//! as deterministic as the serial loop was.

use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::UNIX_EPOCH;

use rayon::prelude::*;
use vorpal_ann::{AnnIndex, Embedder, LexicalEmbedder, tokenize};
use vorpal_ingest::{
  ExtractScratch, Manifest, OutlineExtractor, Resolver, StreamWork, cache_file_name, link_writer,
  load_product, save_product, save_product_with, stream_apply,
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
  /// Confidently resolved references (single visible definition).
  pub resolved: u64,
  /// Approximately resolved references (multiple candidates; labeled edges).
  pub ambiguous: u64,
  /// References whose name is defined nowhere in the tree (std/dependencies) — honest.
  pub external: u64,
  /// References with in-tree candidates none of which is safely attributable.
  pub masked: u64,
}

impl IndexReport {
  /// References that produced no edge (external + masked).
  pub fn unresolved(&self) -> u64 {
    self.external + self.masked
  }
}

/// In-flight byte ceiling for the streaming ingest (§7.5): sources+products in transit stay
/// under this regardless of corpus size. Sized to feed every worker generously; the essential
/// output (the graph under construction) is not part of transit and scales with the corpus.
fn stream_budget_bytes() -> u64 {
  let threads = std::thread::available_parallelism()
    .map(|n| n.get() as u64)
    .unwrap_or(1);
  (threads * 8 * 1024 * 1024).clamp(32 * 1024 * 1024, 512 * 1024 * 1024)
}

/// Ingest `src`, resolve cross-file references, and persist the knowledge graph to `out`.
pub fn build_index(src: &Path, out: &Path) -> Result<IndexReport, Box<dyn Error>> {
  let extractor = OutlineExtractor::new()?;
  let manifest = Manifest::scan(src, |p| extractor.handles(p))?;
  let manifest_path = out.join("manifest.bin");

  // Whole-tree fast path: nothing changed → reuse the persisted index without touching a file.
  // The report only needs the node count, read from the segment header — no heap read, no edge
  // read, no CSR rebuild. An unreadable/corrupt index falls through to a rebuild instead of
  // wedging every subsequent run on the same error.
  if let Ok(prior) = Manifest::load(&manifest_path) {
    if manifest.unchanged_since(&prior)
      && out.join("strings.heap").exists()
      && out.join("edges.bin").exists()
    {
      if let Ok(nodes) = Kg::peek_node_count(out) {
        return Ok(IndexReport {
          reused: true,
          indexed: 0,
          skipped: manifest.len() as u64,
          nodes,
          resolved: 0,
          ambiguous: 0,
          external: 0,
          masked: 0,
        });
      }
    }
  }

  // Incremental path, streamed (§7.5): replay cached products for stat-unchanged files,
  // re-parse the rest — admission is byte-budget-gated, extraction fans out over scoped
  // workers with per-worker scratch, and products flow straight into the sharded single-writer
  // commit, so a product exists in RAM only between extraction and application. Products are
  // **self-validating** (§3.4): each carries the stat of the source it was extracted from, so
  // any cached product whose stamp matches replays — whoever wrote it. Read/extract errors
  // skip the file (as before); a cache-write error is fatal (as before). Sequence-ordered
  // per-shard application keeps the output bit-identical to the batch path.
  let products_dir = out.join("products");
  fs::create_dir_all(&products_dir)?;

  let (writer, references, stream) = stream_apply(
    manifest.entries(),
    stream_budget_bytes(),
    |entry, scratch: &mut ExtractScratch| {
      let cache = products_dir.join(cache_file_name(&entry.path));
      if let Ok(product) = load_product(&cache) {
        if product.source_size == entry.size && product.source_mtime_ns == entry.mtime_ns {
          return Ok(StreamWork::Replayed(entry.path.clone(), product));
        }
      }
      // Changed, new, or cache-missing: re-parse (unreadable files are skipped, not fatal).
      let Ok(source) = scratch.read_source(Path::new(&entry.path)) else {
        return Ok(StreamWork::Skipped);
      };
      let Some(mut product) = extractor.extract_product(&entry.path, source) else {
        return Ok(StreamWork::Skipped);
      };
      product.source_size = entry.size;
      product.source_mtime_ns = entry.mtime_ns;
      save_product_with(&cache, &product, &mut scratch.encode)?;
      Ok(StreamWork::Parsed(entry.path.clone(), product))
    },
  )?;
  let (reparsed, replayed) = (stream.parsed, stream.replayed);

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
  // from scratch, so stale state is structurally impossible; resolution links the merged
  // graph over the sharded table/resolve passes.
  let (kg, resolve) = link_writer(writer, &references, &Resolver::new());
  // Embeddings stay off the commit hot path (§3.4): the graph persists now; the ANN tier is
  // built lazily by the first search and validated by stamp, so incremental re-indexes never
  // pay a full vector-graph rebuild (at kernel scale that rebuild dominated the whole run).
  // The manifest is the commit point and always lands last.
  kg.save(out)?;
  manifest.save(&manifest_path)?;
  Ok(IndexReport {
    reused: false,
    indexed: reparsed,
    skipped: replayed,
    nodes: kg.node_count(),
    resolved: resolve.resolved,
    ambiguous: resolve.ambiguous,
    external: resolve.external,
    masked: resolve.masked,
  })
}

/// Build the semantic tier over every KG node: each definition embeds its name (double-weighted),
/// signature, and file *basename* — never the full path, whose directory tokens are shared junk
/// that drowns the signal — through the pluggable embedder (default: the deterministic lexical
/// hasher) into the adaptive ANN index persisted beside the graph. Rows embed in parallel with
/// no per-node intermediate text; the order-preserving collect keeps the index bit-identical to
/// the serial build.
/// The ANN freshness stamp: xxh3 of the node segment bytes. Any node change (name,
/// signature, count, order) changes the segment, which invalidates the stamp — necessary-
/// condition semantics, same as every other cache in the pipeline.
fn ann_stamp_of(index_dir: &Path) -> io::Result<u64> {
  Ok(xxhash_rust::xxh3::xxh3_64(&fs::read(
    index_dir.join("nodes.vseg"),
  )?))
}

/// Build the ANN tier iff its stamp no longer matches the persisted graph (or it does not
/// exist). Queries call this before touching `ann.bin`; `vorpal index` never does.
fn ensure_ann(index_dir: &Path) -> Result<(), Box<dyn Error>> {
  let stamp_path = index_dir.join("ann.stamp");
  let current = ann_stamp_of(index_dir)?;
  let fresh = fs::read(&stamp_path)
    .ok()
    .and_then(|bytes| bytes.try_into().ok().map(u64::from_le_bytes))
    .is_some_and(|stored| stored == current)
    && index_dir.join("ann.bin").exists();
  if fresh {
    return Ok(());
  }
  let kg = Kg::load(index_dir)?;
  build_ann(&kg, index_dir).map_err(|err| err as Box<dyn Error>)?;
  fs::write(&stamp_path, current.to_le_bytes())?;
  Ok(())
}

fn build_ann(kg: &Kg, out: &Path) -> Result<(), Box<dyn Error + Send + Sync>> {
  let embedder = LexicalEmbedder::default();
  let dim = embedder.dim();
  let n = kg.node_count();
  // One flat row-major matrix, rows embedded in place in parallel: no per-row heap vector
  // (at kernel scale the per-row form allocated millions of 1 KB vectors before flattening).
  let mut vectors = vec![0.0f32; n * dim];
  vectors
    .par_chunks_mut(dim)
    .enumerate()
    .for_each(|(i, row)| {
      if let Some(view) = kg.node(NodeId::new(i as u64)) {
        let basename = view.path.rsplit('/').next().unwrap_or(view.path);
        let parts = [view.name, view.name, view.signature, basename];
        embedder.embed_parts_into(&parts, row);
      }
    });
  let ids: Vec<u64> = (0..n as u64).collect();
  AnnIndex::build_flat(dim, ids, vectors, None).save(&out.join("ann.bin"))?;
  Ok(())
}

/// A discovered `.vorpal/index` a search can bank products into: where its products live, and
/// how to spell file keys the way that index's `build_index` runs will (§3.4).
struct WarmRoot {
  products_dir: PathBuf,
  /// Canonical directory containing `.vorpal` — file keys are derived relative to it.
  canonical_root: PathBuf,
  /// The manifest's path-spelling prefix (`"./"`, `""`, `"sub/dir/"`, an absolute base…):
  /// `key(file) = prefix + (file relative to root)` reproduces the walker's exact strings.
  key_prefix: String,
}

/// Process-wide cache of discovered warm roots (one per index root encountered).
static WARM_ROOTS: OnceLock<Mutex<HashMap<PathBuf, Option<Arc<WarmRoot>>>>> = OnceLock::new();

/// Bank one file's extraction product into the nearest existing `.vorpal/index` cache — the
/// **search-feeds-index** hook (§3.4). A search has already walked, read, and matched the
/// file; this persists the extraction so the next `vorpal index` replays it instead of
/// re-parsing (products are self-validating via their source stat stamp).
///
/// Deliberately conservative: it only feeds an index that already exists (searches never
/// create index state in un-indexed trees), silently skips unsupported or unreadable files,
/// and returns whether a product was written. A fresh product for the file's current stat is
/// left untouched, so repeated matches are near-free.
pub fn warm_product_cache(file: &Path) -> io::Result<bool> {
  let Ok(canonical) = file.canonicalize() else {
    return Ok(false);
  };
  let Some(index_root) = find_index_root(&canonical) else {
    return Ok(false);
  };
  let Some(warm) = warm_root_for(index_root)? else {
    return Ok(false);
  };
  let Ok(rel) = canonical.strip_prefix(&warm.canonical_root) else {
    return Ok(false);
  };
  let keyed = format!("{}{}", warm.key_prefix, rel.display());

  let meta = fs::metadata(&canonical)?;
  let mtime_ns = meta
    .modified()?
    .duration_since(UNIX_EPOCH)
    .map(|d| d.as_nanos() as u64)
    .unwrap_or(0);
  let cache = warm.products_dir.join(cache_file_name(&keyed));
  if let Ok(existing) = load_product(&cache) {
    if existing.source_size == meta.len() && existing.source_mtime_ns == mtime_ns {
      return Ok(false);
    }
  }

  let Ok(source) = fs::read_to_string(&canonical) else {
    return Ok(false);
  };
  let extractor = OutlineExtractor::new().map_err(io::Error::other)?;
  let Some(mut product) = extractor.extract_product(&keyed, &source) else {
    return Ok(false);
  };
  product.source_size = meta.len();
  product.source_mtime_ns = mtime_ns;
  fs::create_dir_all(&warm.products_dir)?;
  save_product(&cache, &product)?;
  Ok(true)
}

/// The nearest ancestor directory holding an existing default-location index.
fn find_index_root(file: &Path) -> Option<PathBuf> {
  let mut dir = file.parent()?;
  loop {
    if dir.join(".vorpal/index/manifest.bin").is_file() {
      return Some(dir.to_path_buf());
    }
    dir = dir.parent()?;
  }
}

/// Get-or-build the cached [`WarmRoot`] for an index root. `None` (also cached) means the
/// root's key spelling could not be established — warming is skipped rather than guessed.
fn warm_root_for(index_root: PathBuf) -> io::Result<Option<Arc<WarmRoot>>> {
  let roots = WARM_ROOTS.get_or_init(|| Mutex::new(HashMap::new()));
  if let Some(cached) = roots.lock().unwrap().get(&index_root) {
    return Ok(cached.clone());
  }
  let built = build_warm_root(&index_root)?.map(Arc::new);
  let mut lock = roots.lock().unwrap();
  Ok(lock.entry(index_root).or_insert_with(|| built).clone())
}

/// Establish how this index spells file keys: resolve one manifest entry to its canonical
/// path, take its root-relative form, and split the entry string into
/// `prefix + root-relative suffix` (`"./"` for `vorpal index .`, `"sub/"` for
/// `vorpal index sub`, an absolute base for absolute invocations). String-suffix matching —
/// not path joining — so spelling variants like `"./"` survive. An entry that cannot be
/// verified (deleted file, `..`/symlink spelling) yields `None` and warming stays off for
/// this root rather than guessing keys.
fn build_warm_root(index_root: &Path) -> io::Result<Option<WarmRoot>> {
  let index_dir = index_root.join(".vorpal").join("index");
  let manifest = Manifest::load(&index_dir.join("manifest.bin"))?;
  let canonical_root = index_root.canonicalize()?;
  for entry in manifest.entries() {
    let Ok(canonical_entry) = index_root.join(&entry.path).canonicalize() else {
      continue;
    };
    let Ok(rel) = canonical_entry.strip_prefix(&canonical_root) else {
      continue;
    };
    let rel = rel.display().to_string();
    if let Some(prefix) = entry.path.strip_suffix(&rel) {
      return Ok(Some(WarmRoot {
        products_dir: index_dir.join("products"),
        canonical_root,
        key_prefix: prefix.to_string(),
      }));
    }
  }
  Ok(None)
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
  ensure_ann(index_dir)?;
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
