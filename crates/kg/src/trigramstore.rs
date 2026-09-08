//! The trigram family: the text tier's persisted base — per file-bucket inverted postings
//! `trigram → (file ordinal, next-byte mask)` under the SAME bucket law as every other family
//! (`bucket_of(file) = FileKey & (B-1)`), so one changed file dirties one bucket and every other
//! slab hard-links across generations exactly like `usage/`.
//!
//! A sidecar in policy terms (docs/INDEX_FORMAT.md #4): never part of generation identity, never
//! written by a from-scratch `vorpal index`; the daemon's warm fills it once and the compose
//! lanes keep it current by delta. **Validity is per bucket**: a slab records the fold of its
//! bucket's per-file source digests (`content_fold`), and a reader treats the bucket as live only
//! while that fold equals the current generation's — otherwise the bucket is *uncovered* and
//! every file in it is a candidate. Wrong is impossible; stale is merely slow.
//!
//! Slab (`trigrams/<k>.tri`):
//! `[VTRI][version u32][bucket u32][files u32][content_fold u64][trigrams u32][unindexed u32][pool_len u64][flags u64]`
//! + file table `files × file_key u64` (ascending; ordinal = position)
//! + unindexed `unindexed × ordinal u32` (files the builder could not read as indexed — always candidates)
//! + key table `trigrams × {key u32, pool_off u32, count u32}` (ascending key)
//! + pool: per key `count × {ordinal delta LEB128, next_mask u8}`.
//! TOC (`trigrams/toc.bin`): `[VTRT][version][buckets u32][total postings u64]` +
//! per slab `{files u32, pad u32, len u64, digest u64, content_fold u64}`.

use std::fs;
use std::io::{self, Write};
use std::path::Path;

use vorpal_core::trigram::{PlanTerm, mask_admits, pack as pack_posting};
use vorpal_mem::{AccessPattern, CorpusProbe, Hotness, MappedStore, ResourcePolicy, StoreKind};

pub const TRIGRAMS_DIR: &str = "trigrams";
pub const TRIGRAMS_TOC: &str = "trigrams/toc.bin";
const SLAB_MAGIC: &[u8; 4] = b"VTRI";
const TOC_MAGIC: &[u8; 4] = b"VTRT";
pub const VERSION: u32 = 1;
/// magic + version + bucket + files + content_fold + trigrams + unindexed + pool_len + flags.
const SLAB_HEADER: usize = 48;
/// key u32 + pool_off u32 + count u32.
const KEY_ROW: usize = 12;
/// magic + version + bucket count + total postings.
const TOC_HEADER: usize = 20;
/// files u32 + pad u32 + len u64 + digest u64 + content_fold u64.
const TOC_ROW: usize = 32;

/// Whether `name` (generation-relative) is a trigram-family member.
pub fn is_trigrams_member(name: &str) -> bool {
  if name == TRIGRAMS_TOC {
    return true;
  }
  name
    .strip_prefix("trigrams/")
    .and_then(|f| f.strip_suffix(".tri"))
    .is_some_and(|k| !k.is_empty() && k.len() <= 5 && k.bytes().all(|b| b.is_ascii_digit()))
}

/// One file's contribution: its distinct trigrams with OR'd next-byte masks, sorted by key.
/// `indexed == false` records a file the builder could not read as the generation indexed it
/// (changed since, unreadable, oversized): it stays a candidate for every query, never guessed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileTrigrams {
  pub file_key: u64,
  pub indexed: bool,
  pub postings: Vec<(u32, u8)>,
}

/// Fold the per-file source digests of one bucket into its validity key. Callers pass the
/// bucket's `(file_key, source_xxh3)` pairs in any order; the fold sorts them so it is a pure
/// function of the set — and invariant under stamp-only changes (a touch moves mtime, not bytes).
pub fn content_fold(pairs: &mut Vec<(u64, u64)>) -> u64 {
  pairs.sort_unstable();
  pairs.dedup();
  let mut bytes = Vec::with_capacity(pairs.len() * 16);
  for (key, digest) in pairs.iter() {
    bytes.extend_from_slice(&key.to_le_bytes());
    bytes.extend_from_slice(&digest.to_le_bytes());
  }
  xxhash_rust::xxh3::xxh3_64(&bytes)
}

#[derive(Clone, Copy)]
struct TocRow {
  files: u32,
  len: u64,
  digest: u64,
  content_fold: u64,
}

struct Toc {
  rows: Vec<TocRow>,
}

impl Toc {
  fn load(path: &Path) -> Option<Toc> {
    let bytes = fs::read(path).ok()?;
    if bytes.len() < TOC_HEADER || &bytes[0..4] != TOC_MAGIC {
      return None;
    }
    if u32::from_le_bytes(bytes[4..8].try_into().ok()?) != VERSION {
      return None;
    }
    let buckets = u32::from_le_bytes(bytes[8..12].try_into().ok()?) as usize;
    if bytes.len() != TOC_HEADER + buckets * TOC_ROW {
      return None;
    }
    let mut rows = Vec::with_capacity(buckets);
    for k in 0..buckets {
      let at = TOC_HEADER + k * TOC_ROW;
      let row = &bytes[at..at + TOC_ROW];
      rows.push(TocRow {
        files: u32::from_le_bytes(row[0..4].try_into().ok()?),
        len: u64::from_le_bytes(row[8..16].try_into().ok()?),
        digest: u64::from_le_bytes(row[16..24].try_into().ok()?),
        content_fold: u64::from_le_bytes(row[24..32].try_into().ok()?),
      });
    }
    Some(Toc { rows })
  }
}

fn write_toc(dir: &Path, rows: &[TocRow], total_postings: u64) -> io::Result<()> {
  let family_dir = dir.join(TRIGRAMS_DIR);
  let tmp = family_dir.join("toc.bin.tmp");
  let mut out = fs::File::create(&tmp)?;
  out.write_all(TOC_MAGIC)?;
  out.write_all(&VERSION.to_le_bytes())?;
  out.write_all(&(rows.len() as u32).to_le_bytes())?;
  out.write_all(&total_postings.to_le_bytes())?;
  for row in rows {
    out.write_all(&row.files.to_le_bytes())?;
    out.write_all(&0u32.to_le_bytes())?;
    out.write_all(&row.len.to_le_bytes())?;
    out.write_all(&row.digest.to_le_bytes())?;
    out.write_all(&row.content_fold.to_le_bytes())?;
  }
  drop(out);
  fs::rename(&tmp, dir.join(TRIGRAMS_TOC))
}

fn push_varint(out: &mut Vec<u8>, mut v: u32) {
  while v >= 0x80 {
    out.push((v as u8) | 0x80);
    v >>= 7;
  }
  out.push(v as u8);
}

#[inline]
fn read_varint(bytes: &[u8], at: &mut usize) -> Option<u32> {
  let mut v: u32 = 0;
  let mut shift = 0;
  loop {
    let b = *bytes.get(*at)?;
    *at += 1;
    v |= u32::from(b & 0x7f) << shift;
    if b & 0x80 == 0 {
      return Some(v);
    }
    shift += 7;
    if shift > 28 {
      return None;
    }
  }
}

/// One bucket's build input in packed form: the file table (ascending keys; ordinal =
/// position), the ordinals of files the builder could not read as indexed, and every
/// posting as [`vorpal_core::trigram::pack`]ed u64s in any order. The heal fills these from
/// per-thread buffers with no per-file vector in between.
#[derive(Default)]
pub struct BucketBuild {
  pub bucket: u32,
  pub file_keys: Vec<u64>,
  pub unindexed: Vec<u32>,
  pub flat: Vec<u64>,
}

