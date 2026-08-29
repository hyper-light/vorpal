//! This module defines the supported programming languages for vorpal.
//!
//! It provides a set of customized languages with expando_char / pre_process_pattern,
//! and a set of stub languages without preprocessing.
//! A rule of thumb: if your language does not accept identifiers like `$VAR`.
//! You need use `impl_lang_expando!` macro and a standalone file for testing.
//! Otherwise, you can define it as a stub language using `impl_lang!`.
//! To see the full list of languages, visit `<https://vorpal.github.io/reference/languages.html>`
//!
//! ```
//! use vorpal_language::{LanguageExt, SupportLang};
//!
//! let lang: SupportLang = "rs".parse().unwrap();
//! let src = "fn foo() {}";
//! let root = lang.grep(src);
//! let found = root.root().find_all("fn $FNAME() {}").next().unwrap();
//! assert_eq!(found.start_pos().line(), 0);
//! assert_eq!(found.text(), "fn foo() {}");
//! ```

mod bash;
mod cpp;
mod csharp;
mod css;
mod dart;
mod elixir;
mod go;
mod haskell;
mod hcl;
mod html;
mod json;
mod kotlin;
mod lua;
mod markdown;
mod nix;
mod parsers;
mod php;
mod python;
mod ruby;
mod rust;
mod scala;
mod solidity;
mod swift;
mod yaml;

pub use html::Html;
use vorpal_core::matcher::{Pattern, PatternBuilder, PatternError};

use ignore::types::{Types, TypesBuilder};
use serde::de::Visitor;
use serde::{Deserialize, Deserializer, Serialize, de};
use std::borrow::Cow;
use std::fmt;
use std::fmt::{Display, Formatter};
use std::path::Path;
use std::str::FromStr;
use vorpal_core::Node;
use vorpal_core::meta_var::MetaVariable;
use vorpal_core::tree_sitter::{StrDoc, TSLanguage, TSRange};

pub use vorpal_core::language::Language;
pub use vorpal_core::tree_sitter::LanguageExt;

/// this macro implements bare-bone methods for a language
macro_rules! impl_lang {
  ($lang: ident, $func: ident) => {
    #[derive(Clone, Copy, Debug)]
    pub struct $lang;
    impl Language for $lang {
      fn kind_to_id(&self, kind: &str) -> u16 {
        self
          .get_ts_language()
          .id_for_node_kind(kind, /*named*/ true)
      }
      fn field_to_id(&self, field: &str) -> Option<u16> {
        self
          .get_ts_language()
          .field_id_for_name(field)
          .map(|f| f.get())
      }
      fn build_pattern(&self, builder: &PatternBuilder) -> Result<Pattern, PatternError> {
        builder.build(|src| StrDoc::try_new(src, self.clone()))
      }
    }
    impl LanguageExt for $lang {
      fn get_ts_language(&self) -> TSLanguage {
        parsers::$func().into()
      }
    }
  };
}

fn pre_process_pattern(expando: char, query: &str) -> std::borrow::Cow<'_, str> {
  let mut ret = Vec::with_capacity(query.len());
  let mut dollar_count = 0;
  for c in query.chars() {
    if c == '$' {
      dollar_count += 1;
      continue;
    }
    let need_replace = matches!(c, 'A'..='Z' | '_') // $A or $$A or $$$A
      || dollar_count == 3; // anonymous multiple
    let sigil = if need_replace { expando } else { '$' };
    ret.extend(std::iter::repeat_n(sigil, dollar_count));
    dollar_count = 0;
    ret.push(c);
  }
  // trailing anonymous multiple
  let sigil = if dollar_count == 3 { expando } else { '$' };
  ret.extend(std::iter::repeat_n(sigil, dollar_count));
  std::borrow::Cow::Owned(ret.into_iter().collect())
}

