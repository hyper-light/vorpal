//! The adaptive resource model (§8.1): probes → per-store page / arena / prefetch / NUMA policy.
//!
//! Near-zero baseline is a hard requirement: a few-file run must stay on native pages, a single
//! small arena, prefetch distance 0, and no NUMA machinery — the same call sites that serve
//! 10⁹ LOC. All escalation is a pure function of the [`CorpusProbe`] projections vs. the
//! [`HardwareProbe`] TLB reach, so decisions are deterministic and testable.

use crate::probe::{CorpusProbe, HardwareProbe, StoreKind};

/// Huge pages are a Linux server feature. macOS Apple Silicon has no superpages (§8.2), so on
/// non-Linux targets the policy never escalates past native pages regardless of corpus size.
const HUGE_PAGES_SUPPORTED: bool = cfg!(target_os = "linux");

/// The page-backing decision for one store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PagePolicy {
  /// Native base pages (4 KiB / 16 KiB), no `madvise` intent.
  Native,
  /// Transparent huge pages via `MADV_HUGEPAGE` (no reserved pool needed).
  TransparentHuge2M,
  /// Explicit `MAP_HUGETLB` 2 MiB pages (from a reserved pool; never stalls on compaction).
  ExplicitHuge2M,
  /// Explicit `MAP_HUGETLB` 1 GiB pages.
  ExplicitHuge1G,
}

/// Expected access pattern → `madvise(MADV_RANDOM|MADV_SEQUENTIAL)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessPattern {
  Random,
  Sequential,
}

/// Whether a store is on the hot random-access path (candidate for huge pages) or cold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hotness {
  Hot,
  Cold,
}

/// The resolved policy for a single store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StorePolicy {
  pub page: PagePolicy,
  pub access: AccessPattern,
  pub hotness: Hotness,
}

/// Pure page decision, factored out so it is testable on every platform (independent of the
/// `cfg` gate the integrated [`ResourcePolicy`] applies).
pub(crate) fn decide_page(
  allow_huge: bool,
  huge_2m_available: bool,
  hot_bytes: u64,
  stlb_reach_base: u64,
  stlb_reach_2m: u64,
  hugetlb_2m_reserved: bool,
  hugetlb_1g_reserved: bool,
) -> PagePolicy {
  if !allow_huge || !huge_2m_available {
    return PagePolicy::Native;
  }
  // Fits base-page TLB reach → huge pages would only waste a scarce pool.
  if hot_bytes <= stlb_reach_base {
    return PagePolicy::Native;
  }
  // Beyond 2 MiB reach and a 1 GiB pool exists → go all the way.
  if hot_bytes > stlb_reach_2m && hugetlb_1g_reserved {
    return PagePolicy::ExplicitHuge1G;
  }
  // Prefer an explicit pool (no compaction stalls) when reserved; else transparent.
  if hugetlb_2m_reserved {
    PagePolicy::ExplicitHuge2M
  } else {
    PagePolicy::TransparentHuge2M
  }
}

/// The per-run policy object. Built once from the two probes, queried per store / batch.
#[derive(Debug, Clone, Copy)]
pub struct ResourcePolicy {
  hw: HardwareProbe,
  corpus: CorpusProbe,
}

impl ResourcePolicy {
  pub fn new(hw: HardwareProbe, corpus: CorpusProbe) -> Self {
    Self { hw, corpus }
  }

  /// Probe the machine and pair it with the corpus projection.
  pub fn probe(corpus: CorpusProbe) -> Self {
    Self::new(HardwareProbe::detect(), corpus)
  }

  pub fn hardware(&self) -> &HardwareProbe {
    &self.hw
  }

  pub fn corpus(&self) -> &CorpusProbe {
    &self.corpus
  }

  /// The page / access policy for one store. Cold stores stay native (§8.2); hot stores
  /// escalate only when their projected working set exceeds TLB reach.
  pub fn for_store(&self, kind: StoreKind, access: AccessPattern, hotness: Hotness) -> StorePolicy {
    let page = match hotness {
      Hotness::Cold => PagePolicy::Native,
      Hotness::Hot => decide_page(
        HUGE_PAGES_SUPPORTED,
        self.hw.huge_2m_available(),
        self.corpus.projected_hot_bytes(kind),
        self.hw.stlb_reach_base_bytes,
        self.hw.stlb_reach_2m_bytes,
        self.hw.hugetlb_2m_reserved,
        self.hw.hugetlb_1g_reserved,
      ),
    };
    StorePolicy {
      page,
      access,
      hotness,
    }
  }

  /// Bump-arena backing-chunk size: `clamp(next_pow2(batch_bytes), 64 KiB, 2 MiB)` (§8.3). A
  /// tiny batch gets one 64 KiB chunk; a large ingest gets 2 MiB chunks (huge-page-backable).
  pub fn arena_chunk_bytes(&self, batch_bytes: u64) -> usize {
    const MIN: u64 = 64 * 1024;
    const MAX: u64 = 2 * 1024 * 1024;
    // clamp first so `next_power_of_two` cannot overflow.
    batch_bytes.clamp(MIN, MAX).next_power_of_two().min(MAX) as usize
  }

