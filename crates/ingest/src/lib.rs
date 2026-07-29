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
  peek_product_error_nodes, peek_product_grammar_digest, peek_product_stamps, save_product,
  save_product_with,
  validate_product,
};
pub use vorpal_kg::{Kg, KgWriter, NodeDef, NodeId, SymbolKind};
pub use vorpal_resolve::{
  Confidence, RefKind, Reference, ResolutionGrade, ResolveReason, ResolveStats, Resolver,
};

/// The grammar-generation digest for the language of `path`, or `None` if the path maps to no
/// supported language. The product-cache replay gate compares this against the digest a cached
/// product was stamped with, so editing a grammar invalidates exactly its language's products.
pub fn grammar_digest_for_path(path: &str) -> Option<u64> {
  use vorpal_language::Language;
  vorpal_language::SupportLang::from_path(path).map(vorpal_language::grammar_digest)
}

/// The full extraction-identity a product is keyed on: its language's grammar generation folded
/// with the outline-rule digest that extracted it. A change to *either* — the parser or the
/// extraction rules — yields a different identity, so the stale product re-parses. Returns `None`
/// when the path maps to no supported language.
pub fn extraction_identity_for_path(path: &str, rules_digest: u64) -> Option<u64> {
  grammar_digest_for_path(path).map(|g| extraction_identity(g, rules_digest))
}

/// Combine a grammar digest and a rules digest into one product-identity digest (order-fixed
/// xxh3, so it never accidentally cancels the way a XOR could).
pub fn extraction_identity(grammar_digest: u64, rules_digest: u64) -> u64 {
  let mut buf = [0u8; 16];
  buf[..8].copy_from_slice(&grammar_digest.to_le_bytes());
  buf[8..].copy_from_slice(&rules_digest.to_le_bytes());
  xxhash_rust::xxh3::xxh3_64(&buf)
}

/// A single digest over every supported grammar — the coarse stamp the whole-tree fast path
/// records in the manifest, so editing any grammar forces a re-index (which then re-parses,
/// via [`grammar_digest_for_path`], only the files whose language actually changed).
pub fn global_grammar_stamp() -> u64 {
  vorpal_language::global_grammar_stamp()
}
