//! Reset-per-batch bump arena sized from the policy (§8.3, §7.2).
//!
//! Each ingest worker owns one arena; per-file/per-batch entities and interned strings allocate
//! into it, and the whole arena is `reset()` between batches — no per-object free, no global
//! allocator on the extract hot loop, and backing chunks are reused (streaming arena reuse).
//! Peak RAM is O(batch × workers), independent of repo size.

use bumpalo::Bump;

use crate::policy::ResourcePolicy;

/// A per-worker bump arena with `reset`-based reuse.
pub struct BatchArena {
  bump: Bump,
}

impl BatchArena {
  /// Create an arena with a fixed backing-chunk capacity.
  pub fn with_capacity(bytes: usize) -> Self {
    Self {
      bump: Bump::with_capacity(bytes),
    }
  }

  /// Create an arena whose chunk size is derived from the policy for a given batch size (§8.1).
  pub fn from_policy(policy: &ResourcePolicy, batch_bytes: u64) -> Self {
    Self::with_capacity(policy.arena_chunk_bytes(batch_bytes))
  }

  /// Allocate a value in the arena; the reference is valid until the next [`BatchArena::reset`].
  pub fn alloc<T>(&self, value: T) -> &mut T {
    self.bump.alloc(value)
  }

  /// Copy a slice into the arena.
  pub fn alloc_slice_copy<T: Copy>(&self, src: &[T]) -> &mut [T] {
    self.bump.alloc_slice_copy(src)
  }

  /// Bulk-free everything (O(1)); retains backing chunks for reuse by the next batch.
  pub fn reset(&mut self) {
    self.bump.reset();
  }

  /// Total bytes of backing chunks the arena has reserved. Retained across [`BatchArena::reset`]
  /// for reuse (streaming arena reuse), so this measures the steady-state footprint, not live
  /// objects — it does not drop to zero on reset.
  pub fn allocated_bytes(&self) -> usize {
    self.bump.allocated_bytes()
  }

  /// Escape hatch for APIs that want the underlying [`Bump`] (e.g. `bumpalo::collections`).
  pub fn bump(&self) -> &Bump {
    &self.bump
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::probe::{CorpusProbe, HardwareProbe};

  #[test]
  fn alloc_then_reset_reuses_capacity() {
    let mut arena = BatchArena::with_capacity(64 * 1024);
    let reserved = arena.allocated_bytes();
    assert!(reserved >= 64 * 1024, "reserved {reserved}");
    {
      let a = arena.alloc(7u64);
      let s = arena.alloc_slice_copy(&[1u32, 2, 3, 4]);
      assert_eq!(*a, 7);
      assert_eq!(s, &[1, 2, 3, 4]);
    }
    arena.reset();
    // Reset retains chunk capacity for reuse rather than freeing it (streaming arena reuse).
    assert!(arena.allocated_bytes() >= 64 * 1024);
    // Usable again after reset.
    let b = arena.alloc(9u64);
    assert_eq!(*b, 9);
  }

  #[test]
  fn from_policy_sizes_the_chunk() {
    let rp = ResourcePolicy::new(HardwareProbe::detect(), CorpusProbe::new(4_000, 3));
    let arena = BatchArena::from_policy(&rp, 0);
    // Baseline: at least the 64 KiB floor is reserved.
    assert!(arena.bump().chunk_capacity() >= 64 * 1024 - 1);
  }
}
