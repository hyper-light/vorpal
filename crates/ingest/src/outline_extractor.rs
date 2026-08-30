//! The concrete engine-backed extractor: L0 tree-sitter parse → L1 outline rules (§3.1).

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use rayon::prelude::*;
use vorpal_core::Language;
use vorpal_kg::KgWriter;
use vorpal_language::{LanguageExt, SupportLang};
use vorpal_outline::DEFAULT_OUTLINE_RULES;
use vorpal_outline::combined_extractor::CombinedExtractors;
use vorpal_outline::extractor::{SerializableOutlineRule, parse_outline_rules};
use vorpal_outline::model::OutlineItem;

use vorpal_kg::NodeId;
use vorpal_resolve::Reference;

use crate::pipeline::FileExtractor;
use crate::product::{self, FileProduct, ProductRef};
use crate::references::{extract_references, ref_spec, resolved_ref_spec};

type LangExtractors = HashMap<SupportLang, CombinedExtractors<SupportLang>>;

/// The compiled default rule set, shared process-wide: compiling ~20 languages' rules costs
/// ~15 ms and every `OutlineExtractor::new` (CLI one-shots, MCP daemon re-indexes) was paying
/// it; now only the first does.
static DEFAULT_EXTRACTORS: OnceLock<Result<Arc<LangExtractors>, String>> = OnceLock::new();

/// Compiles the bundled outline rules into one [`CombinedExtractors`] per language and runs them
/// against parsed files. Language is chosen from the file extension (§3.1 "all languages").
pub struct OutlineExtractor {
  by_lang: Arc<LangExtractors>,
  /// Digest of the exact outline-rule source this extractor was compiled from. Folded into each
  /// product's identity so editing an extraction rule invalidates products it produced — the
  /// grammar digest alone cannot see a rule change.
  rules_digest: u64,
}

impl OutlineExtractor {
  /// The built-in outline rule set (`DEFAULT_OUTLINE_RULES`), compiled once per process.
  pub fn new() -> Result<Self, String> {
    let by_lang = DEFAULT_EXTRACTORS
      .get_or_init(|| compile_rules(DEFAULT_OUTLINE_RULES).map(Arc::new))
      .clone()?;
    Ok(Self {
      by_lang,
      rules_digest: xxhash_rust::xxh3::xxh3_64(DEFAULT_OUTLINE_RULES.as_bytes()),
    })
  }

  /// Compile an arbitrary outline-rule YAML document (bundled or user-supplied).
  pub fn from_rules(rules_yaml: &str) -> Result<Self, String> {
    Ok(Self {
      by_lang: Arc::new(compile_rules(rules_yaml)?),
      rules_digest: xxhash_rust::xxh3::xxh3_64(rules_yaml.as_bytes()),
    })
  }

  /// How many languages have compiled extractors.
  pub fn languages(&self) -> usize {
    self.by_lang.len()
  }

  /// The digest of the outline rules this extractor uses — a component of product identity.
  pub fn rules_digest(&self) -> u64 {
    self.rules_digest
  }
}

/// Parse the YAML rule set and compile each language's rules — languages compile in parallel
/// (they are independent), with any failure surfaced eagerly, exactly as the serial path did.
fn compile_rules(rules_yaml: &str) -> Result<LangExtractors, String> {
  let rules =
    parse_outline_rules::<SupportLang>(rules_yaml).map_err(|e| format!("parse rules: {e}"))?;

  let mut grouped: HashMap<SupportLang, Vec<SerializableOutlineRule<SupportLang>>> = HashMap::new();
  for rule in rules {
    // Slim builds: rules for disabled grammars parse (vocabulary) but never compile
    // (compiling a pattern parses it with the grammar — an unimplemented!() stub there).
    if !rule.common().language.is_enabled() {
      continue;
    }
    grouped
      .entry(rule.common().language)
      .or_default()
      .push(rule);
  }

  grouped
    .into_par_iter()
    .map(|(lang, lang_rules)| {
      let combined = CombinedExtractors::try_from(lang_rules, &Default::default())
        .map_err(|e| format!("compile {lang} rules: {e}"))?;
      Ok((lang, combined))
    })
    .collect()
}

