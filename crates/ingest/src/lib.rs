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
pub mod refspec_config;
mod pack;
mod pipeline;
mod product;
mod references;
mod retained;
mod selfcheck;
pub mod requests;
pub use requests::RequestReport;
pub mod signature;
pub mod similar;
pub use similar::SimilarReport;
pub mod typefacts;

pub use manifest::{FileStat, Manifest};
pub use outline_extractor::{OutlineExtractor, RuleSource};

/// The extraction environment an index build runs under (F-M3): everything beyond the bundled
/// defaults that shapes what extraction sees. Today that is extra outline-rule sources
/// (custom/dynamic languages); serialized ref specs and canaries join it in F-M4. The default
/// environment is byte-for-byte the bundled behavior — same rules digest, same products.
#[derive(Debug, Clone, Default)]
pub struct ExtractionEnv {
  /// Extra outline-rule documents, each labeled by a stable machine-independent origin.
  pub outline_sources: Vec<RuleSource>,
  /// Serialized reference-extraction specs (F-M4), same labeling contract. Each document names
  /// its language; kinds resolve under the strict policy (typos fail registration, loudly).
  pub ref_spec_sources: Vec<RuleSource>,
  /// Extraction canaries for dynamic languages — the same trust gate builtin languages get
  /// from the compiled-in canary table. A dynamic language extracting without one is reported
  /// as unverified on every build, never silently trusted.
  pub canaries: Vec<DynamicCanary>,
  /// The project's `languageInjections` configuration, serialized (C3a): it shapes what the
  /// index extracts from host files, so its exact bytes fold into the rules digest — editing
  /// an injection rule re-keys products like editing an outline rule does.
  pub injection_config: Option<RuleSource>,
}

/// One dynamic language's extraction canary: `source` is extracted as `path` and must yield at
/// least `min_items` outline items and `min_refs` references.
#[derive(Debug, Clone)]
pub struct DynamicCanary {
  pub lang: String,
  pub path: String,
  pub source: String,
  pub min_items: usize,
  pub min_refs: usize,
}

impl ExtractionEnv {
  /// Whether this is byte-for-byte the bundled behavior (no custom rules, specs, canaries,
  /// or injection config). Retained fast paths that re-extract with the bundled extractor
  /// (the daemon's serve-immediately probe and live overlay) key off this: a custom
  /// environment falls through to the full env-aware pipeline instead of silently
  /// extracting under the wrong rules.
  pub fn is_default(&self) -> bool {
    self.outline_sources.is_empty()
      && self.ref_spec_sources.is_empty()
      && self.canaries.is_empty()
      && self.injection_config.is_none()
  }

  /// The extractor this environment describes. Languages named by the sources must already be
  /// registered — dlopen is the caller's job (a one-shot at startup), never extraction's.
  pub fn extractor(&self) -> Result<OutlineExtractor, String> {
    OutlineExtractor::with_env(
      &self.outline_sources,
      &self.ref_spec_sources,
      self.injection_config.as_ref(),
    )
  }

  /// Dynamic languages this environment extracts but does not canary-verify — computed against
  /// what the extractor actually compiled, sorted, deduped. Empty for the default environment.
  pub fn unverified_langs(&self, extractor: &OutlineExtractor) -> Vec<String> {
    let verified: std::collections::HashSet<&str> =
      self.canaries.iter().map(|c| c.lang.as_str()).collect();
    extractor
      .dynamic_langs()
      .into_iter()
      .filter(|lang| !verified.contains(lang.as_str()))
      .collect()
  }
}
pub use selfcheck::{verify_default_extraction, verify_env_extraction, verify_extraction};
pub use pack::{
  BucketMeta, PACK_DIR, PACK_TOC, PackFormat, PackMsg, PackReader, PackWriter, bucket_count_for,
  bucket_file_name, is_pack_member, splice_toc_digest,
};
pub use retained::RetainedIndex;
pub use pipeline::{
  ByteBudget, ExtractScratch, FileExtractor, FileOutcome, IngestStats, Ingestor, StreamStats,
  PairingHandle, StreamWork, apply_products_sharded, link_writer, link_writer_spilled,
  link_writer_spilled_with_flows, release_freed_pages, spawn_sig_pairing,
  stream_apply, stream_apply_spilled,
};
pub use product::{
  FileProduct, ProductRef, ProductRequest, ProductSignature, ProductView, RefView,
  RequestView, SignatureView, cache_file_name, decode_product,
  decode_product_view, encode_product_into, load_product, peek_product_digest,
  peek_product_error_bytes, peek_product_error_nodes, peek_product_grammar_digest,
  peek_product_stamps, save_product,
  save_product_with,
  validate_product,
};
pub use vorpal_kg::{Kg, KgWriter, NodeDef, NodeId, SymbolKind};
pub use vorpal_resolve::{
  Confidence, RefKind, Reference, ResolutionGrade, ResolveReason, ResolveStats, Resolver,
};
pub use vorpal_resolve::Interner;

