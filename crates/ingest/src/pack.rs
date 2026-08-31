//! The products pack: every cached extraction product, mapped zero-copy at replay.
//!
//! The loose-file cache paid one `open(2)` per product; at kernel scale that is 72k opens
//! per re-index, and macOS serializes enough of the open path that even 18 threads only
//! doubled throughput — the replay was syscall-bound, not byte-bound. The pack replaces it
//! for index runs: one mmap (per bucket, in the bucketed format), per-entry slices, no
//! opens. Loose files remain the write path for *search-banked* products (concurrent scan
//! processes must not contend on one file) and are consolidated into the pack — then
//! deleted — by the next index run.
//!
//! **Flat layout v1** (`products.pack`): magic + version, then length-prefixed records
//! `[path_len u32][path][body_len u32][body]` where `body` is the ordinary product codec.
//! The sidecar (`products.idx`) is `magic + version + covered_len u64 + count u64` plus
//! `[path_len u32][path][off u64][len u32]` per live entry — **an optimization, not a
//! source of truth**: a run killed after appending but before the sidecar lands loses no
//! work, because open() scans any records beyond `covered_len` (bounds-checked; a torn tail
//! record simply ends the scan) and products remain self-validating at decode time.
//!
//! **Bucketed layout v2** (`products/`, SUBSECOND.md P4.1, written behind
//! `VORPAL_FORMAT=next` until the flip): the pack becomes `products/<k>.pack` bucket files
//! plus `products/toc.bin`. A file's bucket is `file_key & (B-1)` where
//! `file_key = xxh3_64(tree-relative path)` — the P4.0 identity — and B is a **pure
//! function of the live file count** ([`bucket_count_for`]): stamping B at creation would
//! make an incremental build that grows past a threshold diverge byte-wise from a scratch
//! build of the same tree, violating the convergence law. Record encoding inside a bucket
//! is exactly the v1 record encoding, with **tree-relative** path spellings: v1 embedded
//! absolute canonical paths, which made pack bytes — and everything hashed over them —
//! mount-dependent, and kept a moved tree from reusing its own product cache.
//! The TOC carries per-bucket `{entry count, byte length, xxh3 digest}` rows (the Merkle
//! spine P4.4 hashes into the generation id) followed by the slot table (the v2 sidecar).
//! Buckets land `.tmp` + rename with the TOC last; on any TOC/bucket mismatch (a killed
//! legacy-mode run) the reader rebuilds slots by scanning the self-describing bucket
//! records — the same recovery posture as v1.
//!
//! Why file-per-bucket and not bucket regions in one file: a single bucket-major file still
//! rewrites everything after the first changed bucket (≈half the pack on average), and can
//! never share unchanged bytes across generations. Separate files make an edit rewrite
//! O(changed buckets), and unchanged bucket files **hard-link** into the next generation —
//! sealed generations are immutable, rename-over never writes through a link, and GC of an
//! old generation is refcount-safe.
//!
//! **Determinism.** Records stream to a spool in arrival order *during* a run, but every
//! publish rewrites the pack in **canonical order** — v1: live entries sorted by path;
//! v2: bucket-major, path-sorted within each bucket — so the published bytes are a pure
//! function of the `(path, body)` set, independent of worker completion order or
//! incremental history. Two independent indexes of the same corpus therefore produce
//! byte-identical artifacts. Reads are order-agnostic (they build a map), so the recovery
//! scan paths are unaffected.

use std::collections::HashMap;
use std::fs;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use vorpal_kg::identity::{FileKey, tree_relative};
use vorpal_mem::{AccessPattern, CorpusProbe, Hotness, MappedStore, ResourcePolicy, StoreKind};

pub const PACK_FILE: &str = "products.pack";
pub const PACK_INDEX: &str = "products.idx";
/// The bucketed layout's directory and TOC, relative to the generation dir.
pub const PACK_DIR: &str = "products";
pub const PACK_TOC: &str = "products/toc.bin";
const PACK_MAGIC: &[u8; 4] = b"VPPK";
const IDX_MAGIC: &[u8; 4] = b"VPPI";
const BUCKET_MAGIC: &[u8; 4] = b"VPPB";
const TOC_MAGIC: &[u8; 4] = b"VPPT";
// v2: publishes are canonically ordered (entries sorted by path). v1 packs were written in
// arrival order; bumping the version retires them so the first index under this build rebuilds
// a canonical pack rather than inheriting a stale, unsorted layout.
const PACK_VERSION: u32 = 2;
/// Version counter for the bucketed file kinds (`VPPB`/`VPPT`) — independent of the flat
/// pack's counter; both start their own history.
const BUCKET_VERSION: u32 = 1;
/// Bucket-file header: magic + version + bucket index.
const BUCKET_HEADER: usize = 12;
/// TOC header: magic + version + bucket count u32 + total entries u64.
const TOC_HEADER: usize = 20;
/// One per-bucket TOC row: entry count u32 + byte length u64 + xxh3 digest u64.
const TOC_ROW: usize = 20;

/// Which pack layout a writer publishes. Readers sniff; only writers choose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackFormat {
  Flat,
  Bucketed,
}

impl PackFormat {
  /// The format this process writes: flat until the flip, bucketed under
  /// `VORPAL_FORMAT=next` (locked Phase-4 compat posture — v1 stays readable for one
  /// release either way).
  pub fn from_env() -> PackFormat {
    match std::env::var("VORPAL_FORMAT") {
      Ok(v) if v == "next" => PackFormat::Bucketed,
      _ => PackFormat::Flat,
    }
  }
}

// Bucket-count law (P4.1): B = clamp(next_pow2(files / BUCKET_TARGET_FILES), MIN, MAX),
// a pure function of the live file count — see the module docs for why purity is load-
// bearing. Constants from the recorded two-scale sweep (docs/wip/SUBSECOND.md §P4.1,
// `bucket-sweep` — linux kernel 76 868 files and vorpal repo ~2k): the kernel edit-one
// wall was 0.43 s at B=256, 0.54 s at B=1024, and 1.24 s at B=4096 (per-bucket link/mmap
// overhead beats byte savings once buckets shrink past ~100 files), so the target lands
// the kernel exactly on its measured optimum: 76 868 / 512 → next_pow2(150) = 256.
const BUCKET_TARGET_FILES: usize = 512;
const BUCKET_MIN: u32 = 16;
/// Also the naming bound: `{:04}` bucket file names sort numerically only below 10 000.
const BUCKET_MAX: u32 = 4096;

/// The bucket count for a corpus of `files` live files. Pure, monotonic, power-of-two.
pub fn bucket_count_for(files: usize) -> u32 {
  let want = files.div_ceil(BUCKET_TARGET_FILES).max(1) as u64;
  let pow2 = want.next_power_of_two().min(u64::from(BUCKET_MAX)) as u32;
  pow2.clamp(BUCKET_MIN, BUCKET_MAX)
}

/// [`bucket_count_for`] with the measurement override applied. `VORPAL_PACK_BUCKETS` exists
/// for the recorded sweeps and for tests that need a fixed B; setting it in production
/// breaks scratch/incremental byte-convergence unless both builds see the same value.
fn bucket_count_effective(files: usize) -> u32 {
  match std::env::var("VORPAL_PACK_BUCKETS").ok().and_then(|v| v.parse::<u32>().ok()) {
    Some(forced) if forced > 0 => forced.next_power_of_two().min(BUCKET_MAX),
    _ => bucket_count_for(files),
  }
}

/// The bucket a tree-relative path lands in, for a power-of-two bucket count.
fn bucket_of(tree_relative_path: &str, buckets: u32) -> u32 {
  (FileKey::of(tree_relative_path).0 & u64::from(buckets - 1)) as u32
}

/// The generation-relative file name of bucket `k` (`products/0007.pack`).
pub fn bucket_file_name(bucket: u32) -> String {
  format!("{PACK_DIR}/{bucket:04}.pack")
}

/// Whether `name` (a generation-relative artifact name) belongs to the bucketed pack:
/// the TOC or a `products/<k>.pack` bucket file. The artifact enumeration sites
/// (content-id, export/import, commit staging) extend their flat lists with this.
pub fn is_pack_member(name: &str) -> bool {
  if name == PACK_TOC {
    return true;
  }
  name
    .strip_prefix("products/")
    .and_then(|f| f.strip_suffix(".pack"))
    .is_some_and(|k| !k.is_empty() && k.len() <= 5 && k.bytes().all(|b| b.is_ascii_digit()))
}