impl BucketBuild {
  /// The packed build of a sorted `files` slice (ordinal = index).
  fn from_files(bucket: u32, files: &[FileTrigrams], flat: &mut Vec<u64>) -> (Vec<u64>, Vec<u32>) {
    debug_assert!(files.windows(2).all(|w| w[0].file_key < w[1].file_key));
    flat.clear();
    flat.reserve(files.iter().map(|f| f.postings.len()).sum());
    let mut unindexed = Vec::new();
    for (ordinal, file) in files.iter().enumerate() {
      if !file.indexed {
        unindexed.push(ordinal as u32);
      }
      for &(key, mask) in &file.postings {
        flat.push(pack_posting(key, ordinal as u32, mask));
      }
    }
    let _ = bucket;
    (files.iter().map(|f| f.file_key).collect(), unindexed)
  }
}

/// Reusable encode scratch: the packed postings and the output slab. A thread keeps one across
/// buckets so the heal's per-bucket buffers are paged in once, not per bucket.
#[derive(Default)]
pub struct EncodeScratch {
  flat: Vec<u64>,
  out: Vec<u8>,
}

/// Encode a slab into `scratch.out` (cleared first); returns the posting count. `files` must
/// be sorted by `file_key` (ordinal = index).
fn encode_slab_into(bucket: u32, content_fold: u64, files: &[FileTrigrams], scratch: &mut EncodeScratch) -> u64 {
  let (file_keys, unindexed) = BucketBuild::from_files(bucket, files, &mut scratch.flat);
  encode_packed_into(bucket, content_fold, &file_keys, &unindexed, &mut scratch.flat, &mut scratch.out)
}

/// Encode a slab from packed postings: `flat` is sorted in place, `out` is cleared and
/// assembled once — header, file table, unindexed list, key table, pool — nothing copied
/// twice. Returns the posting count.
pub fn encode_packed_into(
  bucket: u32,
  content_fold: u64,
  file_keys: &[u64],
  unindexed_ordinals: &[u32],
  flat: &mut Vec<u64>,
  out: &mut Vec<u8>,
) -> u64 {
  debug_assert!(file_keys.windows(2).all(|w| w[0] < w[1]));
  out.clear();
  flat.sort_unstable();
  let postings_total = flat.len();
  // Distinct keys, for the key-table size (the pool is appended after it).
  let mut trigrams: u32 = 0;
  let mut prev_key: Option<u32> = None;
  for &p in flat.iter() {
    let key = (p >> 32) as u32;
    if prev_key != Some(key) {
      trigrams += 1;
      prev_key = Some(key);
    }
  }
  let keys_off = SLAB_HEADER + file_keys.len() * 8 + unindexed_ordinals.len() * 4;
  let pool_off = keys_off + trigrams as usize * KEY_ROW;
  out.reserve(pool_off + postings_total * 3);
  out.extend_from_slice(SLAB_MAGIC);
  out.extend_from_slice(&VERSION.to_le_bytes());
  out.extend_from_slice(&bucket.to_le_bytes());
  out.extend_from_slice(&(file_keys.len() as u32).to_le_bytes());
  out.extend_from_slice(&content_fold.to_le_bytes());
  out.extend_from_slice(&trigrams.to_le_bytes());
  out.extend_from_slice(&(unindexed_ordinals.len() as u32).to_le_bytes());
  out.extend_from_slice(&0u64.to_le_bytes()); // pool_len, patched below
  out.extend_from_slice(&0u64.to_le_bytes());
  for key in file_keys {
    out.extend_from_slice(&key.to_le_bytes());
  }
  for ordinal in unindexed_ordinals {
    out.extend_from_slice(&ordinal.to_le_bytes());
  }
  debug_assert_eq!(out.len(), keys_off);
  out.resize(pool_off, 0);
  let mut key_row = keys_off;
  let mut i = 0usize;
  while i < flat.len() {
    let key = (flat[i] >> 32) as u32;
    let list_off = (out.len() - pool_off) as u32;
    let mut count: u32 = 0;
    let mut prev: u32 = 0;
    while i < flat.len() && (flat[i] >> 32) as u32 == key {
      let ordinal = ((flat[i] >> 8) & 0x00FF_FFFF) as u32;
      let mask = (flat[i] & 0xFF) as u8;
      push_varint(out, ordinal - prev);
      out.push(mask);
      prev = ordinal;
      count += 1;
      i += 1;
    }
    out[key_row..key_row + 4].copy_from_slice(&key.to_le_bytes());
    out[key_row + 4..key_row + 8].copy_from_slice(&list_off.to_le_bytes());
    out[key_row + 8..key_row + 12].copy_from_slice(&count.to_le_bytes());
    key_row += KEY_ROW;
  }
  let pool_len = (out.len() - pool_off) as u64;
  out[32..40].copy_from_slice(&pool_len.to_le_bytes());
  postings_total as u64
}

/// Encoded slab + its posting count, through a fresh scratch (the delta and test paths).
fn encode_slab(bucket: u32, content_fold: u64, files: &[FileTrigrams]) -> (Vec<u8>, u64) {
  let mut scratch = EncodeScratch::default();
  let postings = encode_slab_into(bucket, content_fold, files, &mut scratch);
  (scratch.out, postings)
}

/// Persist the family: `files` in any order (bucketed and sorted here), `buckets` the family
/// bucket count, `folds[k]` the current content fold of bucket `k`, `prior` the hard-link
/// source. A bucket whose bytes equal the prior's TOC row hard-links instead of writing.
pub fn save(
  dir: &Path,
  files: Vec<FileTrigrams>,
  buckets: u32,
  folds: &[u64],
  prior: Option<&Path>,
) -> io::Result<()> {
  use rayon::prelude::*;
  if buckets == 0 || folds.len() != buckets as usize {
    return Err(io::Error::other("trigram family requires a bucket count with one fold per bucket"));
  }
  let family_dir = dir.join(TRIGRAMS_DIR);
  fs::create_dir_all(&family_dir)?;
  let mut by_bucket: Vec<Vec<FileTrigrams>> = (0..buckets).map(|_| Vec::new()).collect();
  for file in files {
    by_bucket[(file.file_key & u64::from(buckets - 1)) as usize].push(file);
  }
  struct Built {
    files: u32,
    bytes: Vec<u8>,
    digest: u64,
    postings: u64,
  }
  let built: Vec<Built> = by_bucket
    .into_par_iter()
    .enumerate()
    .map(|(bucket, mut files)| {
      files.sort_unstable_by_key(|f| f.file_key);
      files.dedup_by_key(|f| f.file_key);
      let (bytes, postings) = encode_slab(bucket as u32, folds[bucket], &files);
      Built {
        files: files.len() as u32,
        digest: xxhash_rust::xxh3::xxh3_64(&bytes),
        bytes,
        postings,
      }
    })
    .collect();
  let prior_toc = prior.and_then(|p| Toc::load(&p.join(TRIGRAMS_TOC)));
  let prior_ok = prior_toc.as_ref().is_some_and(|toc| toc.rows.len() as u32 == buckets);
  for (bucket, slab) in built.iter().enumerate() {
    let name = format!("{bucket:04}.tri");
    let carried = prior_ok
      && prior_toc.as_ref().is_some_and(|toc| {
        let row = &toc.rows[bucket];
        row.len == slab.bytes.len() as u64 && row.digest == slab.digest
      });
    if carried {
      let from = prior.map(|p| p.join(TRIGRAMS_DIR).join(&name)).expect("carried implies a prior");
      let to = family_dir.join(&name);
      if from == to {
        continue;
      }
      let _ = fs::remove_file(&to);
      if fs::hard_link(&from, &to).is_ok() {
        continue;
      }
    }
    let tmp = family_dir.join(format!("{name}.tmp"));
    fs::write(&tmp, &slab.bytes)?;
    fs::rename(&tmp, family_dir.join(&name))?;
  }
  let rows: Vec<TocRow> = built
    .iter()
    .enumerate()
    .map(|(k, b)| TocRow {
      files: b.files,
      len: b.bytes.len() as u64,
      digest: b.digest,
      content_fold: folds[k],
    })
    .collect();
  write_toc(dir, &rows, built.iter().map(|b| b.postings).sum())?;
  sweep_stale(&family_dir, buckets);
  Ok(())
}

