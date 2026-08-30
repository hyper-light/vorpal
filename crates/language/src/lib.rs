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
mod astro;
mod html;
mod html_injection;
mod svelte;
mod vue;
mod json;
mod kotlin;
mod lua;
mod markdown;
mod nix;
mod php;
mod python;
mod ruby;
mod rust;
mod scala;
mod solidity;
mod swift;
mod yaml;

pub use astro::Astro;
pub use html::Html;
pub use svelte::Svelte;
pub use vue::Vue;
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

/// Imports the tree-sitter parser crate when its feature flag is on; a disabled flag leaves
/// an `unimplemented!()` stub (kept unreachable by `is_enabled` gating — vorpal artifacts
/// enable every grammar; the stub exists for library embedders' subset builds and wasm).
macro_rules! conditional_lang {
  ($lang: ident, $flag: literal, $field: ident) => {{
    #[cfg(feature = $flag)]
    {
      $lang::$field.into()
    }
    #[cfg(not(feature = $flag))]
    {
      unimplemented!("tree-sitter parser is not implemented when feature flag is off.")
    }
  }};
  ($lang: ident, $flag: literal) => {
    conditional_lang!($lang, $flag, LANGUAGE)
  };
}

/// One declarative row per built-in language (F-M5) — THE single authority every capability
/// surface is generated from: the parser binding (the `parsers` module), the struct + trait
/// impls, the `SupportLang` variant, the compiled-in (`all_langs`) and vocabulary
/// (`all_variants`) tables, `is_enabled`, name aliases, extension routing, and the
/// `execute_lang_method!` dispatch. Adding a language = one row here + the Cargo entries +
/// data files (outline rules, ref spec, canary, corpus fixtures) — nothing else to keep in
/// sync by hand.
///
/// Row shape (fields in this exact order):
/// ```text
/// Variant { parser: fn_name(ts_crate[, SYMBOL]), feature: "cargo-feature",
///           kind: plain | expando('c') | custom, aliases: [..], extensions: [..] }
/// ```
/// `kind: custom` skips struct emission (the type is hand-written — `Html`, which carries
/// injection support); `expando` selects the metavariable expando char for grammars where `$`
/// is not a valid identifier character. The leading `$` argument is the standard macro-in-
/// macro dollar-escape (it lets this macro emit `execute_lang_method!`).
macro_rules! langs {
  ($d:tt $(
    $variant:ident {
      parser: $parser:ident($ts_crate:ident $(, $ts_field:ident)?),
      feature: $feature:literal,
      kind: $kind:tt $(($kchar:literal))?,
      aliases: [$($alias:literal),+ $(,)?],
      extensions: [$($ext:literal),+ $(,)?],
      $(filenames: [$($fname:literal),+ $(,)?],)?
    }
  )*) => {
    /// One binding per language row; a disabled feature leaves an `unimplemented!()` stub
    /// that `is_enabled` gating keeps unreachable (vorpal artifacts enable every grammar).
    pub mod parsers {
      use vorpal_core::tree_sitter::TSLanguage;
      $(
        pub fn $parser() -> TSLanguage {
          conditional_lang!($ts_crate, $feature $(, $ts_field)?)
        }
      )*
    }

    $( lang_struct!($variant, $parser, $kind $(($kchar))?); )*

    /// Represents all built-in languages.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Hash)]
    pub enum SupportLang {
      $($variant,)*
    }

    impl SupportLang {
      /// The languages COMPILED INTO this build (feature-gated). Every capability surface —
      /// extension routing, digests, specs, canaries — iterates this set; a subset build
      /// simply has a shorter list and never touches a disabled grammar's stub.
      pub const fn all_langs() -> &'static [SupportLang] {
        &[
          $(
            #[cfg(feature = $feature)]
            SupportLang::$variant,
          )*
        ]
      }

      /// EVERY variant, enabled or not — the vocabulary surface. Serde/config parsing accepts
      /// all of these (a full-language rule file must parse on a subset build; disabled groups
      /// are dropped before compilation), while `FromStr` — the "please use this language now"
      /// path — rejects disabled ones with a build-shape error.
      pub const fn all_variants() -> &'static [SupportLang] {
        &[ $(SupportLang::$variant,)* ]
      }

      /// Whether this variant's grammar is compiled into the current build.
      pub const fn is_enabled(self) -> bool {
        match self {
          $(SupportLang::$variant => cfg!(feature = $feature),)*
        }
      }
    }

    $( impl_alias!($variant => &[$($alias),+]); )*

    const fn alias(lang: SupportLang) -> &'static [&'static str] {
      match lang {
        $(SupportLang::$variant => $variant::ALIAS,)*
      }
    }

    /// File extensions per language (adapted from ripgrep's default types).
    fn extensions(lang: SupportLang) -> &'static [&'static str] {
      match lang {
        $(SupportLang::$variant => &[$($ext),+],)*
      }
    }

    /// Exact file names that route to a language regardless of extension (`Dockerfile`,
    /// `Makefile`, `CMakeLists.txt`) — empty for most languages.
    fn filenames(lang: SupportLang) -> &'static [&'static str] {
      match lang {
        $(SupportLang::$variant => &[$($($fname),+)?],)*
      }
    }

    macro_rules! execute_lang_method {
      ($d me: path, $d method: ident, $d($d pname:tt),*) => {
        match $d me {
          $(SupportLang::$variant => $variant.$d method($d($d pname,)*),)*
        }
      };
    }
  };
}