/// this macro will implement expando_char and pre_process_pattern
/// use this if your language does not accept $ as valid identifier char
macro_rules! impl_lang_expando {
  ($lang: ident, $func: ident, $char: expr) => {
    #[derive(Clone, Copy, Debug)]
    pub struct $lang;
    impl Language for $lang {
      fn kind_to_id(&self, kind: &str) -> u16 {
        self
          .get_ts_language()
          .id_for_node_kind(kind, /*named*/ true)
      }
      fn field_to_id(&self, field: &str) -> Option<u16> {
        self
          .get_ts_language()
          .field_id_for_name(field)
          .map(|f| f.get())
      }
      fn expando_char(&self) -> char {
        $char
      }
      fn pre_process_pattern<'q>(&self, query: &'q str) -> std::borrow::Cow<'q, str> {
        pre_process_pattern(self.expando_char(), query)
      }
      fn build_pattern(&self, builder: &PatternBuilder) -> Result<Pattern, PatternError> {
        builder.build(|src| StrDoc::try_new(src, self.clone()))
      }
    }
    impl LanguageExt for $lang {
      fn get_ts_language(&self) -> TSLanguage {
        $crate::parsers::$func().into()
      }
    }
  };
}

pub trait Alias: Display {
  const ALIAS: &'static [&'static str];
}

/// Implements the `ALIAS` associated constant for the given lang, which is
/// then used to define the `alias` const fn and a `Deserialize` impl.
macro_rules! impl_alias {
  ($lang:ident => $as:expr) => {
    impl Alias for $lang {
      const ALIAS: &'static [&'static str] = $as;
    }

    impl fmt::Display for $lang {
      fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self)
      }
    }

    impl<'de> Deserialize<'de> for $lang {
      fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
      where
        D: Deserializer<'de>,
      {
        let vis = AliasVisitor {
          aliases: Self::ALIAS,
        };
        deserializer.deserialize_str(vis)?;
        Ok($lang)
      }
    }

    impl From<$lang> for SupportLang {
      fn from(_: $lang) -> Self {
        Self::$lang
      }
    }
  };
}
/// Generates as convenience conversions between the lang types
/// and `SupportedType`.
macro_rules! impl_aliases {
  ($($lang:ident => $as:expr),* $(,)?) => {
    $(impl_alias!($lang => $as);)*
    const fn alias(lang: SupportLang) -> &'static [&'static str] {
      match lang {
        $(SupportLang::$lang => $lang::ALIAS),*
      }
    }
  };
}

/* Customized Language with expando_char / pre_process_pattern */
// https://en.cppreference.com/w/cpp/language/identifiers
impl_lang_expando!(C, language_c, '𐀀');
impl_lang_expando!(Cpp, language_cpp, '𐀀');
// https://docs.microsoft.com/en-us/dotnet/csharp/language-reference/language-specification/lexical-structure#643-identifiers
// all letter number is accepted
// https://www.compart.com/en/unicode/category/Nl
impl_lang_expando!(CSharp, language_c_sharp, 'µ');
// https://www.w3.org/TR/CSS21/grammar.html#scanner
impl_lang_expando!(Css, language_css, '_');
// https://github.com/elixir-lang/tree-sitter-elixir/blob/a2861e88a730287a60c11ea9299c033c7d076e30/grammar.js#L245
impl_lang_expando!(Elixir, language_elixir, 'µ');
// we can use any Unicode code point categorized as "Letter"
// https://go.dev/ref/spec#letter
impl_lang_expando!(Go, language_go, 'µ');
// GHC supports Unicode syntax per
// https://ghc.gitlab.haskell.org/ghc/doc/users_guide/exts/unicode_syntax.html
// and the tree-sitter-haskell grammar parses it too.
impl_lang_expando!(Haskell, language_haskell, 'µ');
// https://developer.hashicorp.com/terraform/language/syntax/configuration#identifiers
impl_lang_expando!(Hcl, language_hcl, 'µ');
// https://github.com/fwcd/tree-sitter-kotlin/pull/93
impl_lang_expando!(Kotlin, language_kotlin, 'µ');
// Nix uses $ for string interpolation (e.g., "${pkgs.hello}")
impl_lang_expando!(Nix, language_nix, '_');
// PHP accepts unicode to be used as some name not var name though
impl_lang_expando!(Php, language_php, 'µ');
// we can use any char in unicode range [:XID_Start:]
// https://docs.python.org/3/reference/lexical_analysis.html#identifiers
// see also [PEP 3131](https://peps.python.org/pep-3131/) for further details.
impl_lang_expando!(Python, language_python, 'µ');
// https://github.com/tree-sitter/tree-sitter-ruby/blob/f257f3f57833d584050336921773738a3fd8ca22/grammar.js#L30C26-L30C78
impl_lang_expando!(Ruby, language_ruby, 'µ');
// we can use any char in unicode range [:XID_Start:]
// https://doc.rust-lang.org/reference/identifiers.html
impl_lang_expando!(Rust, language_rust, 'µ');
//https://docs.swift.org/swift-book/documentation/the-swift-programming-language/lexicalstructure/#Identifiers
impl_lang_expando!(Swift, language_swift, 'µ');

