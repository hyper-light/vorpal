//! The text tier's index-side glue: per-bucket content folds from the product pack, the
//! heal that fills uncovered buckets from source, a process-wide cache of open stores, and
//! the candidate step every byte-level search shares (`code_search`, `structural_search`,
//! `rule_search`, `text_search`).
//!
//! The family itself (`trigrams/<k>.tri`) lives in `vorpal_kg::trigramstore`; this module
//! knows the pack (which files a bucket holds and their source digests) and the tree root
//! (where to read them). A bucket is live only while its recorded fold equals the fold this
//! module derives from the current pack — so a stale bucket is uncovered, never wrong.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use vorpal_core::trigram::{MaskMap, PlanTerm, extract, extract_packed, plan};
use vorpal_ingest::PackReader;
use vorpal_kg::identity::FileKey;
use vorpal_kg::trigramstore::{self, CandidateSet, FileTrigrams, TrigramStore, Verdict};

/// Files above this size are recorded unindexed (always candidates) rather than extracted:
/// generated giants contribute few distinct trigrams per byte and dominate build memory.
pub const MAX_INDEXED_FILE_BYTES: u64 = 16 << 20;

/// `VORPAL_NO_TEXT_INDEX=1` disables the tier in this process — the parity oracle's lever.
pub fn text_index_disabled() -> bool {
  std::env::var("VORPAL_NO_TEXT_INDEX").is_ok_and(|v| v == "1")
}

/// The current per-bucket content folds of a bucketed pack: for each bucket, the fold of its
/// live files' `(file_key, source_xxh3)` pairs. `None` for a flat pack or one loaded without a
/// consistent TOC (the tier then stays absent for this generation).
pub fn content_folds(pack: &PackReader) -> Option<Vec<u64>> {
  let buckets = pack.bucket_meta()?.len();
  if buckets == 0 || !pack.is_bucketed() {
    return None;
  }
  let mask = buckets as u64 - 1;
  let mut per: Vec<Vec<(u64, u64)>> = vec![Vec::new(); buckets];
  for (path, body) in pack.entries() {
    let key = FileKey::of(path).0;
    let digest = vorpal_ingest::peek_product_digest(body)?;
    per[(key & mask) as usize].push((key, digest));
  }
  Some(per.iter_mut().map(trigramstore::content_fold).collect())
}

/// An open text index for one generation: the mapped family plus the folds it was validated
/// against.
pub struct TextIndex {
  store: TrigramStore,
  folds: Vec<u64>,
}

impl TextIndex {
  /// Open the family under `generation_dir`, validated against `pack`. `None` when the
  /// family is absent (nothing to prune with — a heal fills it).
  pub fn open(generation_dir: &Path, pack: &PackReader) -> Option<TextIndex> {
    if text_index_disabled() {
      return None;
    }
    let folds = content_folds(pack)?;
    let store = TrigramStore::open(generation_dir, &folds)?;
    Some(TextIndex { store, folds })
  }

  /// `(live, total)` buckets.
  pub fn coverage(&self) -> (u32, u32) {
    self.store.coverage()
  }

  pub fn is_fresh(&self) -> bool {
    let (live, total) = self.coverage();
    live == total
  }

  /// A one-word status for reports: `fresh`, `partial(live/total)`.
  pub fn status(&self) -> String {
    let (live, total) = self.coverage();
    if live == total {
      "fresh".to_string()
    } else {
      format!("partial({live}/{total})")
    }
  }

  /// The candidate set for an AND plan.
  pub fn candidates(&self, terms: &[PlanTerm]) -> CandidateSet {
    self.store.candidates(terms)
  }

  /// The candidate set for a matcher's required literals, or `None` when no literal is at
  /// least three bytes long (nothing can be pruned).
  pub fn candidates_for_literals(&self, literals: &[&str]) -> Option<CandidateSet> {
    let bytes: Vec<&[u8]> = literals.iter().map(|l| l.as_bytes()).collect();
    let terms = plan(&bytes)?;
    Some(self.candidates(&terms))
  }

