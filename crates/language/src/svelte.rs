use super::pre_process_pattern;
use vorpal_core::Language;
use vorpal_core::matcher::{Pattern, PatternBuilder, PatternError};
use vorpal_core::tree_sitter::{LanguageExt, StrDoc, TSLanguage, TSRange};
use vorpal_core::{Node, matcher::KindMatcher};

use crate::html_injection::{find_lang, node_to_range};

/// Svelte host: `<script>`/`<style>` blocks inject their languages (C3/Wave 2).
#[derive(Clone, Copy, Debug)]
pub struct Svelte;
impl Language for Svelte {
  fn expando_char(&self) -> char {
    'z'
  }
  fn pre_process_pattern<'q>(&self, query: &'q str) -> std::borrow::Cow<'q, str> {
    pre_process_pattern(self.expando_char(), query)
  }
  fn kind_to_id(&self, kind: &str) -> u16 {
    crate::parsers::language_svelte().id_for_node_kind(kind, true)
  }
  fn field_to_id(&self, field: &str) -> Option<u16> {
    crate::parsers::language_svelte()
      .field_id_for_name(field)
      .map(|f| f.get())
  }
  fn build_pattern(&self, builder: &PatternBuilder) -> Result<Pattern, PatternError> {
    builder.build(|src| StrDoc::try_new(src, *self))
  }
}
impl LanguageExt for Svelte {
  fn get_ts_language(&self) -> TSLanguage {
    crate::parsers::language_svelte()
  }
  fn injectable_languages(&self) -> Option<&'static [&'static str]> {
    Some(&["css", "js", "ts", "scss"])
  }
  fn extract_injections<L: LanguageExt>(
    &self,
    root: Node<StrDoc<L>>,
  ) -> Vec<(String, Vec<TSRange>)> {
    let lang = root.lang();
    let mut ret = Vec::new();
    let matcher = KindMatcher::new("script_element", lang.clone());
    for script in root.find_all(matcher) {
      let injected = find_lang(&script).unwrap_or_else(|| "js".into());
      if let Some(content) = script.children().find(|c| c.kind() == "raw_text") {
        ret.push((injected, vec![node_to_range(&content)]));
      }
    }
    let matcher = KindMatcher::new("style_element", lang.clone());
    for style in root.find_all(matcher) {
      let injected = find_lang(&style).unwrap_or_else(|| "css".into());
      if let Some(content) = style.children().find(|c| c.kind() == "raw_text") {
        ret.push((injected, vec![node_to_range(&content)]));
      }
    }
    ret
  }
}
