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
