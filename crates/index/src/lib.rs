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

pub mod annfiles;
pub mod autowarm;
pub mod graph_predicates;
pub mod records;

use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::UNIX_EPOCH;

use vorpal_ann::{AnnIndex, Embedder, LexicalEmbedder, tokenize};
use vorpal_ingest::{
  ExtractScratch, Manifest, OutlineExtractor, PackMsg, PackReader, PackWriter, Resolver,
  StreamWork, cache_file_name, decode_product, encode_product_into, link_writer_spilled,
  load_product, peek_product_stamps, save_product, stream_apply_spilled,
  validate_product,
};
// `Kg` is imported once and re-exported for downstream surfaces (CLI) that route all graph
// access through this crate.
pub use vorpal_kg::{Direction, EdgeType, Kg};
use vorpal_kg::NodeId;

/// Summary of an indexing run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexReport {
  /// The tree was unchanged since the last index — reused without re-parsing (§3.4).
  pub reused: bool,
  /// Files re-parsed this run (changed, new, or cache-missing).
  pub indexed: u64,
  /// Files whose cached extraction product was replayed without a parse.
  pub skipped: u64,
  /// Files whose tree-sitter parse produced ERROR nodes — some of their definitions may be
  /// missing from the graph. A language-agnostic parse-health signal (graceful degradation
  /// made visible), 0 when every file parsed cleanly.
  pub error_files: u64,
  /// Total tree-sitter ERROR nodes across all files — the magnitude behind `error_files`, so a
  /// corpus with one badly-broken file reads differently from one with many lightly-broken files.
  pub error_nodes: u64,
  pub nodes: usize,
  /// Confidently resolved references (single visible definition).
  pub resolved: u64,
  /// Approximately resolved references (multiple candidates; labeled edges).
  pub ambiguous: u64,
  /// References whose name is defined nowhere in the tree (std/dependencies) — honest.
  pub external: u64,
  /// References with in-tree candidates none of which is safely attributable.
  pub masked: u64,
  /// The cache-validity mode this run used (`fast-stat` / `verified`).
  pub cache_mode: &'static str,
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
  // Tunable for memory-vs-throughput experiments; the default keeps every worker fed with
  // lookahead without letting decoded products pile up ahead of the committers.
  if let Some(mb) = std::env::var("VORPAL_STREAM_BUDGET_MB")
    .ok()
    .and_then(|v| v.parse::<u64>().ok())
    .filter(|&mb| mb > 0)
  {
    return mb * 1024 * 1024;
  }
  let threads = std::thread::available_parallelism()
    .map(|n| n.get() as u64)
    .unwrap_or(1);
  (threads * 8 * 1024 * 1024).clamp(32 * 1024 * 1024, 512 * 1024 * 1024)
}

/// How cache validity is decided (IMPROVEMENTS 07-29 §3) — an explicit, testable product
/// contract instead of an environment convention.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CacheMode {
  /// Stat (path, size, mtime) is trusted outside the racy-mtime window; content digests are
  /// verified only inside it. Fast (a no-change re-index is a metadata sweep) with one
  /// documented blind spot: a same-size edit whose mtime is deliberately preserved, made
  /// outside the racy window, can replay stale extraction.
  #[default]
  FastStat,
  /// Content-authoritative: every replay decision verifies the stored source digest against
  /// the file's current bytes. Reads every candidate file — slower, and immune to
  /// preserved-mtime edits. `VORPAL_VERIFY_CACHE=1` selects this mode for any entry point
  /// that does not pass one explicitly (CI convention).
  Verified,
}

impl CacheMode {
  /// The mode the environment requests when a caller passes none.
  fn from_env() -> CacheMode {
    if std::env::var_os("VORPAL_VERIFY_CACHE").is_some_and(|v| v == "1") {
      CacheMode::Verified
    } else {
      CacheMode::FastStat
    }
  }

  pub fn label(self) -> &'static str {
    match self {
      CacheMode::FastStat => "fast-stat",
      CacheMode::Verified => "verified",
    }
  }
}

/// Ingest `src`, resolve cross-file references, and persist the knowledge graph to `out`,
/// with cache validity decided by [`CacheMode::from_env`] (fast-stat unless
/// `VORPAL_VERIFY_CACHE=1`).
pub fn build_index(src: &Path, out: &Path) -> Result<IndexReport, Box<dyn Error>> {
  build_index_with(src, out, CacheMode::from_env())
}