fn sweep_stale(family_dir: &Path, buckets: u32) {
  if let Ok(dirents) = fs::read_dir(family_dir) {
    for entry in dirents.flatten() {
      if let Ok(name) = entry.file_name().into_string() {
        let stale = name
          .strip_suffix(".tri")
          .and_then(|k| k.parse::<u32>().ok())
          .is_some_and(|k| k >= buckets);
        if stale || name.ends_with(".tmp") {
          let _ = fs::remove_file(entry.path());
        }
      }
    }
  }
}

/// Rebuild only the buckets a removed or added file maps to; every other slab hard-links from
/// `prior` (or is left alone when the caller's family batch already link-carried it) and the
/// TOC re-splices. `folds` are the CURRENT per-bucket content folds — a touched bucket records
/// its new fold, an untouched bucket keeps the prior's row.
pub fn apply_delta(
  staging: &Path,
  prior: &Path,
  buckets: u32,
  folds: &[u64],
  removed: &[u64],
  added: Vec<FileTrigrams>,
) -> io::Result<()> {
  use rayon::prelude::*;
  if buckets == 0 || folds.len() != buckets as usize {
    return Err(io::Error::other("trigram delta requires a bucket count with one fold per bucket"));
  }
  let prior_toc = Toc::load(&prior.join(TRIGRAMS_TOC))
    .ok_or_else(|| io::Error::other("trigram delta requires a readable prior family"))?;
  if prior_toc.rows.len() as u32 != buckets {
    return Err(io::Error::other("trigram delta: bucket count moved"));
  }
  let store = TrigramStore::open_unchecked(prior)
    .ok_or_else(|| io::Error::other("trigram delta requires a readable prior family"))?;
  let mut removed_by: Vec<Vec<u64>> = (0..buckets).map(|_| Vec::new()).collect();
  for &key in removed {
    removed_by[(key & u64::from(buckets - 1)) as usize].push(key);
  }
  let mut added_by: Vec<Vec<FileTrigrams>> = (0..buckets).map(|_| Vec::new()).collect();
  for file in added {
    added_by[(file.file_key & u64::from(buckets - 1)) as usize].push(file);
  }
  let family_dir = staging.join(TRIGRAMS_DIR);
  let pre_carried = family_dir.is_dir();
  if !pre_carried {
    fs::create_dir_all(&family_dir)?;
  }
  enum BucketOut {
    Linked,
    Absent,
    Wrote { files: u32, len: u64, digest: u64, postings: u64 },
  }
  let outs: io::Result<Vec<BucketOut>> = (0..buckets as usize)
    .into_par_iter()
    .map(|bucket| -> io::Result<BucketOut> {
      let name = format!("{bucket:04}.tri");
      let link = || -> io::Result<BucketOut> {
        if pre_carried {
          return Ok(BucketOut::Linked);
        }
        let (from, to) = (prior.join(TRIGRAMS_DIR).join(&name), family_dir.join(&name));
        let _ = fs::remove_file(&to);
        if fs::hard_link(&from, &to).is_err() {
          fs::copy(&from, &to)?;
        }
        Ok(BucketOut::Linked)
      };
      let (removed_b, added_b) = (&removed_by[bucket], &added_by[bucket]);
      if removed_b.is_empty() && added_b.is_empty() {
        return link();
      }
      let slab = match &store.buckets[bucket] {
        BucketState::Live(slab) => slab,
        // No complete prior slab to delta against: a slab holding only the changed files
        // would read as live and prune every other file in the bucket. Leave the bucket
        // absent (uncovered); the heal rebuilds it from source.
        BucketState::Uncovered => {
          let _ = fs::remove_file(family_dir.join(&name));
          return Ok(BucketOut::Absent);
        }
      };
      let (bytes, postings, file_count) = match slab.replace_streaming(bucket as u32, folds[bucket], removed_b, added_b) {
        // The common shape — every changed file already had an ordinal, none added or
        // removed — streams old lists straight into the new slab with no per-file decode.
        Some((bytes, postings)) => (bytes, postings, slab.files as u32),
        None => {
          let mut files: Vec<FileTrigrams> = slab.decode_files();
          if !removed_b.is_empty() {
            files.retain(|f| !removed_b.contains(&f.file_key));
          }
          if !added_b.is_empty() {
            let replaced: std::collections::HashSet<u64> = added_b.iter().map(|f| f.file_key).collect();
            files.retain(|f| !replaced.contains(&f.file_key));
            files.extend(added_b.iter().cloned());
          }
          files.sort_unstable_by_key(|f| f.file_key);
          let (bytes, postings) = encode_slab(bucket as u32, folds[bucket], &files);
          (bytes, postings, files.len() as u32)
        }
      };
      let digest = xxhash_rust::xxh3::xxh3_64(&bytes);
      let prior_row = prior_toc.rows[bucket];
      if prior_row.digest == digest && prior_row.len == bytes.len() as u64 {
        return link();
      }
      let tmp = family_dir.join(format!("{name}.tmp"));
      fs::write(&tmp, &bytes)?;
      fs::rename(&tmp, family_dir.join(&name))?;
      Ok(BucketOut::Wrote {
        files: file_count,
        len: bytes.len() as u64,
        digest,
        postings,
      })
    })
    .collect();
  let outs = outs?;
  let mut rows = prior_toc.rows.clone();
  let mut total: u64 = fs::read(prior.join(TRIGRAMS_TOC))
    .ok()
    .and_then(|b| b.get(12..20).map(|s| u64::from_le_bytes(s.try_into().unwrap())))
    .unwrap_or(0);
  for (bucket, out) in outs.iter().enumerate() {
    match out {
      BucketOut::Linked => {}
      BucketOut::Absent => {
        rows[bucket] = TocRow {
          files: 0,
          len: 0,
          digest: 0,
          content_fold: 0,
        };
      }
      BucketOut::Wrote { files, len, digest, postings } => {
        let prior_postings = match &store.buckets[bucket] {
          BucketState::Live(slab) => slab.postings_total(),
          BucketState::Uncovered => 0,
        };
        total = total.saturating_sub(prior_postings) + postings;
        rows[bucket] = TocRow {
          files: *files,
          len: *len,
          digest: *digest,
          content_fold: folds[bucket],
        };
      }
    }
  }
  write_toc(staging, &rows, total)
}