impl OutlineExtractor {
  /// Whether a file at `path` would be extracted: a known extension with compiled outline rules
  /// and/or a reference-extraction spec (a language may have either independently).
  pub fn handles(&self, path: &str) -> bool {
    SupportLang::from_path(path)
      .is_some_and(|lang| self.by_lang.contains_key(&lang) || ref_spec(lang).is_some())
  }
}

impl OutlineExtractor {
  /// Extract one file into a cacheable [`FileProduct`]: outline items plus references keyed by
  /// their enclosing definition's *entity path* (stable across runs, unlike `NodeId`s). This is
  /// the single extraction path — live ingest applies the product immediately; incremental
  /// re-index replays persisted products for unchanged files.
  pub fn extract_product(&self, path: &str, source: &str) -> Option<FileProduct> {
    let lang = SupportLang::from_path(path)?;
    let combined = self.by_lang.get(&lang);
    let spec = resolved_ref_spec(lang);
    if combined.is_none() && spec.is_none() {
      return None;
    }
    // The parse tree (`grep`) is owned locally; everything extracted is copied into the owned
    // product before it drops. Reference extraction runs even without outline rules (the file
    // node is the only definition span).
    let grep = lang.grep(source);
    let root = grep.root();
    // Graceful-degradation telemetry (all languages): count the tree-sitter ERROR nodes this
    // parse produced (0 = clean) AND measure the damage — merged error ranges give an honest
    // affected-byte count (nested ERRORs never double-count) plus up to eight representative
    // spans, so health policies can threshold on a ratio and humans can look at the wreckage
    // without re-parsing (IMPROVEMENTS #11).
    //
    // The scan is gated on the root's O(1) `has_error` subtree flag: a clean parse (the
    // overwhelming majority of files) has provably zero ERROR nodes, so we skip the full-tree
    // DFS entirely. When the flag is set we walk exactly as before — a MISSING-only tree (flag
    // set, no ERROR node) still yields `(0, 0, [])`, byte-identical to the ungated result.
    let (error_nodes, error_bytes, error_spans) = if root.has_error() {
      let mut error_ranges: Vec<(u32, u32)> = root
        .dfs()
        .filter(|node| node.is_error())
        .map(|node| {
          let range = node.range();
          (range.start as u32, range.end as u32)
        })
        .collect();
      let error_nodes = error_ranges.len() as u32;
      error_ranges.sort_unstable();
      let mut merged: Vec<(u32, u32)> = Vec::new();
      for (start, end) in error_ranges {
        match merged.last_mut() {
          Some((_, last_end)) if start <= *last_end => *last_end = (*last_end).max(end),
          _ => merged.push((start, end)),
        }
      }
      let error_bytes: u64 = merged.iter().map(|&(s, e)| u64::from(e - s)).sum();
      let error_spans: Vec<(u32, u32)> = merged.into_iter().take(8).collect();
      (error_nodes, error_bytes, error_spans)
    } else {
      (0u32, 0u64, Vec::new())
    };
    let items: Vec<OutlineItem<'_>> = combined
      .map(|c| c.extract(grep.root()).collect())
      .unwrap_or_default();

    let (entities, spans) = local_layout(&items);

    let mut raw = Vec::new();
    if let Some(spec) = spec {
      extract_references(grep.root(), spec, &spans, &entities, &mut raw);
    }
    // The single ownership point: names/qualifiers rode through extraction as borrows of
    // `source`; they are copied exactly once, here, into the detachable product.
    let refs = raw
      .into_iter()
      .map(|r| ProductRef {
        from_entity_index: r.from.raw() as u32,
        name: r.name.into_owned(),
        kind: product::refkind_tag(r.kind),
        start: r.start,
        end: r.end,
        qualifier: r.qualifier.map(Cow::into_owned),
        form: product::refform_tag(r.form),
        alias: r.alias.map(Cow::into_owned),
      })
      .collect();

    Some(FileProduct {
      version: product::PRODUCT_FORMAT_VERSION,
      // The never-matching default stamp: persisting callers stat the source and stamp the
      // product; an unstamped product can never replay. The content digest is stamped from
      // the exact bytes extraction saw — the identity staged validation trusts.
      source_size: 0,
      source_mtime_ns: 0,
      source_xxh3: xxhash_rust::xxh3::xxh3_64(source.as_bytes()),
      // Extraction identity: the language's grammar generation folded with the outline-rule
      // digest, so the cache invalidates a product once either the parser or the rules change.
      grammar_digest: crate::extraction_identity(
        vorpal_language::grammar_digest(lang),
        self.rules_digest,
      ),
      error_nodes,
      error_bytes,
      error_spans,
      items: items.into_iter().map(product::own_item).collect(),
      refs,
    })
  }
}

