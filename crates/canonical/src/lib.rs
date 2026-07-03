//! `vorpal-canonical` — the canonical `blake3`→`NodeId` index (§9.6): the identity, dedup, and
//! incremental-skip spine, and the **only** associative index in the store (§9.2).
//!
//! Shape (§9.6): an LSM — a **hot mutable overlay** absorbs write-heavy ingest; `seal` freezes it
//! into an immutable **sorted sealed segment** (binary-searched; §11.6 further compresses this to
//! a minimal perfect hash). Reads merge overlay-over-sealed newest-first.
//!
//! Roles:
//! - **Identity / dedup** — `blake3(path:entityPath)` is the permanent identity; the index maps it
//!   to a dense monotone [`NodeId`](vorpal_segment::NodeId) (a physical locator, §9.2). Re-seeing a
//!   key returns its existing id (dedup), never a new one.
//! - **Incremental skip** (§3.4) — each entry carries a content hash; [`CanonicalIndex::is_unchanged`]
//!   is the hash-skip check that lets ingest bypass unchanged files.

mod index;
mod key;

pub use index::{Assignment, CanonicalIndex};
pub use key::CanonicalKey;
