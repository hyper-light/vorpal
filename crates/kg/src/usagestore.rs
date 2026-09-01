//! The usage family (P4.5a): `referenced-name-hash → file_key` postings ON DISK — the
//! retained daemon's dirty-name machinery generalized to the CLI edit path. Given the
//! names an edit (re)defines or removes, `files_referencing` bounds exactly which files'
//! resolutions could differ; everything outside that closure carries byte-wise.
//!
//! Derived from the evidence rows at save time (every reference occurrence already
//! records the referenced name's hash), so there is no new pipeline plumbing and the
//! family is a pure function of the occurrence set. Slabs bucket by `name_hash & (B-1)`
//! under the SAME bucket law as every other family; each slab holds sorted, deduped
//! 12-byte pairs `[name_hash u32][file_key u64]`, and the TOC carries per-slab digests —
//! unchanged slabs hard-link across generations exactly like the rest.
//!
//! Slab (`usage/<k>.idx`): `[VUSG][version][bucket u32][pairs u64]` + pairs.
//! TOC (`usage/toc.bin`): `[VUST][version][bucket count u32][total pairs u64]` +
//! per-slab `{pairs u64, byte len u64, digest u64}`.

use std::fs;
use std::io::{self, Write};
use std::path::Path;

use vorpal_mem::{AccessPattern, CorpusProbe, Hotness, MappedStore, ResourcePolicy, StoreKind};

pub const USAGE_DIR: &str = "usage";
pub const USAGE_TOC: &str = "usage/toc.bin";
const SLAB_MAGIC: &[u8; 4] = b"VUSG";
const TOC_MAGIC: &[u8; 4] = b"VUST";
const VERSION: u32 = 1;
/// Slab header: magic + version + bucket u32 + pair count u64.
const SLAB_HEADER: usize = 20;
/// One pair: name_hash u32 + file_key u64.
const ROW: usize = 12;
/// TOC header: magic + version + bucket count u32 + total pairs u64.
const TOC_HEADER: usize = 20;
/// One per-slab TOC row: pairs u64 + byte len u64 + digest u64.
const TOC_ROW: usize = 24;

/// Whether `name` (generation-relative) is a usage-family member.
pub fn is_usage_member(name: &str) -> bool {
  if name == USAGE_TOC {
    return true;
  }
  name
    .strip_prefix("usage/")
    .and_then(|f| f.strip_suffix(".idx"))
    .is_some_and(|k| !k.is_empty() && k.len() <= 5 && k.bytes().all(|b| b.is_ascii_digit()))
}

struct TocRow {
  pairs: u64,
  len: u64,
  digest: u64,
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
    let total = u64::from_le_bytes(bytes[12..20].try_into().ok()?);
    if bytes.len() < TOC_HEADER + buckets * TOC_ROW {
      return None;
    }
    let mut rows = Vec::with_capacity(buckets);
    for k in 0..buckets {
      let at = TOC_HEADER + k * TOC_ROW;
      let row = &bytes[at..at + TOC_ROW];
      rows.push(TocRow {
        pairs: u64::from_le_bytes(row[0..8].try_into().ok()?),
        len: u64::from_le_bytes(row[8..16].try_into().ok()?),
        digest: u64::from_le_bytes(row[16..24].try_into().ok()?),
      });
    }
    if rows.iter().map(|r| r.pairs).sum::<u64>() != total {
      return None;
    }
    Some(Toc { rows })
  }
}

