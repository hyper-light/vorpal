//! Version-stable hashing (invariant I3).
//!
//! Everything here uses a fixed, specified algorithm so that two builds — even at different Rust
//! compiler versions — agree byte-for-byte. `std::hash::DefaultHasher` is deliberately **not** used
//! anywhere that a value crosses the wire or feeds identity/dedup: std documents its output as
//! unstable across releases, which would silently break the distributed byte-identity proof.

use xxhash_rust::xxh3::{xxh3_64, xxh3_128};

/// 32-bit frame-integrity checksum (low half of `xxh3_64`). Not cryptographic — it catches
/// corruption/truncation on the wire, not tampering (that is the transport's / HMAC's job).
#[inline]
pub fn checksum32(bytes: &[u8]) -> u32 {
  (xxh3_64(bytes) & 0xFFFF_FFFF) as u32
}

/// Version-stable 64-bit content hash for identity and dedup.
#[inline]
pub fn stable_hash64(bytes: &[u8]) -> u64 {
  xxh3_64(bytes)
}

/// Version-stable 128-bit content id (e.g. `MatchRecord.content_hash` for cross-node dedup).
#[inline]
pub fn content_id128(bytes: &[u8]) -> u128 {
  xxh3_128(bytes)
}

/// 32-byte cryptographic digest for rule-set integrity and agent-binary pinning.
#[inline]
pub fn digest32(bytes: &[u8]) -> [u8; 32] {
  *blake3::hash(bytes).as_bytes()
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn hashing_is_deterministic_within_build() {
    assert_eq!(stable_hash64(b"vorpal"), stable_hash64(b"vorpal"));
    assert_eq!(content_id128(b"vorpal"), content_id128(b"vorpal"));
    assert_eq!(digest32(b"vorpal"), digest32(b"vorpal"));
  }

  #[test]
  fn stable_hash_golden_vectors() {
    // Golden vectors pin the algorithm (I3): a change to the hashing function — the exact failure
    // that would silently break distributed byte-identity — trips these. Values are xxh3 with the
    // crate's default seed of 0, captured from a known-good build.
    assert_eq!(stable_hash64(b""), GOLDEN_EMPTY);
    assert_eq!(stable_hash64(b"vorpal"), GOLDEN_VORPAL);
  }

  // Pinned from a known-good build. `GOLDEN_EMPTY` equals the xxh3-64 of empty input from the
  // reference implementation, cross-checking the crate itself.
  const GOLDEN_EMPTY: u64 = 0x2d06_8005_38d3_94c2;
  const GOLDEN_VORPAL: u64 = 0x1af7_e799_3196_9346;

  #[test]
  #[ignore = "run with --ignored --nocapture to capture golden vectors"]
  fn print_golden() {
    println!("empty  = {:#018x}", stable_hash64(b""));
    println!("vorpal = {:#018x}", stable_hash64(b"vorpal"));
  }

  #[test]
  fn checksum_is_low_half_of_hash() {
    assert_eq!(checksum32(b"vorpal"), (stable_hash64(b"vorpal") & 0xFFFF_FFFF) as u32);
  }
}
