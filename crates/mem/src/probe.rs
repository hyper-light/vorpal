//! Hardware and corpus probes that feed the adaptive resource model (§8.1).
//!
//! Two side-effect-free probes — one of the machine, one of the corpus about to be indexed —
//! produce the inputs [`crate::policy`] turns into per-store decisions. Nothing here allocates
//! or reserves memory; the corpus probe reuses numbers the ingest `discover` walk already has.

#[cfg(target_os = "linux")]
use std::fs;

/// Conservative L2 STLB entry count used to estimate TLB reach.
///
/// Real parts vary (Skylake ~1536, Sunny/Golden Cove ~2048; Apple Silicon is large but
/// undocumented). 1536 is a safe lower bound. A CPUID/sysctl-based refinement can replace this
/// later (§8.1 "measured STLB reach"); until then the estimate only gates *escalation*, so a
/// low bound is conservative (we escalate slightly later, never wrongly).
const STLB_ENTRIES: u64 = 1536;

/// 2 MiB in bytes.
const PAGE_2M: u64 = 2 * 1024 * 1024;

/// Static description of the machine's memory hierarchy relevant to page / TLB policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HardwareProbe {
  /// OS base page size in bytes (4 KiB on x86-64 Linux; 16 KiB on Apple Silicon).
  pub base_page_bytes: usize,
  /// Transparent huge pages are enabled (`always`/`madvise`) → `MADV_HUGEPAGE` can promote.
  pub thp_2m_enabled: bool,
  /// A 2 MiB `hugetlb` pool is reserved → explicit `MAP_HUGETLB` maps will succeed.
  pub hugetlb_2m_reserved: bool,
  /// A 1 GiB `hugetlb` pool is reserved.
  pub hugetlb_1g_reserved: bool,
  /// Number of NUMA nodes (1 on UMA / laptops / Apple Silicon).
  pub numa_nodes: usize,
  /// Estimated L2 STLB reach with base pages, bytes — the working set that stays TLB-resident.
  pub stlb_reach_base_bytes: u64,
  /// Estimated L2 STLB reach with 2 MiB pages, bytes (~512× the base reach).
  pub stlb_reach_2m_bytes: u64,
}

impl HardwareProbe {
  /// Probe the running machine. Cheap: a `sysconf` call plus a few small `/sys` reads on Linux.
  pub fn detect() -> Self {
    let base_page_bytes = base_page_bytes();
    Self {
      base_page_bytes,
      thp_2m_enabled: thp_enabled(),
      hugetlb_2m_reserved: hugetlb_reserved(2048),
      hugetlb_1g_reserved: hugetlb_reserved(1_048_576),
      numa_nodes: numa_nodes(),
      stlb_reach_base_bytes: STLB_ENTRIES * base_page_bytes as u64,
      stlb_reach_2m_bytes: STLB_ENTRIES * PAGE_2M,
    }
  }

  /// Any 2 MiB huge-page mechanism (THP or a reserved `hugetlb` pool) is usable.
  pub fn huge_2m_available(&self) -> bool {
    self.thp_2m_enabled || self.hugetlb_2m_reserved
  }
}

fn base_page_bytes() -> usize {
  // SAFETY: `sysconf` is a pure query with no preconditions.
  let v = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
  if v > 0 { v as usize } else { 4096 }
}

#[cfg(target_os = "linux")]
fn thp_enabled() -> bool {
  match fs::read_to_string("/sys/kernel/mm/transparent_hugepage/enabled") {
    // Content looks like "always [madvise] never"; the bracketed token is active.
    Ok(s) => s.contains("[always]") || s.contains("[madvise]"),
    Err(_) => false,
  }
}
#[cfg(not(target_os = "linux"))]
fn thp_enabled() -> bool {
  // macOS Apple Silicon has no superpages (16 KiB base pages only); x86-64 macOS super pages
  // are undocumented/limited. Treat huge pages as a Linux-only feature (§8.2).
  false
}

#[cfg(target_os = "linux")]
fn hugetlb_reserved(size_kb: u32) -> bool {
  let path = format!("/sys/kernel/mm/hugepages/hugepages-{size_kb}kB/nr_hugepages");
  match fs::read_to_string(&path) {
    Ok(s) => s.trim().parse::<u64>().unwrap_or(0) > 0,
    Err(_) => false,
  }
}
#[cfg(not(target_os = "linux"))]
fn hugetlb_reserved(_size_kb: u32) -> bool {
  false
}

#[cfg(target_os = "linux")]
fn numa_nodes() -> usize {
  let mut n = 0usize;
  if let Ok(dir) = fs::read_dir("/sys/devices/system/node") {
    for entry in dir.flatten() {
      if let Some(name) = entry.file_name().to_str() {
        if let Some(rest) = name.strip_prefix("node") {
          if !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()) {
            n += 1;
          }
        }
      }
    }
  }
  n.max(1)
}
#[cfg(not(target_os = "linux"))]
fn numa_nodes() -> usize {
  1
}