  /// The union of each branch's candidate set (a disjunction of AND-plans, see
  /// [`vorpal_core::matcher::regex_literal_branches`]). `None` when any branch has no
  /// literal of three bytes or more — that branch could match anywhere, nothing prunes.
  pub fn candidates_for_branches(&self, branches: &[Vec<String>]) -> Option<CandidateSet> {
    let mut union: Option<CandidateSet> = None;
    for branch in branches {
      let refs: Vec<&str> = branch.iter().map(String::as_str).collect();
      let set = self.candidates_for_literals(&refs)?;
      match union.as_mut() {
        Some(u) => u.union_with(&set),
        None => union = Some(set),
      }
    }
    union
  }

  pub fn verdict(&self, set: &CandidateSet, file_key: u64) -> Verdict {
    self.store.verdict(set, file_key)
  }

  pub fn folds(&self) -> &[u64] {
    &self.folds
  }
}

type IndexCache = Mutex<Vec<(PathBuf, Arc<TextIndex>)>>;
static CACHE: OnceLock<IndexCache> = OnceLock::new();
const CACHE_CAP: usize = 8;

/// Process-wide cache of open text indexes keyed by the generation dir (immutable content-
/// addressed dirs make this safe, exactly like `cached_pack`); a heal invalidates its entry.
pub fn cached(generation_dir: &Path, pack: &PackReader) -> Option<Arc<TextIndex>> {
  if text_index_disabled() {
    return None;
  }
  let cache = CACHE.get_or_init(|| Mutex::new(Vec::new()));
  {
    let mut guard = cache.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(pos) = guard.iter().position(|(dir, _)| dir == generation_dir) {
      let entry = guard.remove(pos);
      let index = entry.1.clone();
      guard.push(entry);
      return Some(index);
    }
  }
  let index = Arc::new(TextIndex::open(generation_dir, pack)?);
  let mut guard = cache.lock().unwrap_or_else(|p| p.into_inner());
  if let Some(pos) = guard.iter().position(|(dir, _)| dir == generation_dir) {
    return Some(guard[pos].1.clone());
  }
  guard.push((generation_dir.to_path_buf(), index.clone()));
  if guard.len() > CACHE_CAP {
    guard.remove(0);
  }
  Some(index)
}

fn invalidate(generation_dir: &Path) {
  if let Some(cache) = CACHE.get() {
    let mut guard = cache.lock().unwrap_or_else(|p| p.into_inner());
    guard.retain(|(dir, _)| dir != generation_dir);
  }
}

/// What a heal did.
#[derive(Debug, Clone, Copy, Default)]
pub struct HealReport {
  pub rebuilt_buckets: u32,
  pub total_buckets: u32,
  pub files: u64,
  pub unindexed: u64,
  pub postings: u64,
  pub secs: f64,
}

/// Extract one file's contribution as the generation indexed it: the bytes at `path` must
/// hash to the product's source digest, else the file is recorded unindexed (a candidate for
/// every query — the tier never guesses at bytes it did not see).
pub fn file_trigrams(
  root: &str,
  rel_path: &str,
  product: &[u8],
  scratch: &mut MaskMap,
) -> FileTrigrams {
  let mut read = ReadScratch::default();
  file_trigrams_with(root, rel_path, product, scratch, &mut read)
}

/// Per-thread read buffers for the heal: one file buffer and one path buffer reused across
/// every file a worker handles, so the corpus streams through a few resident pages instead
/// of faulting a fresh buffer per file (measured: 318k minor faults and 6.3 GB of churn for
/// 1.4 GB of kernel source before this).
#[derive(Default)]
pub struct ReadScratch {
  bytes: Vec<u8>,
  path: String,
}