/// The per-bucket content folds the family under `dir` was built against (its TOC rows), or
/// `None` when the family is absent or foreign. A bucket never written reads as fold 0.
pub fn toc_folds(dir: &Path) -> Option<Vec<u64>> {
  Some(Toc::load(&dir.join(TRIGRAMS_TOC))?.rows.iter().map(|r| r.content_fold).collect())
}

/// Write (or rewrite) exactly the buckets in `rebuilt`, leaving every other slab as it is:
/// the heal path, which fills the buckets a reader found uncovered. `rebuilt[i].1` must hold
/// every file of bucket `rebuilt[i].0`. Without a prior TOC (first heal) the other rows start
/// absent; a slab this call does not write is absent or whatever the existing TOC says.
pub fn heal_buckets(
  dir: &Path,
  buckets: u32,
  folds: &[u64],
  rebuilt: Vec<(u32, Vec<FileTrigrams>)>,
) -> io::Result<()> {
  let builds: Vec<BucketBuild> = rebuilt
    .into_iter()
    .map(|(bucket, mut files)| {
      files.sort_unstable_by_key(|f| f.file_key);
      files.dedup_by_key(|f| f.file_key);
      let mut flat = Vec::new();
      let (file_keys, unindexed) = BucketBuild::from_files(bucket, &files, &mut flat);
      BucketBuild {
        bucket,
        file_keys,
        unindexed,
        flat,
      }
    })
    .collect();
  heal_buckets_packed(dir, buckets, folds, builds)
}

/// One healed slab's TOC facts, returned by [`heal_write_slab`] and committed by
/// [`heal_commit_rows`].
#[derive(Debug, Clone, Copy)]
pub struct HealRow {
  pub bucket: u32,
  pub files: u32,
  pub len: u64,
  pub digest: u64,
  pub postings: u64,
}

/// Encode one bucket from packed postings into `out` (reused) and write it as
/// `trigrams/<k>.tri` (tmp + rename). The TOC is not touched — commit the rows with
/// [`heal_commit_rows`] once the chunk is written. `file_keys` ascending and complete.
pub fn heal_write_slab(
  dir: &Path,
  bucket: u32,
  fold: u64,
  file_keys: &[u64],
  unindexed: &[u32],
  flat: &mut Vec<u64>,
  out: &mut Vec<u8>,
) -> io::Result<HealRow> {
  let family_dir = dir.join(TRIGRAMS_DIR);
  fs::create_dir_all(&family_dir)?;
  let postings = encode_packed_into(bucket, fold, file_keys, unindexed, flat, out);
  let digest = xxhash_rust::xxh3::xxh3_64(out);
  let name = format!("{bucket:04}.tri");
  let tmp = family_dir.join(format!("{name}.tmp"));
  fs::write(&tmp, &*out)?;
  fs::rename(&tmp, family_dir.join(&name))?;
  Ok(HealRow {
    bucket,
    files: file_keys.len() as u32,
    len: out.len() as u64,
    digest,
    postings,
  })
}

/// Splice healed rows into the TOC (creating it when absent), keeping every other row.
pub fn heal_commit_rows(dir: &Path, buckets: u32, folds: &[u64], healed: &[HealRow]) -> io::Result<()> {
  if buckets == 0 || folds.len() != buckets as usize {
    return Err(io::Error::other("trigram heal requires a bucket count with one fold per bucket"));
  }
  fs::create_dir_all(dir.join(TRIGRAMS_DIR))?;
  let existing = Toc::load(&dir.join(TRIGRAMS_TOC)).filter(|toc| toc.rows.len() as u32 == buckets);
  let mut rows: Vec<TocRow> = match &existing {
    Some(toc) => toc.rows.clone(),
    None => vec![
      TocRow {
        files: 0,
        len: 0,
        digest: 0,
        content_fold: 0,
      };
      buckets as usize
    ],
  };
  let mut total: u64 = existing
    .as_ref()
    .and_then(|_| fs::read(dir.join(TRIGRAMS_TOC)).ok())
    .and_then(|b| b.get(12..20).map(|s| u64::from_le_bytes(s.try_into().unwrap())))
    .unwrap_or(0);
  // A replaced row's prior postings leave the total; a never-written row contributed none.
  // Prior counts come from the prior slabs' key rows only when a TOC row was live.
  let prior = TrigramStore::open_unchecked(dir);
  for row in healed {
    let prior_postings = prior
      .as_ref()
      .and_then(|store| match &store.buckets[row.bucket as usize] {
        BucketState::Live(slab) if rows[row.bucket as usize].len != row.len || rows[row.bucket as usize].digest != row.digest => Some(slab.postings_total()),
        _ => None,
      })
      .unwrap_or(0);
    total = total.saturating_sub(prior_postings) + row.postings;
    rows[row.bucket as usize] = TocRow {
      files: row.files,
      len: row.len,
      digest: row.digest,
      content_fold: folds[row.bucket as usize],
    };
  }
  write_toc(dir, &rows, total)
}

/// [`heal_buckets`] over packed builds — the heal's own path (per-thread buffers, no per-file
/// vectors). Each build's `file_keys` must be ascending and complete for its bucket.
pub fn heal_buckets_packed(
  dir: &Path,
  buckets: u32,
  folds: &[u64],
  rebuilt: Vec<BucketBuild>,
) -> io::Result<()> {
  use rayon::prelude::*;
  if buckets == 0 || folds.len() != buckets as usize {
    return Err(io::Error::other("trigram heal requires a bucket count with one fold per bucket"));
  }
  let family_dir = dir.join(TRIGRAMS_DIR);
  fs::create_dir_all(&family_dir)?;
  let existing = Toc::load(&dir.join(TRIGRAMS_TOC)).filter(|toc| toc.rows.len() as u32 == buckets);
  let mut rows: Vec<TocRow> = match &existing {
    Some(toc) => toc.rows.clone(),
    None => vec![
      TocRow {
        files: 0,
        len: 0,
        digest: 0,
        content_fold: 0,
      };
      buckets as usize
    ],
  };
  let mut total: u64 = existing
    .as_ref()
    .and_then(|_| fs::read(dir.join(TRIGRAMS_TOC)).ok())
    .and_then(|b| b.get(12..20).map(|s| u64::from_le_bytes(s.try_into().unwrap())))
    .unwrap_or(0);
  let prior = TrigramStore::open_unchecked(dir);
  let written: io::Result<Vec<(u32, u32, u64, u64, u64)>> = rebuilt
    .into_par_iter()
    .map_init(Vec::<u8>::new, |out, mut build| -> io::Result<(u32, u32, u64, u64, u64)> {
      let bucket = build.bucket;
      let postings = encode_packed_into(bucket, folds[bucket as usize], &build.file_keys, &build.unindexed, &mut build.flat, out);
      let digest = xxhash_rust::xxh3::xxh3_64(out);
      let name = format!("{bucket:04}.tri");
      let tmp = family_dir.join(format!("{name}.tmp"));
      fs::write(&tmp, &*out)?;
      fs::rename(&tmp, family_dir.join(&name))?;
      Ok((bucket, build.file_keys.len() as u32, out.len() as u64, digest, postings))
    })
    .collect();
  for (bucket, files, len, digest, postings) in written? {
    let prior_postings = prior
      .as_ref()
      .and_then(|store| match &store.buckets[bucket as usize] {
        BucketState::Live(slab) => Some(slab.postings_total()),
        BucketState::Uncovered => None,
      })
      .unwrap_or(0);
    total = total.saturating_sub(prior_postings) + postings;
    rows[bucket as usize] = TocRow {
      files,
      len,
      digest,
      content_fold: folds[bucket as usize],
    };
  }
  write_toc(dir, &rows, total)
}

