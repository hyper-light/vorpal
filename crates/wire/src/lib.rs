//! Vorpal fleet wire protocol — the shared framing and message layer spoken between the
//! coordinator (local `vorpal`) and a remote `vorpal-agent`, and between coordinators in the
//! coordinator tree.
//!
//! Design goals (see `docs/REMOTE.md`):
//! * **A node is authenticated but not trusted.** Every read validates length against a negotiated
//!   ceiling *before* allocating and verifies a payload checksum, so a malformed or hostile frame
//!   cannot exhaust coordinator memory.
//! * **Version-stable identity (I3).** Hashing that crosses the wire or feeds identity/dedup uses a
//!   fixed algorithm (`xxh3` / `blake3`), never `std::hash::DefaultHasher` (whose output std does
//!   not guarantee across Rust releases).
//! * **Portable framing.** The 16-byte header is explicit little-endian, independent of host
//!   endianness, so an arm64 pod and an amd64 coordinator interoperate.
//! * **Rules travel as canonicalized opaque bytes + digest.** The heavy, std-bound rule types live
//!   in `vorpal-config`; shipping their canonical bytes + a `blake3` digest keeps this crate free
//!   of a dependency cycle and makes the digest exactly the bytes that were shipped.

#[cfg(feature = "tokio")]
pub mod aio;
pub mod canon;
pub mod frame;
pub mod hash;
pub mod io;
pub mod msg;

#[cfg(feature = "tokio")]
pub use aio::{AsyncFrameReader, AsyncFrameWriter};
pub use canon::{canonical_json_digest, to_canonical_json};
pub use frame::{
  DEFAULT_MAX_FRAME, FRAME_HEADER_LEN, Frame, FrameHeader, MAGIC, PROTOCOL_VERSION, WireError, flag,
};
pub use hash::{checksum32, content_id128, digest32, stable_hash64};
pub use io::{FrameReader, FrameWriter};
pub use msg::*;