fn file_trigrams_with(
  root: &str,
  rel_path: &str,
  product: &[u8],
  scratch: &mut MaskMap,
  read: &mut ReadScratch,
) -> FileTrigrams {
  use std::io::Read;
  let key = FileKey::of(rel_path).0;
  let unindexed = FileTrigrams {
    file_key: key,
    indexed: false,
    postings: Vec::new(),
  };
  let Some(digest) = vorpal_ingest::peek_product_digest(product) else {
    return unindexed;
  };
  read.path.clear();
  read.path.push_str(root);
  read.path.push('/');
  read.path.push_str(rel_path);
  let Ok(mut file) = std::fs::File::open(&read.path) else {
    return unindexed;
  };
  let Ok(meta) = file.metadata() else {
    return unindexed;
  };
  if meta.len() > MAX_INDEXED_FILE_BYTES {
    return unindexed;
  }
  read.bytes.clear();
  read.bytes.reserve(meta.len() as usize);
  if file.read_to_end(&mut read.bytes).is_err() {
    return unindexed;
  }
  if xxhash_rust::xxh3::xxh3_64(&read.bytes) != digest {
    return unindexed;
  }
  let mut postings = Vec::new();
  extract(&read.bytes, scratch, &mut postings);
  FileTrigrams {
    file_key: key,
    indexed: true,
    postings,
  }
}

/// A heal worker's whole working set, taken from a pool per bucket and returned after: the
/// mask map, the read buffers, the packed posting buffer and the slab output buffer. Every
/// buffer keeps its capacity across buckets, so after the first buckets a worker touches no
/// new pages — rayon's `fold`/`map_init` hand out state per *split*, which re-grew and
/// re-faulted these buffers dozens of times per bucket (measured: 569k faults for the kernel
/// heal); a pool bounds them to one set per concurrently healing bucket.
#[derive(Default)]
struct HealScratch {
  map: MaskMap,
  read: ReadScratch,
  flat: Vec<u64>,
  out: Vec<u8>,
}

/// Build and write one bucket from source into the reused `scratch`, sequentially over its
/// files (ordinal = position in the key-sorted file table). The packed buffer is reserved
/// once from the bucket's source size (kernel C yields ~0.09 distinct trigrams per byte) and
/// the slab is written through the same output buffer every time.
fn heal_bucket(
  generation_dir: &Path,
  pack: &PackReader,
  root: &str,
  k: u32,
  fold: u64,
  scratch: &mut HealScratch,
) -> std::io::Result<(trigramstore::HealRow, u64)> {
  let mut entries: Vec<(u64, &str, &[u8])> = pack
    .bucket_entries(k)
    .into_iter()
    .map(|(rel, body)| (FileKey::of(rel).0, rel, body))
    .collect();
  entries.sort_unstable_by_key(|(key, ..)| *key);
  entries.dedup_by_key(|(key, ..)| *key);
  let file_keys: Vec<u64> = entries.iter().map(|(key, ..)| *key).collect();
  let source_bytes: u64 = entries
    .iter()
    .filter_map(|(_, _, body)| vorpal_ingest::peek_product_stamps(body).map(|(size, _)| size))
    .sum();
  scratch.flat.clear();
  // Measured on the kernel: 130.4 M postings over 1.4 GB of source, 0.093 per byte; 0.11
  // leaves slack for denser trees without doubling the pooled buffer's footprint.
  scratch.flat.reserve((source_bytes as usize / 100) * 11);
  let mut unindexed: Vec<u32> = Vec::new();
  for (ordinal, (_, rel, body)) in entries.iter().enumerate() {
    if !extract_file_packed(root, rel, body, ordinal as u32, &mut scratch.map, &mut scratch.read, &mut scratch.flat) {
      unindexed.push(ordinal as u32);
    }
  }
  let row = trigramstore::heal_write_slab(generation_dir, k, fold, &file_keys, &unindexed, &mut scratch.flat, &mut scratch.out)?;
  Ok((row, unindexed.len() as u64))
}

