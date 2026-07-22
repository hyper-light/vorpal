//! Stage-0 loader framing + signing (docs/REMOTE.md §2, §6).
//!
//! The coordinator ships a tiny `vorpal-loader` to a node, then streams
//! `[LoaderHeader][payload]` on its stdin. The loader verifies an **Ed25519 signature over the
//! payload's blake3 hash**, then execs the agent from a memfd — so the multi-MB agent binary never
//! touches the node's disk (zero persistent residue). This module is the shared, cross-platform
//! core: the coordinator uses [`sign_payload`] to build the stream, the loader uses
//! [`verify_and_extract`] to validate it and recover the agent bytes. The exec itself is in
//! `main.rs` (platform-specific).
//!
//! Trust model: the transport (SSH / `kubectl exec` / …) is already authenticated, so the signature
//! defends against *tampering or truncation of the payload in transit* and proves the bytes came
//! from the coordinator that holds the private key — it is integrity/authenticity, not secrecy. The
//! public key is supplied to the loader out-of-band (embedded at release time, or passed by the
//! coordinator per session); the private key never leaves the coordinator.

use std::io::{self, Read};

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};

pub use ed25519_dalek::{SigningKey as Ed25519SigningKey, VerifyingKey as Ed25519VerifyingKey};

/// Magic prefixing every loader stream (`"VLD1"`), so a mispiped/foreign stream is rejected before
/// anything is trusted.
pub const LOADER_MAGIC: [u8; 4] = *b"VLD1";

/// `flags` bit: the payload is zstd-compressed (reserved; the raw path is the default today).
pub const FLAG_ZSTD: u32 = 1 << 0;

/// Fixed header length: magic(4) + flags(4) + payload_len(8) + blake3(32) + signature(64).
pub const LOADER_HEADER_LEN: usize = 4 + 4 + 8 + 32 + 64;

/// The fixed-size, little-endian header that precedes the agent payload on the loader's stdin.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoaderHeader {
  pub flags: u32,
  /// Length in bytes of the payload that follows the header (compressed size if `FLAG_ZSTD`).
  pub payload_len: u64,
  /// blake3 of the **raw agent ELF** (post-decompress) — content address + fast integrity check.
  pub agent_blake3: [u8; 32],
  /// Ed25519 signature over `agent_blake3` by the coordinator's signing key.
  pub signature: [u8; 64],
}

impl LoaderHeader {
  /// Serialize to the fixed 112-byte little-endian header.
  pub fn to_bytes(&self) -> [u8; LOADER_HEADER_LEN] {
    let mut out = [0u8; LOADER_HEADER_LEN];
    out[0..4].copy_from_slice(&LOADER_MAGIC);
    out[4..8].copy_from_slice(&self.flags.to_le_bytes());
    out[8..16].copy_from_slice(&self.payload_len.to_le_bytes());
    out[16..48].copy_from_slice(&self.agent_blake3);
    out[48..112].copy_from_slice(&self.signature);
    out
  }

  /// Parse the fixed header, validating the magic.
  pub fn from_bytes(buf: &[u8; LOADER_HEADER_LEN]) -> Result<Self, LoaderError> {
    if buf[0..4] != LOADER_MAGIC {
      return Err(LoaderError::BadMagic);
    }
    let flags = u32::from_le_bytes(buf[4..8].try_into().unwrap());
    let payload_len = u64::from_le_bytes(buf[8..16].try_into().unwrap());
    let mut agent_blake3 = [0u8; 32];
    agent_blake3.copy_from_slice(&buf[16..48]);
    let mut signature = [0u8; 64];
    signature.copy_from_slice(&buf[48..112]);
    Ok(Self { flags, payload_len, agent_blake3, signature })
  }
}

/// Everything that can go wrong verifying a loader stream. Any variant means "do not exec".
#[derive(Debug)]
pub enum LoaderError {
  Io(io::Error),
  BadMagic,
  /// `payload_len` exceeds the caller's ceiling (a malformed/hostile header must not drive an
  /// unbounded allocation).
  PayloadTooLarge { len: u64, max: u64 },
  /// The payload's blake3 does not match the header — corruption or truncation.
  HashMismatch,
  /// The signature does not verify against the configured public key.
  BadSignature,
  /// The header advertised zstd but this build has no decompressor.
  ZstdUnsupported,
  Decompress(String),
}