// Stub Language without preprocessing
// Language Name, tree-sitter-name, alias, extension
impl_lang!(Bash, language_bash);
impl_lang!(Java, language_java);
impl_lang!(JavaScript, language_javascript);
impl_lang!(Json, language_json);
impl_lang!(Lua, language_lua);
impl_lang!(Markdown, language_markdown);
impl_lang!(Scala, language_scala);
impl_lang!(Solidity, language_solidity);
impl_lang!(Tsx, language_tsx);
impl_lang!(TypeScript, language_typescript);
impl_lang!(Dart, language_dart);
impl_lang!(Yaml, language_yaml);
// See ripgrep for extensions
// https://github.com/BurntSushi/ripgrep/blob/master/crates/ignore/src/default_types.rs

/// Represents all built-in languages.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Hash)]
pub enum SupportLang {
  Bash,
  C,
  Cpp,
  CSharp,
  Css,
  Dart,
  Go,
  Elixir,
  Haskell,
  Hcl,
  Html,
  Java,
  JavaScript,
  Json,
  Kotlin,
  Lua,
  Markdown,
  Nix,
  Php,
  Python,
  Ruby,
  Rust,
  Scala,
  Solidity,
  Swift,
  Tsx,
  TypeScript,
  Yaml,
}

impl SupportLang {
  pub const fn all_langs() -> &'static [SupportLang] {
    use SupportLang::*;
    &[
      Bash, C, Cpp, CSharp, Css, Dart, Elixir, Go, Haskell, Hcl, Html, Java, JavaScript, Json,
      Kotlin, Lua, Markdown, Nix, Php, Python, Ruby, Rust, Scala, Solidity, Swift, Tsx, TypeScript,
      Yaml,
    ]
  }

  pub fn file_types(&self) -> Types {
    file_types(*self)
  }
}

impl fmt::Display for SupportLang {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(f, "{self:?}")
  }
}

#[derive(Debug)]
pub enum SupportLangErr {
  LanguageNotSupported(String),
}

impl Display for SupportLangErr {
  fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), std::fmt::Error> {
    use SupportLangErr::*;
    match self {
      LanguageNotSupported(lang) => write!(f, "{lang} is not supported!"),
    }
  }
}

impl std::error::Error for SupportLangErr {}

impl<'de> Deserialize<'de> for SupportLang {
  fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
  where
    D: Deserializer<'de>,
  {
    deserializer.deserialize_str(SupportLangVisitor)
  }
}

struct SupportLangVisitor;

impl Visitor<'_> for SupportLangVisitor {
  type Value = SupportLang;

  fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
    f.write_str("SupportLang")
  }

  fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
  where
    E: de::Error,
  {
    v.parse().map_err(de::Error::custom)
  }
}
struct AliasVisitor {
  aliases: &'static [&'static str],
}

impl Visitor<'_> for AliasVisitor {
  type Value = &'static str;

  fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
    write!(f, "one of {:?}", self.aliases)
  }

  fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
  where
    E: de::Error,
  {
    self
      .aliases
      .iter()
      .copied()
      .find(|&a| v.eq_ignore_ascii_case(a))
      .ok_or_else(|| de::Error::invalid_value(de::Unexpected::Str(v), &self))
  }
}