/// Read `rel_path` through the thread's buffers, verify it against the product's source
/// digest, and append its packed postings for `ordinal`; `false` when the file could not be
/// read as the generation indexed it (the caller records it unindexed).
fn extract_file_packed(
  root: &str,
  rel_path: &str,
  product: &[u8],
  ordinal: u32,
  scratch: &mut MaskMap,
  read: &mut ReadScratch,
  flat: &mut Vec<u64>,
) -> bool {
  use std::io::Read;
  let Some(digest) = vorpal_ingest::peek_product_digest(product) else {
    return false;
  };
  read.path.clear();
  read.path.push_str(root);
  read.path.push('/');
  read.path.push_str(rel_path);
  let Ok(mut file) = std::fs::File::open(&read.path) else {
    return false;
  };
  let Ok(meta) = file.metadata() else {
    return false;
  };
  if meta.len() > MAX_INDEXED_FILE_BYTES {
    return false;
  }
  read.bytes.clear();
  read.bytes.reserve(meta.len() as usize);
  if file.read_to_end(&mut read.bytes).is_err() {
    return false;
  }
  if xxhash_rust::xxh3::xxh3_64(&read.bytes) != digest {
    return false;
  }
  extract_packed(&read.bytes, ordinal, scratch, flat);
  true
}

/// Per-generation lookup from file key to run index, plus the languages present — built once
/// per generation (the runs are immutable per content-addressed dir) so a query with a
/// complete candidate set touches only its admitted files instead of every run.
pub struct RunIndex {
  by_key: HashMap<u64, u32>,
  langs: Vec<vorpal_ingest::SgLang>,
  /// Files per language (`None` = no language maps to the path), for population counts.
  counts: Vec<(Option<vorpal_ingest::SgLang>, u64)>,
}

impl RunIndex {
  fn build(runs: &[crate::annfiles::FileRun], pack: &PackReader) -> RunIndex {
    use vorpal_language::Language;
    let mut by_key = HashMap::with_capacity(runs.len());
    let mut langs: Vec<vorpal_ingest::SgLang> = Vec::new();
    let mut counts: Vec<(Option<vorpal_ingest::SgLang>, u64)> = Vec::new();
    for (i, run) in runs.iter().enumerate() {
      by_key.insert(FileKey::of(pack.stored_key(&run.path)).0, i as u32);
      let lang = vorpal_ingest::SgLang::from_path(&run.path);
      if let Some(lang) = lang
        && !langs.contains(&lang)
      {
        langs.push(lang);
      }
      match counts.iter_mut().find(|(l, _)| *l == lang) {
        Some(row) => row.1 += 1,
        None => counts.push((lang, 1)),
      }
    }
    RunIndex { by_key, langs, counts }
  }
  /// How many of the generation's files satisfy `keep` on their language.
  pub fn count_where(&self, keep: impl Fn(Option<vorpal_ingest::SgLang>) -> bool) -> u64 {
    self.counts.iter().filter(|(lang, _)| keep(*lang)).map(|(_, n)| n).sum()
  }
  /// The run index of `file_key`, if the generation indexed that file.
  pub fn run_of(&self, file_key: u64) -> Option<u32> {
    self.by_key.get(&file_key).copied()
  }
  /// Every language the generation's files map to.
  pub fn langs(&self) -> &[vorpal_ingest::SgLang] {
    &self.langs
  }
}

type RunIndexCache = Mutex<Vec<(PathBuf, Arc<RunIndex>)>>;
static RUN_INDEX: OnceLock<RunIndexCache> = OnceLock::new();

/// The cached [`RunIndex`] of a generation.
pub fn cached_run_index(generation_dir: &Path, runs: &[crate::annfiles::FileRun], pack: &PackReader) -> Arc<RunIndex> {
  let cache = RUN_INDEX.get_or_init(|| Mutex::new(Vec::new()));
  {
    let mut guard = cache.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(pos) = guard.iter().position(|(dir, _)| dir == generation_dir) {
      let entry = guard.remove(pos);
      let index = entry.1.clone();
      guard.push(entry);
      return index;
    }
  }
  let index = Arc::new(RunIndex::build(runs, pack));
  let mut guard = cache.lock().unwrap_or_else(|p| p.into_inner());
  if let Some(pos) = guard.iter().position(|(dir, _)| dir == generation_dir) {
    return guard[pos].1.clone();
  }
  guard.push((generation_dir.to_path_buf(), index.clone()));
  if guard.len() > CACHE_CAP {
    guard.remove(0);
  }
  index
}

