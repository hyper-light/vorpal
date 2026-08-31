//! The runtime language universe (F-M2): every language this process can parse — enabled
//! builtins plus any dynamically registered grammars — behind one type, [`SgLang`], with the
//! grammar-identity functions (per-language digest, global stamp) the index keys its caches on.
//!
//! Moved verbatim from `vorpal-cli`'s `lang` module so that ingest/index/MCP can speak the same
//! universe the CLI scan path does; the CLI re-exports this crate. Registration (custom
//! languages, globs, injections) stays one-shot process-wide: callers register at startup,
//! before any extraction — `vorpal-ingest` never triggers a `dlopen` itself.

mod injection;
mod lang_globs;

use anyhow::{Context, Result};
use ignore::types::Types;
use serde::{Deserialize, Serialize};
use vorpal_core::matcher::{Pattern, PatternBuilder, PatternError};
use vorpal_core::{
  Node,
  tree_sitter::{StrDoc, TSLanguage, TSRange},
};
use vorpal_dynamic::DynamicLang;
use vorpal_language::{Language, LanguageExt, SupportLang};

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fmt::{Debug, Display, Formatter};
use std::path::Path;
use std::str::FromStr;

pub use injection::SerializableInjection;
pub use lang_globs::LanguageGlobs;
pub use vorpal_dynamic::CustomLang;

/// Registration/lookup failures, exposed as a typed error so binaries can map them onto their
/// own error surfaces (the CLI re-wraps these into its `ErrorContext` for exit codes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistryError {
  /// A language name matched neither a builtin nor a registered custom language.
  UnrecognizableLanguage(String),
  /// A custom language library could not be found or loaded.
  CustomLanguage,
  /// A `languageInjections` rule failed to parse or compile.
  LangInjection,
  /// Glob configuration could not be built.
  ParseConfiguration,
}

impl Display for RegistryError {
  fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
    match self {
      RegistryError::UnrecognizableLanguage(lang) => {
        write!(f, "Language `{lang}` is not supported")
      }
      RegistryError::CustomLanguage => write!(f, "Cannot load custom language library"),
      RegistryError::LangInjection => write!(f, "Cannot parse languageInjections in config"),
      RegistryError::ParseConfiguration => write!(f, "Cannot parse configuration"),
    }
  }
}

impl std::error::Error for RegistryError {}

#[derive(Copy, Clone, PartialEq, Eq, Serialize, Deserialize, Hash)]
#[serde(untagged)]
pub enum SgLang {
  // inlined support lang expando char
  Builtin(SupportLang),
  Custom(DynamicLang),
}

impl SgLang {
  pub fn file_types(&self) -> Types {
    let default_types = match self {
      Builtin(b) => b.file_types(),
      Custom(c) => c.file_types(),
    };
    lang_globs::merge_globs(self, default_types)
  }

  // register_globs must be called after register_custom_language
  pub fn register_custom_language(base: &Path, langs: HashMap<String, CustomLang>) -> Result<()> {
    CustomLang::register(base, langs).context(RegistryError::CustomLanguage)
  }

  // TODO: add tests
  // register_globs must be called after register_custom_language
  pub fn register_globs(langs: LanguageGlobs) -> Result<()> {
    lang_globs::register(langs)
  }

  pub fn register_injections(injections: Vec<SerializableInjection>) -> Result<()> {
    injection::register_injetables(injections)
  }

  pub fn all_langs() -> Vec<Self> {
    let builtin = SupportLang::all_langs().iter().copied().map(Self::Builtin);
    let customs = DynamicLang::all_langs().into_iter().map(Self::Custom);
    builtin.chain(customs).collect()
  }

  /// Whether this language can actually parse in this build: builtins report their compiled-in
  /// grammar (the vocabulary/capability split — every vorpal artifact enables all of them;
  /// library embedders may subset), and a registered dynamic language is enabled by definition.
  pub fn is_enabled(&self) -> bool {
    match self {
      Builtin(b) => b.is_enabled(),
      Custom(_) => true,
    }
  }

  pub fn injectable_sg_langs(&self) -> Option<impl Iterator<Item = Self>> {
    let langs = self.injectable_languages()?;
    // TODO: handle injected languages not found
    // e.g vue can inject scss which is not supported by vorpal
    // we should report an error here
    // Dedup because aliases like "ts" and "typescript" resolve to the same SgLang
    let deduped: HashSet<_> = langs
      .iter()
      .filter_map(|s| SgLang::from_str(s).ok())
      .collect();
    Some(deduped.into_iter())
  }

