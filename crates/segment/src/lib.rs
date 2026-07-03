//! `vorpal-segment` — the immutable `.vseg` columnar segment container (§9.1) and the dense-id
//! segment directory (§9.2), mmapped through [`vorpal_mem`].
//!
//! A store (nodes / vectors / chunks / …) is a sequence of **immutable, page-aligned, mmap'd
//! segments**; sealed segments never mutate (deletion is a tombstone elsewhere). This crate
//! implements the framing + integrity + O(1) point access for the **HOT** column stripe — a
//! point lookup is `base + row·stride`, one cache line, zero decode, zero deserialize. WARM/COLD
//! codec blocks (FastLanes/FSST/zstd) are a later layer that slots into the same directory.
//!
//! Identity model (§9.2): internal `NodeId = logical_id_base + row` is a dense monotone
//! **physical locator**; `blake3(path:entityPath)` (held elsewhere, in the canonical index) is
//! the permanent identity. The [`SegmentDirectory`] resolves a `NodeId` to `(segment, row)` by
//! binary search over a tiny resident `id_base → segment` table.

mod builder;
mod directory;
mod error;
mod format;
mod id;
mod segment;

pub use builder::SegmentBuilder;
pub use directory::{SegmentDirectory, SegmentId};
pub use error::SegmentError;
pub use format::{ColumnPlacement, LogicalType};
pub use id::NodeId;
pub use segment::{ColumnView, Segment};
