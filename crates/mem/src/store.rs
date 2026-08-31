//! Adaptive mmap wrapper applying the page / `madvise` policy (§8.2).
//!
//! One `map`/`map_anon` call site serves every scale: the [`ResourcePolicy`] chooses the page
//! backing and access advice, and this layer applies it — `MADV_HUGEPAGE` / `MAP_HUGETLB` only
//! on Linux (macOS Apple Silicon has no superpages and falls back to 16 KiB native pages),
//! `MADV_RANDOM`/`SEQUENTIAL` everywhere, `WILLNEED`/`DONTNEED` for bounded-RSS streaming scans.

use std::fs::File;
use std::io;
use std::path::Path;

use memmap2::{Mmap, MmapMut, MmapOptions};

#[cfg(target_os = "linux")]
use crate::policy::PagePolicy;
use crate::policy::{AccessPattern, Hotness, ResourcePolicy, StorePolicy};
use crate::probe::StoreKind;

/// A read-only, page-policy-aware mmap of a file segment.
pub struct MappedStore {
  mmap: Mmap,
  policy: StorePolicy,
}

impl MappedStore {
  /// Map `path` read-only and apply the resolved policy's access + huge-page advice.
  ///
  /// # Safety-adjacent note
  /// mmap of a file is `unsafe` in `memmap2` because concurrent external truncation is UB; the
  /// caller owns the segment file (append-only, sealed) so this holds by construction (§9.1).
  pub fn map_file(
    path: &Path,
    kind: StoreKind,
    access: AccessPattern,
    hotness: Hotness,
    policy: &ResourcePolicy,
  ) -> io::Result<Self> {
    let file = File::open(path)?;
    // SAFETY: sealed append-only segment; not mutated or truncated while mapped (§9.1).
    let mmap = unsafe { MmapOptions::new().map(&file)? };
    let policy = policy.for_store(kind, access, hotness);
    apply_advice(&mmap, &policy);
    Ok(Self { mmap, policy })
  }

  pub fn as_bytes(&self) -> &[u8] {
    &self.mmap
  }

  pub fn policy(&self) -> StorePolicy {
    self.policy
  }

  /// Stage the whole mapping into RAM ahead of a scan (`MADV_WILLNEED`).
  #[cfg(unix)]
  pub fn advise_willneed(&self) -> io::Result<()> {
    self.mmap.advise(memmap2::Advice::WillNeed)
  }

  /// Drop the mapping's resident pages behind a streaming cursor (`MADV_DONTNEED`).
  #[cfg(unix)]
  pub fn advise_dontneed(&self) -> io::Result<()> {
    // SAFETY: this is a read-only, sealed file segment; DONTNEED only drops resident pages
    // (they refault from the backing file on next access), so no write can be lost.
    unsafe {
      self
        .mmap
        .unchecked_advise(memmap2::UncheckedAdvice::DontNeed)
    }
  }
}

/// A writable anonymous mapping (per-batch scratch / in-RAM append store), page-policy-aware.
pub struct AnonStore {
  mmap: MmapMut,
  policy: StorePolicy,
}

impl AnonStore {
  /// Allocate an anonymous mapping of `len` bytes under the resolved policy. On Linux an
  /// explicit-huge policy maps with `MAP_HUGETLB`; otherwise native pages + huge-page advice.
  pub fn new(
    len: usize,
    kind: StoreKind,
    access: AccessPattern,
    hotness: Hotness,
    policy: &ResourcePolicy,
  ) -> io::Result<Self> {
    let policy = policy.for_store(kind, access, hotness);
    let mut opts = MmapOptions::new();
    opts.len(len);
    #[cfg(target_os = "linux")]
    match policy.page {
      // 21 = log2(2 MiB) = MAP_HUGE_2MB; 30 = log2(1 GiB) = MAP_HUGE_1GB.
      PagePolicy::ExplicitHuge2M => {
        opts.huge(Some(21));
      }
      PagePolicy::ExplicitHuge1G => {
        opts.huge(Some(30));
      }
      _ => {}
    }
    let mmap = opts.map_anon()?;
    apply_advice_mut(&mmap, &policy);
    Ok(Self { mmap, policy })
  }

  pub fn as_bytes(&self) -> &[u8] {
    &self.mmap
  }

  pub fn as_mut_bytes(&mut self) -> &mut [u8] {
    &mut self.mmap
  }

  pub fn policy(&self) -> StorePolicy {
    self.policy
  }
}

/// A writable, file-backed scratch mapping: multi-GB working sets (factorization
/// blocks, streamed CSR arrays) ride the OS pager instead of anonymous RSS — the
/// kernel writes cold pages back to the scratch file under pressure and refaults them
/// on demand, so peak anonymous memory stays bounded no matter the corpus (the §8
/// one-code-path law at the 10⁹-LOC end).
///
/// Scratch is DEFINITIONALLY cold storage: native pages + the caller's access advice
/// (huge-page policies target hot anonymous/persistent stores; file-backed THP is not
/// generally applicable). Lifecycle: callers name the file inside their own scratch
/// area, call [`ScratchMmap::delete`] on success, and sweep leftovers at start-up —
/// a crash can only leave a dead file, never a wrong artifact.
pub struct ScratchMmap {
  mmap: MmapMut,
  path: std::path::PathBuf,
}

