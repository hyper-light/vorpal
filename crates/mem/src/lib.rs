//! `vorpal-mem` — the adaptive memory substrate (`docs/ARCHITECTURE.md` §8, §9.1–9.3).
//!
//! Realizes the memory / cache / TLB foundation the rest of vorpal builds on. The design
//! goal is **one code path from 1 file to 10⁹ LOC** with resource use proportional to input:
//! nothing is pre-sized for "huge," and the baseline touches native pages and a small arena
//! only. Every heavy knob (huge pages, big arenas, prefetch, NUMA) is *derived* from two cheap
//! probes and stays dormant until the data justifies it.
//!
//! Modules:
//! - [`probe`] — cheap hardware + corpus probes (§8.1).
//! - [`policy`] — data-derived per-store page / arena / prefetch / NUMA decisions with a
//!   near-zero baseline (§8.1).
//! - [`store`] — adaptive mmap wrapper applying the page/`madvise` policy, cfg-gated for Linux
//!   huge pages vs. macOS 16 KiB native pages (§8.2).
//! - [`prefetch`] — portable software-prefetch hints for beam / CSR traversal (§8.3).
//! - [`csr`] — index-over-pointer CSR adjacency with a prefetching frontier walk (§8.3, §9.3).
//! - [`arena`] — reset-per-batch bump arena sized from the policy (§8.3).

pub mod arena;
/// The tiny libc surface the workspace's trace instrumentation needs (vorpal-mem owns
/// the libc dependency; callers avoid growing their own).
pub mod carry_libc {
  pub use libc::{RUSAGE_SELF, getrusage, rusage};
}
pub mod csr;
pub mod pod;
pub mod policy;
pub mod prefetch;
pub mod probe;
pub mod store;

pub use arena::BatchArena;
pub use csr::Csr;
pub use pod::PodColumn;
pub use policy::{AccessPattern, Hotness, PagePolicy, ResourcePolicy, StorePolicy};
pub use prefetch::{prefetch_read, prefetch_read_nta, prefetch_slice_ahead};
pub use probe::{CorpusProbe, HardwareProbe, StoreKind};
pub use store::{AnonStore, MappedStore};