/// Persist the usage postings: `pairs` is the raw `(name_hash, from-file_key)` stream (one
/// per evidence occurrence; duplicates welcome — deduped here), `buckets` the family
/// bucket count, `prior` the hard-link source.
/// Apply a per-file postings delta without re-bucketing the world: only the buckets a
/// removed or added pair maps to are rebuilt (prior rows minus removals plus additions,
/// re-sorted, re-deduped); every other member hard-links and the TOC re-splices. The
/// full-swap `save` re-encoded every bucket to rediscover that nothing moved — measured
/// waste on every compose.
pub(crate) fn apply_delta(
  staging: &Path,
  prior: &Path,
  buckets: u32,
  removed: &std::collections::HashSet<(u32, u64)>,
  added: &[(u32, u64)],
) -> io::Result<()> {
  if buckets == 0 {
    return Err(io::Error::other("usage family requires a bucket count"));
  }
  let store = UsageStore::open(prior)
    .ok_or_else(|| io::Error::other("usage delta requires a readable prior family"))?;
  if store.slabs.len() != buckets as usize {
    return Err(io::Error::other("usage delta: bucket count moved"));
  }
  let mut touched: Vec<Vec<(u32, u64)>> = vec![Vec::new(); buckets as usize];
  let mut is_touched = vec![false; buckets as usize];
  for &(hash, key) in removed.iter().chain(added) {
    is_touched[(hash & (buckets - 1)) as usize] = true;
    let _ = key;
  }
  for &(hash, key) in added {
    touched[(hash & (buckets - 1)) as usize].push((hash, key));
  }
  let usage_dir = staging.join(USAGE_DIR);
  fs::create_dir_all(&usage_dir)?;
  let mut toc = fs::read(prior.join(USAGE_TOC))
    .map_err(|_| io::Error::other("usage delta: prior TOC unreadable"))?;
  for bucket in 0..buckets as usize {
    let name = format!("{bucket:04}.idx");
    if !is_touched[bucket] {
      let (from, to) = (prior.join(USAGE_DIR).join(&name), usage_dir.join(&name));
      let _ = fs::remove_file(&to);
      if fs::hard_link(&from, &to).is_err() {
        fs::copy(&from, &to)?;
      }
      continue;
    }
    let mut rows: Vec<(u32, u64)> = Vec::new();
    if let Some(slab) = &store.slabs[bucket] {
      for i in 0..slab.pairs {
        let pair = slab.pair(i);
        if !removed.contains(&pair) {
          rows.push(pair);
        }
      }
    }
    rows.extend_from_slice(&touched[bucket]);
    rows.sort_unstable();
    rows.dedup();
    let mut bytes = Vec::with_capacity(SLAB_HEADER + rows.len() * ROW);
    bytes.extend_from_slice(SLAB_MAGIC);
    bytes.extend_from_slice(&VERSION.to_le_bytes());
    bytes.extend_from_slice(&(bucket as u32).to_le_bytes());
    bytes.extend_from_slice(&(rows.len() as u64).to_le_bytes());
    for (hash, key) in &rows {
      bytes.extend_from_slice(&hash.to_le_bytes());
      bytes.extend_from_slice(&key.to_le_bytes());
    }
    let digest = xxhash_rust::xxh3::xxh3_64(&bytes);
    // Digest-carry parity with the full saver: a net-identical bucket (a pair removed
    // and re-added) hard-links instead of rewriting, preserving the inode carry laws.
    let at = TOC_HEADER + bucket * TOC_ROW;
    let prior_digest = u64::from_le_bytes(
      toc
        .get(at + 16..at + 24)
        .ok_or_else(|| io::Error::other("usage delta: TOC too short"))?
        .try_into()
        .map_err(|_| io::Error::other("usage delta: TOC"))?,
    );
    let prior_len = u64::from_le_bytes(
      toc[at + 8..at + 16].try_into().map_err(|_| io::Error::other("usage delta: TOC"))?,
    );
    if prior_digest == digest && prior_len == bytes.len() as u64 {
      let (from, to) = (prior.join(USAGE_DIR).join(&name), usage_dir.join(&name));
      let _ = fs::remove_file(&to);
      if fs::hard_link(&from, &to).is_err() {
        fs::copy(&from, &to)?;
      }
      continue;
    }
    let tmp = usage_dir.join(format!("{name}.tmp"));
    fs::write(&tmp, &bytes)?;
    fs::rename(&tmp, usage_dir.join(&name))?;
    let prior_pairs_here = u64::from_le_bytes(
      toc
        .get(at..at + 8)
        .ok_or_else(|| io::Error::other("usage delta: TOC too short"))?
        .try_into()
        .map_err(|_| io::Error::other("usage delta: TOC"))?,
    );
    let prior_total = u64::from_le_bytes(
      toc[12..20].try_into().map_err(|_| io::Error::other("usage delta: TOC header"))?,
    );
    toc[12..20]
      .copy_from_slice(&(prior_total - prior_pairs_here + rows.len() as u64).to_le_bytes());
    toc[at..at + 8].copy_from_slice(&(rows.len() as u64).to_le_bytes());
    toc[at + 8..at + 16].copy_from_slice(&(bytes.len() as u64).to_le_bytes());
    toc[at + 16..at + 24].copy_from_slice(&digest.to_le_bytes());
  }
  let toc_tmp = usage_dir.join("toc.bin.tmp");
  fs::write(&toc_tmp, &toc)?;
  fs::rename(&toc_tmp, staging.join(USAGE_TOC))?;
  Ok(())
}

