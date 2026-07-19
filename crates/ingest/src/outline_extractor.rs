//! The concrete engine-backed extractor: L0 tree-sitter parse → L1 outline rules (§3.1).

use std::collections::HashMap;

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
use crate::references::{extract_references, ref_spec};

/// Compiles the bundled outline rules into one [`CombinedExtractors`] per language and runs them
/// against parsed files. Language is chosen from the file extension (§3.1 "all languages").
pub struct OutlineExtractor {
  by_lang: HashMap<SupportLang, CombinedExtractors<SupportLang>>,
}

impl OutlineExtractor {
  /// Compile the built-in outline rule set (`DEFAULT_OUTLINE_RULES`).
  pub fn new() -> Result<Self, String> {
    Self::from_rules(DEFAULT_OUTLINE_RULES)
  }

  /// Compile an arbitrary outline-rule YAML document (bundled or user-supplied).
  pub fn from_rules(rules_yaml: &str) -> Result<Self, String> {
    let rules =
      parse_outline_rules::<SupportLang>(rules_yaml).map_err(|e| format!("parse rules: {e}"))?;

    let mut grouped: HashMap<SupportLang, Vec<SerializableOutlineRule<SupportLang>>> =
      HashMap::new();
    for rule in rules {
      grouped
        .entry(rule.common().language)
        .or_default()
        .push(rule);
    }

    let mut by_lang = HashMap::new();
    for (lang, lang_rules) in grouped {
      let combined = CombinedExtractors::try_from(lang_rules, &Default::default())
        .map_err(|e| format!("compile {lang} rules: {e}"))?;
      by_lang.insert(lang, combined);
    }
    Ok(Self { by_lang })
  }

  /// How many languages have compiled extractors.
  pub fn languages(&self) -> usize {
    self.by_lang.len()
  }

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
    let spec = ref_spec(lang);
    if combined.is_none() && spec.is_none() {
      return None;
    }
    // The parse tree (`grep`) is owned locally; everything extracted is copied into the owned
    // product before it drops. Reference extraction runs even without outline rules (the file
    // node is the only definition span).
    let grep = lang.grep(source);
    let items: Vec<OutlineItem<'_>> = combined
      .map(|c| c.extract(grep.root()).collect())
      .unwrap_or_default();

    // Local definition layout mirroring `KgWriter`'s identity convention: index 0 = the file
    // (entity ""), then items (entity = name) and members (entity = owner.member).
    let mut entities: Vec<String> = vec![String::new()];
    let mut spans: Vec<(std::ops::Range<usize>, NodeId)> = vec![(0..usize::MAX, NodeId::new(0))];
    for item in &items {
      entities.push(item.entry.name.to_string());
      spans.push((
        item.entry.range.byte_offset.clone(),
        NodeId::new(entities.len() as u64 - 1),
      ));
      for member in &item.members {
        entities.push(product::member_entity_path(
          &item.entry.name,
          &member.entry.name,
        ));
        spans.push((
          member.entry.range.byte_offset.clone(),
          NodeId::new(entities.len() as u64 - 1),
        ));
      }
    }

    let mut raw = Vec::new();
    if let Some(spec) = spec {
      extract_references(grep.root(), spec, &spans, path, &mut raw);
    }
    let refs = raw
      .into_iter()
      .map(|r| ProductRef {
        from_entity: entities[r.from.raw() as usize].clone(),
        name: r.name,
        kind: product::refkind_tag(r.kind),
        start: r.evidence.0,
        end: r.evidence.1,
      })
      .collect();

    Some(FileProduct {
      items: items.into_iter().map(product::own_item).collect(),
      refs,
    })
  }
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
      crate::pipeline::apply_product(path, &prod, writer, references);
    }
  }
}