/// Splice a recomputed digest for `bucket` into TOC bytes in place — the stamp-only commit
/// cutoff patches stamp windows inside copied bucket files, which changes their bytes but
/// not their lengths, so only the digest column moves. Returns false if the TOC is too
/// short or the bucket is out of range (caller falls back to a full rewrite).
pub fn splice_toc_digest(toc: &mut [u8], bucket: u32, digest: u64) -> bool {
  if toc.len() < TOC_HEADER || &toc[0..4] != TOC_MAGIC {
    return false;
  }
  let buckets = u32::from_le_bytes(match toc[8..12].try_into() {
    Ok(b) => b,
    Err(_) => return false,
  });
  if bucket >= buckets {
    return false;
  }
  let at = TOC_HEADER + TOC_ROW * bucket as usize + 12;
  let Some(window) = toc.get_mut(at..at + 8) else {
    return false;
  };
  window.copy_from_slice(&digest.to_le_bytes());
  true
}

/// A live entry: body offset + length within its pack file.
type Entry = (u64, u32);

/// Where an entry's path bytes live: the retained sidecar/TOC buffer, or the mapped pack
/// itself (recovery-scanned records). Referencing ranges instead of owning strings is what
/// makes open() allocation-free per entry — at kernel scale the old `HashMap<Box<str>, _>`
/// build was 72k heap allocations + SipHash, ~5 ms of pure tax on every one-shot query and
/// every replay.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PathSrc {
  Sidecar,
  Pack,
}

#[derive(Clone, Copy)]
struct Slot {
  path_src: PathSrc,
  /// Which mapped bucket file the body (and `PathSrc::Pack` path bytes) live in. Always 0
  /// for the flat layout.
  bucket: u16,
  path_off: u64,
  path_len: u32,
  body_off: u64,
  body_len: u32,
}

/// Keys are already xxh3-mixed, so the map hasher is the identity — full hash quality with
/// zero re-hash work. Deterministic and dependency-free.
#[derive(Default)]
struct PrehashedId(u64);

impl std::hash::Hasher for PrehashedId {
  fn finish(&self) -> u64 {
    self.0
  }
  fn write(&mut self, bytes: &[u8]) {
    // Keys arrive prehashed via write_u64; if the map ever hashes raw bytes anyway, fold
    // them correctly instead of failing — same distribution, never a panic.
    for &byte in bytes {
      self.0 = self.0.rotate_left(8) ^ u64::from(byte).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    }
  }
  fn write_u64(&mut self, key: u64) {
    self.0 = key;
  }
}

type PrehashedMap = HashMap<u64, Slot, std::hash::BuildHasherDefault<PrehashedId>>;

/// One bucket's TOC row, retained for the writer's link-reuse check and the cutoff's
/// digest recompute.
#[derive(Clone, Copy)]
pub struct BucketMeta {
  pub entries: u32,
  pub len: u64,
  pub digest: u64,
}

/// Read side: the mapped pack plus its live-entry index.
pub struct PackReader {
  /// One mapped store per bucket (index 0 only, for the flat layout). `None` marks a
  /// bucket file the recovery scan found missing — its products simply refetch.
  stores: Vec<Option<Arc<MappedStore>>>,
  /// The generation directory this reader mapped — the writer's hard-link source.
  dir: PathBuf,
  /// Retained sidecar/TOC bytes — the backing for `PathSrc::Sidecar` ranges.
  sidecar: Vec<u8>,
  /// `xxh3(stored path)` → slot. Every hit is byte-verified against the slot's path range,
  /// so a 64-bit collision can never serve the wrong product.
  index: PrehashedMap,
  /// Same-hash-different-path residents (astronomically rare): checked after a failed
  /// primary verification.
  overflow: Vec<(u64, Slot)>,
  /// Bucketed layout: stored paths are tree-relative; lookups by absolute path strip
  /// `root` at the API boundary.
  relative_keys: bool,
  /// The canonical tree root for absolute→relative stripping (open_rooted).
  root: Option<String>,
  /// Per-bucket TOC rows, when this is a bucketed pack loaded through a consistent TOC
  /// (`None` after a recovery scan — link-reuse and digest carry then disable themselves).
  meta: Option<Vec<BucketMeta>>,
}

impl PackReader {
  /// Open the pack under `dir`, if present and well-formed — bucketed layout first, then
  /// flat. Lookups against a bucketed pack use stored (tree-relative) spellings; callers
  /// holding absolute paths must use [`PackReader::open_rooted`].
  pub fn open(dir: &Path) -> Option<PackReader> {
    Self::open_rooted(dir, None)
  }

  /// [`PackReader::open`] with the canonical tree root, so `get`/`entry`/`body_locus`
  /// accept the absolute canonical spellings production callers hold (manifest entries,
  /// File-node names). The root is irrelevant to the flat layout.
  pub fn open_rooted(dir: &Path, root: Option<&str>) -> Option<PackReader> {
    let root = root.map(|r| r.to_string());
    if dir.join(PACK_TOC).is_file() || dir.join(PACK_DIR).is_dir() {
      if let Some(reader) = Self::open_bucketed(dir, root.clone()) {
        return Some(reader);
      }
      // A products/ dir that yields nothing readable falls through to the flat pack —
      // e.g. an interrupted first bucketed publish beside a still-complete flat pack.
    }
    Self::open_flat(dir, root)
  }

  fn map_store(path: &Path) -> Option<Arc<MappedStore>> {
    Some(Arc::new(
      MappedStore::map_file(
        path,
        StoreKind::VectorsFull,
        AccessPattern::Sequential,
        Hotness::Hot,
        &ResourcePolicy::probe(CorpusProbe::new(0, 0)),
      )
      .ok()?,
    ))
  }

  fn open_flat(dir: &Path, root: Option<String>) -> Option<PackReader> {
    let pack_path = dir.join(PACK_FILE);
    let store = Self::map_store(&pack_path)?;
    let bytes = store.as_bytes();
    if bytes.len() < 8 || &bytes[0..4] != PACK_MAGIC {
      return None;
    }
    if u32::from_le_bytes(bytes[4..8].try_into().ok()?) != PACK_VERSION {
      return None;
    }

    let mut reader = PackReader {
      stores: vec![Some(store.clone())],
      dir: dir.to_path_buf(),
      sidecar: Vec::new(),
      index: PrehashedMap::default(),
      overflow: Vec::new(),
      relative_keys: false,
      root,
      meta: None,
    };
    let mut scan_from = 8usize;
    if let Some(covered) = reader.load_sidecar(&dir.join(PACK_INDEX), store.as_bytes().len()) {
      scan_from = covered;
    }
    // Recovery / tail scan: pick up records the sidecar has not seen. A torn final record
    // fails a bounds check and ends the scan; whatever decoded cleanly is kept.
    reader.scan_records(&store, 0, scan_from);
    Some(reader)
  }

  /// Walk one mapped file's self-describing records from `scan_from`, inserting slots
  /// against bucket index `bucket`. Torn tails end the scan silently.
  fn scan_records(&mut self, store: &Arc<MappedStore>, bucket: u16, scan_from: usize) {
    let bytes = store.as_bytes();
    let read_u32 = |at: usize| -> Option<u32> {
      Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
    };
    let mut at = scan_from;
    while at + 8 <= bytes.len() {
      let Some(path_len) = read_u32(at).map(|v| v as usize) else {
        break;
      };
      let Some(path_end) = (at + 4)
        .checked_add(path_len)
        .filter(|&e| e + 4 <= bytes.len())
      else {
        break;
      };
      let Some(Ok(path)) = bytes.get(at + 4..path_end).map(std::str::from_utf8) else {
        break;
      };
      let Some(body_len) = read_u32(path_end).map(|v| v as usize) else {
        break;
      };
      let body_at = path_end + 4;
      let Some(body_end) = body_at.checked_add(body_len).filter(|&e| e <= bytes.len()) else {
        break;
      };
      let slot = Slot {
        path_src: PathSrc::Pack,
        bucket,
        path_off: (at + 4) as u64,
        path_len: path_len as u32,
        body_off: body_at as u64,
        body_len: body_len as u32,
      };
      self.insert(xxhash_rust::xxh3::xxh3_64(path.as_bytes()), slot);
      at = body_end;
    }
  }

  fn open_bucketed(dir: &Path, root: Option<String>) -> Option<PackReader> {
    let mut reader = PackReader {
      stores: Vec::new(),
      dir: dir.to_path_buf(),
      sidecar: Vec::new(),
      index: PrehashedMap::default(),
      overflow: Vec::new(),
      relative_keys: true,
      root,
      meta: None,
    };
    if reader.load_toc(dir).is_some() {
      return Some(reader);
    }
    // TOC missing or inconsistent (killed legacy-mode run): rebuild from the
    // self-describing bucket records. Whatever decodes cleanly is served; missing
    // products refetch through the ordinary pipeline.
    reader.sidecar = Vec::new();
    reader.index = PrehashedMap::default();
    reader.overflow = Vec::new();
    reader.meta = None;
    reader.stores = Vec::new();
    let mut names: Vec<(u32, PathBuf)> = fs::read_dir(dir.join(PACK_DIR))
      .ok()?
      .filter_map(|e| {
        let entry = e.ok()?;
        let name = entry.file_name().into_string().ok()?;
        let k: u32 = name.strip_suffix(".pack")?.parse().ok()?;
        (k < BUCKET_MAX).then(|| (k, entry.path()))
      })
      .collect();
    names.sort_unstable_by_key(|(k, _)| *k);
    for (k, path) in names {
      let Some(store) = Self::map_store(&path) else {
        continue;
      };
      let bytes = store.as_bytes();
      if bytes.len() < BUCKET_HEADER || &bytes[0..4] != BUCKET_MAGIC {
        continue;
      }
      if u32::from_le_bytes(bytes[4..8].try_into().ok()?) != BUCKET_VERSION {
        continue;
      }
      if reader.stores.len() <= k as usize {
        reader.stores.resize(k as usize + 1, None);
      }
      reader.scan_records(&store, k as u16, BUCKET_HEADER);
      reader.stores[k as usize] = Some(store);
    }
    (!reader.stores.is_empty()).then_some(reader)
  }