impl std::fmt::Display for LoaderError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      LoaderError::Io(e) => write!(f, "loader io error: {e}"),
      LoaderError::BadMagic => write!(f, "not a vorpal loader stream (bad magic)"),
      LoaderError::PayloadTooLarge { len, max } => {
        write!(f, "loader payload {len} bytes exceeds ceiling {max}")
      }
      LoaderError::HashMismatch => write!(f, "agent blake3 hash mismatch (corrupt/truncated payload)"),
      LoaderError::BadSignature => write!(f, "agent Ed25519 signature verification failed"),
      LoaderError::ZstdUnsupported => write!(f, "payload is zstd-compressed but this loader has no decompressor"),
      LoaderError::Decompress(m) => write!(f, "loader decompress failed: {m}"),
    }
  }
}

impl std::error::Error for LoaderError {}

impl From<io::Error> for LoaderError {
  fn from(e: io::Error) -> Self {
    LoaderError::Io(e)
  }
}

/// Build the `[header][payload]` stream for `agent_bytes`, signed by `signing_key`. The payload is
/// raw today (`compress` is reserved for a zstd path); the signature is over the raw agent's
/// blake3, so the loader can verify authenticity from a 32-byte message regardless of payload size.
pub fn sign_payload(agent_bytes: &[u8], signing_key: &SigningKey, compress: bool) -> Vec<u8> {
  let hash = blake3::hash(agent_bytes);
  let signature = signing_key.sign(hash.as_bytes());
  let (flags, payload): (u32, &[u8]) = if compress {
    // Reserved: a zstd path would compress here and set FLAG_ZSTD. Raw for now.
    (0, agent_bytes)
  } else {
    (0, agent_bytes)
  };
  let header = LoaderHeader {
    flags,
    payload_len: payload.len() as u64,
    agent_blake3: *hash.as_bytes(),
    signature: signature.to_bytes(),
  };
  let mut out = Vec::with_capacity(LOADER_HEADER_LEN + payload.len());
  out.extend_from_slice(&header.to_bytes());
  out.extend_from_slice(payload);
  out
}

/// Read a `[header][payload]` stream from `reader`, verify blake3 **and** the Ed25519 signature
/// against `verifying_key`, and return the recovered agent bytes. Reads **exactly** the framed
/// bytes and no more, so a caller streaming the agent's own protocol after the payload keeps it
/// intact. `max_payload` bounds the allocation against a malformed/hostile header.
pub fn verify_and_extract<R: Read>(
  reader: &mut R,
  verifying_key: &VerifyingKey,
  max_payload: u64,
) -> Result<Vec<u8>, LoaderError> {
  let mut header_buf = [0u8; LOADER_HEADER_LEN];
  reader.read_exact(&mut header_buf)?;
  let header = LoaderHeader::from_bytes(&header_buf)?;

  if header.payload_len > max_payload {
    return Err(LoaderError::PayloadTooLarge { len: header.payload_len, max: max_payload });
  }
  let mut payload = vec![0u8; header.payload_len as usize];
  reader.read_exact(&mut payload)?;

  let agent = if header.flags & FLAG_ZSTD != 0 {
    return Err(LoaderError::ZstdUnsupported);
  } else {
    payload
  };

  // Integrity: the payload's hash must match the header's claim.
  let got = blake3::hash(&agent);
  if got.as_bytes() != &header.agent_blake3 {
    return Err(LoaderError::HashMismatch);
  }
  // Authenticity: the signature over that hash must verify against the trusted key. `verify_strict`
  // rejects the malleable/small-order edge cases plain `verify` accepts.
  let sig = Signature::from_bytes(&header.signature);
  verifying_key
    .verify_strict(got.as_bytes(), &sig)
    .map_err(|_| LoaderError::BadSignature)?;
  Ok(agent)
}

/// Parse a 32-byte Ed25519 public key from lowercase/uppercase hex.
pub fn verifying_key_from_hex(hex: &str) -> Option<VerifyingKey> {
  let bytes = decode_hex(hex.trim())?;
  let arr: [u8; 32] = bytes.try_into().ok()?;
  VerifyingKey::from_bytes(&arr).ok()
}

/// Lowercase-hex-encode a public key for passing to the loader.
pub fn verifying_key_to_hex(key: &VerifyingKey) -> String {
  encode_hex(key.as_bytes())
}

/// Parse a 32-byte Ed25519 private (signing) key from hex — the coordinator/release signer.
pub fn signing_key_from_hex(hex: &str) -> Option<SigningKey> {
  let bytes = decode_hex(hex.trim())?;
  let arr: [u8; 32] = bytes.try_into().ok()?;
  Some(SigningKey::from_bytes(&arr))
}

/// Lowercase-hex-encode a private (signing) key.
pub fn signing_key_to_hex(key: &SigningKey) -> String {
  encode_hex(&key.to_bytes())
}