/// Struct emission per `kind`: the standard shapes delegate to the existing helper macros;
/// `custom` emits nothing (the type is hand-written elsewhere in this crate).
macro_rules! lang_struct {
  ($v:ident, $p:ident, plain) => {
    impl_lang!($v, $p);
  };
  ($v:ident, $p:ident, custom) => {};
  ($v:ident, $p:ident, expando($c:literal)) => {
    impl_lang_expando!($v, $p, $c);
  };
}

langs! { $
  Bash { parser: language_bash(tree_sitter_bash), feature: "tree-sitter-bash", kind: plain, aliases: ["bash"], extensions: ["bash", "bats", "cgi", "command", "env", "fcgi", "ksh", "sh", "tmux", "tool", "zsh"], }
  // https://en.cppreference.com/w/cpp/language/identifiers
  C { parser: language_c(tree_sitter_c), feature: "tree-sitter-c", kind: expando('𐀀'), aliases: ["c"], extensions: ["c", "h"], }
  // CMake: `$` only appears in ${var} references, never in raw identifiers/arguments.
  CMake { parser: language_cmake(tree_sitter_cmake), feature: "tree-sitter-cmake", kind: expando('µ'), aliases: ["cmake"], extensions: ["cmake"], filenames: ["CMakeLists.txt"], }
  // Erlang atoms/vars are ASCII+_; `$` prefixes char literals.
  Erlang { parser: language_erlang(tree_sitter_erlang), feature: "tree-sitter-erlang", kind: expando('_'), aliases: ["erlang", "erl"], extensions: ["erl", "hrl"], }
  Cpp { parser: language_cpp(tree_sitter_cpp), feature: "tree-sitter-cpp", kind: expando('𐀀'), aliases: ["cc", "c++", "cpp", "cxx"], extensions: ["cc", "hpp", "cpp", "c++", "hh", "cxx", "cu", "ino"], }
  // https://docs.microsoft.com/en-us/dotnet/csharp/language-reference/language-specification/lexical-structure#643-identifiers
  // all letter number is accepted: https://www.compart.com/en/unicode/category/Nl
  CSharp { parser: language_c_sharp(tree_sitter_c_sharp), feature: "tree-sitter-c-sharp", kind: expando('µ'), aliases: ["cs", "csharp"], extensions: ["cs"], }
  // https://www.w3.org/TR/CSS21/grammar.html#scanner
  Css { parser: language_css(tree_sitter_css), feature: "tree-sitter-css", kind: expando('_'), aliases: ["css"], extensions: ["css", "scss"], }
  Dart { parser: language_dart(tree_sitter_dart), feature: "tree-sitter-dart", kind: plain, aliases: ["dart"], extensions: ["dart"], }
  // Dockerfile words interpolate $VAR, so `$` cannot survive in patterns.
  Dockerfile { parser: language_dockerfile(tree_sitter_dockerfile), feature: "tree-sitter-dockerfile", kind: expando('µ'), aliases: ["dockerfile", "docker"], extensions: ["dockerfile"], filenames: ["Dockerfile", "Containerfile"], }
  // https://github.com/elixir-lang/tree-sitter-elixir/blob/a2861e88a730287a60c11ea9299c033c7d076e30/grammar.js#L245
  Elixir { parser: language_elixir(tree_sitter_elixir), feature: "tree-sitter-elixir", kind: expando('µ'), aliases: ["ex", "elixir"], extensions: ["ex", "exs"], }
  // any Unicode code point categorized as "Letter": https://go.dev/ref/spec#letter
  Go { parser: language_go(tree_sitter_go), feature: "tree-sitter-go", kind: expando('µ'), aliases: ["go", "golang"], extensions: ["go"], }
  // GraphQL names are /[_A-Za-z][_0-9A-Za-z]*/ — `_` is the only safe expando.
  GraphQL { parser: language_graphql(tree_sitter_graphql), feature: "tree-sitter-graphql", kind: expando('_'), aliases: ["graphql", "gql"], extensions: ["graphql", "gql", "graphqls"], }
  // GHC supports Unicode syntax (https://ghc.gitlab.haskell.org/ghc/doc/users_guide/exts/unicode_syntax.html)
  // and the tree-sitter-haskell grammar parses it too.
  Haskell { parser: language_haskell(tree_sitter_haskell), feature: "tree-sitter-haskell", kind: expando('µ'), aliases: ["hs", "haskell"], extensions: ["hs"], }
  // https://developer.hashicorp.com/terraform/language/syntax/configuration#identifiers
  Hcl { parser: language_hcl(tree_sitter_hcl), feature: "tree-sitter-hcl", kind: expando('µ'), aliases: ["hcl"], extensions: ["hcl", "nomad", "tf", "tfvars", "workflow"], }
  // Hand-written type: carries the injection machinery (crates/language/src/html.rs).
  Html { parser: language_html(tree_sitter_html), feature: "tree-sitter-html", kind: custom, aliases: ["html"], extensions: ["html", "htm", "xhtml"], }
  // INI keys are conventionally [A-Za-z0-9._-]; `$` does not lex as part of a key.
  Ini { parser: language_ini(tree_sitter_ini), feature: "tree-sitter-ini", kind: expando('_'), aliases: ["ini", "properties"], extensions: ["ini", "cfg", "properties"], filenames: [".editorconfig"], }
  Java { parser: language_java(tree_sitter_java), feature: "tree-sitter-java", kind: plain, aliases: ["java"], extensions: ["java"], }
  JavaScript { parser: language_javascript(tree_sitter_javascript), feature: "tree-sitter-javascript", kind: plain, aliases: ["javascript", "js", "jsx"], extensions: ["cjs", "js", "mjs", "jsx"], }
  // Julia identifiers admit unicode letters (µ), never `$` (interpolation sigil).
  Julia { parser: language_julia(tree_sitter_julia), feature: "tree-sitter-julia", kind: expando('µ'), aliases: ["julia", "jl"], extensions: ["jl"], }
  // JSDoc: an injection-target grammar (JS comment blocks via languageInjections config).
  JsDoc { parser: language_jsdoc(tree_sitter_jsdoc), feature: "tree-sitter-jsdoc", kind: plain, aliases: ["jsdoc"], extensions: ["jsdoc"], }
  Json { parser: language_json(tree_sitter_json), feature: "tree-sitter-json", kind: plain, aliases: ["json"], extensions: ["json"], }
  // https://github.com/fwcd/tree-sitter-kotlin/pull/93
  Kotlin { parser: language_kotlin(tree_sitter_kotlin), feature: "tree-sitter-kotlin", kind: expando('µ'), aliases: ["kotlin", "kt"], extensions: ["kt", "ktm", "kts"], }
  Lua { parser: language_lua(tree_sitter_lua), feature: "tree-sitter-lua", kind: plain, aliases: ["lua"], extensions: ["lua"], }
  // Make: `$` is THE macro character; it can never appear raw in a target/variable name.
  Make { parser: language_make(tree_sitter_make), feature: "tree-sitter-make", kind: expando('µ'), aliases: ["make", "makefile", "gnumake"], extensions: ["mk", "mak", "make"], filenames: ["Makefile", "makefile", "GNUmakefile"], }
  Markdown { parser: language_markdown(tree_sitter_md), feature: "tree-sitter-md", kind: plain, aliases: ["markdown", "md"], extensions: ["markdown", "md"], }
  // Astro host: frontmatter is TypeScript; script/style blocks inject (hand-written type).
  Astro { parser: language_astro(tree_sitter_astro), feature: "tree-sitter-astro", kind: custom, aliases: ["astro"], extensions: ["astro"], }
  // Objective-C rides the C identifier rules.
  ObjectiveC { parser: language_objc(tree_sitter_objc), feature: "tree-sitter-objc", kind: expando('𐀀'), aliases: ["objc", "objective-c", "objectivec"], extensions: ["m"], }
  // OCaml identifiers are [A-Za-z0-9_'].
  OCaml { parser: language_ocaml(tree_sitter_ocaml, LANGUAGE_OCAML), feature: "tree-sitter-ocaml", kind: expando('_'), aliases: ["ocaml", "ml"], extensions: ["ml"], }
  // Nix uses $ for string interpolation, e.g. "${pkgs.hello}"
  Nix { parser: language_nix(tree_sitter_nix), feature: "tree-sitter-nix", kind: expando('_'), aliases: ["nix"], extensions: ["nix"], }
  // PHP accepts unicode in some names (not variable names, though)
  Php { parser: language_php(tree_sitter_php, LANGUAGE_PHP_ONLY), feature: "tree-sitter-php", kind: expando('µ'), aliases: ["php"], extensions: ["php"], }
  // Protobuf identifiers are [A-Za-z0-9_].
  Proto { parser: language_proto(tree_sitter_proto), feature: "tree-sitter-proto", kind: expando('_'), aliases: ["proto", "protobuf"], extensions: ["proto"], }
  // any char in [:XID_Start:]: https://docs.python.org/3/reference/lexical_analysis.html#identifiers
  // see also PEP 3131 (https://peps.python.org/pep-3131/)
  // Perl sigils make `$` structural; identifiers are [A-Za-z0-9_].
  Perl { parser: language_perl(tree_sitter_perl), feature: "tree-sitter-perl", kind: expando('_'), aliases: ["perl", "pl"], extensions: ["pl", "pm", "t"], }
  // PowerShell variables are `$x`; function/command names admit unicode letters.
  PowerShell { parser: language_powershell(tree_sitter_powershell), feature: "tree-sitter-powershell", kind: expando('µ'), aliases: ["powershell", "pwsh", "ps1"], extensions: ["ps1", "psm1", "psd1"], }
  Python { parser: language_python(tree_sitter_python), feature: "tree-sitter-python", kind: expando('µ'), aliases: ["py", "python"], extensions: ["py", "py3", "pyi", "bzl", "bazel"], }
  // https://github.com/tree-sitter/tree-sitter-ruby/blob/f257f3f57833d584050336921773738a3fd8ca22/grammar.js#L30C26-L30C78
  // R identifiers are letters, digits, `.` and `_`.
  R { parser: language_r(tree_sitter_r), feature: "tree-sitter-r", kind: expando('_'), aliases: ["r"], extensions: ["r", "R"], }
  Ruby { parser: language_ruby(tree_sitter_ruby), feature: "tree-sitter-ruby", kind: expando('µ'), aliases: ["rb", "ruby"], extensions: ["rb", "rbw", "gemspec"], }
  // any char in [:XID_Start:]: https://doc.rust-lang.org/reference/identifiers.html
  Rust { parser: language_rust(tree_sitter_rust), feature: "tree-sitter-rust", kind: expando('µ'), aliases: ["rs", "rust"], extensions: ["rs"], }
  Scala { parser: language_scala(tree_sitter_scala), feature: "tree-sitter-scala", kind: plain, aliases: ["scala"], extensions: ["scala", "sc", "sbt"], }
  // Svelte host: script/style blocks inject (hand-written type).
  Svelte { parser: language_svelte(tree_sitter_svelte), feature: "tree-sitter-svelte", kind: custom, aliases: ["svelte"], extensions: ["svelte"], }
  // SQL identifiers: `$` only appears in dialect-specific dollar quoting.
  Sql { parser: language_sql(tree_sitter_sequel), feature: "tree-sitter-sequel", kind: expando('_'), aliases: ["sql"], extensions: ["sql"], }
  Solidity { parser: language_solidity(tree_sitter_solidity), feature: "tree-sitter-solidity", kind: plain, aliases: ["sol", "solidity"], extensions: ["sol"], }
  // https://docs.swift.org/swift-book/documentation/the-swift-programming-language/lexicalstructure/#Identifiers
  Swift { parser: language_swift(tree_sitter_swift), feature: "tree-sitter-swift", kind: expando('µ'), aliases: ["swift"], extensions: ["swift"], }
  // TOML bare keys are [A-Za-z0-9_-] — `$` is not valid, so patterns need an expando char.
  Toml { parser: language_toml(tree_sitter_toml), feature: "tree-sitter-toml", kind: expando('_'), aliases: ["toml"], extensions: ["toml"], }
  Tsx { parser: language_tsx(tree_sitter_typescript, LANGUAGE_TSX), feature: "tree-sitter-typescript", kind: plain, aliases: ["tsx"], extensions: ["tsx"], }
  TypeScript { parser: language_typescript(tree_sitter_typescript, LANGUAGE_TYPESCRIPT), feature: "tree-sitter-typescript", kind: plain, aliases: ["ts", "typescript"], extensions: ["ts", "cts", "mts"], }
  // XML Names exclude `$` (and µ — NameStartChar begins at U+00C0); `_` is valid.
  Xml { parser: language_xml(tree_sitter_xml, LANGUAGE_XML), feature: "tree-sitter-xml", kind: expando('_'), aliases: ["xml"], extensions: ["xml", "xsd", "xsl", "xslt", "svg", "rss", "atom", "plist", "xaml", "csproj", "props", "targets"], }
  // Zig identifiers are [A-Za-z0-9_]; `$` never lexes.
  Zig { parser: language_zig(tree_sitter_zig), feature: "tree-sitter-zig", kind: expando('_'), aliases: ["zig"], extensions: ["zig"], }
  // Vue SFC host: script/style blocks inject (hand-written type).
  Vue { parser: language_vue(tree_sitter_vue), feature: "tree-sitter-vue", kind: custom, aliases: ["vue"], extensions: ["vue"], }
  Yaml { parser: language_yaml(tree_sitter_yaml), feature: "tree-sitter-yaml", kind: plain, aliases: ["yaml", "yml"], extensions: ["yaml", "yml"], }
}