  /// Load the TOC and map every bucket it names. Returns `None` on ANY inconsistency —
  /// bad header, missing bucket file, byte-length mismatch — and the caller falls back to
  /// the recovery scan; a TOC is never half-trusted.
  fn load_toc(&mut self, dir: &Path) -> Option<()> {
    let bytes = fs::read(dir.join(PACK_TOC)).ok()?;
    if bytes.len() < TOC_HEADER || &bytes[0..4] != TOC_MAGIC {
      return None;
    }
    if u32::from_le_bytes(bytes[4..8].try_into().ok()?) != BUCKET_VERSION {
      return None;
    }
    let buckets = u32::from_le_bytes(bytes[8..12].try_into().ok()?) as usize;
    if buckets == 0 || buckets > BUCKET_MAX as usize {
      return None;
    }
    let total = u64::from_le_bytes(bytes[12..20].try_into().ok()?) as usize;
    let mut meta = Vec::with_capacity(buckets);
    for k in 0..buckets {
      let at = TOC_HEADER + TOC_ROW * k;
      let row = bytes.get(at..at + TOC_ROW)?;
      meta.push(BucketMeta {
        entries: u32::from_le_bytes(row[0..4].try_into().ok()?),
        len: u64::from_le_bytes(row[4..12].try_into().ok()?),
        digest: u64::from_le_bytes(row[12..20].try_into().ok()?),
      });
    }
    let mut stores = Vec::with_capacity(buckets);
    for (k, row) in meta.iter().enumerate() {
      let path = dir.join(PACK_DIR).join(format!("{k:04}.pack"));
      if fs::metadata(&path).ok()?.len() != row.len {
        return None; // bucket file from a different publish than this TOC
      }
      stores.push(Some(Self::map_store(&path)?));
    }
    // Slot table: bucket-major, path-sorted within buckets; bucket implicit via row counts.
    let mut index = PrehashedMap::with_capacity_and_hasher(total, Default::default());
    let mut overflow = Vec::new();
    let mut at = TOC_HEADER + TOC_ROW * buckets;
    for (k, row) in meta.iter().enumerate() {
      let store_len = stores[k].as_ref().map(|s| s.as_bytes().len()).unwrap_or(0);
      for _ in 0..row.entries {
        let path_len = u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?) as usize;
        let path = std::str::from_utf8(bytes.get(at + 4..at + 4 + path_len)?).ok()?;
        let off_at = at + 4 + path_len;
        let off = u64::from_le_bytes(bytes.get(off_at..off_at + 8)?.try_into().ok()?);
        let len = u32::from_le_bytes(bytes.get(off_at + 8..off_at + 12)?.try_into().ok()?);
        if off as usize + len as usize > store_len {
          return None;
        }
        let slot = Slot {
          path_src: PathSrc::Sidecar,
          bucket: k as u16,
          path_off: (at + 4) as u64,
          path_len: path_len as u32,
          body_off: off,
          body_len: len,
        };
        let hash = xxhash_rust::xxh3::xxh3_64(path.as_bytes());
        // Canonical TOCs carry unique paths; same-hash residents go to overflow.
        match index.entry(hash) {
          std::collections::hash_map::Entry::Vacant(slot_entry) => {
            slot_entry.insert(slot);
          }
          std::collections::hash_map::Entry::Occupied(_) => overflow.push((hash, slot)),
        }
        at = off_at + 12;
      }
    }
    self.sidecar = bytes;
    self.index = index;
    self.overflow = overflow;
    self.stores = stores;
    self.meta = Some(meta);
    Some(())
  }

  /// Parse the flat sidecar into slots referencing its retained bytes. Returns the covered
  /// length on success; on any inconsistency the whole sidecar is discarded (recovery scan
  /// takes over), never half-trusted.
  fn load_sidecar(&mut self, path: &Path, pack_len: usize) -> Option<usize> {
    let bytes = fs::read(path).ok()?;
    if bytes.len() < 24 || &bytes[0..4] != IDX_MAGIC {
      return None;
    }
    if u32::from_le_bytes(bytes[4..8].try_into().ok()?) != PACK_VERSION {
      return None;
    }
    let covered = u64::from_le_bytes(bytes[8..16].try_into().ok()?) as usize;
    if covered > pack_len {
      return None; // sidecar from a different pack generation
    }
    let count = u64::from_le_bytes(bytes[16..24].try_into().ok()?) as usize;
    let mut index = PrehashedMap::with_capacity_and_hasher(count, Default::default());
    let mut overflow = Vec::new();
    let mut at = 24usize;
    for _ in 0..count {
      let path_len = u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?) as usize;
      let path = std::str::from_utf8(bytes.get(at + 4..at + 4 + path_len)?).ok()?;
      let off_at = at + 4 + path_len;
      let off = u64::from_le_bytes(bytes.get(off_at..off_at + 8)?.try_into().ok()?);
      let len = u32::from_le_bytes(bytes.get(off_at + 8..off_at + 12)?.try_into().ok()?);
      if off as usize + len as usize > pack_len {
        return None;
      }
      let slot = Slot {
        path_src: PathSrc::Sidecar,
        bucket: 0,
        path_off: (at + 4) as u64,
        path_len: path_len as u32,
        body_off: off,
        body_len: len,
      };
      let hash = xxhash_rust::xxh3::xxh3_64(path.as_bytes());
      // Canonical sidecars carry unique paths; same-hash residents go to overflow.
      match index.entry(hash) {
        std::collections::hash_map::Entry::Vacant(slot_entry) => {
          slot_entry.insert(slot);
        }
        std::collections::hash_map::Entry::Occupied(_) => overflow.push((hash, slot)),
      }
      at = off_at + 12;
    }
    self.sidecar = bytes;
    self.index = index;
    self.overflow = overflow;
    Some(covered)
  }

  fn bucket_bytes(&self, bucket: u16) -> &[u8] {
    self
      .stores
      .get(bucket as usize)
      .and_then(|s| s.as_ref())
      .map(|s| s.as_bytes())
      .unwrap_or(&[])
  }

  fn path_bytes(&self, slot: &Slot) -> &[u8] {
    let (start, end) = (slot.path_off as usize, (slot.path_off + slot.path_len as u64) as usize);
    match slot.path_src {
      PathSrc::Sidecar => self.sidecar.get(start..end).unwrap_or(&[]),
      PathSrc::Pack => self.bucket_bytes(slot.bucket).get(start..end).unwrap_or(&[]),
    }
  }

  /// Insert (recovery scan): replace an existing same-path slot wherever it lives, else
  /// claim the primary map slot or overflow. Runs only for records past the sidecar's
  /// covered length (a handful after a kill; whole files only when the sidecar/TOC is
  /// gone), so the owned needle allocation is off the common open path.
  fn insert(&mut self, hash: u64, slot: Slot) {
    let Some(&existing) = self.index.get(&hash) else {
      self.index.insert(hash, slot);
      return;
    };
    let needle = self.path_bytes(&slot).to_vec();
    if self.path_bytes(&existing) == needle.as_slice() {
      self.index.insert(hash, slot);
      return;
    }
    if let Some(i) = (0..self.overflow.len())
      .find(|&i| self.overflow[i].0 == hash && self.path_bytes(&self.overflow[i].1) == needle)
    {
      self.overflow[i].1 = slot;
      return;
    }
    self.overflow.push((hash, slot));
  }

  /// The stored-key spelling of a caller's path: bucketed packs store tree-relative
  /// spellings, so an absolute path is stripped against the reader's root — the single
  /// conversion point on the read side.
  fn stored_key<'a>(&self, path: &'a str) -> &'a str {
    match (&self.root, self.relative_keys) {
      (Some(root), true) => tree_relative(path, root),
      _ => path,
    }
  }

  fn lookup(&self, path: &str) -> Option<&Slot> {
    let path = self.stored_key(path);
    let hash = xxhash_rust::xxh3::xxh3_64(path.as_bytes());
    if let Some(slot) = self.index.get(&hash) {
      if self.path_bytes(slot) == path.as_bytes() {
        return Some(slot);
      }
    }
    self
      .overflow
      .iter()
      .find(|(h, slot)| *h == hash && self.path_bytes(slot) == path.as_bytes())
      .map(|(_, slot)| slot)
  }

  fn body(&self, slot: &Slot) -> Option<&[u8]> {
    self
      .bucket_bytes(slot.bucket)
      .get(slot.body_off as usize..(slot.body_off + slot.body_len as u64) as usize)
  }

  /// The cached product bytes for `path`, if packed. Decode + stamp validation stay the
  /// caller's job — exactly as with a loose file's bytes.
  pub fn get(&self, path: &str) -> Option<&[u8]> {
    self.body(self.lookup(path)?)
  }

  fn entry(&self, path: &str) -> Option<Entry> {
    self.lookup(path).map(|slot| (slot.body_off, slot.body_len))
  }

  /// The `(bucket, file offset, length)` of `path`'s product body — the positioned-patch
  /// handle the stamp-only commit cutoff uses (product stamp bytes sit at fixed offsets
  /// inside the body). Bucket is 0 for the flat layout; [`bucket_file_name`] names the
  /// file to patch, [`PackReader::is_bucketed`] says which layout this is.
  pub fn body_locus(&self, path: &str) -> Option<(u32, u64, u32)> {
    self
      .lookup(path)
      .map(|slot| (u32::from(slot.bucket), slot.body_off, slot.body_len))
  }

  /// Whether this reader mapped the bucketed layout.
  pub fn is_bucketed(&self) -> bool {
    self.relative_keys
  }

  /// This reader with its stripping root replaced — for callers that must open first and
  /// derive the root from the generation's own manifest (query surfaces handed only an
  /// index directory). A rootless bucketed reader still answers exact stored-key lookups;
  /// it never guesses at absolute ones (a suffix walk could byte-verify against a
  /// same-suffix twin and serve the wrong product).
  pub fn with_root(mut self, root: Option<String>) -> Self {
    self.root = root;
    self
  }

  /// The number of buckets actually mapped (1 for the flat layout).
  pub fn loaded_buckets(&self) -> u32 {
    self.stores.len() as u32
  }

  /// The number of live entries this reader serves.
  pub fn live_entries(&self) -> usize {
    self.index.len() + self.overflow.len()
  }

  /// Per-bucket TOC rows, when this pack was loaded through a consistent TOC.
  pub fn bucket_meta(&self) -> Option<&[BucketMeta]> {
    self.meta.as_deref()
  }

  /// Every packed `(stored path, product bytes)` pair, in unspecified order — whole-bank
  /// sweeps (coverage overviews) sort their own results. Bucketed packs yield
  /// tree-relative spellings. Bytes are the raw cached product; decode and stamp
  /// validation stay the caller's job.
  pub fn entries(&self) -> impl Iterator<Item = (&str, &[u8])> {
    self
      .index
      .values()
      .chain(self.overflow.iter().map(|(_, slot)| slot))
      .filter_map(|slot| {
        let path = std::str::from_utf8(self.path_bytes(slot)).ok()?;
        Some((path, self.body(slot)?))
      })
  }

  /// One bucket's live `(tree-relative path, body span)` set, path-sorted — what the
  /// writer's hard-link carry emits into the new TOC (offsets in an identical file are
  /// identical, so these ARE the rewrite's slots).
  fn bucket_slots(&self, bucket: u16) -> Vec<(&str, u64, u32)> {
    let mut rows: Vec<(&str, u64, u32)> = self
      .index
      .values()
      .chain(self.overflow.iter().map(|(_, slot)| slot))
      .filter(|slot| slot.bucket == bucket)
      .filter_map(|slot| {
        let path = std::str::from_utf8(self.path_bytes(slot)).ok()?;
        Some((path, slot.body_off, slot.body_len))
      })
      .collect();
    rows.sort_unstable_by(|a, b| a.0.cmp(b.0));
    rows
  }
}