impl_aliases! {
  Bash => &["bash"],
  C => &["c"],
  Cpp => &["cc", "c++", "cpp", "cxx"],
  CSharp => &["cs", "csharp"],
  Css => &["css"],
  Dart => &["dart"],
  Elixir => &["ex", "elixir"],
  Go => &["go", "golang"],
  Haskell => &["hs", "haskell"],
  Hcl => &["hcl"],
  Html => &["html"],
  Java => &["java"],
  JavaScript => &["javascript", "js", "jsx"],
  Json => &["json"],
  Kotlin => &["kotlin", "kt"],
  Lua => &["lua"],
  Markdown => &["markdown", "md"],
  Nix => &["nix"],
  Php => &["php"],
  Python => &["py", "python"],
  Ruby => &["rb", "ruby"],
  Rust => &["rs", "rust"],
  Scala => &["scala"],
  Solidity => &["sol", "solidity"],
  Swift => &["swift"],
  TypeScript => &["ts", "typescript"],
  Tsx => &["tsx"],
  Yaml => &["yaml", "yml"],
}

/// Implements the language names and aliases.
impl FromStr for SupportLang {
  type Err = SupportLangErr;
  fn from_str(s: &str) -> Result<Self, Self::Err> {
    for &lang in Self::all_langs() {
      for moniker in alias(lang) {
        if s.eq_ignore_ascii_case(moniker) {
          return Ok(lang);
        }
      }
    }
    Err(SupportLangErr::LanguageNotSupported(s.to_string()))
  }
}

macro_rules! execute_lang_method {
  ($me: path, $method: ident, $($pname:tt),*) => {
    use SupportLang as S;
    match $me {
      S::Bash => Bash.$method($($pname,)*),
      S::C => C.$method($($pname,)*),
      S::Cpp => Cpp.$method($($pname,)*),
      S::CSharp => CSharp.$method($($pname,)*),
      S::Css => Css.$method($($pname,)*),
      S::Dart => Dart.$method($($pname,)*),
      S::Elixir => Elixir.$method($($pname,)*),
      S::Go => Go.$method($($pname,)*),
      S::Haskell => Haskell.$method($($pname,)*),
      S::Hcl => Hcl.$method($($pname,)*),
      S::Html => Html.$method($($pname,)*),
      S::Java => Java.$method($($pname,)*),
      S::JavaScript => JavaScript.$method($($pname,)*),
      S::Json => Json.$method($($pname,)*),
      S::Kotlin => Kotlin.$method($($pname,)*),
      S::Lua => Lua.$method($($pname,)*),
      S::Markdown => Markdown.$method($($pname,)*),
      S::Nix => Nix.$method($($pname,)*),
      S::Php => Php.$method($($pname,)*),
      S::Python => Python.$method($($pname,)*),
      S::Ruby => Ruby.$method($($pname,)*),
      S::Rust => Rust.$method($($pname,)*),
      S::Scala => Scala.$method($($pname,)*),
      S::Solidity => Solidity.$method($($pname,)*),
      S::Swift => Swift.$method($($pname,)*),
      S::Tsx => Tsx.$method($($pname,)*),
      S::TypeScript => TypeScript.$method($($pname,)*),
      S::Yaml => Yaml.$method($($pname,)*),
    }
  }
}

macro_rules! impl_lang_method {
  ($method: ident, ($($pname:tt: $ptype:ty),*) => $return_type: ty) => {
    #[inline]
    fn $method(&self, $($pname: $ptype),*) -> $return_type {
      execute_lang_method!{ self, $method, $($pname),* }
    }
  };
}
impl Language for SupportLang {
  impl_lang_method!(kind_to_id, (kind: &str) => u16);
  impl_lang_method!(field_to_id, (field: &str) => Option<u16>);
  impl_lang_method!(meta_var_char, () => char);
  impl_lang_method!(expando_char, () => char);
  impl_lang_method!(extract_meta_var, (source: &str) => Option<MetaVariable>);
  impl_lang_method!(build_pattern, (builder: &PatternBuilder) => Result<Pattern, PatternError>);
  fn pre_process_pattern<'q>(&self, query: &'q str) -> Cow<'q, str> {
    execute_lang_method! { self, pre_process_pattern, query }
  }
  fn from_path<P: AsRef<Path>>(path: P) -> Option<Self> {
    from_extension(path.as_ref())
  }
}

impl LanguageExt for SupportLang {
  impl_lang_method!(get_ts_language, () => TSLanguage);
  impl_lang_method!(injectable_languages, () => Option<&'static [&'static str]>);
  fn extract_injections<L: LanguageExt>(
    &self,
    root: Node<StrDoc<L>>,
  ) -> Vec<(String, Vec<TSRange>)> {
    match self {
      SupportLang::Html => Html.extract_injections(root),
      _ => Vec::new(),
    }
  }
}