/// Local definition layout for reference attribution: index 0 = the file, then items and their
/// members in the same order as the graph writer. Entity paths come from
/// [`vorpal_kg::layout_entity_paths`] — the single identity authority — so a reference resolves
/// to exactly the node the writer created, overloads included. Spans are built in lockstep so
/// each entity index maps to its byte range.
pub(crate) fn local_layout(
  items: &[OutlineItem<'_>],
) -> (Vec<String>, Vec<(std::ops::Range<usize>, NodeId)>) {
  let entities = vorpal_kg::layout_entity_paths(items);
  let mut spans: Vec<(std::ops::Range<usize>, NodeId)> = vec![(0..usize::MAX, NodeId::new(0))];
  let mut idx = 1u64;
  for item in items {
    spans.push((item.entry.range.byte_offset.clone(), NodeId::new(idx)));
    idx += 1;
    for member in &item.members {
      spans.push((member.entry.range.byte_offset.clone(), NodeId::new(idx)));
      idx += 1;
    }
  }
  debug_assert_eq!(entities.len(), spans.len(), "layout entity/span count mismatch");
  (entities, spans)
}

impl FileExtractor for OutlineExtractor {
  fn handles(&self, path: &str) -> bool {
    OutlineExtractor::handles(self, path)
  }

  fn extract_into<'i>(
    &self,
    interner: &'i vorpal_resolve::Interner,
    path: &str,
    source: &str,
    writer: &mut KgWriter,
    references: &mut Vec<Reference<'i>>,
  ) {
    // One extraction path for live and incremental ingest: build the product, apply it.
    if let Some(prod) = self.extract_product(path, source) {
      crate::pipeline::apply_product(interner, path, prod, writer, references);
    }
  }
}

#[cfg(test)]
mod identity_tests {
  use super::OutlineExtractor;
  use crate::extraction_identity as id;

  #[test]
  fn extraction_identity_distinguishes_grammar_and_rules() {
    assert_ne!(id(1, 2), id(1, 3), "a rules change must change identity");
    assert_ne!(id(1, 2), id(4, 2), "a grammar change must change identity");
    assert_ne!(id(1, 2), id(2, 1), "combine is order-sensitive, not a plain XOR");
    assert_eq!(id(7, 9), id(7, 9), "deterministic");
  }

  #[test]
  fn rules_digest_is_source_derived_and_stable() {
    // The default extractor and one compiled from the same default source agree; the digest is
    // a pure function of the rule bytes, so it is stable and non-zero.
    let a = OutlineExtractor::new().unwrap();
    let b = OutlineExtractor::from_rules(vorpal_outline::DEFAULT_OUTLINE_RULES).unwrap();
    assert_eq!(a.rules_digest(), b.rules_digest());
    assert_ne!(a.rules_digest(), 0);
  }
}