  /// Starting software-prefetch distance (§8.1 table). 0 for tiny corpora (the helper compiles
  /// to a no-op); a warm-up sweep (§8.4) refines it for the daemon.
  pub fn prefetch_distance(&self) -> usize {
    let n = self.corpus.est_nodes();
    if n < 1_000_000 {
      0
    } else if n < 100_000_000 {
      4
    } else {
      8
    }
  }

  /// NUMA sharding engages only on a multi-socket machine at Meta scale (§8.1 / §8.3).
  pub fn numa_enabled(&self) -> bool {
    self.hw.numa_nodes >= 2 && self.corpus.est_nodes() >= 100_000_000
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::probe::{CorpusProbe, HardwareProbe};

  fn hw_with(thp: bool, pool2m: bool, pool1g: bool, nodes: usize) -> HardwareProbe {
    HardwareProbe {
      base_page_bytes: 4096,
      thp_2m_enabled: thp,
      hugetlb_2m_reserved: pool2m,
      hugetlb_1g_reserved: pool1g,
      numa_nodes: nodes,
      stlb_reach_base_bytes: 6 * 1024 * 1024,
      stlb_reach_2m_bytes: 3 * 1024 * 1024 * 1024,
    }
  }

  // --- pure decision logic, exercised on every platform -----------------------------------

  #[test]
  fn decide_page_stays_native_below_reach() {
    let p = decide_page(true, true, 1_000_000, 6 << 20, 3 << 30, false, false);
    assert_eq!(p, PagePolicy::Native);
  }

  #[test]
  fn decide_page_thp_when_over_base_reach() {
    let p = decide_page(true, true, 500 << 20, 6 << 20, 3 << 30, false, false);
    assert_eq!(p, PagePolicy::TransparentHuge2M);
  }

  #[test]
  fn decide_page_explicit_2m_when_pool_reserved() {
    let p = decide_page(true, true, 500 << 20, 6 << 20, 3 << 30, true, false);
    assert_eq!(p, PagePolicy::ExplicitHuge2M);
  }

  #[test]
  fn decide_page_1g_when_over_2m_reach_and_pool() {
    let p = decide_page(true, true, 8u64 << 30, 6 << 20, 3 << 30, true, true);
    assert_eq!(p, PagePolicy::ExplicitHuge1G);
  }

  #[test]
  fn decide_page_native_when_huge_disallowed() {
    // The macOS case: even a huge hot set + a "reserved pool" yields native.
    let p = decide_page(false, true, 8u64 << 30, 6 << 20, 3 << 30, true, true);
    assert_eq!(p, PagePolicy::Native);
  }

  // --- integrated policy ------------------------------------------------------------------

  #[test]
  fn baseline_is_free_for_a_few_files() {
    let rp = ResourcePolicy::new(hw_with(true, false, false, 1), CorpusProbe::new(4_000, 3));
    let sp = rp.for_store(StoreKind::AnnAdjacency, AccessPattern::Random, Hotness::Hot);
    assert_eq!(sp.page, PagePolicy::Native);
    assert_eq!(rp.prefetch_distance(), 0);
    assert!(!rp.numa_enabled());
    assert_eq!(rp.arena_chunk_bytes(0), 64 * 1024);
  }

  #[test]
  fn cold_stores_never_get_huge_pages() {
    let rp = ResourcePolicy::new(
      hw_with(true, false, false, 1),
      CorpusProbe::new(1 << 40, 1 << 20),
    );
    let sp = rp.for_store(
      StoreKind::VectorsFull,
      AccessPattern::Sequential,
      Hotness::Cold,
    );
    assert_eq!(sp.page, PagePolicy::Native);
  }

  #[test]
  fn arena_chunk_clamps() {
    let rp = ResourcePolicy::new(hw_with(true, false, false, 1), CorpusProbe::new(0, 0));
    assert_eq!(rp.arena_chunk_bytes(1_000), 64 * 1024);
    assert_eq!(rp.arena_chunk_bytes(100_000), 128 * 1024);
    assert_eq!(rp.arena_chunk_bytes(3 << 20), 2 << 20);
  }

  #[test]
  fn prefetch_and_numa_scale_with_corpus() {
    let meta = ResourcePolicy::new(
      hw_with(true, false, false, 2),
      CorpusProbe::new(80_000_000_000, 5_000_000),
    );
    assert_eq!(meta.prefetch_distance(), 8);
    assert!(meta.numa_enabled());

    let single_socket = ResourcePolicy::new(
      hw_with(true, false, false, 1),
      CorpusProbe::new(80_000_000_000, 5_000_000),
    );
    assert!(!single_socket.numa_enabled(), "one socket → no NUMA");
  }

  #[test]
  fn huge_pages_track_platform_support() {
    assert_eq!(HUGE_PAGES_SUPPORTED, cfg!(target_os = "linux"));
  }
}