/// One mapped slab.
struct Slab {
  store: MappedStore,
  files: usize,
  unindexed: usize,
  trigrams: usize,
  content_fold: u64,
  pool_len: usize,
}

impl Slab {
  fn bytes(&self) -> &[u8] {
    self.store.as_bytes()
  }
  #[inline]
  fn file_table_off(&self) -> usize {
    SLAB_HEADER
  }
  #[inline]
  fn unindexed_off(&self) -> usize {
    SLAB_HEADER + self.files * 8
  }
  #[inline]
  fn keys_off(&self) -> usize {
    self.unindexed_off() + self.unindexed * 4
  }
  #[inline]
  fn pool_off(&self) -> usize {
    self.keys_off() + self.trigrams * KEY_ROW
  }
  #[inline]
  fn file_key(&self, ordinal: usize) -> u64 {
    let at = self.file_table_off() + ordinal * 8;
    u64::from_le_bytes(self.bytes()[at..at + 8].try_into().unwrap())
  }
  fn contains_file(&self, key: u64) -> bool {
    let (mut lo, mut hi) = (0usize, self.files);
    while lo < hi {
      let mid = (lo + hi) / 2;
      match self.file_key(mid).cmp(&key) {
        std::cmp::Ordering::Less => lo = mid + 1,
        std::cmp::Ordering::Greater => hi = mid,
        std::cmp::Ordering::Equal => return true,
      }
    }
    false
  }
  fn unindexed_ordinals(&self) -> impl Iterator<Item = u32> + '_ {
    let at = self.unindexed_off();
    (0..self.unindexed).map(move |i| u32::from_le_bytes(self.bytes()[at + i * 4..at + i * 4 + 4].try_into().unwrap()))
  }
  #[inline]
  fn key_row(&self, i: usize) -> (u32, u32, u32) {
    let at = self.keys_off() + i * KEY_ROW;
    let b = &self.bytes()[at..at + KEY_ROW];
    (
      u32::from_le_bytes(b[0..4].try_into().unwrap()),
      u32::from_le_bytes(b[4..8].try_into().unwrap()),
      u32::from_le_bytes(b[8..12].try_into().unwrap()),
    )
  }
  /// `(pool offset, count)` of `key`, if this bucket holds it.
  fn find_key(&self, key: u32) -> Option<(usize, usize)> {
    let (mut lo, mut hi) = (0usize, self.trigrams);
    while lo < hi {
      let mid = (lo + hi) / 2;
      let (k, off, count) = self.key_row(mid);
      match k.cmp(&key) {
        std::cmp::Ordering::Less => lo = mid + 1,
        std::cmp::Ordering::Greater => hi = mid,
        std::cmp::Ordering::Equal => return Some((off as usize, count as usize)),
      }
    }
    None
  }
  /// Decode one posting list into `(ordinal, mask)` pairs, ascending ordinal.
  fn postings(&self, pool_off: usize, count: usize, out: &mut Vec<(u32, u8)>) {
    out.clear();
    let pool = &self.bytes()[self.pool_off()..self.pool_off() + self.pool_len];
    let mut at = pool_off;
    let mut ordinal: u32 = 0;
    for _ in 0..count {
      let Some(delta) = read_varint(pool, &mut at) else {
        break;
      };
      ordinal += delta;
      let Some(&mask) = pool.get(at) else {
        break;
      };
      at += 1;
      out.push((ordinal, mask));
    }
  }
  fn postings_total(&self) -> u64 {
    (0..self.trigrams).map(|i| u64::from(self.key_row(i).2)).sum()
  }
  /// The ordinal of `key` in this slab's file table, if present.
  fn ordinal_of(&self, key: u64) -> Option<u32> {
    let (mut lo, mut hi) = (0usize, self.files);
    while lo < hi {
      let mid = (lo + hi) / 2;
      match self.file_key(mid).cmp(&key) {
        std::cmp::Ordering::Less => lo = mid + 1,
        std::cmp::Ordering::Greater => hi = mid,
        std::cmp::Ordering::Equal => return Some(mid as u32),
      }
    }
    None
  }
  /// Re-encode this slab with `added` replacing the postings of files that already have an
  /// ordinal here, in one streaming pass over the key table: the file table and every other
  /// file's postings are copied as they are, the replaced ordinals are dropped from each old
  /// list and the new postings merged in. `None` when the shape is not a pure replacement
  /// (a removal without a re-add, or a file this slab has never seen) — the caller then
  /// takes the decode path. Returns `(slab bytes, posting count)`.
  fn replace_streaming(
    &self,
    bucket: u32,
    content_fold: u64,
    removed: &[u64],
    added: &[FileTrigrams],
  ) -> Option<(Vec<u8>, u64)> {
    // Every removed key must be re-added (a modification), every added key must exist.
    let mut replaced: Vec<(u32, &FileTrigrams)> = Vec::with_capacity(added.len());
    for file in added {
      replaced.push((self.ordinal_of(file.file_key)?, file));
    }
    for key in removed {
      if !added.iter().any(|f| f.file_key == *key) {
        return None;
      }
    }
    replaced.sort_unstable_by_key(|(o, _)| *o);
    let replaced_ordinals: Vec<u32> = replaced.iter().map(|(o, _)| *o).collect();
    // Per replaced file, a cursor into its key-sorted postings.
    let mut cursors: Vec<usize> = vec![0; replaced.len()];
    let bytes = self.bytes();
    let file_table = &bytes[self.file_table_off()..self.unindexed_off()];
    let prior_unindexed: Vec<u32> = self.unindexed_ordinals().collect();
    let mut unindexed: Vec<u32> = prior_unindexed
      .iter()
      .copied()
      .filter(|o| !replaced_ordinals.contains(o))
      .collect();
    for (o, file) in &replaced {
      if !file.indexed {
        unindexed.push(*o);
      }
    }
    unindexed.sort_unstable();
    // The key table size is the size of (old keys ∪ new keys) minus keys whose lists empty
    // out; a counting pass over the two sorted key streams settles it, so the slab is written
    // into ONE buffer in one pass — no side buffers, no splice.
    let trigrams = {
      let mut count: u32 = 0;
      let mut cursors_c: Vec<usize> = vec![0; replaced.len()];
      let mut old_i = 0usize;
      loop {
        let old_key = (old_i < self.trigrams).then(|| self.key_row(old_i).0);
        let new_key = replaced
          .iter()
          .zip(cursors_c.iter())
          .filter_map(|((_, file), &c)| file.postings.get(c).map(|p| p.0))
          .min();
        let key = match (old_key, new_key) {
          (None, None) => break,
          (Some(a), None) => a,
          (None, Some(b)) => b,
          (Some(a), Some(b)) => a.min(b),
        };
        let mut nonempty = false;
        if old_key == Some(key) {
          let (_, _, old_count) = self.key_row(old_i);
          old_i += 1;
          // an old list survives unless every posting belonged to a replaced file
          nonempty |= old_count as usize > replaced.len();
          if !nonempty {
            // small list: check membership exactly
            let (_, off, count) = self.key_row(old_i - 1);
            let mut probe: Vec<(u32, u8)> = Vec::with_capacity(count as usize);
            self.postings(off as usize, count as usize, &mut probe);
            nonempty |= probe.iter().any(|(o, _)| replaced_ordinals.binary_search(o).is_err());
          }
        }
        for (i, (_, file)) in replaced.iter().enumerate() {
          if let Some(&(k, _)) = file.postings.get(cursors_c[i])
            && k == key
          {
            nonempty = true;
            cursors_c[i] += 1;
          }
        }
        if nonempty {
          count += 1;
        }
      }
      count
    };
    let keys_off = SLAB_HEADER + self.files * 8 + unindexed.len() * 4;
    let pool_off = keys_off + trigrams as usize * KEY_ROW;
    let mut out: Vec<u8> = Vec::with_capacity(pool_off + self.pool_len + added.iter().map(|f| f.postings.len() * 3).sum::<usize>());
    out.extend_from_slice(SLAB_MAGIC);
    out.extend_from_slice(&VERSION.to_le_bytes());
    out.extend_from_slice(&bucket.to_le_bytes());
    out.extend_from_slice(&(self.files as u32).to_le_bytes());
    out.extend_from_slice(&content_fold.to_le_bytes());
    out.extend_from_slice(&trigrams.to_le_bytes());
    out.extend_from_slice(&(unindexed.len() as u32).to_le_bytes());
    out.extend_from_slice(&0u64.to_le_bytes()); // pool_len, patched
    out.extend_from_slice(&0u64.to_le_bytes());
    out.extend_from_slice(file_table);
    for o in &unindexed {
      out.extend_from_slice(&o.to_le_bytes());
    }
    debug_assert_eq!(out.len(), keys_off);
    out.resize(pool_off, 0);
    let mut key_row = keys_off;
    let mut old_list: Vec<(u32, u8)> = Vec::new();
    let mut merged: Vec<(u32, u8)> = Vec::new();
    let mut postings_total: u64 = 0;
    let mut written_keys: u32 = 0;
    let mut old_i = 0usize;
    loop {
      // The next key: the smaller of the old table's next key and any replaced file's next key.
      let old_key = (old_i < self.trigrams).then(|| self.key_row(old_i).0);
      let new_key = replaced
        .iter()
        .zip(cursors.iter())
        .filter_map(|((_, file), &c)| file.postings.get(c).map(|p| p.0))
        .min();
      let key = match (old_key, new_key) {
        (None, None) => break,
        (Some(a), None) => a,
        (None, Some(b)) => b,
        (Some(a), Some(b)) => a.min(b),
      };
      merged.clear();
      if old_key == Some(key) {
        let (_, off, count) = self.key_row(old_i);
        self.postings(off as usize, count as usize, &mut old_list);
        old_i += 1;
        // drop the replaced ordinals (sorted both sides: two-pointer)
        let mut r = 0usize;
        for &(ordinal, mask) in &old_list {
          while r < replaced_ordinals.len() && replaced_ordinals[r] < ordinal {
            r += 1;
          }
          if r < replaced_ordinals.len() && replaced_ordinals[r] == ordinal {
            continue;
          }
          merged.push((ordinal, mask));
        }
      }
      for (i, (ordinal, file)) in replaced.iter().enumerate() {
        if let Some(&(k, mask)) = file.postings.get(cursors[i])
          && k == key
        {
          merged.push((*ordinal, mask));
          cursors[i] += 1;
        }
      }
      if merged.is_empty() {
        continue;
      }
      merged.sort_unstable_by_key(|&(o, _)| o);
      let list_off = (out.len() - pool_off) as u32;
      let mut prev = 0u32;
      for &(ordinal, mask) in &merged {
        push_varint(&mut out, ordinal - prev);
        out.push(mask);
        prev = ordinal;
      }
      if written_keys >= trigrams {
        return None; // the counting pass and the merge disagree — take the decode path
      }
      out[key_row..key_row + 4].copy_from_slice(&key.to_le_bytes());
      out[key_row + 4..key_row + 8].copy_from_slice(&list_off.to_le_bytes());
      out[key_row + 8..key_row + 12].copy_from_slice(&(merged.len() as u32).to_le_bytes());
      key_row += KEY_ROW;
      written_keys += 1;
      postings_total += merged.len() as u64;
    }
    if written_keys != trigrams {
      return None;
    }
    let pool_len = (out.len() - pool_off) as u64;
    out[32..40].copy_from_slice(&pool_len.to_le_bytes());
    Some((out, postings_total))
  }
  /// Invert the slab back into per-file contributions (the delta path's input).
  fn decode_files(&self) -> Vec<FileTrigrams> {
    let unindexed: std::collections::HashSet<u32> = self.unindexed_ordinals().collect();
    let mut files: Vec<FileTrigrams> = (0..self.files)
      .map(|ordinal| FileTrigrams {
        file_key: self.file_key(ordinal),
        indexed: !unindexed.contains(&(ordinal as u32)),
        postings: Vec::new(),
      })
      .collect();
    let mut list = Vec::new();
    for i in 0..self.trigrams {
      let (key, off, count) = self.key_row(i);
      self.postings(off as usize, count as usize, &mut list);
      for &(ordinal, mask) in &list {
        if let Some(file) = files.get_mut(ordinal as usize) {
          file.postings.push((key, mask));
        }
      }
    }
    // key rows are visited ascending, so each file's postings are already key-sorted
    files
  }
}

