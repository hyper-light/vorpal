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
}

impl OutlineExtractor {
  /// The built-in outline rule set (`DEFAULT_OUTLINE_RULES`), compiled once per process.
  pub fn new() -> Result<Self, String> {
    let by_lang = DEFAULT_EXTRACTORS
      .get_or_init(|| compile_rules(DEFAULT_OUTLINE_RULES).map(Arc::new))
      .clone()?;
    Ok(Self { by_lang })
  }

  /// Compile an arbitrary outline-rule YAML document (bundled or user-supplied).
  pub fn from_rules(rules_yaml: &str) -> Result<Self, String> {
    Ok(Self {
      by_lang: Arc::new(compile_rules(rules_yaml)?),
    })
  }

  /// How many languages have compiled extractors.
  pub fn languages(&self) -> usize {
    self.by_lang.len()
  }
}

/// Parse the YAML rule set and compile each language's rules — languages compile in parallel
/// (they are independent), with any failure surfaced eagerly, exactly as the serial path did.
fn compile_rules(rules_yaml: &str) -> Result<LangExtractors, String> {
  let rules =
    parse_outline_rules::<SupportLang>(rules_yaml).map_err(|e| format!("parse rules: {e}"))?;

  let mut grouped: HashMap<SupportLang, Vec<SerializableOutlineRule<SupportLang>>> = HashMap::new();
  for rule in rules {
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
    // Graceful-degradation telemetry (all languages): note when tree-sitter could not fully
    // parse this file, so definitions it dropped are surfaced as a count, never hidden.
    let parse_errors = grep.root().dfs().any(|node| node.is_error());
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
      // The grammar generation this product was extracted with — the cache invalidates a
      // product once its language's grammar digest no longer matches.
      grammar_digest: vorpal_language::grammar_digest(lang),
      parse_errors,
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

  fn extract_into(
    &self,
    path: &str,
    source: &str,
    writer: &mut KgWriter,
    references: &mut Vec<Reference>,
  ) {
    // One extraction path for live and incremental ingest: build the product, apply it.
    if let Some(prod) = self.extract_product(path, source) {
      crate::pipeline::apply_product(path, prod, writer, references);
    }
  }
}
