//! The concrete engine-backed extractor: L0 tree-sitter parse → L1 outline rules (§3.1).

use std::collections::HashMap;

use vorpal_core::Language;
use vorpal_kg::KgWriter;
use vorpal_language::{LanguageExt, SupportLang};
use vorpal_outline::DEFAULT_OUTLINE_RULES;
use vorpal_outline::combined_extractor::CombinedExtractors;
use vorpal_outline::extractor::{SerializableOutlineRule, parse_outline_rules};
use vorpal_outline::model::OutlineItem;

use vorpal_resolve::Reference;

use crate::pipeline::FileExtractor;

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

  /// Whether a file at `path` would be extracted (known extension + compiled rules).
  pub fn handles(&self, path: &str) -> bool {
    SupportLang::from_path(path).is_some_and(|lang| self.by_lang.contains_key(&lang))
  }
}

impl FileExtractor for OutlineExtractor {
  fn extract_into(
    &self,
    path: &str,
    source: &str,
    writer: &mut KgWriter,
    _references: &mut Vec<Reference>,
  ) {
    let Some(lang) = SupportLang::from_path(path) else {
      return;
    };
    let Some(combined) = self.by_lang.get(&lang) else {
      return;
    };
    // The parse tree (`grep`) is owned locally; items borrow it and are copied into the KG heap
    // by `ingest_file` before it drops — nothing borrowed escapes this scope.
    let grep = lang.grep(source);
    let items: Vec<OutlineItem<'_>> = combined.extract(grep.root()).collect();
    writer.ingest_file(path, &items);
  }
}