/// blake3 of the agent bytes, lowercase hex — the value a coordinator pins per `(os,arch)`.
pub fn agent_blake3_hex(agent_bytes: &[u8]) -> String {
  encode_hex(blake3::hash(agent_bytes).as_bytes())
}

/// Mint a fresh per-session Ed25519 keypair (coordinator side).
#[cfg(feature = "keygen")]
pub fn generate_keypair() -> (SigningKey, VerifyingKey) {
  let sk = SigningKey::generate(&mut rand_core::OsRng);
  let vk = sk.verifying_key();
  (sk, vk)
}

fn encode_hex(bytes: &[u8]) -> String {
  let mut s = String::with_capacity(bytes.len() * 2);
  for b in bytes {
    s.push_str(&format!("{b:02x}"));
  }
  s
}

fn decode_hex(s: &str) -> Option<Vec<u8>> {
  if s.len() % 2 != 0 {
    return None;
  }
  (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok()).collect()
}

#[cfg(test)]
mod tests {
  use super::*;

  fn key() -> SigningKey {
    // A fixed test key (deterministic tests; never a real secret).
    SigningKey::from_bytes(&[7u8; 32])
  }

  #[test]
  fn round_trips_and_recovers_the_agent() {
    let sk = key();
    let vk = sk.verifying_key();
    let agent = b"\x7fELF...pretend-this-is-a-multi-mb-binary...".repeat(100);
    let stream = sign_payload(&agent, &sk, false);
    let got = verify_and_extract(&mut &stream[..], &vk, 10 << 20).unwrap();
    assert_eq!(got, agent);
  }

  #[test]
  fn rejects_a_tampered_payload() {
    let sk = key();
    let vk = sk.verifying_key();
    let agent = b"agent-v1-bytes".to_vec();
    let mut stream = sign_payload(&agent, &sk, false);
    // Flip a payload byte (after the 112-byte header): blake3 no longer matches the header.
    let last = stream.len() - 1;
    stream[last] ^= 0xff;
    assert!(matches!(
      verify_and_extract(&mut &stream[..], &vk, 10 << 20),
      Err(LoaderError::HashMismatch)
    ));
  }

  #[test]
  fn rejects_a_foreign_signature() {
    let sk = key();
    let attacker = SigningKey::from_bytes(&[9u8; 32]);
    let agent = b"agent-bytes".to_vec();
    // Signed by the attacker, but verified against the trusted key ⇒ rejected.
    let stream = sign_payload(&agent, &attacker, false);
    let trusted = sk.verifying_key();
    assert!(matches!(
      verify_and_extract(&mut &stream[..], &trusted, 10 << 20),
      Err(LoaderError::BadSignature)
    ));
  }

  #[test]
  fn rejects_bad_magic_and_oversized_len() {
    let vk = key().verifying_key();
    // Bad magic.
    let mut junk = vec![0u8; LOADER_HEADER_LEN];
    junk[0] = b'X';
    assert!(matches!(verify_and_extract(&mut &junk[..], &vk, 1 << 20), Err(LoaderError::BadMagic)));
    // Oversized payload_len is rejected before allocating.
    let sk = key();
    let mut stream = sign_payload(b"x", &sk, false);
    stream[8..16].copy_from_slice(&(u64::MAX).to_le_bytes());
    assert!(matches!(
      verify_and_extract(&mut &stream[..], &sk.verifying_key(), 1 << 20),
      Err(LoaderError::PayloadTooLarge { .. })
    ));
  }

  #[test]
  fn reads_exactly_the_payload_leaving_a_trailing_stream_intact() {
    // The loader must not over-read: bytes after the payload are the agent's own wire stream.
    let sk = key();
    let vk = sk.verifying_key();
    let agent = b"the-agent".to_vec();
    let mut stream = sign_payload(&agent, &sk, false);
    let trailer = b"WIRE-FRAMES-FOR-THE-AGENT";
    stream.extend_from_slice(trailer);
    let mut cursor = &stream[..];
    let got = verify_and_extract(&mut cursor, &vk, 1 << 20).unwrap();
    assert_eq!(got, agent);
    // The cursor now points exactly at the trailer — nothing consumed past the payload.
    assert_eq!(cursor, trailer);
  }

  #[test]
  fn hex_round_trips() {
    let vk = key().verifying_key();
    let hex = verifying_key_to_hex(&vk);
    assert_eq!(verifying_key_from_hex(&hex).unwrap().as_bytes(), vk.as_bytes());
    assert!(verifying_key_from_hex("not-hex").is_none());
  }
}