/// A digest of a language's compiled grammar generation. It fingerprints the parser's
/// observable structure — ABI version, declared semver, node/field/state counts, and the full
/// ordered list of node-kind and field names — so any grammar edit that adds, removes, renames,
/// or restructures productions changes the digest. This is the identity the product cache keys
/// on: a cached product carries the digest of the grammar that produced it, and replays only
/// while that still matches the linked grammar (so editing a grammar — e.g. adding PEP 810's
/// `lazy` node — invalidates exactly the stale products, not the whole cache).
///
/// It is a structural fingerprint, not a hash of the grammar source: the rare edit that changes
/// parse *actions* without changing any count or name (a pure precedence tweak) can escape it.
/// Such edits do not change the node/field surface products are built from, so the residual risk
/// is negligible; a `PRODUCT_FORMAT_VERSION` bump remains the escape hatch for anything subtler.
pub fn grammar_digest(lang: SupportLang) -> u64 {
  use std::sync::OnceLock;
  static CACHE: OnceLock<Vec<u64>> = OnceLock::new();
  let all = SupportLang::all_langs();
  let digests = CACHE.get_or_init(|| all.iter().map(|l| compute_grammar_digest(*l)).collect());
  all
    .iter()
    .position(|l| *l == lang)
    .map(|i| digests[i])
    .unwrap_or(0)
}

/// A single digest over **every** supported grammar, in `all_langs` order — the coarse stamp the
/// whole-tree fast path checks so that editing any grammar forces a re-index that then re-parses
/// (via the per-file [`grammar_digest`] gate) only the files whose language actually changed.
pub fn global_grammar_stamp() -> u64 {
  use std::sync::OnceLock;
  static STAMP: OnceLock<u64> = OnceLock::new();
  *STAMP.get_or_init(|| {
    let mut h = xxhash_rust::xxh3::Xxh3::new();
    for lang in SupportLang::all_langs() {
      h.update(&grammar_digest(*lang).to_le_bytes());
    }
    h.digest()
  })
}

/// Human-facing facts about one compiled-in grammar, for `vorpal grammars`. Everything here is
/// read from the linked parser at runtime — no build metadata — so it always reflects exactly
/// what this binary will parse with.
#[derive(Debug, Clone)]
pub struct GrammarInfo {
  pub lang: SupportLang,
  /// The grammar's own name, as compiled in (usually the language's canonical id).
  pub name: Option<&'static str>,
  /// tree-sitter ABI version of the generated parser.
  pub abi_version: usize,
  /// Grammar semver declared in the source `tree-sitter.json`, if the author provided it.
  pub semver: Option<(u8, u8, u8)>,
  pub node_kinds: usize,
  pub parse_states: usize,
  /// The generation digest the product cache keys on (see [`grammar_digest`]).
  pub digest: u64,
}

/// Runtime facts about `lang`'s compiled-in grammar (see [`GrammarInfo`]).
pub fn grammar_info(lang: SupportLang) -> GrammarInfo {
  let ts = lang.get_ts_language();
  GrammarInfo {
    lang,
    name: ts.name(),
    abi_version: ts.abi_version(),
    semver: ts
      .metadata()
      .map(|m| (m.major_version, m.minor_version, m.patch_version)),
    node_kinds: ts.node_kind_count(),
    parse_states: ts.parse_state_count(),
    digest: grammar_digest(lang),
  }
}

