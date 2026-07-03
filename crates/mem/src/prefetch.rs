//! Portable software-prefetch hints (§8.3).
//!
//! Used to turn pointer-chase stalls into overlapped latency on ANN beam search and CSR
//! frontier expansion. `x86_64` uses the stable `_mm_prefetch`; `aarch64`'s intrinsic is still
//! unstable (`feature(stdarch_aarch64_prefetch)`) so we emit `prfm` via inline `asm!` (stable);
//! other targets compile to a no-op. All forms are hints — never a correctness dependency.

/// Prefetch the cache line containing `p` into all cache levels (temporal locality, T0).
#[inline(always)]
pub fn prefetch_read<T>(p: *const T) {
  #[cfg(target_arch = "x86_64")]
  // SAFETY: `_mm_prefetch` is a hint; any address is valid, no memory is dereferenced.
  unsafe {
    core::arch::x86_64::_mm_prefetch::<{ core::arch::x86_64::_MM_HINT_T0 }>(p as *const i8);
  }
  #[cfg(target_arch = "aarch64")]
  // SAFETY: `prfm` is a hint; it never faults and reads no architected state.
  unsafe {
    core::arch::asm!("prfm pldl1keep, [{p}]", p = in(reg) p, options(nostack, preserves_flags));
  }
  #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
  {
    let _ = p;
  }
}

/// Non-temporal read prefetch (streaming): stage `p` while minimizing cache pollution — for
/// one-shot scans such as the f32 rerank pass (§8.3, `_MM_HINT_NTA`).
#[inline(always)]
pub fn prefetch_read_nta<T>(p: *const T) {
  #[cfg(target_arch = "x86_64")]
  // SAFETY: hint only; see `prefetch_read`.
  unsafe {
    core::arch::x86_64::_mm_prefetch::<{ core::arch::x86_64::_MM_HINT_NTA }>(p as *const i8);
  }
  #[cfg(target_arch = "aarch64")]
  // SAFETY: hint only; see `prefetch_read`.
  unsafe {
    core::arch::asm!("prfm pldl1strm, [{p}]", p = in(reg) p, options(nostack, preserves_flags));
  }
  #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
  {
    let _ = p;
  }
}

/// Prefetch the element `distance` positions ahead of index `i` in `slice` — the software
/// pipeline for a streamed columnar / frontier scan. `distance == 0` is a no-op (baseline).
#[inline(always)]
pub fn prefetch_slice_ahead<T>(slice: &[T], i: usize, distance: usize) {
  if distance == 0 {
    return;
  }
  if let Some(t) = slice.get(i.wrapping_add(distance)) {
    prefetch_read(t as *const T);
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn prefetch_helpers_are_callable() {
    let data: Vec<u64> = (0..1024).collect();
    // Exercise all forms; correctness is "does not crash / does not affect results".
    for i in 0..data.len() {
      prefetch_slice_ahead(&data, i, 8);
    }
    prefetch_read(data.as_ptr());
    prefetch_read_nta(data.as_ptr());
    prefetch_slice_ahead(&data, 1020, 8); // near the end: out-of-range ahead is ignored
    let sum: u64 = data.iter().sum();
    assert_eq!(sum, (0..1024).sum());
  }
}