enum BucketState {
  Live(Slab),
  Uncovered,
}

/// The candidate verdict for one file under a plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
  /// Holds every required trigram (or its bucket is uncovered): scan it.
  Candidate,
  /// Indexed in a live bucket and provably missing a required trigram: skip it.
  Pruned,
  /// Not in the family's file tables: the index does not know it; scan it.
  Unknown,
}

/// The files a plan admits, per bucket.
pub struct CandidateSet {
  buckets: u32,
  uncovered: Vec<bool>,
  /// Admitted file keys, ascending.
  admitted: Vec<u64>,
}

impl CandidateSet {
  pub fn admitted(&self) -> &[u64] {
    &self.admitted
  }
  pub fn uncovered_buckets(&self) -> usize {
    self.uncovered.iter().filter(|u| **u).count()
  }
  /// True when every bucket was live: `admitted()` is then the complete candidate set and a
  /// consumer may iterate it instead of testing every file.
  pub fn is_complete(&self) -> bool {
    !self.uncovered.iter().any(|u| *u)
  }
  pub fn buckets(&self) -> u32 {
    self.buckets
  }
  /// Fold `other` in: a file admitted by either branch stays a candidate, a bucket uncovered
  /// in either stays uncovered. The OR of two AND-plans — how an alternation prunes.
  pub fn union_with(&mut self, other: &CandidateSet) {
    debug_assert_eq!(self.buckets, other.buckets);
    for (mine, theirs) in self.uncovered.iter_mut().zip(&other.uncovered) {
      *mine |= *theirs;
    }
    let mut merged: Vec<u64> = Vec::with_capacity(self.admitted.len() + other.admitted.len());
    let (a, b) = (&self.admitted, &other.admitted);
    let (mut i, mut j) = (0usize, 0usize);
    while i < a.len() && j < b.len() {
      match a[i].cmp(&b[j]) {
        std::cmp::Ordering::Less => {
          merged.push(a[i]);
          i += 1;
        }
        std::cmp::Ordering::Greater => {
          merged.push(b[j]);
          j += 1;
        }
        std::cmp::Ordering::Equal => {
          merged.push(a[i]);
          i += 1;
          j += 1;
        }
      }
    }
    merged.extend_from_slice(&a[i..]);
    merged.extend_from_slice(&b[j..]);
    self.admitted = merged;
  }
}