/// One element of a grammar's observable surface, in enumeration order. This enumeration is
/// THE definition of "what a grammar is" for identity purposes: the product-cache digest
/// ([`grammar_digest_of`]) and the remote extraction-parity fingerprint both consume it —
/// each with its own byte framing, which is why events are structured rather than bytes —
/// so the two identities can never drift on *which* surface they observe (ADOPTION F-M0).
pub enum GrammarSurfaceEvent<'t> {
  Name(Option<&'t str>),
  Abi(u64),
  Metadata(Option<(u8, u8, u8)>),
  Counts {
    node_kinds: usize,
    parse_states: usize,
    fields: usize,
  },
  /// Emitted once per node-kind id, ascending; `name` is `None` for unnamed gaps.
  NodeKind { name: Option<&'t str>, named: bool },
  /// Emitted once per field id (1-based), ascending.
  Field(Option<&'t str>),
}

/// Walk `ts`'s observable surface in the fixed order every identity consumer relies on:
/// name, ABI, metadata, counts, node kinds ascending, fields ascending.
pub fn grammar_surface(ts: &TSLanguage, mut f: impl FnMut(GrammarSurfaceEvent<'_>)) {
  use GrammarSurfaceEvent as E;
  f(E::Name(ts.name()));
  f(E::Abi(ts.abi_version() as u64));
  f(E::Metadata(ts.metadata().map(|md| {
    (md.major_version, md.minor_version, md.patch_version)
  })));
  let node_kinds = ts.node_kind_count();
  let fields = ts.field_count();
  f(E::Counts {
    node_kinds,
    parse_states: ts.parse_state_count(),
    fields,
  });
  for id in 0..node_kinds as u16 {
    f(E::NodeKind {
      name: ts.node_kind_for_id(id),
      named: ts.node_kind_is_named(id),
    });
  }
  for id in 1..=fields as u16 {
    f(E::Field(ts.field_name_for_id(id)));
  }
}

/// The structural digest of ANY tree-sitter language (not just a compiled-in variant):
/// xxh3 over the [`grammar_surface`] events with the historical framing, so values are
/// byte-identical to every digest ever written into a product header (pinned by test).
pub fn grammar_digest_of(ts: &TSLanguage) -> u64 {
  use GrammarSurfaceEvent as E;
  let mut h = xxhash_rust::xxh3::Xxh3::new();
  grammar_surface(ts, |event| match event {
    E::Name(Some(name)) => h.update(name.as_bytes()),
    E::Name(None) => {}
    E::Abi(abi) => h.update(&abi.to_le_bytes()),
    E::Metadata(Some((major, minor, patch))) => h.update(&[major, minor, patch]),
    E::Metadata(None) => {}
    E::Counts {
      node_kinds,
      parse_states,
      fields,
    } => {
      h.update(&(node_kinds as u64).to_le_bytes());
      h.update(&(parse_states as u64).to_le_bytes());
      h.update(&(fields as u64).to_le_bytes());
    }
    // Node-kind and field names, in id order — captures additions, removals, and renames;
    // 0xff delimits so distinct name boundaries can't collide.
    E::NodeKind { name, named } => {
      if let Some(name) = name {
        h.update(name.as_bytes());
        h.update(&[u8::from(named)]);
      }
      h.update(&[0xff]);
    }
    E::Field(name) => {
      if let Some(name) = name {
        h.update(name.as_bytes());
      }
      h.update(&[0xff]);
    }
  });
  h.digest()
}

fn compute_grammar_digest(lang: SupportLang) -> u64 {
  grammar_digest_of(&lang.get_ts_language())
}

fn extensions(lang: SupportLang) -> &'static [&'static str] {
  use SupportLang::*;
  match lang {
    Bash => &[
      "bash", "bats", "cgi", "command", "env", "fcgi", "ksh", "sh", "tmux", "tool", "zsh",
    ],
    C => &["c", "h"],
    Cpp => &["cc", "hpp", "cpp", "c++", "hh", "cxx", "cu", "ino"],
    CSharp => &["cs"],
    Css => &["css", "scss"],
    Dart => &["dart"],
    Elixir => &["ex", "exs"],
    Go => &["go"],
    Haskell => &["hs"],
    Hcl => &["hcl", "nomad", "tf", "tfvars", "workflow"],
    Html => &["html", "htm", "xhtml"],
    Java => &["java"],
    JavaScript => &["cjs", "js", "mjs", "jsx"],
    Json => &["json"],
    Kotlin => &["kt", "ktm", "kts"],
    Lua => &["lua"],
    Markdown => &["markdown", "md"],
    Nix => &["nix"],
    Php => &["php"],
    Python => &["py", "py3", "pyi", "bzl", "bazel"],
    Ruby => &["rb", "rbw", "gemspec"],
    Rust => &["rs"],
    Scala => &["scala", "sc", "sbt"],
    Solidity => &["sol"],
    Swift => &["swift"],
    TypeScript => &["ts", "cts", "mts"],
    Tsx => &["tsx"],
    Yaml => &["yaml", "yml"],
  }
}

/// Guess which programming language a file is written in
/// Adapt from `<https://github.com/Wilfred/difftastic/blob/master/src/parse/guess_language.rs>`
/// N.B do not confuse it with `FromStr` trait. This function is to guess language from file extension.
fn from_extension(path: &Path) -> Option<SupportLang> {
  let ext = path.extension()?.to_str()?;
  SupportLang::all_langs()
    .iter()
    .copied()
    .find(|&l| extensions(l).contains(&ext))
}

fn add_custom_file_type<'b>(
  builder: &'b mut TypesBuilder,
  file_type: &str,
  suffix_list: &[&str],
) -> &'b mut TypesBuilder {
  for suffix in suffix_list {
    let glob = format!("*.{suffix}");
    builder
      .add(file_type, &glob)
      .expect("file pattern must compile");
  }
  builder.select(file_type)
}