/// Fill every uncovered bucket of the generation under `generation_dir` from source. One heal
/// per directory at a time in this process (a second caller waits, then finds nothing to do).
/// `Ok(None)` when the generation cannot carry the tier (flat pack, no derivable root).
pub fn heal(generation_dir: &Path) -> std::io::Result<Option<HealReport>> {
  use rayon::prelude::*;
  if text_index_disabled() {
    return Ok(None);
  }
  static HEALS: OnceLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> = OnceLock::new();
  let dir_lock = {
    let mut map = HEALS
      .get_or_init(|| Mutex::new(HashMap::new()))
      .lock()
      .unwrap_or_else(|p| p.into_inner());
    map.entry(generation_dir.to_path_buf()).or_default().clone()
  };
  let _guard = dir_lock.lock().unwrap_or_else(|p| p.into_inner());
  let started = Instant::now();
  let Some(pack) = crate::cached_pack(generation_dir) else {
    return Ok(None);
  };
  let Some(root) = pack.root().map(str::to_string) else {
    return Ok(None);
  };
  let Some(folds) = content_folds(&pack) else {
    return Ok(None);
  };
  let buckets = folds.len() as u32;
  let uncovered: Vec<u32> = match TrigramStore::open(generation_dir, &folds) {
    Some(store) => (0..buckets).filter(|&k| !store.bucket_is_live(k)).collect(),
    None => (0..buckets).collect(),
  };
  let mut report = HealReport {
    total_buckets: buckets,
    ..HealReport::default()
  };
  if uncovered.is_empty() {
    return Ok(Some(report));
  }
  vorpal_kg::phase_stamp(&format!("trigrams heal: {} of {} buckets", uncovered.len(), buckets));
  // Buckets are rebuilt a chunk at a time and written as each chunk completes, so the
  // working set is a few buckets' postings, not the corpus's (measured 2.4 GB RSS on the
  // kernel when every bucket was held before the write; the chunked heal holds ~8).
  // Buckets heal in parallel, each sequential over its files with one pooled scratch;
  // a chunk's rows commit to the TOC together so an interrupted heal leaves a consistent
  // family behind.
  const CHUNK: usize = 16;
  let pool: Mutex<Vec<HealScratch>> = Mutex::new(Vec::new());
  for chunk in uncovered.chunks(CHUNK) {
    let healed: std::io::Result<Vec<(trigramstore::HealRow, u64)>> = chunk
      .par_iter()
      .map(|&k| {
        let mut scratch = pool.lock().unwrap_or_else(|p| p.into_inner()).pop().unwrap_or_default();
        let result = heal_bucket(generation_dir, &pack, &root, k, folds[k as usize], &mut scratch);
        pool.lock().unwrap_or_else(|p| p.into_inner()).push(scratch);
        result
      })
      .collect();
    let healed = healed?;
    for (row, unindexed) in &healed {
      report.files += u64::from(row.files);
      report.unindexed += unindexed;
      report.postings += row.postings;
    }
    report.rebuilt_buckets += healed.len() as u32;
    vorpal_kg::phase_stamp(&format!("trigrams heal: chunk written ({} buckets)", healed.len()));
    let rows: Vec<trigramstore::HealRow> = healed.into_iter().map(|(row, _)| row).collect();
    trigramstore::heal_commit_rows(generation_dir, buckets, &folds, &rows)?;
    vorpal_kg::phase_stamp("trigrams heal: chunk committed");
  }
  invalidate(generation_dir);
  report.secs = started.elapsed().as_secs_f64();
  vorpal_kg::phase_stamp(&format!(
    "trigrams heal: done ({} buckets, {} files, {} postings, {:.2} s)",
    report.rebuilt_buckets, report.files, report.postings, report.secs
  ));
  Ok(Some(report))
}