pub(crate) fn save(
  dir: &Path,
  mut pairs: Vec<(u32, u64)>,
  buckets: u32,
  prior: Option<&Path>,
) -> io::Result<()> {
  use rayon::prelude::*;
  if buckets == 0 {
    return Err(io::Error::other("usage family requires a bucket count"));
  }
  let usage_dir = dir.join(USAGE_DIR);
  fs::create_dir_all(&usage_dir)?;
  // Bucket-major, then (name, file) — one sort makes slab extraction a contiguous split
  // and the within-slab order the binary-searchable one.
  pairs.par_sort_unstable_by_key(|&(name, file)| (name & (buckets - 1), name, file));
  pairs.dedup();
  let prior_toc = prior.and_then(|p| Toc::load(&p.join(USAGE_TOC)));
  let prior_ok = prior_toc.as_ref().is_some_and(|toc| toc.rows.len() as u32 == buckets);

  struct Built {
    pairs: u64,
    bytes: Vec<u8>,
    digest: u64,
  }
  let mut starts = Vec::with_capacity(buckets as usize + 1);
  let mut cursor = 0usize;
  for bucket in 0..buckets {
    starts.push(cursor);
    while cursor < pairs.len() && (pairs[cursor].0 & (buckets - 1)) == bucket {
      cursor += 1;
    }
  }
  starts.push(cursor);
  let built: Vec<Built> = (0..buckets as usize)
    .into_par_iter()
    .map(|bucket| {
      let slab = &pairs[starts[bucket]..starts[bucket + 1]];
      let mut bytes = Vec::with_capacity(SLAB_HEADER + slab.len() * ROW);
      bytes.extend_from_slice(SLAB_MAGIC);
      bytes.extend_from_slice(&VERSION.to_le_bytes());
      bytes.extend_from_slice(&(bucket as u32).to_le_bytes());
      bytes.extend_from_slice(&(slab.len() as u64).to_le_bytes());
      for &(name, file) in slab {
        bytes.extend_from_slice(&name.to_le_bytes());
        bytes.extend_from_slice(&file.to_le_bytes());
      }
      let digest = xxhash_rust::xxh3::xxh3_64(&bytes);
      Built {
        pairs: slab.len() as u64,
        bytes,
        digest,
      }
    })
    .collect();

  for (bucket, slab) in built.iter().enumerate() {
    let name = format!("{bucket:04}.idx");
    let carried = prior_ok
      && prior_toc.as_ref().is_some_and(|toc| {
        let row = &toc.rows[bucket];
        row.pairs == slab.pairs && row.len == slab.bytes.len() as u64 && row.digest == slab.digest
      });
    if carried {
      let from = prior
        .map(|p| p.join(USAGE_DIR).join(&name))
        .expect("carried implies a prior");
      let to = usage_dir.join(&name);
      if from == to {
        continue; // legacy same-directory publish: already in place
      }
      let _ = fs::remove_file(&to);
      if fs::hard_link(&from, &to).is_ok() {
        continue;
      }
      // Link refused: fall through to the write — same bytes, full cost.
    }
    let tmp = usage_dir.join(format!("{name}.tmp"));
    fs::write(&tmp, &slab.bytes)?;
    fs::rename(&tmp, usage_dir.join(&name))?;
  }
  let total: u64 = built.iter().map(|s| s.pairs).sum();
  let toc_tmp = usage_dir.join("toc.bin.tmp");
  let mut out = fs::File::create(&toc_tmp)?;
  out.write_all(TOC_MAGIC)?;
  out.write_all(&VERSION.to_le_bytes())?;
  out.write_all(&buckets.to_le_bytes())?;
  out.write_all(&total.to_le_bytes())?;
  for slab in &built {
    out.write_all(&slab.pairs.to_le_bytes())?;
    out.write_all(&(slab.bytes.len() as u64).to_le_bytes())?;
    out.write_all(&slab.digest.to_le_bytes())?;
  }
  drop(out);
  fs::rename(&toc_tmp, dir.join(USAGE_TOC))?;
  if let Ok(dirents) = fs::read_dir(&usage_dir) {
    for entry in dirents.flatten() {
      if let Ok(name) = entry.file_name().into_string() {
        let stale = name
          .strip_suffix(".idx")
          .and_then(|k| k.parse::<u32>().ok())
          .is_some_and(|k| k >= buckets);
        if stale || name.ends_with(".tmp") {
          let _ = fs::remove_file(entry.path());
        }
      }
    }
  }
  Ok(())
}

/// One mapped usage slab.
struct Slab {
  store: MappedStore,
  pairs: usize,
}

impl Slab {
  fn pair(&self, i: usize) -> (u32, u64) {
    let at = SLAB_HEADER + i * ROW;
    let b = &self.store.as_bytes()[at..at + ROW];
    (
      u32::from_le_bytes(b[0..4].try_into().unwrap()),
      u64::from_le_bytes(b[4..12].try_into().unwrap()),
    )
  }
}

