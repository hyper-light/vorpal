//! The one machine datum the GEMM kernels block on: the level-2 cache size, read
//! from the platform's own enumeration (never guessed) — x86 CPUID leaf 4 /
//! 0x8000001D, macOS `hw.l2cachesize`, Linux sysfs — and `None` where nothing
//! enumerates it (a VM masking CPUID, a container without sysfs), in which case
//! the callers assume NO reuse (one tile per panel) rather than a size.

use std::sync::OnceLock;

/// Bytes of the level-2 data/unified cache, read once per process.
pub(super) fn l2_cache_bytes() -> Option<usize> {
  static L2: OnceLock<Option<usize>> = OnceLock::new();
  *L2.get_or_init(probe)
}

#[cfg(target_arch = "x86_64")]
fn probe() -> Option<usize> {
  use std::arch::x86_64::{__cpuid, __cpuid_count};
  // `cpuid` is architecturally present on every x86-64 CPU (the instruction
  // predates the 64-bit ISA) — the intrinsic is safe on this target.
  let (max_basic, max_extended) = (__cpuid(0).eax, __cpuid(0x8000_0000).eax);
  let mut leaves = Vec::with_capacity(2);
  if max_basic >= 4 {
    leaves.push(4u32);
  }
  if max_extended >= 0x8000_001D {
    leaves.push(0x8000_001Du32);
  }
  for leaf in leaves {
    for sub in 0..32u32 {
      let r = __cpuid_count(leaf, sub);
      let kind = r.eax & 0x1F;
      if kind == 0 {
        break;
      }
      let level = (r.eax >> 5) & 0x7;
      // 1 = data, 3 = unified — the caches a weight panel can live in.
      if level == 2 && (kind == 1 || kind == 3) {
        let ways = ((r.ebx >> 22) & 0x3FF) as usize + 1;
        let partitions = ((r.ebx >> 12) & 0x3FF) as usize + 1;
        let line = (r.ebx & 0xFFF) as usize + 1;
        let sets = r.ecx as usize + 1;
        return Some(ways * partitions * line * sets);
      }
    }
  }
  None
}

#[cfg(all(not(target_arch = "x86_64"), target_os = "macos"))]
fn probe() -> Option<usize> {
  unsafe extern "C" {
    fn sysctlbyname(
      name: *const std::ffi::c_char,
      oldp: *mut std::ffi::c_void,
      oldlenp: *mut usize,
      newp: *mut std::ffi::c_void,
      newlen: usize,
    ) -> std::ffi::c_int;
  }
  let name = c"hw.l2cachesize";
  let mut value: u64 = 0;
  let mut len = std::mem::size_of::<u64>();
  // SAFETY: `sysctlbyname` is part of libSystem, linked into every macOS
  // binary; `name` is a NUL-terminated literal; `value`/`len` are live locals
  // sized to the u64 the key returns, and no new value is written (null, 0).
  let rc = unsafe {
    sysctlbyname(
      name.as_ptr(),
      (&mut value as *mut u64).cast(),
      &mut len,
      std::ptr::null_mut(),
      0,
    )
  };
  (rc == 0 && value > 0).then_some(value as usize)
}

#[cfg(all(not(target_arch = "x86_64"), target_os = "linux"))]
fn probe() -> Option<usize> {
  let base = std::path::Path::new("/sys/devices/system/cpu/cpu0/cache");
  let entries = std::fs::read_dir(base).ok()?;
  for entry in entries.flatten() {
    let dir = entry.path();
    let level = std::fs::read_to_string(dir.join("level")).ok()?;
    let kind = std::fs::read_to_string(dir.join("type")).ok()?;
    if level.trim() == "2" && matches!(kind.trim(), "Data" | "Unified") {
      let size = std::fs::read_to_string(dir.join("size")).ok()?;
      let size = size.trim();
      let (digits, unit) = size.split_at(size.trim_end_matches(|c: char| c.is_ascii_alphabetic()).len());
      let value: usize = digits.parse().ok()?;
      return Some(match unit {
        "K" => value << 10,
        "M" => value << 20,
        _ => value,
      });
    }
  }
  None
}

#[cfg(not(any(target_arch = "x86_64", target_os = "macos", target_os = "linux")))]
fn probe() -> Option<usize> {
  None
}

#[cfg(test)]
mod tests {
  #[test]
  fn l2_probe_is_plausible_when_present() {
    match super::l2_cache_bytes() {
      // 64 KiB (the smallest L2 any 64-bit CPU shipped) .. 1 GiB.
      Some(bytes) => assert!((1 << 16..=1 << 30).contains(&bytes), "L2 {bytes}"),
      None => eprintln!("L2 size not enumerated on this machine (callers assume no reuse)"),
    }
  }
}