/// The grammar-generation digest for the language of `path`, or `None` if the path maps to no
/// supported language. The product-cache replay gate compares this against the digest a cached
/// product was stamped with, so editing a grammar invalidates exactly its language's products.
pub fn grammar_digest_for_path(path: &str) -> Option<u64> {
  vorpal_lang_registry::from_path(std::path::Path::new(path)).and_then(grammar_generation_for)
}

/// The grammar-generation identity of `lang` as an extraction HOST (C3a): its own digest,
/// folded with the digests of every language it can inject (sorted by name — registration
/// order can never perturb it). A language with no injections keeps its bare digest, so the
/// 40+ non-host languages' identities are untouched by the injection machinery. The injectable
/// set is the runtime one (builtin + any registered `languageInjections`); the custom half of
/// that configuration is additionally folded into the rules digest by the extraction
/// environment, so editing it re-keys products even though this per-language fold cannot see
/// which rule changed.
pub fn grammar_generation_for(lang: SgLang) -> Option<u64> {
  use vorpal_language::LanguageExt;
  let base = vorpal_lang_registry::grammar_digest(lang)?;
  let Some(injectables) = lang.injectable_languages() else {
    return Some(base);
  };
  let mut entries: Vec<(String, u64)> = injectables
    .iter()
    .filter_map(|name| name.parse::<SgLang>().ok())
    .filter_map(|sub| vorpal_lang_registry::grammar_digest(sub).map(|d| (sub.to_string(), d)))
    .collect();
  if entries.is_empty() {
    return Some(base);
  }
  entries.sort_unstable();
  entries.dedup();
  let mut h = xxhash_rust::xxh3::Xxh3::new();
  h.update(&base.to_le_bytes());
  for (name, digest) in &entries {
    h.update(name.as_bytes());
    h.update(&[0]);
    h.update(&digest.to_le_bytes());
  }
  Some(h.digest())
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
  // Four identity inputs: the grammar generation, the rules digest, the typefacts table
  // version, and the signature scheme version — editing capture semantics re-keys products
  // with no format bump.
  let mut buf = [0u8; 28];
  buf[..8].copy_from_slice(&grammar_digest.to_le_bytes());
  buf[8..16].copy_from_slice(&rules_digest.to_le_bytes());
  buf[16..24].copy_from_slice(&typefacts::TYPEFACTS_VERSION.to_le_bytes());
  buf[24..].copy_from_slice(&signature::SIGNATURE_VERSION.to_le_bytes());
  xxhash_rust::xxh3::xxh3_64(&buf)
}

/// A single digest over the whole runtime language universe — builtins plus any registered
/// dynamic grammars — the coarse stamp the whole-tree fast path records in the manifest, so a
/// change to any grammar forces a re-index (which then re-parses, via
/// [`grammar_digest_for_path`], only the files whose language actually changed). Sorted-fold
/// formula (v2): registration order can never perturb it.
pub fn global_grammar_stamp() -> u64 {
  let base = vorpal_lang_registry::global_grammar_stamp();
  // Injection hosts (C3a): a host language's extraction identity folds the grammars it can
  // inject, so the whole-tree stamp must see that fold too — otherwise turning injections on
  // (or changing an injected grammar) would leave the fast path serving pre-injection
  // products until an unrelated edit. Only hosts whose folded identity differs from their
  // bare digest contribute; for everything else this is exactly the registry stamp.
  let mut hosts: Vec<(String, u64)> = SgLang::all_langs()
    .into_iter()
    .filter_map(|lang| {
      let bare = vorpal_lang_registry::grammar_digest(lang)?;
      let generation = grammar_generation_for(lang)?;
      (generation != bare).then(|| (lang.to_string(), generation))
    })
    .collect();
  if hosts.is_empty() {
    return base;
  }
  hosts.sort_unstable();
  let mut h = xxhash_rust::xxh3::Xxh3::new();
  h.update(&base.to_le_bytes());
  for (name, generation) in &hosts {
    h.update(name.as_bytes());
    h.update(&[0]);
    h.update(&generation.to_le_bytes());
  }
  h.digest()
}

/// Whether `lang` has reference-extraction semantics (calls/imports/types) — pure-structural
/// languages (CSS, HTML, JSON, Markdown, YAML) do not. Public so the language matrix can be
/// generated from code truth instead of a hand-maintained list.
pub fn has_reference_extraction(lang: impl Into<SgLang>) -> bool {
  references::ref_spec(lang.into()).is_some()
}

pub use vorpal_lang_registry::SgLang;
pub use vorpal_language::SupportLang;

/// The canonical language name for a user-supplied alias (`"ts"` → `"TypeScript"`), or `None`
/// for an unknown language — the shared vocabulary of search-filter `lang` arguments.
pub fn canonical_language(name: &str) -> Option<String> {
  name
    .parse::<SgLang>()
    .ok()
    .map(|lang| lang.to_string())
}

/// The canonical language name `path` maps to by extension, or `None` for unsupported paths.
pub fn language_name_of(path: &str) -> Option<String> {
  vorpal_lang_registry::from_path(std::path::Path::new(path)).map(|lang| lang.to_string())
}
