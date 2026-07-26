//! `vorpal-ingest` — the streaming, bounded-memory ingest pipeline (§3.4).
//!
//! Drives `file → read → hash-skip → parse → extract → ingest → seal`. An [`Ingestor`] holds
//! one file's source + parse tree in flight at a time (dropped after each file), so its peak
//! transient memory is `O(largest file)` — and parallel callers fanning [`OutlineExtractor`]
//! across a worker pool (§7.5) are bounded at `O(workers × largest file)` — independent of
//! repo size either way, the property sylk's whole-repo buffer lacked.
//! Parse/extract is decoupled behind [`FileExtractor`]; [`OutlineExtractor`] is the concrete
//! implementation that runs the L0 tree-sitter engine + `vorpal-outline` rules (L1) and feeds the
//! `vorpal-kg` assembler (L3).
//!
//! Content-hash skip (§3.4) is the incremental spine: unchanged file bytes are never re-parsed.
//! A single [`Ingestor`] is a single-writer-per-shard sink (§7.5); scale-out shards it by path.

mod manifest;
mod outline_extractor;
mod pack;
mod pipeline;
mod product;
mod references;

pub use manifest::{FileStat, Manifest};
pub use outline_extractor::OutlineExtractor;
pub use pack::{PackMsg, PackReader, PackWriter};
pub use pipeline::{
  ByteBudget, ExtractScratch, FileExtractor, FileOutcome, IngestStats, Ingestor, StreamStats,
  StreamWork, apply_products_sharded, link_writer, link_writer_spilled, release_freed_pages,
  stream_apply, stream_apply_spilled,
};
pub use product::{
  FileProduct, ProductRef, ProductView, RefView, cache_file_name, decode_product,
  decode_product_view, encode_product_into, load_product, peek_product_digest,
  peek_product_parse_errors, peek_product_stamps, save_product, save_product_with,
  validate_product,
};
pub use vorpal_kg::{Kg, KgWriter, NodeDef, NodeId, SymbolKind};
pub use vorpal_resolve::{RefKind, Reference, ResolveStats, Resolver};