  pub fn augmented_file_type(&self) -> Types {
    let self_type = self.file_types();
    let injector = Self::all_langs().into_iter().filter_map(|lang| {
      lang
        .injectable_sg_langs()?
        .any(|l| l == *self)
        .then_some(lang)
    });
    let injector_types = injector.map(|lang| lang.file_types());
    let all_types = std::iter::once(self_type).chain(injector_types);
    lang_globs::merge_types(all_types)
  }

  pub fn file_types_for_langs(langs: impl Iterator<Item = Self>) -> Types {
    let types = langs.map(|lang| lang.augmented_file_type());
    lang_globs::merge_types(types)
  }
}

impl Display for SgLang {
  fn fmt(&self, f: &mut Formatter) -> std::fmt::Result {
    match self {
      Builtin(b) => write!(f, "{b}"),
      Custom(c) => write!(f, "{}", c.name()),
    }
  }
}

impl Debug for SgLang {
  fn fmt(&self, f: &mut Formatter) -> std::fmt::Result {
    match self {
      Builtin(b) => write!(f, "{b:?}"),
      Custom(c) => write!(f, "{:?}", c.name()),
    }
  }
}

#[derive(Debug)]
pub enum SgLangErr {
  LanguageNotSupported(String),
}

impl Display for SgLangErr {
  fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), std::fmt::Error> {
    use SgLangErr::*;
    match self {
      LanguageNotSupported(lang) => write!(f, "{lang} is not supported!"),
    }
  }
}

impl std::error::Error for SgLangErr {}

impl FromStr for SgLang {
  type Err = SgLangErr;
  fn from_str(s: &str) -> Result<Self, Self::Err> {
    if let Ok(b) = SupportLang::from_str(s) {
      Ok(SgLang::Builtin(b))
    } else if let Ok(c) = DynamicLang::from_str(s) {
      Ok(SgLang::Custom(c))
    } else {
      Err(SgLangErr::LanguageNotSupported(s.into()))
    }
  }
}

impl From<SupportLang> for SgLang {
  fn from(value: SupportLang) -> Self {
    Self::Builtin(value)
  }
}
impl From<DynamicLang> for SgLang {
  fn from(value: DynamicLang) -> Self {
    Self::Custom(value)
  }
}

use SgLang::*;
impl Language for SgLang {
  fn pre_process_pattern<'q>(&self, query: &'q str) -> Cow<'q, str> {
    match self {
      Builtin(b) => b.pre_process_pattern(query),
      Custom(c) => c.pre_process_pattern(query),
    }
  }

  #[inline]
  fn meta_var_char(&self) -> char {
    match self {
      Builtin(b) => b.meta_var_char(),
      Custom(c) => c.meta_var_char(),
    }
  }

  #[inline]
  fn expando_char(&self) -> char {
    match self {
      Builtin(b) => b.expando_char(),
      Custom(c) => c.expando_char(),
    }
  }

  fn kind_to_id(&self, kind: &str) -> u16 {
    match self {
      Builtin(b) => b.kind_to_id(kind),
      Custom(c) => c.kind_to_id(kind),
    }
  }
  fn field_to_id(&self, field: &str) -> Option<u16> {
    match self {
      Builtin(b) => b.field_to_id(field),
      Custom(c) => c.field_to_id(field),
    }
  }
  fn from_path<P: AsRef<Path>>(path: P) -> Option<Self> {
    // respect user overriding like languageGlobs and custom lang
    // TODO: test this preference
    let path = path.as_ref();
    lang_globs::from_path(path)
      .or_else(|| DynamicLang::from_path(path).map(Custom))
      .or_else(|| SupportLang::from_path(path).map(Builtin))
  }
  fn build_pattern(&self, builder: &PatternBuilder) -> std::result::Result<Pattern, PatternError> {
    builder.build(|src| StrDoc::try_new(src, *self))
  }
}

impl LanguageExt for SgLang {
  fn get_ts_language(&self) -> TSLanguage {
    match self {
      Builtin(b) => b.get_ts_language(),
      Custom(c) => c.get_ts_language(),
    }
  }

  fn injectable_languages(&self) -> Option<&'static [&'static str]> {
    injection::injectable_languages(*self)
  }

  fn extract_injections<L: LanguageExt>(
    &self,
    root: Node<StrDoc<L>>,
  ) -> Vec<(String, Vec<TSRange>)> {
    injection::extract_injections(self, root)
  }
}

/// The language `path` maps to in the current runtime universe — user glob overrides first,
/// then registered dynamic languages, then builtin extensions. Free-function form of
/// [`Language::from_path`] so callers need not import the trait.
pub fn from_path(path: &Path) -> Option<SgLang> {
  <SgLang as Language>::from_path(path)
}