/// [`build_index`] with an explicit [`CacheMode`] — the first-class form CLI/MCP/bindings
/// select rather than routing through an environment variable.
pub fn build_index_with(
  src: &Path,
  out: &Path,
  cache_mode: CacheMode,
) -> Result<IndexReport, Box<dyn Error>> {
  let extractor = OutlineExtractor::new()?;
  // Extraction identity for this run: the whole grammar set folded with the outline-rule digest.
  // Both the whole-tree fast path (via the manifest stamp) and the per-file replay gates key on
  // it, so editing a grammar OR an outline rule invalidates reuse just as a file edit would.
  let rules_digest = extractor.rules_digest();
  let mut manifest = Manifest::scan(src, |p| extractor.handles(p))?;
  manifest.set_grammar_stamp(vorpal_ingest::extraction_identity(
    vorpal_ingest::global_grammar_stamp(),
    rules_digest,
  ));
  // Generation layout (IMPROVEMENTS §4): `out` is the index *root*. The live artifacts sit in
  // an immutable, content-addressed generation dir named by `out/CURRENT`; this run reads the
  // prior generation, stages a new one, and commits it with one atomic pointer swap — a
  // concurrent reader sees the complete old index or the complete new one, never a mixture.
  // A legacy flat root (no CURRENT) resolves to itself, so its artifacts still serve as the
  // prior; the first rebuild migrates it into a generation.
  let prior = vorpal_kg::resolve_index_dir(out);
  let manifest_path = prior.join("manifest.bin");

  // Staged cache validation (IMPROVEMENTS §6): stat (size+mtime) is the cheap hint; the v6
  // content digest is the identity. Digests are verified (a) always under
  // `VORPAL_VERIFY_CACHE=1` and (b) automatically for files in the **racy window** — mtime
  // within 2s of the previous manifest's write, where an edit can restore size+mtime within
  // timestamp granularity (the git racily-clean hazard). Both gates also suppress the
  // whole-tree reuse fast path below, which is otherwise stat-only.
  let verify_all = cache_mode == CacheMode::Verified;
  let prior_manifest_ns: u64 = fs::metadata(&manifest_path)
    .and_then(|m| m.modified())
    .ok()
    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
    .map(|d| d.as_nanos() as u64)
    .unwrap_or(0);
  // Racy files get their digests verified *inside* the fast path (reading only those few
  // files against the pack's stored digests) — an immediate no-change re-index keeps its
  // 0.1s reuse, while a stat-invisible racy edit falls through to a rebuild.
  let racy_files_verify = || -> bool {
    let racy: Vec<&vorpal_ingest::FileStat> = manifest
      .entries()
      .iter()
      .filter(|e| e.mtime_ns.abs_diff(prior_manifest_ns) < 2_000_000_000)
      .collect();
    if racy.is_empty() {
      return true;
    }
    let Some(pack) = PackReader::open(&prior) else {
      return false;
    };
    racy.iter().all(|entry| {
      let stored = pack
        .get(&entry.path)
        .and_then(vorpal_ingest::peek_product_digest);
      match (stored, fs::read(&entry.path)) {
        (Some(digest), Ok(bytes)) => xxhash_rust::xxh3::xxh3_64(&bytes) == digest,
        _ => false,
      }
    })
  };

  // Whole-tree fast path: nothing changed → reuse the persisted index without touching a file.
  // The report only needs the node count, read from the segment header — no heap read, no edge
  // read, no CSR rebuild. An unreadable/corrupt index falls through to a rebuild instead of
  // wedging every subsequent run on the same error.
  if let Ok(prior_manifest) = Manifest::load(&manifest_path) {
    if manifest.unchanged_since(&prior_manifest)
      && manifest.grammar_stamp() == prior_manifest.grammar_stamp()
      && !verify_all
      && prior.join("strings.heap").exists()
      && prior.join("graph.bin").exists()
      && racy_files_verify()
    {
      // Backfill: index dirs written before the name-index sidecar existed gain it here,
      // once — sublinear name lookup without forcing a rebuild. (An additive, self-validating
      // sidecar — like the lazy ANN tier, it is the one kind of write an existing generation
      // admits.)
      if !prior.join("names.idx").exists() {
        if let Ok(kg) = Kg::load(&prior) {
          let _ = kg.write_names_index(&prior);
        }
      }
      if let Ok(nodes) = Kg::peek_node_count(&prior) {
        return Ok(IndexReport {
          reused: true,
          cache_mode: cache_mode.label(),
          error_files: 0,
          error_nodes: 0,
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
  // The loose bank lives at the *root* (outside any generation): concurrent searches feed it
  // and every build consumes it, whichever generation is live.
  let products_dir = out.join("products");
  fs::create_dir_all(&products_dir)?;
  // Stage the new generation in a scratch dir under `gen/`; it becomes `gen/<content-id>` at
  // commit. Fresh per run (a crashed run's staging is swept by the next commit's GC).
  let staging = out
    .join("gen")
    .join(format!(".staging-{}", std::process::id()));
  let _ = fs::remove_dir_all(&staging);
  fs::create_dir_all(&staging)?;

  let digest_must_match = move |entry_mtime_ns: u64, stored_digest: Option<u64>, path: &Path| {
    let racy = entry_mtime_ns.abs_diff(prior_manifest_ns) < 2_000_000_000;
    if !verify_all && !racy {
      return true;
    }
    match (stored_digest, fs::read(path)) {
      (Some(stored), Ok(bytes)) => xxhash_rust::xxh3::xxh3_64(&bytes) == stored,
      _ => false,
    }
  };

  // Product cache, two tiers (§3.4): the **pack** (one mapped file — replay pays zero opens;
  // the loose-file cache cost one `open` per product, 72k of them at kernel scale, and macOS
  // serializes the open path) plus **loose files**, still written by search banking (separate
  // processes must not contend on the pack) and consolidated — then deleted — here. Loose
  // wins over pack on lookup: it is only ever fresher.
  let pack_reader = PackReader::open(&prior).map(Arc::new);
  let loose: HashSet<OsString> = fs::read_dir(&products_dir)
    .map(|dir| dir.flatten().map(|f| f.file_name()).collect())
    .unwrap_or_default();
  // The writer builds the new generation's pack in staging, copying reused bodies out of the
  // prior generation's mapping — the prior pack is never touched.
  let pack_writer = PackWriter::new(&staging, pack_reader.clone());
  let pack_sink = pack_writer.sink();
  let live_paths: Vec<String> = manifest.entries().iter().map(|e| e.path.clone()).collect();
  let pack_thread = std::thread::spawn(move || pack_writer.finish(live_paths));
  let send_fatal =
    |_| std::io::Error::other("product pack writer failed; see the join error for the cause");

  // References spill to disk between commit and resolve — they are written once and read
  // once, sequentially; buffering them in RAM only raised the peak footprint (~220 MB at
  // kernel scale).
  let error_files = std::sync::atomic::AtomicU64::new(0);
  let error_nodes = std::sync::atomic::AtomicU64::new(0);
  let spill_path = staging.join(".refs.spill");
  let heap_stream = staging.join("strings.heap.tmp");
  let stream_result = stream_apply_spilled(
    manifest.entries(),
    stream_budget_bytes(),
    &spill_path,
    Some(&heap_stream),
    pack_reader.as_deref(),
    |entry, scratch: &mut ExtractScratch| {
      let cache_name = cache_file_name(&entry.path);
      if loose.contains(&OsString::from(&cache_name)) {
        if let Ok(bytes) = fs::read(products_dir.join(&cache_name)) {
          if let Ok(product) = decode_product(&bytes) {
            if product.source_size == entry.size
              && product.source_mtime_ns == entry.mtime_ns
              && Some(product.grammar_digest)
                == vorpal_ingest::extraction_identity_for_path(&entry.path, rules_digest)
              && digest_must_match(
                entry.mtime_ns,
                Some(product.source_xxh3),
                Path::new(&entry.path),
              )
            {
              if product.error_nodes > 0 {
                error_files.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                error_nodes.fetch_add(product.error_nodes as u64, std::sync::atomic::Ordering::Relaxed);
              }
              pack_sink
                .send(PackMsg {
                  path: entry.path.clone(),
                  body: bytes,
                })
                .map_err(send_fatal)?;
              return Ok(StreamWork::Replayed(entry.path.clone(), product));
            }
          }
        }
      }
      if let Some(reader) = &pack_reader {
        if let Some(bytes) = reader.get(&entry.path) {
          // Stamp peek + full view-decode validation here (cheap: no owned strings), so a
          // corrupt entry still falls through to a re-parse; the committer then decodes the
          // validated views again straight from the map and applies them — the product
          // itself never crosses the channel and never materializes.
          if peek_product_stamps(bytes) == Some((entry.size, entry.mtime_ns))
            && vorpal_ingest::peek_product_grammar_digest(bytes)
              == vorpal_ingest::extraction_identity_for_path(&entry.path, rules_digest)
            && digest_must_match(
              entry.mtime_ns,
              vorpal_ingest::peek_product_digest(bytes),
              Path::new(&entry.path),
            )
            && validate_product(bytes)
          {
            let ec = vorpal_ingest::peek_product_error_nodes(bytes).unwrap_or(0);
            if ec > 0 {
              error_files.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
              error_nodes.fetch_add(ec as u64, std::sync::atomic::Ordering::Relaxed);
            }
            return Ok(StreamWork::ReplayedPacked(entry.path.clone()));
          }
        }
      }
      // Changed, new, or cache-missing: re-parse (unreadable files are skipped, not fatal).
      let Ok(source) = scratch.read_source(Path::new(&entry.path)) else {
        return Ok(StreamWork::Skipped);
      };
      let Some(mut product) = extractor.extract_product(&entry.path, source) else {
        return Ok(StreamWork::Skipped);
      };
      if product.error_nodes > 0 {
        error_files.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        error_nodes.fetch_add(product.error_nodes as u64, std::sync::atomic::Ordering::Relaxed);
      }
      product.source_size = entry.size;
      product.source_mtime_ns = entry.mtime_ns;
      scratch.encode.clear();
      encode_product_into(&product, &mut scratch.encode);
      // Move the encoded bytes into the message — cloning re-copied every parsed product
      // (half a gigabyte on a cold kernel build); the scratch buffer regrows on next use.
      pack_sink
        .send(PackMsg {
          path: entry.path.clone(),
          body: std::mem::take(&mut scratch.encode),
        })
        .map_err(send_fatal)?;
      Ok(StreamWork::Parsed(entry.path.clone(), product))
    },
  );
  drop(pack_sink);
  let pack_result = pack_thread.join().expect("pack writer panicked");
  let (writer, spilled_refs, stream) = stream_result?;
  pack_result?;
  // Hard-limit gate before sealing (IMPROVEMENTS §12): node ids, graph endpoints, and string-heap
  // offsets are all 32-bit on disk. Rather than let a corpus past those ceilings silently wrap a
  // cast and persist a corrupt index, fail loudly and actionably. The heap grows monotonically,
  // so a single check on its final size proves no intermediate offset wrapped.
  const U32_CEIL: usize = u32::MAX as usize;
  if writer.node_count() > U32_CEIL {
    return Err(format!(
      "index exceeds the supported node limit: {} definitions (max {U32_CEIL}); node ids are \
       32-bit — split the corpus into multiple indexes",
      writer.node_count()
    )
    .into());
  }
  if writer.heap_len() > U32_CEIL as u64 {
    return Err(format!(
      "index string heap exceeds the 4 GiB addressable limit: {} bytes (max {U32_CEIL}); \
       names/paths/signatures use 32-bit offsets — split the corpus into multiple indexes",
      writer.heap_len()
    )
    .into());
  }
  // Extraction/commit scratch (products in transit, per-worker buffers, shard canonicals)
  // is dead now — hand its pages back before the link pass allocates the table and edges.
  vorpal_ingest::release_freed_pages();
  let (reparsed, replayed) = (stream.parsed, stream.replayed);

  // Loose-file hygiene: everything snapshotted above is now consolidated in the pack (or
  // superseded by a re-parse, or stale) — delete it. Files banked by searches *during* this
  // run are not in the snapshot and survive untouched.
  for name in &loose {
    let _ = fs::remove_file(products_dir.join(name));
  }

  // Full re-link from the complete product set: identity, resolution, and edges are recomputed
  // from scratch, so stale state is structurally impossible; resolution links the merged
  // graph over the sharded table/resolve passes.
  let (kg, resolve, evidence) = link_writer_spilled(writer, spilled_refs, &Resolver::new())?;
  // Persist the per-edge evidence sidecar into the staged generation (§5): span, resolver
  // reason, confidence, and candidate count per edge occurrence — canonically sorted, so it
  // joins the generation's content identity deterministically.
  vorpal_kg::save_evidence(&staging, evidence)?;
  // Embeddings stay off the commit hot path (§3.4): the graph persists now; the ANN tier is
  // built lazily by the first search and validated by stamp, so incremental re-indexes never
  // pay a full vector-graph rebuild (at kernel scale that rebuild dominated the whole run).
  // The manifest is the commit point and always lands last.
  kg.save(&staging)?;
  manifest.save(&staging.join("manifest.bin"))?;
  // Commit: name the staged generation by its content, atomically repoint CURRENT, GC.
  commit_generation(out, &prior, staging)?;
  Ok(IndexReport {
    reused: false,
    cache_mode: cache_mode.label(),
    indexed: reparsed,
    skipped: replayed,
    error_files: error_files.into_inner(),
    error_nodes: error_nodes.into_inner(),
    nodes: kg.node_count(),
    resolved: resolve.resolved,
    ambiguous: resolve.ambiguous,
    external: resolve.external,
    masked: resolve.masked,
  })
}

/// The core artifact set a generation is named by — the complete persisted index, in fixed
/// (sorted) order. Lazy sidecars added after commit (the ANN tier) are deliberately excluded:
/// they are stamp-validated against the node segment, deterministic given the generation, and
/// must not change its identity.
const GENERATION_ARTIFACTS: [&str; 8] = [
  "evidence.bin",
  "graph.bin",
  "manifest.bin",
  "names.idx",
  "nodes.vseg",
  "products.idx",
  "products.pack",
  "strings.heap",
];

/// Commit a staged generation (IMPROVEMENTS §4): name it by its **content**, atomically swap
/// the `CURRENT` pointer, and garbage-collect superseded generations.
///
/// Content-addressing is what preserves the determinism contract under the generation layout:
/// the id is a pure function of the artifact bytes, so two from-scratch builds of the same tree
/// commit the same `gen/<id>`, and an incremental rebuild converges to the byte-identical
/// generation a from-scratch build of the final tree produces — directory names included. A
/// staged generation whose id already exists is byte-identical to the committed one, so staging
/// is simply dropped and `CURRENT` repointed.
///
/// GC keeps two generations — the new one and the one `CURRENT` pointed at when this build
/// began — so a reader that resolved just before the swap keeps a complete index on disk.
/// Readers that hold an even older generation keep their mmaps (POSIX unlink semantics); only
/// *new* opens in a collected generation fail, as a clean retryable error. A legacy flat root
/// that served as the prior is swept the same way: its artifacts are superseded by the
/// generation just committed from them.
fn commit_generation(root: &Path, prior: &Path, staging: PathBuf) -> io::Result<()> {
  // Sweep staging scratch that is not part of the named artifact set (spill files, tmp names)
  // so the generation holds exactly its artifacts.
  for entry in fs::read_dir(&staging)?.flatten() {
    let name = entry.file_name();
    let keep = GENERATION_ARTIFACTS
      .iter()
      .any(|artifact| name.as_os_str() == *artifact);
    if !keep {
      let path = entry.path();
      let _ = if path.is_dir() {
        fs::remove_dir_all(&path)
      } else {
        fs::remove_file(&path)
      };
    }
  }
  // Content id: stream every artifact (fixed order) through one xxh3-128.
  let mut hasher = xxhash_rust::xxh3::Xxh3::new();
  for artifact in GENERATION_ARTIFACTS {
    let path = staging.join(artifact);
    let Ok(mut file) = fs::File::open(&path) else {
      continue; // an artifact a smaller index legitimately lacks still yields a stable id
    };
    hasher.update(artifact.as_bytes());
    let len = file.metadata()?.len();
    hasher.update(&len.to_le_bytes());
    let mut buf = vec![0u8; 1 << 20];
    loop {
      let n = io::Read::read(&mut file, &mut buf)?;
      if n == 0 {
        break;
      }
      hasher.update(&buf[..n]);
    }
  }
  let id = format!("{:032x}", hasher.digest128());
  let final_dir = root.join("gen").join(&id);
  // Dedup guard: an existing same-id generation is byte-identical *by construction*, but only
  // if it is still complete — a tampered or partially-deleted dir must not be trusted over the
  // freshly staged one. Same-id ⇒ same artifact set, so presence-checking staging's artifacts
  // against it is a sufficient completeness test.
  let existing_is_complete = final_dir.exists()
    && GENERATION_ARTIFACTS
      .iter()
      .all(|artifact| !staging.join(artifact).exists() || final_dir.join(artifact).exists());
  if existing_is_complete {
    fs::remove_dir_all(&staging)?;
  } else {
    if final_dir.exists() {
      fs::remove_dir_all(&final_dir)?;
    }
    fs::rename(&staging, &final_dir)?;
  }

  // Carry the prior generation's lazy ANN tier forward (hardlink; copy as fallback) so an
  // incremental rebuild keeps its overlay fast path: the carried tier's base stamp no longer
  // matches the new node segment, which is exactly the condition the search-side overlay
  // reconciles through the `ann.files` digest map — without the carry, every post-edit search
  // would pay the exhaustive fallback until a full re-warm. Hardlinks are safe because ANN
  // artifacts are only ever *replaced* (tmp + rename), never mutated in place: a re-warm in
  // this generation unlinks its name without touching the prior generation's inode. Never
  // overwrites — a generation that already warmed its own tier keeps it.
  if prior != &final_dir {
    for ann_file in ["ann.bin", "ann.files", "ann.stamp"] {
      let from = prior.join(ann_file);
      let to = final_dir.join(ann_file);
      if from.exists() && !to.exists() && fs::hard_link(&from, &to).is_err() {
        let _ = fs::copy(&from, &to);
      }
    }
  }

  // Atomic pointer swap: readers resolve CURRENT in one read; tmp + rename means they see the
  // old pointer or the new one, never a torn write. The tmp is synced so a crash straddling
  // the rename cannot publish a pointer whose bytes never reached disk.
  let pointer = format!("gen/{id}\n");
  let current_tmp = root.join("CURRENT.tmp");
  {
    let mut file = fs::File::create(&current_tmp)?;
    io::Write::write_all(&mut file, pointer.as_bytes())?;
    file.sync_all()?;
  }
  fs::rename(&current_tmp, root.join("CURRENT"))?;

  // GC: keep the new generation and the prior one; sweep everything else under gen/ —
  // superseded generations and dead staging dirs — skipping anything modified in the last two
  // minutes (a concurrent build's staging must not be swept mid-flight). Errors are ignored:
  // GC is hygiene, never correctness.
  let keep_prior = prior.starts_with(root.join("gen")).then(|| prior.to_path_buf());
  if let Ok(entries) = fs::read_dir(root.join("gen")) {
    for entry in entries.flatten() {
      let path = entry.path();
      if path == final_dir || Some(&path) == keep_prior.as_ref() {
        continue;
      }
      let recent = entry
        .metadata()
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|age| age.as_secs() < 120);
      if !recent {
        let _ = fs::remove_dir_all(&path);
      }
    }
  }
  // Legacy migration: when the prior was a flat root, its artifacts are now superseded by the
  // generation committed from them — sweep the known filenames (never products/, gen/, or
  // CURRENT) so the root holds only the generation layout.
  if prior == root {
    for artifact in GENERATION_ARTIFACTS {
      let _ = fs::remove_file(root.join(artifact));
    }
    for scratch in [
      "strings.heap.tmp",
      ".refs.spill",
      "ann.bin",
      "ann.files",
      "ann.stamp",
      "ann.build.lock",
      "products.pack.tmp",
      "products.pack.spool",
      "products.idx.tmp",
    ] {
      let _ = fs::remove_file(root.join(scratch));
    }
  }
  Ok(())
}

/// Build the semantic tier over every KG node: each definition embeds its name (double-weighted),
/// signature, and file *basename* — never the full path, whose directory tokens are shared junk
/// that drowns the signal — through the pluggable embedder (default: the deterministic lexical
/// hasher) into the adaptive ANN index persisted beside the graph. Rows embed in parallel with
/// no per-node intermediate text; the order-preserving collect keeps the index bit-identical to
/// the serial build.
/// The ANN freshness stamp: xxh3 of the node segment bytes of the **loaded** graph. Any node
/// change (name, signature, count, order) changes the segment, which invalidates the stamp —
/// necessary-condition semantics, same as every other cache in the pipeline. Hashing the
/// loaded mapping (never re-reading `nodes.vseg`) pins every freshness decision to the
/// generation actually in hand.
fn stamp_of(kg: &Kg) -> u64 {
  xxhash_rust::xxh3::xxh3_64(kg.node_segment_bytes())
}

/// The vector tier's row set: every non-Import node id, ascending. Import nodes are wiring,
/// not semantic targets (they stay reachable through the exact-name channel); build and
/// fallback must agree on this filter or cold results would resurface import noise.
fn semantic_row_ids(kg: &Kg) -> Vec<u64> {
  (0..kg.node_count() as u64)
    .filter(|&i| {
      kg.node(NodeId::new(i))
        .is_some_and(|view| view.kind != vorpal_kg::SymbolKind::Import)
    })
    .collect()
}

/// Embed node `id`'s parts into `row` — the one embedding recipe (name double-weighted,
/// signature, file basename) shared by the index build, the cold fallback, and the rerank.
fn embed_node_into(kg: &Kg, embedder: &LexicalEmbedder, id: u64, row: &mut [f32]) {
  if let Some(view) = kg.node(NodeId::new(id)) {
    let basename = view.path.rsplit('/').next().unwrap_or(view.path);
    let parts = [view.name, view.name, view.signature, basename];
    embedder.embed_parts_into(&parts, row);
  } else {
    row.fill(0.0);
  }
}

/// The `(dim, base_stamp)` of the persisted ann.bin header, if present and current-format —
/// exposed for coherence tests and diagnostics.
pub fn peek_ann_header(index_dir: &Path) -> Option<(usize, u64)> {
  AnnIndex::peek_header(&vorpal_kg::resolve_index_dir(index_dir).join("ann.bin"))
}

/// Whether the persisted vector tier matches `current_stamp` (and this build's format and
/// embedder shape). Read-only — never blocks on, or triggers, a build.
fn ann_is_fresh(index_dir: &Path, current_stamp: u64, dim: usize) -> bool {
  fs::read(index_dir.join("ann.stamp"))
    .ok()
    .and_then(|bytes| bytes.try_into().ok().map(u64::from_le_bytes))
    .is_some_and(|stored| stored == current_stamp)
    // The bin's own header must carry the same generation: a rebuild window can rename the
    // new bin before the new stamp lands, and the stamp file alone cannot see that.
    && AnnIndex::peek_header(&index_dir.join("ann.bin"))
      .is_some_and(|(bin_dim, bin_stamp)| bin_dim == dim && bin_stamp == current_stamp)
}

/// Build the ANN tier iff its stamp no longer matches the persisted graph (or it does not
/// exist). Queries call this before touching `ann.bin`; `vorpal index` never does.
/// Build the ANN tier now if it is stale — the daemon calls this eagerly (in the
/// background, right after an index refresh) so interactive searches stop paying the
/// build; a search that arrives mid-build serializes on the same lock and proceeds the
/// moment the tier is fresh.
pub fn warm_ann(index_dir: &Path) -> Result<(), Box<dyn Error>> {
  // Warm the generation CURRENT names right now; its artifacts land inside that generation
  // (the ANN tier is the one stamp-validated sidecar an existing generation admits). If a
  // rebuild supersedes it mid-warm, the work is simply for a generation about to retire —
  // harmless, and the next search on the new generation triggers its own warm.
  let index_dir = &vorpal_kg::resolve_index_dir(index_dir);
  // Cross-process exclusion: concurrent warms (a daemon thread + a detached CLI child, or
  // two cold CLIs) must not build twice. The loser skips entirely — whoever asked has
  // already been served by the fallback tier, and the winner's commit flips freshness.
  // fd-lock is advisory and released by the OS on process death, so a crashed warm never
  // wedges the tier.
  let lock_path = index_dir.join("ann.build.lock");
  let lock_file = fs::OpenOptions::new()
    .create(true)
    .truncate(false)
    .write(true)
    .open(&lock_path)?;
  let mut lock = fd_lock::RwLock::new(lock_file);
  let Ok(_guard) = lock.try_write() else {
    return Ok(());
  };
  ensure_ann(index_dir)
}

fn ensure_ann(index_dir: &Path) -> Result<(), Box<dyn Error>> {
  // One build at a time per process: an eager background warm and a foreground search must
  // not both build (duplicate work, racing writes). Late entrants re-check freshness under
  // the lock and find the first builder's work done.
  static ANN_BUILD: Mutex<()> = Mutex::new(());
  let _guard = ANN_BUILD
    .lock()
    .unwrap_or_else(|poisoned| poisoned.into_inner());
  // Load first, stamp the loaded bytes: the stamp this build writes must describe the KG it
  // actually embedded, even if `nodes.vseg` is replaced underneath us mid-decision.
  let kg = Kg::load(index_dir)?;
  let current = stamp_of(&kg);
  if ann_is_fresh(index_dir, current, LexicalEmbedder::default().dim()) {
    return Ok(());
  }
  build_ann(&kg, index_dir, current).map_err(|err| err as Box<dyn Error>)?;
  // Commit order: ann.bin → ann.files (both inside build_ann) → ann.stamp. The stamp is the
  // commit point; a crash anywhere earlier leaves a mismatch that routes searches to the
  // exhaustive fallback until the next warm heals it.
  let stamp_path = index_dir.join("ann.stamp");
  let stamp_tmp = index_dir.join("ann.stamp.tmp");
  fs::write(&stamp_tmp, current.to_le_bytes())?;
  fs::rename(&stamp_tmp, &stamp_path)?;
  Ok(())
}

fn build_ann(kg: &Kg, out: &Path, base_stamp: u64) -> Result<(), Box<dyn Error + Send + Sync>> {
  vorpal_kg::phase_stamp("ann: build start");
  let embedder = LexicalEmbedder::default();
  let dim = embedder.dim();
  let ids = semantic_row_ids(kg);
  let row_ids = ids.clone();
  // Rows embed straight into the index's storage (i8 codes at scale, in parallel): the
  // full-precision matrix never materializes — 2.9 GB of pure transient at kernel scale.
  let index = AnnIndex::build_rows(dim, ids, |i, row| {
    embed_node_into(kg, &embedder, row_ids[i], row)
  });
  vorpal_kg::phase_stamp("ann: save start");
  index
    .with_base_stamp(base_stamp)
    .save(&out.join("ann.bin"))?;
  // The per-file identity map for this generation — what lets a later search remap
  // unchanged files and overlay changed ones instead of rebuilding (§ overlay).
  annfiles::save(out, base_stamp, &annfiles::file_runs_of(kg))?;
  vorpal_kg::phase_stamp("ann: done");
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

/// The nearest ancestor directory holding an existing default-location index — a generation
/// root (carrying `CURRENT`) or a legacy flat dir (carrying `manifest.bin` directly).
fn find_index_root(file: &Path) -> Option<PathBuf> {
  let mut dir = file.parent()?;
  loop {
    let index_dir = dir.join(".vorpal/index");
    if index_dir.join("CURRENT").is_file() || index_dir.join("manifest.bin").is_file() {
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
  // Key spelling comes from the live generation's manifest; the bank itself stays at the
  // root-level `products/` inbox, which every generation's build consumes.
  let manifest = Manifest::load(&vorpal_kg::resolve_index_dir(&index_dir).join("manifest.bin"))?;
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
/// [`rrf_fuse`] with provenance: each fused hit carries its per-channel rank (0-based;
/// `None` where a channel did not surface it) — §11's "expose which rankers contributed".
fn rrf_fuse_explained(lists: &[Vec<u64>], k: usize) -> Vec<(u64, f32, Vec<Option<usize>>)> {
  let mut scores: std::collections::HashMap<u64, (f32, Vec<Option<usize>>)> =
    std::collections::HashMap::new();
  for (channel, list) in lists.iter().enumerate() {
    for (rank, &id) in list.iter().enumerate() {
      let entry = scores
        .entry(id)
        .or_insert_with(|| (0.0, vec![None; lists.len()]));
      entry.0 += 1.0 / (RRF_K + rank as f32);
      entry.1[channel] = Some(rank);
    }
  }
  let mut ranked: Vec<(u64, f32, Vec<Option<usize>>)> = scores
    .into_iter()
    .map(|(id, (score, ranks))| (id, score, ranks))
    .collect();
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
  search_index_impl(index_dir, query, k, false)
}

/// [`search_index`] with ranking provenance: each line gains `(id N; name#r vector#r graph#r)`
/// — the node id (for `fetch_span`/selector refinement) and each contributing channel's
/// 1-based rank. The plain renderer stays byte-stable for humans and captured docs.
pub fn search_index_explained(
  index_dir: &Path,
  query: &str,
  k: usize,
) -> Result<String, Box<dyn Error>> {
  search_index_impl(index_dir, query, k, true)
}

/// The typed twin of [`search_index_explained`]: the same pinned-generation hybrid ranking,
/// returned as records instead of rendered lines (IMPROVEMENTS #7).
pub fn search_records(
  index_dir: &Path,
  query: &str,
  k: usize,
) -> Result<Vec<records::SearchHitRecord>, Box<dyn Error>> {
  let (kg, ranked) = search_core(index_dir, query, k)?;
  const CHANNELS: [&str; 3] = ["name", "vector", "graph"];
  Ok(
    ranked
      .into_iter()
      .filter_map(|(row, score, ranks)| {
        Some(records::SearchHitRecord {
          node: records::node_record(&kg, NodeId::new(row))?,
          score,
          channels: CHANNELS
            .iter()
            .zip(&ranks)
            .filter_map(|(&channel, rank)| {
              rank.map(|rank| records::ChannelRank {
                channel,
                rank: rank + 1,
              })
            })
            .collect(),
        })
      })
      .collect(),
  )
}

fn search_index_impl(
  index_dir: &Path,
  query: &str,
  k: usize,
  explain: bool,
) -> Result<String, Box<dyn Error>> {
  let (kg, ranked) = search_core(index_dir, query, k)?;
  let mut out = String::new();
  for (row, score, ranks) in ranked {
    if let Some(view) = kg.node(NodeId::new(row)) {
      if explain {
        let channels = ["name", "vector", "graph"];
        let mut provenance = format!("id {row}");
        for (channel, rank) in channels.iter().zip(&ranks) {
          if let Some(rank) = rank {
            let _ = write!(provenance, "; {channel}#{}", rank + 1);
          }
        }
        let _ = writeln!(
          out,
          "{score:.4}  {} [{:?}] {}  ({provenance})",
          view.name, view.kind, view.path
        );
      } else {
        let _ = writeln!(
          out,
          "{score:.4}  {} [{:?}] {}",
          view.name, view.kind, view.path
        );
      }
    }
  }
  Ok(out)
}

/// The pinned-generation hybrid ranking shared by the rendered and typed search surfaces:
/// name/semantic/in-degree channels fused by RRF, each hit carrying its per-channel ranks.
#[allow(clippy::type_complexity)]
fn search_core(
  index_dir: &Path,
  query: &str,
  k: usize,
) -> Result<(Kg, Vec<(u64, f32, Vec<Option<usize>>)>), Box<dyn Error>> {
  // Pin one generation for the whole query: the graph AND the ANN tier both come from the
  // directory CURRENT names right now, so a rebuild landing mid-search cannot hand this query
  // a mixed pair (the ANN stamp would catch it, but resolving once makes it impossible).
  let index_dir = &vorpal_kg::resolve_index_dir(index_dir);
  let kg = Kg::load(index_dir)?;
  let current_stamp = stamp_of(&kg);
  let embedder = LexicalEmbedder::default();
  let pool = (k * 4).max(50);
  let query_vec = embedder.embed(query);

  // Semantic candidate pool, by tier — a search NEVER waits on an ANN build:
  // 1. **Base-fresh**: the persisted tier matches this KG generation → beam search.
  // 2. **Overlay**: the base is one or more edits behind, but `ann.files` reconciles —
  //    unchanged files' candidates remap to current ids, changed/new files' rows score
  //    exactly, dead rows tombstone. Milliseconds, and recall for edited code is exact.
  // 3. **Fallback**: anything else — no base, torn artifacts, overlay too large — takes the
  //    fused exhaustive scan (~0.2s at kernel scale, exact recall) and kicks a detached
  //    background warm. Correctness never depends on which tier answered.
  let take = pool * 2;
  let candidates: Vec<u64> = if ann_is_fresh(index_dir, current_stamp, embedder.dim()) {
    AnnIndex::load(&index_dir.join("ann.bin"))?
      .search(&query_vec, take)
      .into_iter()
      .map(|(id, _)| id)
      .collect()
  } else if let Some(overlay) = annfiles::OverlayView::assemble(index_dir, &kg, embedder.dim()) {
    autowarm::maybe_spawn(index_dir);
    // Overfetch by the dead-row count (bounded) so tombstoning cannot starve the pool.
    let bump = (overlay.tombstoned_nodes as usize).min(take);
    let mut ids: Vec<u64> = AnnIndex::load(&index_dir.join("ann.bin"))?
      .search(&query_vec, take + bump)
      .into_iter()
      .filter_map(|(base_id, _)| overlay.remap(base_id))
      .collect();
    let overlay_hits = vorpal_ann::exhaustive_semantic(
      embedder.dim(),
      &overlay.overlay_ids,
      |i, row| embed_node_into(&kg, &embedder, overlay.overlay_ids[i], row),
      &query_vec,
      take,
    );
    // Disjoint by construction (remap targets are unchanged files; overlay ids are
    // changed/new files) — a plain union; the exact rerank below orders everything.
    ids.extend(overlay_hits.into_iter().map(|(id, _)| id));
    ids
  } else {
    // Kick a detached warm so the *next* search takes the fast tier — gated (registered
    // binaries only, opt-out, once per process) and best-effort; see `autowarm`.
    autowarm::maybe_spawn(index_dir);
    let ids = semantic_row_ids(&kg);
    let scored = vorpal_ann::exhaustive_semantic(
      embedder.dim(),
      &ids,
      |i, row| embed_node_into(&kg, &embedder, ids[i], row),
      &query_vec,
      take,
    );
    scored.into_iter().map(|(id, _)| id).collect()
  };

  // Re-score every candidate at full precision by re-embedding its parts against the
  // *current* KG — approximation chooses the pool, never the final semantic order (§10's
  // rerank bar), and rendering can never serve stale content.
  let semantic: Vec<u64> = {
    let mut scored: Vec<(f32, u64)> = candidates
      .into_iter()
      .map(|id| {
        let mut row = vec![0.0f32; embedder.dim()];
        embed_node_into(&kg, &embedder, id, &mut row);
        let exact = row
          .iter()
          .zip(&query_vec)
          .map(|(x, y)| (x - y) * (x - y))
          .sum::<f32>();
        (exact, id)
      })
      .collect();
    scored.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
    scored.truncate(pool);
    scored.into_iter().map(|(_, id)| id).collect()
  };

  let query_tokens = tokenize(query);
  // Parallel scan over the node rows; per-row work is pure, and the indexed flatten keeps
  // ascending-id order, so the collected list is identical to the serial loop's.
  let mut named: Vec<(u64, (u8, usize))> = {
    use rayon::prelude::*;
    (0..kg.node_count() as u64)
      .into_par_iter()
      .filter_map(|i| {
        let view = kg.node(NodeId::new(i))?;
        let name_tokens = tokenize(view.name);
        let tier = if view.name == query {
          0
        } else if !query_tokens.is_empty() && name_tokens == query_tokens {
          1
        } else if !query_tokens.is_empty() && query_tokens.iter().all(|t| name_tokens.contains(t)) {
          2
        } else {
          return None;
        };
        Some((i, (tier, view.name.len())))
      })
      .collect()
  };
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

  Ok((kg, rrf_fuse_explained(&[named, semantic, by_degree], k)))
}

/// Run one graph query verb against a persisted index and render the results — the shared
/// implementation behind the `vorpal-index` binary and the `vorpal graph` subcommand.
/// How a graph query names its target symbol — the CLI/MCP argument surface over
/// [`vorpal_kg::SymbolSelector`]. `merge_all` restores the historical union-over-namesakes
/// behavior explicitly; without it, an ambiguous name yields the candidate list instead.
#[derive(Debug, Clone, Default)]
pub struct GraphTarget {
  pub name: String,
  pub id: Option<u64>,
  /// Durable external id (32 hex chars) — the cross-generation bookmark form. Also accepted
  /// on every surface as an `eid:<hex>` name.
  pub external_id: Option<u128>,
  pub path_suffix: Option<String>,
  pub kind: Option<String>,
  pub merge_all: bool,
  /// Suffix every result line with its node id (scripting/agents; default keeps the
  /// human-stable format).
  pub show_ids: bool,
}

pub fn graph_query(index_dir: &Path, verb: &str, name: &str) -> Result<String, Box<dyn Error>> {
  graph_query_selected(
    index_dir,
    verb,
    &GraphTarget {
      name: name.to_string(),
      ..GraphTarget::default()
    },
  )
}

/// Run one graph verb against a selected target. Identity contract (IMPROVEMENTS §1): a
/// name that matches several definitions is answered with the **candidates**, not a silent
/// union — refine with `id`/`path_suffix`/`kind`, or pass `merge_all` to union explicitly.
/// Answer "why does this relation exist?" for the edge(s) `from_id → to_id` (§5): every
/// retained occurrence's edge type, resolution grade + resolver reason, candidate count, and
/// the source span of the referencing token — with a best-effort snippet of the *current* file
/// bytes at that span (labeled as such; files may have drifted since indexing, exactly as with
/// `fetch_span`).
pub fn explain_edge(
  index_dir: &Path,
  from_id: u64,
  to_id: u64,
) -> Result<String, Box<dyn Error>> {
  // Pin one generation for the whole answer: the graph, the evidence rows, and the snippet
  // digest check all come from the directory CURRENT names right now.
  let dir = vorpal_kg::resolve_index_dir(index_dir);
  let kg = Kg::load(&dir)?;
  explain_edge_on(&kg, Some(&dir), from_id, to_id)
}

/// [`explain_edge`] over an already-loaded graph — the daemon form, which answers from its
/// pinned generation instead of re-resolving the index path (a concurrent `CURRENT` swap can
/// never split the answer from the query that produced the ids). `artifacts_dir` is the
/// generation directory the graph was loaded from: when given, the snippet is shown only if
/// the file's current bytes still match the digest that generation indexed — otherwise the
/// snippet is omitted with an explicit note, never silently inconsistent.
pub fn explain_edge_on(
  kg: &Kg,
  artifacts_dir: Option<&Path>,
  from_id: u64,
  to_id: u64,
) -> Result<String, Box<dyn Error>> {
  let (from, to) = (NodeId::new(from_id), NodeId::new(to_id));
  let (Some(from_view), Some(to_view)) = (kg.node(from), kg.node(to)) else {
    return Err(format!("no such node id ({from_id} and/or {to_id})").into());
  };
  let rows = kg.edge_evidence(from, to);
  if rows.is_empty() {
    return Ok(format!(
      "(no recorded evidence for {} → {} — structural edges carry none, and generations \
       written before the evidence sidecar record none)\n",
      from_view.name, to_view.name
    ));
  }
  let mut out = String::new();
  let _ = writeln!(
    out,
    "{} → {}  ({} occurrence{})",
    from_view.name,
    to_view.name,
    rows.len(),
    if rows.len() == 1 { "" } else { "s" }
  );
  let from_path = from_view.path.to_string();
  for row in rows {
    let reason = vorpal_ingest::ResolveReason::from_tag(row.reason).label();
    let grade = confidence_label(row.confidence);
    let _ = writeln!(
      out,
      "  {}  [{grade}; {reason}; {} candidate{}]  {}:{}..{}",
      vorpal_kg::EdgeType(row.etype).name(),
      row.candidates,
      if row.candidates == 1 { "" } else { "s" },
      from_path,
      row.span_start,
      row.span_end
    );
    // "Why this target and not the alternatives?" — the retained tie-set losers, by identity.
    if !row.alternatives.is_empty() {
      let listed: Vec<String> = row
        .alternatives
        .iter()
        .map(|&alt| match kg.node(NodeId::new(alt as u64)) {
          Some(view) => format!("id {alt} ({} {})", view.name, view.path),
          None => format!("id {alt}"),
        })
        .collect();
      let more = (row.candidates as usize).saturating_sub(1 + row.alternatives.len());
      let suffix = if more > 0 {
        format!(" (+{more} more not retained)")
      } else {
        String::new()
      };
      let _ = writeln!(out, "    beat: {}{}", listed.join(", "), suffix);
    }
    // Snippet, digest-verified against the pinned generation (IMPROVEMENTS 07-29 §4): shown
    // only when the file's current bytes still match the digest this generation indexed, so
    // the rendered token can never be silently inconsistent with the edge. Without a pack
    // digest to check (older generation), the snippet is labeled as current-file contents.
    if let Ok(bytes) = fs::read(&from_path) {
      let indexed_digest = artifacts_dir
        .and_then(|dir| PackReader::open(dir))
        .and_then(|pack| {
          pack
            .get(&from_path)
            .and_then(vorpal_ingest::peek_product_digest)
        });
      let (verdict, show) = match indexed_digest {
        Some(digest) if xxhash_rust::xxh3::xxh3_64(&bytes) == digest => ("source verified", true),
        Some(_) => ("", false),
        None => ("current file contents", true),
      };
      if show {
        let (s, e) = (row.span_start as usize, row.span_end as usize);
        if e <= bytes.len() && s < e && e - s <= 200 {
          if let Ok(text) = std::str::from_utf8(&bytes[s..e]) {
            let _ = writeln!(out, "    `{}` ({verdict})", text.trim());
          }
        }
      } else {
        let _ = writeln!(out, "    (file changed since indexing — snippet omitted)");
      }
    }
  }
  Ok(out)
}

/// A source read checked against the generation that indexed it (IMPROVEMENTS #7): persisted
/// byte offsets are only meaningful against the bytes they were computed from.
pub enum IndexedRead {
  /// The file's current bytes match the indexed digest — offsets are exact.
  Verified(Vec<u8>),
  /// No digest is available to check (generation predates pack digests) — the caller may
  /// slice, labeled as current-file contents.
  Unverified(Vec<u8>),
  /// The file changed since indexing: slicing persisted offsets would return bytes
  /// inconsistent with the node. Callers must refuse, not guess.
  Changed,
}

/// Read `path` and verify it against the digest `artifacts_dir`'s product pack recorded at
/// indexing time (the same check `why` snippets use).
pub fn read_indexed_source(
  artifacts_dir: Option<&Path>,
  path: &str,
) -> Result<IndexedRead, String> {
  let bytes = fs::read(path).map_err(|err| format!("read {path}: {err}"))?;
  let indexed_digest = artifacts_dir
    .and_then(PackReader::open)
    .and_then(|pack| pack.get(path).and_then(vorpal_ingest::peek_product_digest));
  Ok(match indexed_digest {
    Some(digest) if xxhash_rust::xxh3::xxh3_64(&bytes) == digest => IndexedRead::Verified(bytes),
    Some(_) => IndexedRead::Changed,
    None => IndexedRead::Unverified(bytes),
  })
}

/// Answer "why is there NO edge from this node to anything named `name`?" — the no-edge
/// occurrences (external/masked) the resolver retained for that referenced name, plus any real
/// edges that DO exist to nodes of that name (so a partial answer is never mistaken for none).
pub fn explain_absence_on(kg: &Kg, from_id: u64, name: &str) -> Result<String, Box<dyn Error>> {
  let from = NodeId::new(from_id);
  let Some(from_view) = kg.node(from) else {
    return Err(format!("no such node id {from_id}").into());
  };
  let name_hash = xxhash_rust::xxh3::xxh3_64(name.as_bytes()) as u32;
  let absences = kg.evidence_absences(from, name_hash);
  let mut out = String::new();
  // Real edges to same-named nodes first: absence of SOME occurrences ≠ absence of all.
  let mut edges = 0usize;
  for target in kg.nodes_named(name) {
    for row in kg.edge_evidence(from, target) {
      edges += 1;
      let _ = writeln!(
        out,
        "  edge exists: {} → {}  ({}; {})",
        from_view.name,
        name,
        vorpal_kg::EdgeType(row.etype).name(),
        confidence_label(row.confidence)
      );
    }
  }
  if absences.is_empty() && edges == 0 {
    return Ok(format!(
      "(no retained occurrences from {} referencing '{name}' — the reference may not exist,        or the generation predates unresolved-evidence rows)
",
      from_view.name
    ));
  }
  for row in absences {
    let verdict = match row.outcome {
      vorpal_kg::EvidenceOutcome::External => {
        "external: no definition with this name exists in the indexed tree".to_string()
      }
      _ => format!(
        "masked: {} definition{} exist{} but none is safely attributable from this site",
        row.candidates,
        if row.candidates == 1 { "" } else { "s" },
        if row.candidates == 1 { "s" } else { "" }
      ),
    };
    let _ = writeln!(
      out,
      "  no {} edge  [{verdict}]  {}:{}..{}",
      vorpal_kg::EdgeType(row.etype).name(),
      from_view.path,
      row.span_start,
      row.span_end
    );
  }
  Ok(out)
}

/// Map a grade label to the confidence floor traversal enforces (`exact` > `constrained` >
/// `heuristic`); `None`/empty = no floor (structural edges included).
pub fn min_confidence_for_grade(grade: Option<&str>) -> Result<u8, Box<dyn Error>> {
  Ok(match grade.map(str::to_ascii_lowercase).as_deref() {
    None | Some("") => 0,
    Some("heuristic") => 1,
    Some("constrained") => 90,
    Some("exact") => 100,
    Some(other) => {
      return Err(format!("unknown grade '{other}' (exact | constrained | heuristic)").into());
    }
  })
}

/// The relation-specific, selector-consistent, grade-filtered traversal every surface shares
/// (IMPROVEMENTS 07-29 §6): resolve `target` through the same selector contract as the direct
/// graph verbs (ambiguous names list candidates), traverse only `relations` at
/// `min_confidence`+ up to `max_depth`, and render each reached node **with its path** back to
/// the seed — per-edge relation names included, so a traversal answer is auditable, not a bare
/// node set.
/// Resolve a [`GraphTarget`] to its matching node ids — the one selector implementation every
/// surface shares (rendered queries, typed [`records`], and the `eid:<hex>`-as-name wire
/// form).
pub fn resolve_target(kg: &Kg, target: &GraphTarget) -> Result<Vec<NodeId>, Box<dyn Error>> {
  let kind = match target.kind.as_deref() {
    Some(text) => Some(
      vorpal_kg::SymbolKind::parse(text).ok_or_else(|| format!("unknown symbol kind '{text}'"))?,
    ),
    None => None,
  };
  let (name, eid_from_name) = match target.name.strip_prefix("eid:") {
    Some(hex) => (
      "",
      Some(u128::from_str_radix(hex, 16).map_err(|_| format!("malformed external id '{hex}'"))?),
    ),
    None => (target.name.as_str(), None),
  };
  let selector = vorpal_kg::SymbolSelector {
    id: target.id,
    name: (!name.is_empty()).then_some(name),
    path_suffix: target.path_suffix.as_deref(),
    kind,
    external_id: target.external_id.or(eid_from_name),
  };
  Ok(kg.select(&selector))
}

pub fn reachable_query_on(
  kg: &Kg,
  target: &GraphTarget,
  dir: vorpal_kg::Direction,
  relations: &[vorpal_kg::EdgeType],
  max_depth: Option<u32>,
  min_confidence: u8,
) -> Result<String, Box<dyn Error>> {
  let matches = resolve_target(kg, target)?;
  if matches.is_empty() {
    return Ok(format!(
      "(no results for '{}' — no symbol matches that selector)
",
      target.name
    ));
  }
  if matches.len() > 1 && !target.merge_all {
    let mut out = format!(
      "ambiguous: '{}' matches {} definitions — refine with --path/--kind/--id, or --all to merge:
",
      target.name,
      matches.len()
    );
    out.push_str(&render_candidates(kg, &matches));
    return Ok(out);
  }

  let mut out = String::new();
  for &seed in &matches {
    // Parent-edge map for path reconstruction; steps arrive in BFS order (deterministic).
    let steps = kg.reachable_via_paths(seed, dir, relations, max_depth, min_confidence);
    let mut parent: HashMap<u32, (u32, vorpal_kg::EdgeType)> = HashMap::new();
    for step in &steps {
      parent.insert(step.node, step.via);
    }
    let seed_raw = seed.raw() as u32;
    let name_of = |id: u32| {
      kg.node(NodeId::new(id as u64))
        .map(|v| v.name.to_string())
        .unwrap_or_else(|| format!("<{id}>"))
    };
    for step in &steps {
      let Some(view) = kg.node(NodeId::new(step.node as u64)) else {
        continue;
      };
      // Reconstruct the BFS path: node ← … ← seed, rendered seed-first.
      let mut chain: Vec<String> = Vec::new();
      let mut at = step.node;
      while at != seed_raw {
        let Some(&(up, edge)) = parent.get(&at) else {
          break;
        };
        chain.push(format!("-{}→ {}", edge.name(), name_of(at)));
        at = up;
      }
      chain.reverse();
      let _ = writeln!(
        out,
        "{} [{:?}] {} (id {}; depth {}; {} {})",
        view.name,
        view.kind,
        view.path,
        step.node,
        step.depth,
        name_of(seed_raw),
        chain.join(" ")
      );
    }
  }
  if out.is_empty() {
    out = format!("(nothing reachable from '{}' under those filters)
", target.name);
  }
  Ok(out)
}

pub fn graph_query_selected(
  index_dir: &Path,
  verb: &str,
  target: &GraphTarget,
) -> Result<String, Box<dyn Error>> {
  let kg = Kg::load(index_dir)?;
  graph_query_on(&kg, verb, target)
}

/// [`graph_query_selected`] over an already-loaded graph — the daemon path, which serves
/// from its warm cached [`Kg`] instead of re-opening artifacts per tool call.
pub fn graph_query_on(kg: &Kg, verb: &str, target: &GraphTarget) -> Result<String, Box<dyn Error>> {
  let matches = resolve_target(kg, target)?;
  if matches.is_empty() {
    return Ok(format!(
      "(no results for '{}' — no symbol matches that selector)\n",
      target.name
    ));
  }

  let edge = match verb {
    "callers" => Some(vorpal_kg::EdgeType::CALLS),
    "refs" | "references" => Some(vorpal_kg::EdgeType::REFERENCES),
    "importers" => Some(vorpal_kg::EdgeType::IMPORTS),
    "implementors" => Some(vorpal_kg::EdgeType::IMPLEMENTS),
    "typeusers" => Some(vorpal_kg::EdgeType::OF_TYPE),
    "node" => None,
    other => return Err(format!("unknown graph verb '{other}'").into()),
  };

  // `node` is a listing verb: every match IS the answer (ids attached — this verb exists to
  // discover identities for refinement).
  let Some(edge) = edge else {
    return Ok(render_candidates(kg, &matches));
  };

  if matches.len() > 1 && !target.merge_all {
    let mut out = format!(
      "ambiguous: '{}' matches {} definitions — refine with --path/--kind/--id, or --all to merge:\n",
      target.name,
      matches.len()
    );
    out.push_str(&render_candidates(kg, &matches));
    return Ok(out);
  }

  let mut hits: Vec<(NodeId, u8)> = Vec::new();
  for &target_id in &matches {
    for (from, confidence) in kg.incoming_with_confidence(target_id, edge) {
      hits.push((from, confidence));
    }
  }
  hits.sort_unstable_by_key(|&(n, c)| (n.raw(), std::cmp::Reverse(c)));
  hits.dedup_by_key(|&mut (n, _)| n);
  Ok(if hits.is_empty() {
    format!("(no results for '{}')\n", target.name)
  } else if target.show_ids {
    // Identity + evidence mode: each result carries its node id and the edge's resolution
    // confidence label — approximate edges are visibly approximate (IMPROVEMENTS §5).
    let mut out = String::new();
    for &(id, confidence) in &hits {
      if let Some(view) = kg.node(id) {
        let _ = writeln!(
          out,
          "{} [{:?}] {} (id {}; {})",
          view.name,
          view.kind,
          view.path,
          id.raw(),
          confidence_label(confidence)
        );
      }
    }
    out
  } else {
    let ids: Vec<NodeId> = hits.iter().map(|&(id, _)| id).collect();
    format_nodes(kg, &ids)
  })
}

/// The grade of a packed edge confidence. A confidence of `0` is a *structural* edge (containment
/// like `defines`/`has_method`, certain by construction) — not an unresolved reference, which
/// never becomes an edge; everything else carries a resolution grade from the single shared
/// vocabulary ([`vorpal_ingest::ResolutionGrade`]), so a caller can tell an exact binding from a
/// heuristic guess.
fn confidence_label(confidence: u8) -> &'static str {
  if confidence == 0 {
    return "structural";
  }
  vorpal_ingest::ResolutionGrade::from_confidence(vorpal_ingest::Confidence(confidence)).label()
}

/// Render selector candidates with their identities — enough to refine to exactly one.
fn render_candidates(kg: &Kg, ids: &[NodeId]) -> String {
  let mut out = String::new();
  for &id in ids {
    if let Some(view) = kg.node(id) {
      let signature = if view.signature.is_empty() {
        String::new()
      } else {
        format!("  {}", view.signature)
      };
      let eid = view
        .external_id
        .map(|e| format!("  eid:{e:032x}"))
        .unwrap_or_default();
      let _ = writeln!(
        out,
        "id {}  {} [{:?}] {}{}{}",
        id.raw(),
        view.name,
        view.kind,
        view.path,
        signature,
        eid
      );
    }
  }
  out
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
  use super::rrf_fuse_explained;

  fn rrf_fuse(lists: &[Vec<u64>], k: usize) -> Vec<(u64, f32)> {
    rrf_fuse_explained(lists, k)
      .into_iter()
      .map(|(id, score, _)| (id, score))
      .collect()
  }

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
