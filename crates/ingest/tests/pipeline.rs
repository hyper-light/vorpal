//! Orchestration: bounded streaming, content-hash skip, stats, seal — with a stub extractor
//! (no engine), so this exercises the pipeline itself (§3.4).

use std::borrow::Cow;

use vorpal_ingest::{FileExtractor, FileOutcome, Ingestor, KgWriter};
use vorpal_outline::model::{

  EntryRole, OutlineEntry, OutlineItem, SourcePosition, SourceRange, SymbolType,
};

/// One shared session for the whole test binary — bounded vocabulary, no lifetime plumbing.
fn itn() -> &'static vorpal_ingest::Interner {
  static INTERNER: std::sync::OnceLock<vorpal_ingest::Interner> = std::sync::OnceLock::new();
  INTERNER.get_or_init(vorpal_ingest::Interner::default)
}

/// Emits one function item per file, named by the source length — deterministic and engine-free.
struct StubExtractor;

impl FileExtractor for StubExtractor {
  fn extract_into<'i>(
    &self,
    _interner: &'i vorpal_ingest::Interner,
    path: &str,
    source: &str,
    writer: &mut KgWriter,
    _references: &mut Vec<vorpal_ingest::Reference<'i>>,
  ) {
    let item = OutlineItem {
      entry: OutlineEntry {
        role: EntryRole::Item,
        symbol_type: SymbolType::Function,
        name: Cow::Owned(format!("item_{}", source.len())),
        range: SourceRange {
          byte_offset: 0..1,
          start: SourcePosition { line: 0, column: 0 },
          end: SourcePosition { line: 0, column: 1 },
        },
        signature: Cow::Borrowed("sig"),
        ast_kind: Cow::Borrowed(""),
      },
      is_import: false,
      is_exported: true,
      members: vec![],
    };
    writer.ingest_file(path, &[item]);
  }
}

#[test]
fn streams_indexes_and_skips_by_content_hash() {
  let mut ing = Ingestor::new(itn(), StubExtractor);

  assert_eq!(ing.ingest_source("a.x", "aaa"), FileOutcome::Indexed);
  assert_eq!(ing.ingest_source("b.x", "bbbb"), FileOutcome::Indexed);
  // Each file → a File node + one item node.
  assert_eq!(ing.node_count(), 4);

  // Re-ingesting identical bytes is skipped (content-hash spine) — no new nodes.
  assert_eq!(ing.ingest_source("a.x", "aaa"), FileOutcome::Skipped);
  assert_eq!(ing.node_count(), 4);

  // Changed bytes are re-indexed; the file node dedups, a new item is added.
  assert_eq!(ing.ingest_source("a.x", "aa"), FileOutcome::Indexed);
  assert_eq!(ing.node_count(), 5);

  let stats = ing.stats();
  assert_eq!(stats.indexed, 3);
  assert_eq!(stats.skipped, 1);
  assert_eq!(stats.bytes, 3 + 4 + 3 + 2);

  let kg = ing.seal();
  assert_eq!(kg.node_count(), 5);
}
