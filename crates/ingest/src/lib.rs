//! `vorpal-ingest` — the streaming, bounded-memory ingest pipeline (§3.4).
//!
//! Drives `file → read → hash-skip → parse → extract → ingest → seal`. Only one file's source +
//! parse tree is ever in flight (dropped after each file), so peak transient memory is
//! `O(largest file)`, independent of repo size — the property sylk's whole-repo buffer lacked.
//! Parse/extract is decoupled behind [`FileExtractor`]; [`OutlineExtractor`] is the concrete
//! implementation that runs the L0 tree-sitter engine + `vorpal-outline` rules (L1) and feeds the
//! `vorpal-kg` assembler (L3).
//!
//! Content-hash skip (§3.4) is the incremental spine: unchanged file bytes are never re-parsed.
//! A single [`Ingestor`] is a single-writer-per-shard sink (§7.5); scale-out shards it by path.

mod outline_extractor;
mod pipeline;

pub use outline_extractor::OutlineExtractor;
pub use pipeline::{FileExtractor, FileOutcome, IngestStats, Ingestor};
pub use vorpal_kg::{Kg, KgWriter, NodeDef, NodeId, SymbolKind};
pub use vorpal_resolve::{RefKind, Reference, ResolveStats, Resolver};