impl SupportLang {
  /// Vocabulary lookup over EVERY variant (aliases included), no enablement gate — the
  /// serde/config path.
  pub fn from_name_any(s: &str) -> Option<SupportLang> {
    Self::all_variants()
      .iter()
      .copied()
      .find(|&lang| alias(lang).iter().any(|moniker| s.eq_ignore_ascii_case(moniker)))
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
  /// The name is a known language, but its grammar is not compiled into this build.
  LanguageDisabled(String),
}

impl Display for SupportLangErr {
  fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), std::fmt::Error> {
    use SupportLangErr::*;
    match self {
      LanguageNotSupported(lang) => write!(f, "{lang} is not supported!"),
      LanguageDisabled(lang) => write!(
        f,
        "{lang} is not compiled into this build (slim feature set) — use a full build"
      ),
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
    // Vocabulary, not capability: config/rule files must PARSE with every known language
    // name even on slim builds (disabled groups are dropped before compilation); the
    // FromStr path is where enablement gates.
    SupportLang::from_name_any(v)
      .ok_or_else(|| de::Error::custom(SupportLangErr::LanguageNotSupported(v.to_string())))
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

/// Implements the language names and aliases. Known-but-disabled languages get the
/// build-shape error, not "not supported" — the user's spelling was right; the binary is
/// slim.
impl FromStr for SupportLang {
  type Err = SupportLangErr;
  fn from_str(s: &str) -> Result<Self, Self::Err> {
    match Self::from_name_any(s) {
      Some(lang) if lang.is_enabled() => Ok(lang),
      Some(lang) => Err(SupportLangErr::LanguageDisabled(format!("{lang:?}"))),
      None => Err(SupportLangErr::LanguageNotSupported(s.to_string())),
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
      SupportLang::Astro => Astro.extract_injections(root),
      SupportLang::Html => Html.extract_injections(root),
      SupportLang::Svelte => Svelte.extract_injections(root),
      SupportLang::Vue => Vue.extract_injections(root),
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

/// A single digest over **every builtin** grammar, in `all_langs` order. NOTE: since F-M2 the
/// stamp index manifests record is `vorpal-lang-registry::global_grammar_stamp`, which folds
/// registered dynamic languages too (sorted, v2 formula); this builtin-only fold remains as a
/// component identity for callers that deliberately want the compiled-in set alone.
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

/// Guess which programming language a file is written in
/// Adapt from `<https://github.com/Wilfred/difftastic/blob/master/src/parse/guess_language.rs>`
/// N.B do not confuse it with `FromStr` trait. This function is to guess language from file extension.
fn from_extension(path: &Path) -> Option<SupportLang> {
  // Exact-filename routing first: Dockerfile/Makefile/CMakeLists.txt have no extension (or a
  // meaningless one), so their languages declare filenames in the langs! table.
  if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
    if let Some(lang) = SupportLang::all_langs()
      .iter()
      .copied()
      .find(|&l| filenames(l).iter().any(|f| name.eq_ignore_ascii_case(f)))
    {
      return Some(lang);
    }
  }
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
  for fname in filenames(lang) {
    builder
      .add(&lang_name, fname)
      .expect("file name glob must compile");
    builder.select(&lang_name);
  }
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