/// Heal in the background, single-flight per generation dir: the query that found the tier
/// absent or partial proceeds on the exhaustive path meanwhile. Honors the autowarm veto.
pub fn request_heal(generation_dir: &Path) {
  if text_index_disabled() || std::env::var("VORPAL_NO_AUTOWARM").is_ok_and(|v| v == "1" || v == "true" || v == "yes") {
    return;
  }
  static IN_FLIGHT: OnceLock<Mutex<Vec<PathBuf>>> = OnceLock::new();
  let in_flight = IN_FLIGHT.get_or_init(|| Mutex::new(Vec::new()));
  {
    let mut guard = in_flight.lock().unwrap_or_else(|p| p.into_inner());
    if guard.iter().any(|d| d == generation_dir) {
      return;
    }
    guard.push(generation_dir.to_path_buf());
  }
  let dir = generation_dir.to_path_buf();
  std::thread::spawn(move || {
    let _ = heal(&dir);
    if let Some(in_flight) = IN_FLIGHT.get() {
      let mut guard = in_flight.lock().unwrap_or_else(|p| p.into_inner());
      guard.retain(|d| d != &dir);
    }
  });
}

/// One changed file for the compose-lane delta: its absolute path and the size the manifest
/// recorded (the bytes read must still have that length, else the file is unindexed).
pub struct ChangedFile {
  pub path: String,
  pub size: u64,
}

/// A compose lane's text-tier maintenance, run beside the lane: carry the prior generation's
/// family into `staging` and rebuild exactly the buckets the changed files map to — from the
/// bytes on disk, digest-checked against the length the manifest saw. Every other slab
/// hard-links. `Ok(false)` when the prior carries no family (nothing to maintain; the
/// daemon's heal fills a first family later).
pub fn compose_delta(
  prior: &Path,
  staging: &Path,
  tree_root: &str,
  changed: &[ChangedFile],
) -> std::io::Result<bool> {
  if text_index_disabled() || !prior.join(vorpal_kg::TRIGRAMS_TOC).is_file() {
    return Ok(false);
  }
  vorpal_kg::phase_stamp("trigrams delta: start");
  let Some(mut folds) = trigramstore::toc_folds(prior) else {
    return Ok(false);
  };
  let buckets = folds.len() as u32;
  if buckets == 0 {
    return Ok(false);
  }
  vorpal_kg::carry_family_dir(prior, staging, vorpal_kg::TRIGRAMS_DIR, vorpal_kg::is_trigrams_member)?;
  let Some(pack) = crate::cached_pack(prior) else {
    return Ok(false);
  };
  let mask = u64::from(buckets - 1);
  let mut scratch = MaskMap::default();
  let mut removed: Vec<u64> = Vec::with_capacity(changed.len());
  let mut added: Vec<FileTrigrams> = Vec::with_capacity(changed.len());
  // (file_key → new source digest, or None when the bytes could not be trusted)
  let mut new_digests: HashMap<u64, Option<u64>> = HashMap::new();
  for file in changed {
    let rel = vorpal_kg::identity::tree_relative(&file.path, tree_root);
    let key = FileKey::of(rel).0;
    let bytes = match std::fs::read(&file.path) {
      Ok(bytes) if bytes.len() as u64 == file.size && file.size <= MAX_INDEXED_FILE_BYTES => Some(bytes),
      _ => None,
    };
    removed.push(key);
    match bytes {
      Some(bytes) => {
        let mut postings = Vec::new();
        extract(&bytes, &mut scratch, &mut postings);
        new_digests.insert(key, Some(xxhash_rust::xxh3::xxh3_64(&bytes)));
        added.push(FileTrigrams {
          file_key: key,
          indexed: true,
          postings,
        });
      }
      None => {
        new_digests.insert(key, None);
        added.push(FileTrigrams {
          file_key: key,
          indexed: false,
          postings: Vec::new(),
        });
      }
    }
  }
  // Recompute the fold of every touched bucket from the prior pack's file set with the
  // changed files' digests swapped in; a bucket holding an untrusted file gets fold 0
  // (uncovered) so the heal rebuilds it from source.
  let touched: std::collections::BTreeSet<u32> = removed.iter().map(|k| (k & mask) as u32).collect();
  for k in touched {
    let mut pairs: Vec<(u64, u64)> = Vec::new();
    let mut trusted = true;
    for (rel, body) in pack.bucket_entries(k) {
      let key = FileKey::of(rel).0;
      match new_digests.get(&key) {
        Some(Some(digest)) => pairs.push((key, *digest)),
        Some(None) => {
          trusted = false;
          break;
        }
        None => match vorpal_ingest::peek_product_digest(body) {
          Some(digest) => pairs.push((key, digest)),
          None => {
            trusted = false;
            break;
          }
        },
      }
    }
    folds[k as usize] = if trusted { trigramstore::content_fold(&mut pairs) } else { 0 };
  }
  vorpal_kg::phase_stamp("trigrams delta: carried + changed files extracted");
  trigramstore::apply_delta(staging, prior, buckets, &folds, &removed, added)?;
  vorpal_kg::phase_stamp("trigrams delta: done");
  Ok(true)
}