impl ScratchMmap {
  /// Create (or truncate) `path`, size it to `len` bytes, and map it writable with
  /// `access` advice. `len` must be nonzero.
  pub fn create(path: &Path, len: usize, access: AccessPattern) -> io::Result<Self> {
    if len == 0 {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "zero-length scratch mapping",
      ));
    }
    let file = std::fs::OpenOptions::new()
      .read(true)
      .write(true)
      .create(true)
      .truncate(true)
      .open(path)?;
    file.set_len(len as u64)?;
    // SAFETY: the file was just created and sized by this handle; it is owned by this
    // process's scratch lifecycle and never truncated externally while mapped.
    let mmap = unsafe { MmapOptions::new().map_mut(&file)? };
    #[cfg(unix)]
    {
      let _ = mmap.advise(access_advice(access));
    }
    #[cfg(not(unix))]
    let _ = access;
    Ok(Self {
      mmap,
      path: path.to_path_buf(),
    })
  }

  pub fn as_bytes(&self) -> &[u8] {
    &self.mmap
  }

  pub fn as_mut_bytes(&mut self) -> &mut [u8] {
    &mut self.mmap
  }

  pub fn path(&self) -> &Path {
    &self.path
  }

  /// Flush dirty pages to the backing file (needed only if the caller intends to
  /// reopen the scratch by path; pure scratch never calls this).
  pub fn flush(&self) -> io::Result<()> {
    self.mmap.flush()
  }

  /// Unmap and remove the backing file — the success path. (On crash the file
  /// survives; owners sweep their scratch area at start-up.)
  pub fn delete(self) -> io::Result<()> {
    let path = self.path.clone();
    drop(self);
    std::fs::remove_file(path)
  }
}

#[cfg(unix)]
fn access_advice(access: AccessPattern) -> memmap2::Advice {
  match access {
    AccessPattern::Random => memmap2::Advice::Random,
    AccessPattern::Sequential => memmap2::Advice::Sequential,
  }
}

#[cfg(unix)]
fn apply_advice(mmap: &Mmap, policy: &StorePolicy) {
  // Best-effort: advice is a hint; failure (e.g. unsupported on a fs) is non-fatal.
  let _ = mmap.advise(access_advice(policy.access));
  #[cfg(target_os = "linux")]
  if matches!(policy.page, PagePolicy::TransparentHuge2M) {
    let _ = mmap.advise(memmap2::Advice::HugePage);
  }
}
#[cfg(not(unix))]
fn apply_advice(_mmap: &Mmap, _policy: &StorePolicy) {}

#[cfg(unix)]
fn apply_advice_mut(mmap: &MmapMut, policy: &StorePolicy) {
  let _ = mmap.advise(access_advice(policy.access));
  #[cfg(target_os = "linux")]
  if matches!(policy.page, PagePolicy::TransparentHuge2M) {
    let _ = mmap.advise(memmap2::Advice::HugePage);
  }
}
#[cfg(not(unix))]
fn apply_advice_mut(_mmap: &MmapMut, _policy: &StorePolicy) {}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::probe::{CorpusProbe, HardwareProbe};
  use std::io::Write;

  fn tiny_policy() -> ResourcePolicy {
    ResourcePolicy::new(HardwareProbe::detect(), CorpusProbe::new(4_000, 3))
  }

  #[test]
  fn anon_store_roundtrips() {
    let rp = tiny_policy();
    let mut store = AnonStore::new(
      4096,
      StoreKind::NodesHot,
      AccessPattern::Random,
      Hotness::Hot,
      &rp,
    )
    .unwrap();
    store.as_mut_bytes()[0] = 0xAB;
    store.as_mut_bytes()[4095] = 0xCD;
    assert_eq!(store.as_bytes()[0], 0xAB);
    assert_eq!(store.as_bytes()[4095], 0xCD);
  }

  #[test]
  fn scratch_mmap_roundtrips_and_deletes() {
    let mut path = std::env::temp_dir();
    path.push(format!("vorpal-mem-scratch-{}.bin", std::process::id()));
    let mut scratch = ScratchMmap::create(&path, 1 << 16, AccessPattern::Sequential).unwrap();
    assert_eq!(scratch.as_bytes().len(), 1 << 16);
    scratch.as_mut_bytes()[0] = 0x5A;
    scratch.as_mut_bytes()[(1 << 16) - 1] = 0xA5;
    assert_eq!(scratch.as_bytes()[0], 0x5A);
    assert_eq!(scratch.as_bytes()[(1 << 16) - 1], 0xA5);
    assert!(path.exists());
    scratch.delete().unwrap();
    assert!(!path.exists(), "delete must remove the backing file");
    assert!(ScratchMmap::create(&path, 0, AccessPattern::Random).is_err());
    let _ = std::fs::remove_file(&path);
  }

  #[test]
  fn mapped_file_roundtrips_and_advises() {
    let rp = tiny_policy();
    let mut path = std::env::temp_dir();
    path.push(format!("vorpal-mem-test-{}.bin", std::process::id()));
    {
      let mut f = File::create(&path).unwrap();
      f.write_all(b"vorpal segment payload").unwrap();
      f.sync_all().unwrap();
    }
    let store = MappedStore::map_file(
      &path,
      StoreKind::EdgesCsr,
      AccessPattern::Sequential,
      Hotness::Hot,
      &rp,
    )
    .unwrap();
    assert_eq!(store.as_bytes(), b"vorpal segment payload");
    #[cfg(unix)]
    {
      store.advise_willneed().unwrap();
      store.advise_dontneed().unwrap();
    }
    let _ = std::fs::remove_file(&path);
  }
}