/// The mapped read side.
pub struct TrigramStore {
  buckets: Vec<BucketState>,
}

impl TrigramStore {
  /// Map the family under `dir`, validating each bucket against `folds[k]` (the current
  /// generation's per-bucket content fold). `None` = family absent, foreign, or bucketed
  /// differently — the caller scans without it.
  pub fn open(dir: &Path, folds: &[u64]) -> Option<TrigramStore> {
    let mut store = Self::open_unchecked(dir)?;
    if store.buckets.len() != folds.len() {
      return None;
    }
    for (k, state) in store.buckets.iter_mut().enumerate() {
      let stale = matches!(state, BucketState::Live(slab) if slab.content_fold != folds[k]);
      if stale {
        *state = BucketState::Uncovered;
      }
    }
    Some(store)
  }

  /// Map every well-formed slab without content validation — the delta path's view of the
  /// prior family, whose folds are by definition the prior generation's.
  fn open_unchecked(dir: &Path) -> Option<TrigramStore> {
    let toc = Toc::load(&dir.join(TRIGRAMS_TOC))?;
    let mut buckets = Vec::with_capacity(toc.rows.len());
    for (k, row) in toc.rows.iter().enumerate() {
      let path = dir.join(TRIGRAMS_DIR).join(format!("{k:04}.tri"));
      let Ok(meta) = fs::metadata(&path) else {
        buckets.push(BucketState::Uncovered);
        continue;
      };
      if meta.len() != row.len {
        buckets.push(BucketState::Uncovered);
        continue;
      }
      let Ok(store) = MappedStore::map_file(
        &path,
        StoreKind::Canonical,
        AccessPattern::Random,
        Hotness::Cold,
        &ResourcePolicy::probe(CorpusProbe::new(0, 0)),
      ) else {
        buckets.push(BucketState::Uncovered);
        continue;
      };
      match Slab::validate(store, k as u32) {
        Some(slab) => buckets.push(BucketState::Live(slab)),
        None => buckets.push(BucketState::Uncovered),
      }
    }
    Some(TrigramStore { buckets })
  }

  /// `(live, total)` bucket counts.
  pub fn coverage(&self) -> (u32, u32) {
    let live = self.buckets.iter().filter(|b| matches!(b, BucketState::Live(_))).count();
    (live as u32, self.buckets.len() as u32)
  }

  pub fn bucket_count(&self) -> u32 {
    self.buckets.len() as u32
  }

  /// Whether bucket `k` is live (mapped and validated against the current fold).
  pub fn bucket_is_live(&self, k: u32) -> bool {
    matches!(self.buckets.get(k as usize), Some(BucketState::Live(_)))
  }

  /// Files that may satisfy `terms` (an AND plan): per live bucket the intersection of the
  /// terms' posting lists, smallest first, each posting's next-byte mask tested against the
  /// term's expectation; plus every file of an uncovered bucket and every unindexed file.
  pub fn candidates(&self, terms: &[PlanTerm]) -> CandidateSet {
    use rayon::prelude::*;
    let uncovered: Vec<bool> = self.buckets.iter().map(|b| matches!(b, BucketState::Uncovered)).collect();
    // One scratch and one output vector per rayon thread, reused across the buckets that
    // thread visits; the per-thread outputs are joined once at the end.
    let mut admitted: Vec<u64> = self
      .buckets
      .par_iter()
      .fold(
        || (IntersectScratch::default(), Vec::<u64>::new()),
        |(mut scratch, mut out), state| {
          if let BucketState::Live(slab) = state {
            slab.candidates_into(terms, &mut scratch, &mut out);
          }
          (scratch, out)
        },
      )
      .map(|(_, out)| out)
      .reduce(Vec::new, |mut a, b| {
        if a.len() < b.len() {
          let mut b = b;
          b.extend_from_slice(&a);
          return b;
        }
        a.extend_from_slice(&b);
        a
      });
    admitted.sort_unstable();
    admitted.dedup();
    CandidateSet {
      buckets: self.buckets.len() as u32,
      uncovered,
      admitted,
    }
  }

  /// The verdict for `file_key` under `set`.
  pub fn verdict(&self, set: &CandidateSet, file_key: u64) -> Verdict {
    if set.buckets == 0 {
      return Verdict::Unknown;
    }
    let bucket = (file_key & u64::from(set.buckets - 1)) as usize;
    if set.uncovered[bucket] {
      return Verdict::Candidate;
    }
    if set.admitted.binary_search(&file_key).is_ok() {
      return Verdict::Candidate;
    }
    match &self.buckets[bucket] {
      BucketState::Live(slab) if slab.contains_file(file_key) => Verdict::Pruned,
      _ => Verdict::Unknown,
    }
  }
}

impl Slab {
  fn validate(store: MappedStore, bucket: u32) -> Option<Slab> {
    let bytes = store.as_bytes();
    if bytes.len() < SLAB_HEADER || &bytes[0..4] != SLAB_MAGIC {
      return None;
    }
    if u32::from_le_bytes(bytes[4..8].try_into().ok()?) != VERSION
      || u32::from_le_bytes(bytes[8..12].try_into().ok()?) != bucket
    {
      return None;
    }
    let files = u32::from_le_bytes(bytes[12..16].try_into().ok()?) as usize;
    let content_fold = u64::from_le_bytes(bytes[16..24].try_into().ok()?);
    let trigrams = u32::from_le_bytes(bytes[24..28].try_into().ok()?) as usize;
    let unindexed = u32::from_le_bytes(bytes[28..32].try_into().ok()?) as usize;
    let pool_len = u64::from_le_bytes(bytes[32..40].try_into().ok()?) as usize;
    let expected = SLAB_HEADER + files * 8 + unindexed * 4 + trigrams * KEY_ROW + pool_len;
    if bytes.len() != expected {
      return None;
    }
    Some(Slab {
      store,
      files,
      unindexed,
      trigrams,
      content_fold,
      pool_len,
    })
  }

  /// Append this bucket's candidates for `terms` to `out`, using `scratch`'s buffers.
  fn candidates_into(&self, terms: &[PlanTerm], scratch: &mut IntersectScratch, out: &mut Vec<u64>) {
    out.extend(self.unindexed_ordinals().map(|o| self.file_key(o as usize)));
    if terms.is_empty() {
      // no plan: every indexed file is a candidate
      out.extend((0..self.files).map(|o| self.file_key(o)));
      return;
    }
    // Resolve every term; a term this bucket never saw proves the bucket holds no match.
    let IntersectScratch { lists, current, next, merged } = scratch;
    lists.clear();
    for term in terms {
      let Some((off, count)) = self.find_key(term.key) else {
        return;
      };
      lists.push((off, count, term.expected_next));
    }
    lists.sort_unstable_by_key(|&(_, count, _)| count);
    self.postings(lists[0].0, lists[0].1, current);
    let expect = lists[0].2;
    current.retain(|&(_, mask)| mask_admits(mask, expect));
    for &(off, count, expect) in &lists[1..] {
      if current.is_empty() {
        break;
      }
      self.postings(off, count, next);
      merged.clear();
      let (mut i, mut j) = (0usize, 0usize);
      while i < current.len() && j < next.len() {
        match current[i].0.cmp(&next[j].0) {
          std::cmp::Ordering::Less => i += 1,
          std::cmp::Ordering::Greater => j += 1,
          std::cmp::Ordering::Equal => {
            if mask_admits(next[j].1, expect) {
              merged.push(current[i]);
            }
            i += 1;
            j += 1;
          }
        }
      }
      // the intersection becomes the running list; the old list's buffer is reused next round
      std::mem::swap(current, merged);
    }
    out.extend(current.iter().map(|&(ordinal, _)| self.file_key(ordinal as usize)));
  }
}