/// Start [`compose_delta`] on its own thread; the lane joins it with [`finish_compose_delta`]
/// before committing. Failure never fails the lane: the family is dropped from the staged
/// generation and the daemon's heal rebuilds it.
pub fn spawn_compose_delta(
  prior: &Path,
  staging: &Path,
  tree_root: &str,
  changed: Vec<ChangedFile>,
) -> std::thread::JoinHandle<std::io::Result<bool>> {
  let (prior, staging, root) = (prior.to_path_buf(), staging.to_path_buf(), tree_root.to_string());
  std::thread::spawn(move || compose_delta(&prior, &staging, &root, &changed))
}

/// Join a [`spawn_compose_delta`] thread; on any failure remove the staged family so the
/// generation commits without it (absent, never wrong).
pub fn finish_compose_delta(handle: std::thread::JoinHandle<std::io::Result<bool>>, staging: &Path) {
  match handle.join() {
    Ok(Ok(_)) => {}
    Ok(Err(err)) => {
      vorpal_kg::phase_stamp(&format!("trigrams: compose delta dropped the family: {err}"));
      let _ = std::fs::remove_dir_all(staging.join(vorpal_kg::TRIGRAMS_DIR));
    }
    Err(_) => {
      let _ = std::fs::remove_dir_all(staging.join(vorpal_kg::TRIGRAMS_DIR));
    }
  }
}

/// The open pack and text index of a committed generation, for tools that walk the live
/// tree and want the tier's candidate verdicts: `(pack, index)` or `None` when either is
/// unavailable. The pack's root tells the caller how to spell file keys.
pub fn for_generation(generation_dir: &Path) -> Option<(Arc<PackReader>, Arc<TextIndex>)> {
  let pack = crate::cached_pack(generation_dir)?;
  let index = cached(generation_dir, &pack)?;
  Some((pack, index))
}

/// A process-wide pool of file read buffers for the query paths: taken per file, returned
/// after, capacity kept — so a hot query's reads touch pages that are already mapped instead
/// of faulting a fresh buffer per file. Bounded: at most `READ_POOL_CAP` buffers are kept.
/// Buffers above this capacity are dropped on return rather than kept (a giant file must not
/// pin its size for the process's lifetime).
const READ_POOL_MAX_BUFFER: usize = 4 << 20;

std::thread_local! {
  /// This thread's read buffer between files. Rayon's workers persist for the process, so
  /// a buffer taken and given back here is reused across files AND queries with no lock
  /// — the earlier process-wide pool cost two mutex operations per file. A nested take
  /// (none exists today) simply starts from an empty vector.
  static READ_SLOT: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
}

pub fn take_read_buffer() -> Vec<u8> {
  READ_SLOT.with(|slot| std::mem::take(&mut *slot.borrow_mut()))
}

pub fn give_read_buffer(mut buf: Vec<u8>) {
  if buf.capacity() > READ_POOL_MAX_BUFFER {
    return;
  }
  buf.clear();
  READ_SLOT.with(|slot| {
    let mut slot = slot.borrow_mut();
    if slot.capacity() < buf.capacity() {
      *slot = buf;
    }
  });
}