/// The grammar-generation digest for `lang`, or `None` when the language cannot parse in this
/// build (a disabled builtin in a library embedder's subset — never the case in vorpal
/// artifacts, which ship every grammar). Replaces the old `0`-sentinel: absence is typed, not
/// an in-band magic value. Builtins reuse `vorpal-language`'s per-process digest cache; dynamic
/// languages digest their loaded grammar surface once and cache per registration.
pub fn grammar_digest(lang: SgLang) -> Option<u64> {
  match lang {
    Builtin(b) => b.is_enabled().then(|| vorpal_language::grammar_digest(b)),
    Custom(c) => Some(dynamic_digest(c)),
  }
}

fn dynamic_digest(lang: DynamicLang) -> u64 {
  use std::sync::OnceLock;
  static CACHE: OnceLock<Vec<u64>> = OnceLock::new();
  let all = DynamicLang::all_langs();
  let digests = CACHE.get_or_init(|| {
    all
      .iter()
      .map(|l| vorpal_language::grammar_digest_of(&l.get_ts_language()))
      .collect()
  });
  all
    .iter()
    .position(|l| *l == lang)
    .map(|i| digests[i])
    // Registration is one-shot before extraction, so this arm is unreachable in practice; if a
    // caller ever races it, compute the true digest directly — correct, merely uncached.
    .unwrap_or_else(|| vorpal_language::grammar_digest_of(&lang.get_ts_language()))
}

/// One digest over the whole runtime universe — the coarse stamp the index manifest records so
/// the whole-tree fast path re-indexes when any grammar (builtin or registered dynamic)
/// changes. v2 formula: a format tag, the language count, then `(name, digest)` pairs sorted by
/// name — registration order can never perturb it, and two universes that differ in any
/// language, name, or grammar surface stamp differently. Computed fresh on every call (the
/// per-language digests behind it are cached), so a stamp taken after registration always sees
/// the registered languages — there is no stale-`OnceLock` hazard.
pub fn global_grammar_stamp() -> u64 {
  let mut entries: Vec<(String, u64)> = SgLang::all_langs()
    .into_iter()
    .filter_map(|lang| grammar_digest(lang).map(|digest| (lang.to_string(), digest)))
    .collect();
  entries.sort_unstable();
  stamp_of(&entries)
}

fn stamp_of(entries: &[(String, u64)]) -> u64 {
  let mut h = xxhash_rust::xxh3::Xxh3::new();
  h.update(b"vorpal-grammar-stamp/v2\n");
  h.update(&(entries.len() as u64).to_le_bytes());
  for (name, digest) in entries {
    h.update(name.as_bytes());
    h.update(&[0]);
    h.update(&digest.to_le_bytes());
  }
  h.digest()
}

#[cfg(test)]
mod test {
  use super::*;
  use std::mem::size_of;

  #[test]
  fn test_sg_lang_size() {
    assert_eq!(size_of::<SgLang>(), size_of::<DynamicLang>());
  }

  #[test]
  fn builtin_digests_match_language_crate() {
    for lang in SupportLang::all_langs() {
      assert_eq!(
        grammar_digest(SgLang::Builtin(*lang)),
        Some(vorpal_language::grammar_digest(*lang)),
        "registry digest for {lang} must be the language crate's digest"
      );
    }
  }

  #[test]
  fn stamp_is_deterministic_and_nontrivial() {
    assert_eq!(global_grammar_stamp(), global_grammar_stamp());
    assert_ne!(global_grammar_stamp(), 0);
  }

  #[test]
  fn stamp_formula_is_order_independent_and_collision_conscious() {
    let a = vec![("go".to_string(), 7u64), ("rust".to_string(), 9u64)];
    let mut b = vec![("rust".to_string(), 9u64), ("go".to_string(), 7u64)];
    b.sort_unstable();
    // Same universe, different construction order: identical after the canonical sort.
    assert_eq!(stamp_of(&a), stamp_of(&b));
    // Any difference in membership, name, or digest moves the stamp.
    assert_ne!(stamp_of(&a), stamp_of(&a[..1]));
    let renamed = vec![("go".to_string(), 7u64), ("ruby".to_string(), 9u64)];
    assert_ne!(stamp_of(&a), stamp_of(&renamed));
    let redigested = vec![("go".to_string(), 7u64), ("rust".to_string(), 10u64)];
    assert_ne!(stamp_of(&a), stamp_of(&redigested));
    // The count prefix keeps `["ab"]` and `["a","b"]`-shaped folds apart.
    let split = vec![("g".to_string(), 7u64), ("o".to_string(), 7u64)];
    assert_ne!(stamp_of(&[("go".to_string(), 7u64)]), stamp_of(&split));
  }
}