/// One message from the extraction pipeline to the pack thread: a freshly encoded product
/// (new parse, or a loose file being consolidated). Products replayed straight from the
/// pack send **nothing** — their entries are carried into the new sidecar in bulk at
/// [`PackWriter::finish`], from the live path set. At kernel scale the per-file reuse
/// message was 72k sends against one channel; the profile showed every worker blocked on it.
pub struct PackMsg {
  pub path: String,
  pub body: Vec<u8>,
}

/// The post-`sink` half of a [`PackWriter`]: everything `finish` needs once the append
/// channel is dropped.
struct FinishState {
  dir: PathBuf,
  rx: crossbeam_channel::Receiver<PackMsg>,
  reader: Option<Arc<PackReader>>,
}

/// Where one canonical entry's body currently lives: freshly appended to this run's spool
/// (an offset into the local file), or carried from the prior generation's pack (fetched by
/// path through its mapped [`PackReader`]).
enum BodySource {
  Appended(Entry),
  Reused,
}

/// A writer that streams every byte through an xxh3 as it goes to disk — the per-bucket
/// digest column comes for free with the write pass instead of a second read.
struct HashingWriter<W: Write> {
  inner: W,
  hash: xxhash_rust::xxh3::Xxh3,
  written: u64,
}

impl<W: Write> HashingWriter<W> {
  fn new(inner: W) -> Self {
    Self {
      inner,
      hash: xxhash_rust::xxh3::Xxh3::new(),
      written: 0,
    }
  }
}

impl<W: Write> Write for HashingWriter<W> {
  fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
    let n = self.inner.write(buf)?;
    self.hash.update(&buf[..n]);
    self.written += n as u64;
    Ok(n)
  }
  fn flush(&mut self) -> io::Result<()> {
    self.inner.flush()
  }
}

/// Write side: builds **this run's** pack in `dir` (a staging/new-generation directory) —
/// never mutating the prior generation's pack, which is only *read* through `reader`. Feed it
/// with [`PackWriter::sink`] clones from any thread; call [`PackWriter::finish`] after the
/// pipeline ends to publish the canonical pack + sidecar.
pub struct PackWriter {
  dir: PathBuf,
  rx: crossbeam_channel::Receiver<PackMsg>,
  tx: crossbeam_channel::Sender<PackMsg>,
  reader: Option<Arc<PackReader>>,
  /// The canonical tree root — the bucketed format's absolute→tree-relative conversion
  /// point. Required by [`PackFormat::Bucketed`]; ignored by the flat writer.
  root: Option<String>,
  format: PackFormat,
}

impl PackWriter {
  pub fn new(
    dir: &Path,
    reader: Option<Arc<PackReader>>,
    root: Option<String>,
    format: PackFormat,
  ) -> Self {
    let (tx, rx) = crossbeam_channel::bounded(1024);
    Self {
      dir: dir.to_path_buf(),
      rx,
      tx,
      reader,
      root,
      format,
    }
  }

  /// A clone of the append channel. `finish(self)` consumes the writer, so sink-after-finish
  /// is unrepresentable — the type system carries the contract the old `Option` + expect
  /// merely asserted.
  pub fn sink(&self) -> crossbeam_channel::Sender<PackMsg> {
    self.tx.clone()
  }

  /// Drain every append (streamed to disk as it arrives — bounded memory), carry entries for
  /// every path in `live` that was not re-appended from the prior generation's pack, then
  /// publish the **canonical** pack (a pure function of the `(path, body)` set) plus its
  /// sidecar/TOC, via `.tmp` + rename. The prior pack is never touched: reused bodies are
  /// copied out of `reader`'s mapping — or, in the bucketed layout, whole unchanged bucket
  /// files are hard-linked — so the previous generation stays complete for any reader still
  /// holding it. Call only after every [`PackWriter::sink`] clone is dropped.
  pub fn finish(self, live: impl IntoIterator<Item = String>) -> io::Result<()> {
    let PackWriter { dir, rx, tx, reader, root, format } = self;
    let this = FinishState { dir, rx, reader };
    drop(tx);
    // Fresh spool for this run's appended records (magic + version header first). A side
    // file, not `products.pack` itself: the reader may be mapping a same-named prior pack in
    // this very directory (legacy flat layout, tests), and truncating it in place would
    // clobber the bodies reuse is about to copy. The canonical pack lands via tmp + rename at
    // the end, so `products.pack` is only ever a complete prior pack or a complete new one.
    let spool_path = this.dir.join("products.pack.spool");
    let mut file = fs::File::create(&spool_path)?;
    file.write_all(PACK_MAGIC)?;
    file.write_all(&PACK_VERSION.to_le_bytes())?;
    let mut at = 8u64;
    let mut out = BufWriter::with_capacity(1 << 20, file);

    let mut entries: Vec<(String, BodySource)> = Vec::new();
    let mut appended: std::collections::HashSet<Box<str>> = std::collections::HashSet::new();
    while let Ok(PackMsg { path, body }) = this.rx.recv() {
      out.write_all(&(path.len() as u32).to_le_bytes())?;
      out.write_all(path.as_bytes())?;
      out.write_all(&(body.len() as u32).to_le_bytes())?;
      out.write_all(&body)?;
      let body_at = at + 4 + path.len() as u64 + 4;
      appended.insert(path.as_str().into());
      entries.push((path, BodySource::Appended((body_at, body.len() as u32))));
      at = body_at + body.len() as u64;
    }
    out.flush()?;
    drop(out);
    // Bulk reuse: every live path not re-appended carries over from the prior pack (the
    // reader answers under either layout — this is also the one-time v1→v2 migration path:
    // the first bucketed publish reuses every flat-pack body without a re-extract).
    if let Some(reader) = &this.reader {
      for path in live {
        if !appended.contains(path.as_str()) && reader.entry(&path).is_some() {
          entries.push((path, BodySource::Reused));
        }
      }
    }

    match format {
      PackFormat::Flat => this.publish_flat(entries, &spool_path),
      PackFormat::Bucketed => {
        let Some(root) = root else {
          return Err(io::Error::other(
            "bucketed pack publish requires the canonical tree root",
          ));
        };
        this.publish_bucketed(entries, &spool_path, &root)
      }
    }
  }
}