/// The intersection's working buffers, kept per thread across buckets and queries.
#[derive(Default)]
struct IntersectScratch {
  lists: Vec<(usize, usize, Option<u8>)>,
  current: Vec<(u32, u8)>,
  next: Vec<(u32, u8)>,
  merged: Vec<(u32, u8)>,
}

#[cfg(test)]
mod tests {
  use super::*;
  use vorpal_core::trigram::{MaskMap, extract, plan};

  fn file(key: u64, text: &[u8]) -> FileTrigrams {
    let mut scratch = MaskMap::default();
    let mut postings = Vec::new();
    extract(text, &mut scratch, &mut postings);
    FileTrigrams {
      file_key: key,
      indexed: true,
      postings,
    }
  }

  fn tmp(label: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("vorpal-trigrams-{label}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
  }

  #[test]
  fn candidates_never_miss_a_matching_file_and_prune_provable_misses() {
    let dir = tmp("cand");
    let texts: Vec<(u64, &[u8])> = vec![
      (0x10, b"int vfs_read(struct file *f) { return kmalloc(8, GFP); }"),
      (0x21, b"static void schedule_timeout(long t) { }"),
      (0x32, b"void kfree(void *p); /* kmalloc twice: kmalloc */"),
      (0x43, b"abcabd"),
    ];
    let files: Vec<FileTrigrams> = texts.iter().map(|&(k, t)| file(k, t)).collect();
    let folds = vec![1u64, 2, 3, 4];
    save(&dir, files.clone(), 4, &folds, None).unwrap();
    let store = TrigramStore::open(&dir, &folds).unwrap();
    assert_eq!(store.coverage(), (4, 4));
    let literals: Vec<&[u8]> = vec![
      b"kmalloc(", b"schedule_timeout", b"vfs_read(struct", b"abd", b"abcx", b"GFP)", b"zzz", b"kfree(void *p)",
    ];
    for lit in literals {
      let terms = plan(&[lit]).unwrap();
      let set = store.candidates(&terms);
      for &(key, text) in &texts {
        let really = text.windows(lit.len()).any(|w| w == lit);
        let verdict = store.verdict(&set, key);
        if really {
          assert_eq!(verdict, Verdict::Candidate, "false negative for {:?} in {key:#x}", std::str::from_utf8(lit));
        } else {
          assert_ne!(verdict, Verdict::Unknown);
        }
      }
    }
    // `abcx`: file 0x43 holds abc and (via abd) nothing following abc with x → pruned by the mask.
    let set = store.candidates(&plan(&[b"abcx"]).unwrap());
    assert_eq!(store.verdict(&set, 0x43), Verdict::Pruned);
    assert_eq!(store.verdict(&set, 0x99), Verdict::Unknown);
    let _ = fs::remove_dir_all(&dir);
  }

  #[test]
  fn stale_fold_uncovers_the_bucket_and_delta_matches_a_full_save() {
    let a = tmp("a");
    let b = tmp("b");
    let f1 = file(0x10, b"alpha beta gamma");
    let f2 = file(0x21, b"delta epsilon");
    let f3 = file(0x20, b"gamma delta"); // bucket 0 with 0x10 under B=16; 0x21 sits in bucket 1
    let folds_a = vec![7u64; 16];
    save(&a, vec![f1.clone(), f2.clone(), f3.clone()], 16, &folds_a, None).unwrap();
    // Bucket 0 changed (fold 8): f3 rewritten; bucket 1 untouched.
    let f3b = file(0x20, b"gamma zeta");
    let mut folds_b = folds_a.clone();
    folds_b[0] = 8;
    apply_delta(&b, &a, 16, &folds_b, &[0x20], vec![f3b.clone()]).unwrap();
    // A full save of the same logical family must produce byte-identical slabs.
    let c = tmp("c");
    save(&c, vec![f1.clone(), f2.clone(), f3b.clone()], 16, &folds_b, None).unwrap();
    for k in 0..16 {
      let name = format!("{k:04}.tri");
      assert_eq!(
        fs::read(b.join(TRIGRAMS_DIR).join(&name)).unwrap(),
        fs::read(c.join(TRIGRAMS_DIR).join(&name)).unwrap(),
        "delta and full save diverge in bucket {k}"
      );
    }
    assert_eq!(fs::read(b.join(TRIGRAMS_TOC)).unwrap(), fs::read(c.join(TRIGRAMS_TOC)).unwrap());
    #[cfg(unix)]
    {
      use std::os::unix::fs::MetadataExt;
      // the untouched bucket hard-links from the prior; the rewritten one does not
      assert_eq!(
        fs::metadata(a.join(TRIGRAMS_DIR).join("0001.tri")).unwrap().ino(),
        fs::metadata(b.join(TRIGRAMS_DIR).join("0001.tri")).unwrap().ino()
      );
      assert_ne!(
        fs::metadata(a.join(TRIGRAMS_DIR).join("0000.tri")).unwrap().ino(),
        fs::metadata(b.join(TRIGRAMS_DIR).join("0000.tri")).unwrap().ino()
      );
    }
    // Opening `a` against the new folds uncovers bucket 0 only.
    let store = TrigramStore::open(&a, &folds_b).unwrap();
    assert_eq!(store.coverage(), (15, 16));
    let set = store.candidates(&plan(&[b"zeta"]).unwrap());
    assert_eq!(store.verdict(&set, 0x20), Verdict::Candidate); // uncovered → scan
    assert_eq!(store.verdict(&set, 0x21), Verdict::Pruned);
    // Opening `b` against the same folds is fully live and finds zeta in 0x11 only.
    let store = TrigramStore::open(&b, &folds_b).unwrap();
    assert_eq!(store.coverage(), (16, 16));
    let set = store.candidates(&plan(&[b"zeta"]).unwrap());
    assert_eq!(store.verdict(&set, 0x20), Verdict::Candidate);
    assert_eq!(store.verdict(&set, 0x10), Verdict::Pruned);
    // A different bucket count is a foreign family.
    assert!(TrigramStore::open(&b, &[0u64; 8]).is_none());
    for d in [a, b, c] {
      let _ = fs::remove_dir_all(&d);
    }
  }

  #[test]
  fn unindexed_files_are_always_candidates_and_fold_is_order_free() {
    let dir = tmp("unindexed");
    let mut f = file(0x10, b"nothing relevant");
    f.indexed = false;
    save(&dir, vec![f], 16, &[0u64; 16], None).unwrap();
    let store = TrigramStore::open(&dir, &[0u64; 16]).unwrap();
    let set = store.candidates(&plan(&[b"zzzz"]).unwrap());
    assert_eq!(store.verdict(&set, 0x10), Verdict::Candidate);
    assert_eq!(content_fold(&mut vec![(1, 2), (3, 4)]), content_fold(&mut vec![(3, 4), (1, 2), (1, 2)]));
    assert!(is_trigrams_member("trigrams/0007.tri") && is_trigrams_member(TRIGRAMS_TOC));
    assert!(!is_trigrams_member("trigrams/x.tri") && !is_trigrams_member("usage/0007.idx"));
    let _ = fs::remove_dir_all(&dir);
  }
}