/// Which store a projection is for. Each has its own hot-working-set model (§9.1, §10.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreKind {
  /// SoA hot node columns (`kind`/`flags`/edge cursors) — ~32 B/node (§9.1).
  NodesHot,
  /// CSR adjacency: `row_offsets` (8 B/node) + `col_indices` (4 B/edge) (§9.3).
  EdgesCsr,
  /// ANN graph adjacency (degree × 4 B/vector) (§10.2).
  AnnAdjacency,
  /// 1-bit RaBitQ codes, D=768 → 96 B/vector (§10.1).
  AnnCodes,
  /// Full-precision f32 rerank vectors (cold) (§10.1).
  VectorsFull,
  /// Canonical `blake3`→`NodeId` value spine (§9.6).
  Canonical,
}

/// Cheap projection of the corpus about to be indexed. Fields come free from the ingest walk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CorpusProbe {
  pub total_bytes: u64,
  pub file_count: u64,
}

impl CorpusProbe {
  /// Rough source bytes per extracted node (§8.1 `est_nodes ≈ bytes/40`).
  pub const BYTES_PER_NODE: u64 = 40;
  /// Rough average out-degree of the code graph.
  pub const AVG_DEGREE: u64 = 16;
  /// Rough source bytes per embedded chunk.
  pub const BYTES_PER_CHUNK: u64 = 512;
  /// Default ANN graph degree R.
  pub const ANN_DEGREE: u64 = 64;
  /// 1-bit RaBitQ code width for D=768 (768 / 8).
  pub const RABITQ_CODE_BYTES: u64 = 96;
  /// f32 vector width for D=768.
  pub const VECTOR_FULL_BYTES: u64 = 768 * 4;

  pub fn new(total_bytes: u64, file_count: u64) -> Self {
    Self {
      total_bytes,
      file_count,
    }
  }

  pub fn est_nodes(&self) -> u64 {
    (self.total_bytes / Self::BYTES_PER_NODE).max(self.file_count)
  }

  pub fn est_edges(&self) -> u64 {
    self.est_nodes().saturating_mul(Self::AVG_DEGREE)
  }

  pub fn est_vectors(&self) -> u64 {
    (self.total_bytes / Self::BYTES_PER_CHUNK).max(1)
  }

  /// Projected *hot* (randomly-touched) bytes for a store — the number compared against TLB
  /// reach to decide the page policy. Cold segments are excluded by construction (§8.2).
  pub fn projected_hot_bytes(&self, kind: StoreKind) -> u64 {
    match kind {
      StoreKind::NodesHot => self.est_nodes().saturating_mul(32),
      StoreKind::EdgesCsr => self
        .est_nodes()
        .saturating_mul(8)
        .saturating_add(self.est_edges().saturating_mul(4)),
      StoreKind::AnnAdjacency => self
        .est_vectors()
        .saturating_mul(Self::ANN_DEGREE)
        .saturating_mul(4),
      StoreKind::AnnCodes => self.est_vectors().saturating_mul(Self::RABITQ_CODE_BYTES),
      StoreKind::VectorsFull => self.est_vectors().saturating_mul(Self::VECTOR_FULL_BYTES),
      StoreKind::Canonical => self.est_nodes().saturating_mul(8),
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn hardware_probe_is_sane() {
    let hw = HardwareProbe::detect();
    assert!(
      hw.base_page_bytes >= 4096,
      "page size {}",
      hw.base_page_bytes
    );
    assert!(hw.base_page_bytes.is_power_of_two());
    assert!(hw.numa_nodes >= 1);
    assert_eq!(
      hw.stlb_reach_base_bytes,
      STLB_ENTRIES * hw.base_page_bytes as u64
    );
    assert!(hw.stlb_reach_2m_bytes > hw.stlb_reach_base_bytes);
  }

  #[test]
  fn corpus_projections_are_monotone() {
    let small = CorpusProbe::new(2_000, 1);
    let big = CorpusProbe::new(2_000_000_000, 100_000);
    assert!(big.est_nodes() > small.est_nodes());
    assert!(big.est_edges() > small.est_edges());
    assert!(big.est_vectors() > small.est_vectors());
    for kind in [
      StoreKind::NodesHot,
      StoreKind::EdgesCsr,
      StoreKind::AnnAdjacency,
      StoreKind::AnnCodes,
      StoreKind::VectorsFull,
      StoreKind::Canonical,
    ] {
      assert!(big.projected_hot_bytes(kind) > small.projected_hot_bytes(kind));
    }
  }

  #[test]
  fn est_nodes_never_below_file_count() {
    // Many tiny files: the per-file floor dominates the bytes/40 estimate.
    let c = CorpusProbe::new(10, 1000);
    assert!(c.est_nodes() >= 1000);
  }
}