impl FinishState {
  /// Fetch one canonical entry's body: appended records slice the spool mapping, reused
  /// records come back through the prior generation's reader.
  fn body_of<'a>(
    &'a self,
    path: &str,
    source: &BodySource,
    spooled: &'a [u8],
  ) -> io::Result<&'a [u8]> {
    match source {
      BodySource::Appended((off, len)) => spooled
        .get(*off as usize..(*off + u64::from(*len)) as usize)
        .ok_or_else(|| io::Error::other("appended pack entry out of bounds")),
      BodySource::Reused => self
        .reader
        .as_ref()
        .and_then(|r| r.get(path))
        .ok_or_else(|| io::Error::other("reused pack entry vanished from prior pack")),
    }
  }

  /// The flat (v1) publish: one canonical pack file + sidecar. Byte-identical to the
  /// layout every existing generation carries.
  fn publish_flat(self, mut entries: Vec<(String, BodySource)>, spool_path: &Path) -> io::Result<()> {
    let pack_path = self.dir.join(PACK_FILE);
    // Canonical order: sort by path so the published bytes are a pure function of the
    // `(path, body)` set — independent of worker completion order and incremental history
    // (this is what makes an incremental generation converge byte-for-byte to a from-scratch
    // build of the same tree). Paths are unique per entry (one product per file; reuse skips
    // re-appended paths), so this is a total, machine-independent order.
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    let spooled = fs::read(spool_path)?;
    let tmp = self.dir.join("products.pack.tmp");
    let mut out = BufWriter::with_capacity(1 << 20, fs::File::create(&tmp)?);
    out.write_all(PACK_MAGIC)?;
    out.write_all(&PACK_VERSION.to_le_bytes())?;
    let mut new_at = 8u64;
    let mut final_entries: Vec<(String, Entry)> = Vec::with_capacity(entries.len());
    for (path, source) in entries {
      let body: &[u8] = self.body_of(&path, &source, &spooled)?;
      out.write_all(&(path.len() as u32).to_le_bytes())?;
      out.write_all(path.as_bytes())?;
      out.write_all(&(body.len() as u32).to_le_bytes())?;
      out.write_all(body)?;
      let body_at = new_at + 4 + path.len() as u64 + 4;
      new_at = body_at + body.len() as u64;
      final_entries.push((path, (body_at, body.len() as u32)));
    }
    out.flush()?;
    drop(out);
    fs::rename(&tmp, &pack_path)?;
    let _ = fs::remove_file(spool_path);
    let entries = final_entries;

    let covered = fs::metadata(&pack_path)?.len();
    let idx_tmp = self.dir.join("products.idx.tmp");
    let mut idx = BufWriter::with_capacity(1 << 20, fs::File::create(&idx_tmp)?);
    idx.write_all(IDX_MAGIC)?;
    idx.write_all(&PACK_VERSION.to_le_bytes())?;
    idx.write_all(&covered.to_le_bytes())?;
    idx.write_all(&(entries.len() as u64).to_le_bytes())?;
    for (path, (off, len)) in &entries {
      idx.write_all(&(path.len() as u32).to_le_bytes())?;
      idx.write_all(path.as_bytes())?;
      idx.write_all(&off.to_le_bytes())?;
      idx.write_all(&len.to_le_bytes())?;
    }
    idx.flush()?;
    drop(idx);
    fs::rename(&idx_tmp, self.dir.join(PACK_INDEX))?;
    // A stale bucketed layout in this directory would SHADOW the flat pack just published
    // (readers sniff bucketed first) — a format downgrade must leave one truth. Best-effort:
    // a mapped bucket file that resists deletion (Windows) is retried by the next publish.
    if self.dir.join(PACK_DIR).is_dir() {
      let _ = fs::remove_dir_all(self.dir.join(PACK_DIR));
    }
    Ok(())
  }

  /// The bucketed (v2) publish: per-bucket files + TOC, tree-relative spellings, unchanged
  /// buckets hard-linked from the prior generation.
  fn publish_bucketed(
    self,
    entries: Vec<(String, BodySource)>,
    spool_path: &Path,
    root: &str,
  ) -> io::Result<()> {
    let buckets = bucket_count_effective(entries.len());
    let products = self.dir.join(PACK_DIR);
    fs::create_dir_all(&products)?;

    // Canonical order: bucket-major, tree-relative-path-sorted within each bucket — still a
    // pure function of the `(path, body)` set (the root prefix is constant, so relative
    // order equals absolute order within a bucket).
    let rel_of = |path: &str| -> String { tree_relative(path, root).to_string() };
    let mut order: Vec<(u32, String, usize)> = entries
      .iter()
      .enumerate()
      .map(|(i, (path, _))| {
        let rel = rel_of(path);
        (bucket_of(&rel, buckets), rel, i)
      })
      .collect();
    order.sort_unstable_by(|a, b| (a.0, a.1.as_str()).cmp(&(b.0, b.1.as_str())));

    // Spool access is a mapping, not a read: the flat publish re-materialized the whole
    // spool (≈ the pack) in RAM; at kernel scale that is a gigabyte-class spike the
    // pipeline just spent effort never holding at once.
    let spool_map = PackReader::map_store(spool_path)
      .ok_or_else(|| io::Error::other("pack spool vanished before publish"))?;
    let spooled = spool_map.as_bytes();

    // Hard-link eligibility, per bucket: every entry is Reused (bodies byte-identical by
    // definition) and the prior bucket holds exactly as many entries (mine ⊆ prior and
    // |mine| = |prior| ⇒ sets equal) — under the same bucket count and a TOC-consistent
    // prior. Then the prior bucket FILE is byte-identical to what a rewrite would produce,
    // and its TOC row (digest included) carries over without rehashing.
    let prior = self.reader.as_deref().filter(|r| r.is_bucketed());
    let prior_meta: Option<&[BucketMeta]> = prior.and_then(|r| r.bucket_meta());
    let same_dir = prior.is_some_and(|r| {
      match (r.dir.canonicalize(), self.dir.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => r.dir == self.dir,
      }
    });
    let linkable = prior_meta.is_some_and(|m| m.len() as u32 == buckets);

    let mut toc_rows: Vec<BucketMeta> = Vec::with_capacity(buckets as usize);
    // (path bytes, off, len) per slot, bucket-major — the TOC slot table.
    let mut slots: Vec<(String, u64, u32)> = Vec::with_capacity(order.len());
    let mut cursor = 0usize;
    for k in 0..buckets {
      let start = cursor;
      while cursor < order.len() && order[cursor].0 == k {
        cursor += 1;
      }
      let mine = &order[start..cursor];
      let all_reused = mine
        .iter()
        .all(|(_, _, i)| matches!(entries[*i].1, BodySource::Reused));
      let carry = linkable
        && all_reused
        && prior_meta.is_some_and(|m| m[k as usize].entries as usize == mine.len());
      if carry {
        let (reader, meta) = match (prior, prior_meta) {
          (Some(r), Some(m)) => (r, m[k as usize]),
          _ => unreachable!("carry implies a TOC-consistent bucketed prior"),
        };
        let dst = products.join(format!("{k:04}.pack"));
        let src = reader.dir.join(PACK_DIR).join(format!("{k:04}.pack"));
        let linked = if same_dir {
          // Legacy same-directory publish: the canonical file is already in place.
          fs::metadata(&dst).map(|s| s.len() == meta.len).unwrap_or(false)
        } else {
          let _ = fs::remove_file(&dst); // retry-after-kill leftovers
          fs::hard_link(&src, &dst).is_ok()
            && fs::metadata(&dst).map(|s| s.len() == meta.len).unwrap_or(false)
        };
        if linked {
          for (path, off, len) in reader.bucket_slots(k as u16) {
            slots.push((path.to_string(), off, len));
          }
          toc_rows.push(meta);
          continue;
        }
        // Link refused (cross-device, permissions): fall through to the rewrite — same
        // bytes, full cost.
      }
      // Rewrite bucket k from its canonical entry run.
      let tmp = products.join(format!("{k:04}.pack.tmp"));
      let mut out = HashingWriter::new(BufWriter::with_capacity(1 << 20, fs::File::create(&tmp)?));
      out.write_all(BUCKET_MAGIC)?;
      out.write_all(&BUCKET_VERSION.to_le_bytes())?;
      out.write_all(&k.to_le_bytes())?;
      let mut at = BUCKET_HEADER as u64;
      for (_, rel, i) in mine {
        let (path, source) = &entries[*i];
        let body: &[u8] = self.body_of(path, source, spooled)?;
        out.write_all(&(rel.len() as u32).to_le_bytes())?;
        out.write_all(rel.as_bytes())?;
        out.write_all(&(body.len() as u32).to_le_bytes())?;
        out.write_all(body)?;
        let body_at = at + 4 + rel.len() as u64 + 4;
        at = body_at + body.len() as u64;
        slots.push((rel.clone(), body_at, body.len() as u32));
      }
      out.flush()?;
      let digest = out.hash.digest();
      let written = out.written;
      drop(out);
      fs::rename(&tmp, products.join(format!("{k:04}.pack")))?;
      toc_rows.push(BucketMeta {
        entries: mine.len() as u32,
        len: written,
        digest,
      });
    }
    drop(spool_map);
    let _ = fs::remove_file(spool_path);

    // TOC last — the publish's commit record. Buckets already landed via rename, so a kill
    // here leaves prior TOC + new buckets: open() sees the length mismatch and recovers by
    // record scan, never serving a half-trusted mapping.
    let toc_tmp = self.dir.join("products").join("toc.bin.tmp");
    let mut toc = BufWriter::with_capacity(1 << 20, fs::File::create(&toc_tmp)?);
    toc.write_all(TOC_MAGIC)?;
    toc.write_all(&BUCKET_VERSION.to_le_bytes())?;
    toc.write_all(&buckets.to_le_bytes())?;
    toc.write_all(&(slots.len() as u64).to_le_bytes())?;
    for row in &toc_rows {
      toc.write_all(&row.entries.to_le_bytes())?;
      toc.write_all(&row.len.to_le_bytes())?;
      toc.write_all(&row.digest.to_le_bytes())?;
    }
    for (path, off, len) in &slots {
      toc.write_all(&(path.len() as u32).to_le_bytes())?;
      toc.write_all(path.as_bytes())?;
      toc.write_all(&off.to_le_bytes())?;
      toc.write_all(&len.to_le_bytes())?;
    }
    toc.flush()?;
    drop(toc);
    fs::rename(&toc_tmp, self.dir.join(PACK_TOC))?;

    // One truth per directory: retire a stale flat pack (upgrade in a legacy same-dir
    // publish) and any bucket files beyond this publish's count (a legacy-mode bucket-count
    // crossing). Best-effort — sniff order already prefers the TOC.
    let _ = fs::remove_file(self.dir.join(PACK_FILE));
    let _ = fs::remove_file(self.dir.join(PACK_INDEX));
    if let Ok(dirents) = fs::read_dir(&products) {
      for entry in dirents.flatten() {
        if let Ok(name) = entry.file_name().into_string() {
          let stale_bucket = name
            .strip_suffix(".pack")
            .and_then(|k| k.parse::<u32>().ok())
            .is_some_and(|k| k >= buckets);
          if stale_bucket || name.ends_with(".pack.tmp") {
            let _ = fs::remove_file(entry.path());
          }
        }
      }
    }
    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn scratch_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("vorpal-pack-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
  }

  fn flat_writer(dir: &Path, reader: Option<Arc<PackReader>>) -> PackWriter {
    PackWriter::new(dir, reader, None, PackFormat::Flat)
  }

  fn bucketed_writer(dir: &Path, reader: Option<Arc<PackReader>>, root: &str) -> PackWriter {
    PackWriter::new(dir, reader, Some(root.to_string()), PackFormat::Bucketed)
  }

  #[test]
  fn appends_reads_reuses_and_recovers() {
    let dir = scratch_dir("basic");
    let writer = flat_writer(&dir, None);
    let sink = writer.sink();
    sink
      .send(PackMsg {
        path: "a.rs".into(),
        body: b"alpha-bytes".to_vec(),
      })
      .unwrap();
    sink
      .send(PackMsg {
        path: "b.rs".into(),
        body: b"beta".to_vec(),
      })
      .unwrap();
    drop(sink);
    writer.finish(Vec::new()).unwrap();

    let reader = PackReader::open(&dir).unwrap();
    assert_eq!(reader.get("a.rs"), Some(&b"alpha-bytes"[..]));
    assert_eq!(reader.get("b.rs"), Some(&b"beta"[..]));
    assert_eq!(reader.get("missing.rs"), None);

    // Second generation: reuse a, replace b, and survive without a sidecar.
    let reader = Arc::new(reader);
    let writer = flat_writer(&dir, Some(reader.clone()));
    let sink = writer.sink();
    sink
      .send(PackMsg {
        path: "b.rs".into(),
        body: b"beta-v2".to_vec(),
      })
      .unwrap();
    drop(sink);
    // `a.rs` is carried by the bulk-reuse path: live but not re-appended.
    writer
      .finish(vec!["a.rs".to_string(), "b.rs".to_string()])
      .unwrap();
    fs::remove_file(dir.join(PACK_INDEX)).unwrap(); // killed-run recovery: no sidecar
    let recovered = PackReader::open(&dir).unwrap();
    assert_eq!(recovered.get("a.rs"), Some(&b"alpha-bytes"[..]));
    assert_eq!(recovered.get("b.rs"), Some(&b"beta-v2"[..]));

    let _ = fs::remove_dir_all(&dir);
  }

  #[test]
  fn publish_is_byte_identical_regardless_of_arrival_order() {
    // Determinism contract: the same (path, body) set produces byte-identical pack + sidecar
    // no matter the order records arrive in. This is what makes indexes fleet-coherent.
    let products = [
      ("z/last.rs", b"zzz".as_slice()),
      ("a/first.rs", b"aaaa".as_slice()),
      ("m/mid.rs", b"mm".as_slice()),
      ("a/second.rs", b"a2".as_slice()),
    ];
    let build = |tag: &str, order: &[usize]| -> (Vec<u8>, Vec<u8>) {
      let dir = scratch_dir(tag);
      let writer = flat_writer(&dir, None);
      let sink = writer.sink();
      for &i in order {
        sink
          .send(PackMsg {
            path: products[i].0.into(),
            body: products[i].1.to_vec(),
          })
          .unwrap();
      }
      drop(sink);
      writer.finish(Vec::new()).unwrap();
      let pack = fs::read(dir.join(PACK_FILE)).unwrap();
      let idx = fs::read(dir.join(PACK_INDEX)).unwrap();
      let _ = fs::remove_dir_all(&dir);
      (pack, idx)
    };
    let (pack_a, idx_a) = build("order-a", &[0, 1, 2, 3]);
    let (pack_b, idx_b) = build("order-b", &[3, 2, 1, 0]);
    let (pack_c, idx_c) = build("order-c", &[2, 0, 3, 1]);
    assert_eq!(pack_a, pack_b, "pack differs by arrival order");
    assert_eq!(pack_a, pack_c, "pack differs by arrival order");
    assert_eq!(idx_a, idx_b, "sidecar differs by arrival order");
    assert_eq!(idx_a, idx_c, "sidecar differs by arrival order");
    // And the canonical order is genuinely sorted: first live record is a/first.rs.
    let first = &pack_a[12..12 + "a/first.rs".len()];
    assert_eq!(first, b"a/first.rs");

    // A reused-entry publish (second generation, no re-append) must reproduce the same bytes.
    let dir = scratch_dir("reuse-canonical");
    let w1 = flat_writer(&dir, None);
    let s1 = w1.sink();
    for &i in &[1usize, 3, 0, 2] {
      s1.send(PackMsg { path: products[i].0.into(), body: products[i].1.to_vec() }).unwrap();
    }
    drop(s1);
    w1.finish(Vec::new()).unwrap();
    let gen1 = fs::read(dir.join(PACK_FILE)).unwrap();
    let reader = Arc::new(PackReader::open(&dir).unwrap());
    let w2 = flat_writer(&dir, Some(reader));
    drop(w2.sink()); // no appends: everything is carried by bulk reuse
    w2.finish(products.iter().map(|(p, _)| p.to_string())).unwrap();
    let gen2 = fs::read(dir.join(PACK_FILE)).unwrap();
    assert_eq!(gen1, gen2, "no-change reuse re-publish is not byte-stable");
    let _ = fs::remove_dir_all(&dir);
  }

  #[test]
  fn torn_tail_record_is_dropped_not_fatal() {
    let dir = scratch_dir("torn");
    let writer = flat_writer(&dir, None);
    let sink = writer.sink();
    sink
      .send(PackMsg {
        path: "ok.rs".into(),
        body: b"whole".to_vec(),
      })
      .unwrap();
    drop(sink);
    writer.finish(Vec::new()).unwrap();
    // Simulate a crash mid-append: a half-written record after the covered length.
    let mut bytes = fs::read(dir.join(PACK_FILE)).unwrap();
    bytes.extend_from_slice(&(9u32).to_le_bytes());
    bytes.extend_from_slice(b"torn"); // path shorter than declared
    fs::write(dir.join(PACK_FILE), &bytes).unwrap();
    fs::remove_file(dir.join(PACK_INDEX)).unwrap();
    let reader = PackReader::open(&dir).unwrap();
    assert_eq!(reader.get("ok.rs"), Some(&b"whole"[..]));
    let _ = fs::remove_dir_all(&dir);
  }

  // ---- bucketed layout (P4.1) ----

  const ROOT: &str = "/repo/checkout";

  fn send_abs(sink: &crossbeam_channel::Sender<PackMsg>, rel: &str, body: &[u8]) {
    sink
      .send(PackMsg {
        path: format!("{ROOT}/{rel}"),
        body: body.to_vec(),
      })
      .unwrap();
  }

  #[test]
  fn bucketed_roundtrip_relative_keys_and_rooted_lookup() {
    let dir = scratch_dir("v2-roundtrip");
    let writer = bucketed_writer(&dir, None, ROOT);
    let sink = writer.sink();
    send_abs(&sink, "src/a.rs", b"alpha");
    send_abs(&sink, "src/b.py", b"beta");
    send_abs(&sink, "docs/c.go", b"gamma");
    drop(sink);
    writer.finish(Vec::new()).unwrap();

    assert!(dir.join(PACK_TOC).is_file(), "TOC published");
    assert!(!dir.join(PACK_FILE).exists(), "no flat pack in a bucketed publish");

    // Rooted lookups accept the absolute spellings production callers hold…
    let rooted = PackReader::open_rooted(&dir, Some(ROOT)).unwrap();
    assert!(rooted.is_bucketed());
    assert_eq!(rooted.get(&format!("{ROOT}/src/a.rs")), Some(&b"alpha"[..]));
    assert_eq!(rooted.get(&format!("{ROOT}/docs/c.go")), Some(&b"gamma"[..]));
    assert_eq!(rooted.get(&format!("{ROOT}/missing.rs")), None);
    // …and a DIFFERENT mount of the same tree still hits: identity is tree-relative.
    let moved = PackReader::open_rooted(&dir, Some("/other/mount")).unwrap();
    assert_eq!(moved.get("/other/mount/src/a.rs"), Some(&b"alpha"[..]));
    // Rootless readers speak stored (relative) spellings.
    let bare = PackReader::open(&dir).unwrap();
    assert_eq!(bare.get("src/b.py"), Some(&b"beta"[..]));
    // entries() yields tree-relative spellings.
    let mut names: Vec<&str> = bare.entries().map(|(p, _)| p).collect();
    names.sort_unstable();
    assert_eq!(names, ["docs/c.go", "src/a.rs", "src/b.py"]);

    let _ = fs::remove_dir_all(&dir);
  }

  #[test]
  fn bucketed_publish_is_deterministic_and_mount_invariant() {
    let bodies = [
      ("z/last.rs", b"zzz".as_slice()),
      ("a/first.rs", b"aaaa".as_slice()),
      ("m/mid.rs", b"mm".as_slice()),
      ("a/second.rs", b"a2".as_slice()),
    ];
    let build = |tag: &str, root: &str, order: &[usize]| -> Vec<(String, Vec<u8>)> {
      let dir = scratch_dir(tag);
      let writer = bucketed_writer(&dir, None, root);
      let sink = writer.sink();
      for &i in order {
        sink
          .send(PackMsg {
            path: format!("{root}/{}", bodies[i].0),
            body: bodies[i].1.to_vec(),
          })
          .unwrap();
      }
      drop(sink);
      writer.finish(Vec::new()).unwrap();
      let mut files: Vec<(String, Vec<u8>)> = fs::read_dir(dir.join(PACK_DIR))
        .unwrap()
        .map(|e| {
          let e = e.unwrap();
          (e.file_name().into_string().unwrap(), fs::read(e.path()).unwrap())
        })
        .collect();
      files.sort();
      let _ = fs::remove_dir_all(&dir);
      files
    };
    let a = build("v2-det-a", "/mount/one", &[0, 1, 2, 3]);
    let b = build("v2-det-b", "/mount/one", &[3, 2, 1, 0]);
    assert_eq!(a, b, "bucketed publish differs by arrival order");
    // The mount-invariance win v1 could never have: every published byte — bucket files
    // and TOC alike — is identical under a different checkout root.
    let c = build("v2-det-c", "/second/mount/point", &[2, 0, 3, 1]);
    assert_eq!(a, c, "bucketed publish bytes depend on the mount point");
  }

  #[test]
  fn bucketed_edit_rewrites_one_bucket_and_links_the_rest() {
    let root = ROOT;
    let gen1 = scratch_dir("v2-link-gen1");
    let writer = bucketed_writer(&gen1, None, root);
    let sink = writer.sink();
    let files: Vec<String> = (0..64).map(|i| format!("src/mod_{i:02}.rs")).collect();
    for rel in &files {
      send_abs(&sink, rel, format!("body of {rel}").as_bytes());
    }
    drop(sink);
    writer.finish(Vec::new()).unwrap();
    let gen1_reader = Arc::new(PackReader::open_rooted(&gen1, Some(root)).unwrap());
    let gen1_bytes: Vec<(String, Vec<u8>)> = {
      let mut v: Vec<(String, Vec<u8>)> = fs::read_dir(gen1.join(PACK_DIR))
        .unwrap()
        .map(|e| {
          let e = e.unwrap();
          (e.file_name().into_string().unwrap(), fs::read(e.path()).unwrap())
        })
        .collect();
      v.sort();
      v
    };

    // Second generation in a NEW directory: exactly one file re-appended.
    let gen2 = scratch_dir("v2-link-gen2");
    let writer = bucketed_writer(&gen2, Some(gen1_reader.clone()), root);
    let sink = writer.sink();
    send_abs(&sink, &files[7], b"EDITED BODY");
    drop(sink);
    writer
      .finish(files.iter().map(|rel| format!("{root}/{rel}")))
      .unwrap();

    let edited_bucket = bucket_of(&files[7], gen1_reader.loaded_buckets());
    let mut linked = 0usize;
    let mut rewritten = Vec::new();
    for k in 0..gen1_reader.loaded_buckets() {
      let name = format!("{k:04}.pack");
      let a = fs::metadata(gen1.join(PACK_DIR).join(&name)).unwrap();
      let b = fs::metadata(gen2.join(PACK_DIR).join(&name)).unwrap();
      #[cfg(unix)]
      {
        use std::os::unix::fs::MetadataExt;
        if a.ino() == b.ino() {
          linked += 1;
        } else {
          rewritten.push(k);
        }
      }
      #[cfg(not(unix))]
      {
        if a.len() == b.len() {
          linked += 1;
        } else {
          rewritten.push(k);
        }
      }
    }
    assert_eq!(rewritten, vec![edited_bucket], "exactly the edited file's bucket rewrote");
    assert_eq!(
      linked as u32,
      gen1_reader.loaded_buckets() - 1,
      "every untouched bucket hard-linked"
    );

    // The prior generation's bytes are untouched (links share, never mutate)…
    let gen1_after: Vec<(String, Vec<u8>)> = {
      let mut v: Vec<(String, Vec<u8>)> = fs::read_dir(gen1.join(PACK_DIR))
        .unwrap()
        .map(|e| {
          let e = e.unwrap();
          (e.file_name().into_string().unwrap(), fs::read(e.path()).unwrap())
        })
        .collect();
      v.sort();
      v
    };
    assert_eq!(gen1_bytes, gen1_after, "hard-link carry mutated the sealed prior generation");

    // …the new generation serves both carried and edited bodies…
    let gen2_reader = PackReader::open_rooted(&gen2, Some(root)).unwrap();
    assert_eq!(gen2_reader.get(&format!("{root}/{}", files[7])), Some(&b"EDITED BODY"[..]));
    assert_eq!(
      gen2_reader.get(&format!("{root}/{}", files[3])),
      Some(format!("body of {}", files[3]).as_bytes())
    );
    // …and an incremental publish converges byte-for-byte with a scratch build of the
    // same tree (the convergence law, at pack scope).
    let scratch = scratch_dir("v2-link-scratch");
    let writer = bucketed_writer(&scratch, None, root);
    let sink = writer.sink();
    for rel in &files {
      let body = if rel == &files[7] {
        b"EDITED BODY".to_vec()
      } else {
        format!("body of {rel}").into_bytes()
      };
      sink.send(PackMsg { path: format!("{root}/{rel}"), body }).unwrap();
    }
    drop(sink);
    writer.finish(Vec::new()).unwrap();
    for k in 0..gen2_reader.loaded_buckets() {
      let name = format!("{k:04}.pack");
      assert_eq!(
        fs::read(gen2.join(PACK_DIR).join(&name)).unwrap(),
        fs::read(scratch.join(PACK_DIR).join(&name)).unwrap(),
        "incremental bucket {k} diverges from scratch"
      );
    }
    assert_eq!(
      fs::read(gen2.join(PACK_TOC)).unwrap(),
      fs::read(scratch.join(PACK_TOC)).unwrap(),
      "incremental TOC diverges from scratch"
    );

    let _ = fs::remove_dir_all(&gen1);
    let _ = fs::remove_dir_all(&gen2);
    let _ = fs::remove_dir_all(&scratch);
  }

  #[test]
  fn flat_prior_migrates_into_bucketed_without_reappends() {
    // The v1→v2 flip: a bucketed publish over a flat prior reuses every body through the
    // reader — one pack write, zero re-extraction.
    let dir1 = scratch_dir("v2-migrate-flat");
    let writer = flat_writer(&dir1, None);
    let sink = writer.sink();
    for (rel, body) in [("src/a.rs", b"alpha".as_slice()), ("src/b.rs", b"beta")] {
      sink
        .send(PackMsg {
          path: format!("{ROOT}/{rel}"),
          body: body.to_vec(),
        })
        .unwrap();
    }
    drop(sink);
    writer.finish(Vec::new()).unwrap();

    let prior = Arc::new(PackReader::open_rooted(&dir1, Some(ROOT)).unwrap());
    assert!(!prior.is_bucketed());
    let dir2 = scratch_dir("v2-migrate-next");
    let writer = bucketed_writer(&dir2, Some(prior), ROOT);
    drop(writer.sink()); // no appends: pure carry
    writer
      .finish(vec![format!("{ROOT}/src/a.rs"), format!("{ROOT}/src/b.rs")])
      .unwrap();
    let reader = PackReader::open_rooted(&dir2, Some(ROOT)).unwrap();
    assert!(reader.is_bucketed());
    assert_eq!(reader.get(&format!("{ROOT}/src/a.rs")), Some(&b"alpha"[..]));
    assert_eq!(reader.get(&format!("{ROOT}/src/b.rs")), Some(&b"beta"[..]));

    let _ = fs::remove_dir_all(&dir1);
    let _ = fs::remove_dir_all(&dir2);
  }

  #[test]
  fn bucketed_toc_mismatch_recovers_by_record_scan() {
    let dir = scratch_dir("v2-recover");
    let writer = bucketed_writer(&dir, None, ROOT);
    let sink = writer.sink();
    send_abs(&sink, "src/a.rs", b"alpha");
    send_abs(&sink, "src/b.rs", b"beta");
    drop(sink);
    writer.finish(Vec::new()).unwrap();

    // Kill scenario 1: TOC gone entirely.
    fs::remove_file(dir.join(PACK_TOC)).unwrap();
    let reader = PackReader::open_rooted(&dir, Some(ROOT)).unwrap();
    assert_eq!(reader.get(&format!("{ROOT}/src/a.rs")), Some(&b"alpha"[..]));
    assert!(reader.bucket_meta().is_none(), "recovery scan carries no trusted meta");

    // Kill scenario 2: TOC from a different publish (length mismatch on some bucket).
    let writer = bucketed_writer(&dir, None, ROOT);
    let sink = writer.sink();
    send_abs(&sink, "src/a.rs", b"alpha");
    send_abs(&sink, "src/b.rs", b"beta");
    drop(sink);
    writer.finish(Vec::new()).unwrap();
    let victim = fs::read_dir(dir.join(PACK_DIR))
      .unwrap()
      .filter_map(|e| e.ok())
      .find(|e| {
        e.file_name().into_string().is_ok_and(|n| n.ends_with(".pack"))
          && e.metadata().is_ok_and(|m| m.len() > BUCKET_HEADER as u64)
      })
      .expect("a non-empty bucket exists");
    let mut bytes = fs::read(victim.path()).unwrap();
    bytes.extend_from_slice(&(9u32).to_le_bytes());
    bytes.extend_from_slice(b"torn");
    fs::write(victim.path(), &bytes).unwrap();
    let reader = PackReader::open_rooted(&dir, Some(ROOT)).unwrap();
    assert_eq!(reader.get(&format!("{ROOT}/src/a.rs")), Some(&b"alpha"[..]));
    assert_eq!(reader.get(&format!("{ROOT}/src/b.rs")), Some(&b"beta"[..]));

    let _ = fs::remove_dir_all(&dir);
  }

  #[test]
  fn format_toggle_leaves_one_truth_per_directory() {
    // Upgrade then downgrade in one directory (legacy layout): each publish retires the
    // other layout's files so the sniff can never resurrect stale products.
    let dir = scratch_dir("v2-toggle");
    let writer = bucketed_writer(&dir, None, ROOT);
    let sink = writer.sink();
    send_abs(&sink, "src/a.rs", b"alpha-v2");
    drop(sink);
    writer.finish(Vec::new()).unwrap();
    assert!(dir.join(PACK_TOC).is_file());

    let writer = flat_writer(&dir, None);
    let sink = writer.sink();
    sink
      .send(PackMsg {
        path: format!("{ROOT}/src/a.rs"),
        body: b"alpha-v1".to_vec(),
      })
      .unwrap();
    drop(sink);
    writer.finish(Vec::new()).unwrap();
    assert!(!dir.join(PACK_DIR).exists(), "flat publish retired the bucketed layout");
    let reader = PackReader::open(&dir).unwrap();
    assert!(!reader.is_bucketed());
    assert_eq!(reader.get(&format!("{ROOT}/src/a.rs")), Some(&b"alpha-v1"[..]));

    let _ = fs::remove_dir_all(&dir);
  }

  #[test]
  fn bucket_count_law_is_pure_pow2_clamped() {
    assert_eq!(bucket_count_for(0), BUCKET_MIN);
    assert_eq!(bucket_count_for(1), BUCKET_MIN);
    assert_eq!(bucket_count_for(BUCKET_TARGET_FILES * BUCKET_MIN as usize), BUCKET_MIN);
    let kernel_scale = 76_000usize;
    let b = bucket_count_for(kernel_scale);
    assert!(b.is_power_of_two() && (BUCKET_MIN..=BUCKET_MAX).contains(&b));
    // Monotonic in file count.
    let mut prev = 0;
    for files in [0usize, 100, 1_000, 10_000, 100_000, 1_000_000, 10_000_000] {
      let b = bucket_count_for(files);
      assert!(b >= prev, "bucket law not monotonic at {files}");
      prev = b;
    }
    assert_eq!(bucket_count_for(usize::MAX), BUCKET_MAX);
    // The naming bound holds: {:04} names sort numerically for every legal bucket index.
    assert_eq!(format!("{:04}", BUCKET_MAX - 1).len(), 4);
  }

  #[test]
  fn toc_digest_splice_targets_the_right_row() {
    let dir = scratch_dir("v2-splice");
    let writer = bucketed_writer(&dir, None, ROOT);
    let sink = writer.sink();
    send_abs(&sink, "src/a.rs", b"alpha");
    drop(sink);
    writer.finish(Vec::new()).unwrap();
    let reader = PackReader::open(&dir).unwrap();
    let meta = reader.bucket_meta().unwrap().to_vec();
    let mut toc = fs::read(dir.join(PACK_TOC)).unwrap();
    let target = bucket_of("src/a.rs", meta.len() as u32);
    assert!(splice_toc_digest(&mut toc, target, 0xDEAD_BEEF_CAFE_F00D));
    assert!(!splice_toc_digest(&mut toc, meta.len() as u32, 1), "out of range refused");
    fs::write(dir.join(PACK_TOC), &toc).unwrap();
    let reader = PackReader::open(&dir).unwrap();
    let spliced = reader.bucket_meta().unwrap();
    assert_eq!(spliced[target as usize].digest, 0xDEAD_BEEF_CAFE_F00D);
    for (k, row) in spliced.iter().enumerate() {
      if k as u32 != target {
        assert_eq!(row.digest, meta[k].digest, "splice leaked into row {k}");
      }
    }
    let _ = fs::remove_dir_all(&dir);
  }
}