/// The mapped read side: `files_referencing(name_hash)` is a binary search in one slab.
pub struct UsageStore {
  slabs: Vec<Option<Slab>>,
}

impl UsageStore {
  /// Map the usage family under `dir`. `None` = family absent/foreign (callers escalate
  /// to the full pipeline — degraded, never wrong).
  pub fn open(dir: &Path) -> Option<UsageStore> {
    let toc = Toc::load(&dir.join(USAGE_TOC))?;
    let mut slabs = Vec::with_capacity(toc.rows.len());
    for (k, row) in toc.rows.iter().enumerate() {
      let path = dir.join(USAGE_DIR).join(format!("{k:04}.idx"));
      let meta = fs::metadata(&path).ok()?;
      if meta.len() != row.len {
        return None; // mixed generation
      }
      if row.pairs == 0 {
        slabs.push(None);
        continue;
      }
      let store = MappedStore::map_file(
        &path,
        StoreKind::Canonical,
        AccessPattern::Random,
        Hotness::Cold,
        &ResourcePolicy::probe(CorpusProbe::new(0, 0)),
      )
      .ok()?;
      let bytes = store.as_bytes();
      if bytes.len() < SLAB_HEADER
        || &bytes[0..4] != SLAB_MAGIC
        || u32::from_le_bytes(bytes[4..8].try_into().ok()?) != VERSION
        || u32::from_le_bytes(bytes[8..12].try_into().ok()?) != k as u32
      {
        return None;
      }
      let pairs = u64::from_le_bytes(bytes[12..20].try_into().ok()?) as usize;
      if pairs as u64 != row.pairs || bytes.len() != SLAB_HEADER + pairs * ROW {
        return None;
      }
      slabs.push(Some(Slab { store, pairs }));
    }
    Some(UsageStore { slabs })
  }

  /// Every file (by key) whose references mention the name with this hash, ascending.
  /// Over-approximation is the contract (32-bit hashes may collide; a collision only
  /// widens the dirty closure, never narrows it).
  pub fn files_referencing(&self, name_hash: u32) -> Vec<u64> {
    let buckets = self.slabs.len() as u32;
    if buckets == 0 {
      return Vec::new();
    }
    let Some(Some(slab)) = self.slabs.get((name_hash & (buckets - 1)) as usize) else {
      return Vec::new();
    };
    let (mut lo, mut hi) = (0usize, slab.pairs);
    while lo < hi {
      let mid = (lo + hi) / 2;
      if slab.pair(mid).0 < name_hash {
        lo = mid + 1;
      } else {
        hi = mid;
      }
    }
    let mut out = Vec::new();
    let mut at = lo;
    while at < slab.pairs {
      let (name, file) = slab.pair(at);
      if name != name_hash {
        break;
      }
      out.push(file);
      at += 1;
    }
    out
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn roundtrips_dedups_and_answers_ascending() {
    let dir = std::env::temp_dir().join(format!("vorpal-usage-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let pairs = vec![
      (0x10u32, 0xAAu64),
      (0x10, 0xBB),
      (0x10, 0xAA), // duplicate occurrence: one posting
      (0x11, 0xAA),
      (0xF3, 0xCC), // different bucket under B=16
    ];
    save(&dir, pairs.clone(), 16, None).unwrap();
    let store = UsageStore::open(&dir).unwrap();
    assert_eq!(store.files_referencing(0x10), vec![0xAA, 0xBB]);
    assert_eq!(store.files_referencing(0x11), vec![0xAA]);
    assert_eq!(store.files_referencing(0xF3), vec![0xCC]);
    assert_eq!(store.files_referencing(0x12), Vec::<u64>::new());

    // Determinism + carry: identical pairs re-saved with a prior hard-link every slab.
    let dir2 = std::env::temp_dir().join(format!("vorpal-usage2-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir2);
    fs::create_dir_all(&dir2).unwrap();
    save(&dir2, pairs, 16, Some(&dir)).unwrap();
    #[cfg(unix)]
    {
      use std::os::unix::fs::MetadataExt;
      for k in 0..16 {
        let name = format!("{k:04}.idx");
        assert_eq!(
          fs::metadata(dir.join(USAGE_DIR).join(&name)).unwrap().ino(),
          fs::metadata(dir2.join(USAGE_DIR).join(&name)).unwrap().ino(),
          "unchanged usage slab {k} must hard-link"
        );
      }
    }
    let _ = fs::remove_dir_all(&dir);
    let _ = fs::remove_dir_all(&dir2);
  }
}