fn file_types(lang: SupportLang) -> Types {
  let mut builder = TypesBuilder::new();
  let exts = extensions(lang);
  let lang_name = lang.to_string();
  add_custom_file_type(&mut builder, &lang_name, exts);
  builder.build().expect("file type must be valid")
}

pub fn config_file_type() -> Types {
  let mut builder = TypesBuilder::new();
  let builder = add_custom_file_type(&mut builder, "yml", &["yml", "yaml"]);
  builder.build().expect("yaml type must be valid")
}

#[cfg(test)]
mod test {
  use super::*;
  use vorpal_core::{Pattern, matcher::MatcherExt};

  pub fn test_match_lang(query: &str, source: &str, lang: impl LanguageExt) {
    let cand = lang.grep(source);
    let pattern = Pattern::new(query, lang);
    assert!(
      pattern.find_node(cand.root()).is_some(),
      "goal: {pattern:?}, candidate: {}",
      cand.root().get_inner_node().to_sexp(),
    );
  }

  pub fn test_non_match_lang(query: &str, source: &str, lang: impl LanguageExt) {
    let cand = lang.grep(source);
    let pattern = Pattern::new(query, lang);
    assert!(
      pattern.find_node(cand.root()).is_none(),
      "goal: {pattern:?}, candidate: {}",
      cand.root().get_inner_node().to_sexp(),
    );
  }

  pub fn test_replace_lang(
    src: &str,
    pattern: &str,
    replacer: &str,
    lang: impl LanguageExt,
  ) -> String {
    let mut source = lang.grep(src);
    assert!(
      source
        .replace(pattern, replacer)
        .expect("should parse successfully")
    );
    source.generate()
  }

  #[test]
  fn test_js_string() {
    test_match_lang("'a'", "'a'", JavaScript);
    test_match_lang("\"\"", "\"\"", JavaScript);
    test_match_lang("''", "''", JavaScript);
  }

  #[test]
  fn test_guess_by_extension() {
    let path = Path::new("foo.rs");
    assert_eq!(from_extension(path), Some(SupportLang::Rust));
    let path = Path::new("README.md");
    assert_eq!(from_extension(path), Some(SupportLang::Markdown));
    let path = Path::new("README.markdown");
    assert_eq!(from_extension(path), Some(SupportLang::Markdown));
  }

  #[test]
  fn grammar_digest_is_stable_and_per_language() {
    // Determinism: the digest is a pure function of the linked grammar, so it must be identical
    // on every call within a process — the product cache would never hit otherwise.
    for lang in SupportLang::all_langs() {
      assert_eq!(
        crate::grammar_digest(*lang),
        crate::grammar_digest(*lang),
        "{lang:?} digest is not stable"
      );
      assert_ne!(crate::grammar_digest(*lang), 0, "{lang:?} digest is zero");
    }
    // Distinctness: different grammars must not collide (else editing one could silently reuse
    // another's cache). All supported grammars are pairwise distinct.
    let digests: Vec<u64> = SupportLang::all_langs()
      .iter()
      .map(|l| crate::grammar_digest(*l))
      .collect();
    let mut sorted = digests.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), digests.len(), "two grammars share a digest");
    // The global stamp folds them all in and is itself stable.
    assert_eq!(crate::global_grammar_stamp(), crate::global_grammar_stamp());
  }

  // TODO: add test for file_types
}
