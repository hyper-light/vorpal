//! Run every vendored grammar's **upstream corpus** (IMPROVEMENTS #10): the test suites the
//! grammar authors ship, imported at each grammar's pinned commit (see PROVENANCE.json), and
//! executed against the parsers we actually compile. This is the language-specific test
//! evidence the supply-chain item requires — external scanners, error recovery, and extras
//! are exercised exactly as upstream exercises them.
//!
//! Corpus format: `===` header with a test name and optional `:attribute` lines, source
//! text, a `---` divider, then the expected S-expression. Attribute semantics honored:
//! `:skip` and `:platform(...)` skip (platform-conditional expectations are upstream's
//! business); `:error` asserts the parse HAS errors instead of comparing trees;
//! `:language(x)` selects the dialect (typescript/tsx) and skips dialects we don't map.
//! The divider is the LAST dash-only line before the expected tree, because YAML/Markdown
//! sources legitimately contain `---` lines of their own.
//!
//! Gate: **zero failures** across every corpus, with a small explicit allowlist for tests
//! whose expectations upstream ties to flags we don't enable; every allowlist entry names
//! its reason and fails the suite if it starts passing (stale allowlists rot).

use std::fs;
use std::path::{Path, PathBuf};

use vorpal_language::{LanguageExt, SupportLang};

struct CorpusTest {
  name: String,
  attributes: Vec<String>,
  source: String,
  expected: String,
}

/// Split one corpus file into tests. Header: a `=`-only line, name/attribute lines, a
/// `=`-only closing line. Body: source, then the LAST `-`-only line divides source from the
/// expected sexp.
fn parse_corpus(text: &str) -> Vec<CorpusTest> {
  let lines: Vec<&str> = text.lines().collect();
  let is_eq = |line: &str| line.len() >= 3 && line.chars().all(|c| c == '=');
  let is_dash = |line: &str| line.len() >= 3 && line.chars().all(|c| c == '-');

  // Header spans: (open =, close =) with the name between. The close must arrive within a
  // few lines (names and attributes are short) — that distance guard keeps source text
  // containing `=`-only lines (Elixir's `===` operator) from reading as phantom headers:
  // a phantom would need a second `=`-run within eight lines, and its "close" is really
  // the next test's opener, far away.
  let mut headers: Vec<(usize, usize)> = Vec::new();
  let mut i = 0;
  while i < lines.len() {
    if is_eq(lines[i]) {
      let mut j = i + 1;
      while j < lines.len() && j <= i + 8 && !is_eq(lines[j]) {
        j += 1;
      }
      if j < lines.len() && j <= i + 8 && is_eq(lines[j]) && j > i + 1 {
        headers.push((i, j));
        i = j + 1;
        continue;
      }
    }
    i += 1;
  }

  let mut tests = Vec::new();
  for (index, &(open, close)) in headers.iter().enumerate() {
    let body_end = headers.get(index + 1).map(|&(next, _)| next).unwrap_or(lines.len());
    let header_lines = &lines[open + 1..close];
    let name = header_lines
      .iter()
      .filter(|l| !l.trim_start().starts_with(':'))
      .map(|l| l.trim())
      .collect::<Vec<_>>()
      .join(" ")
      .trim()
      .to_string();
    let attributes: Vec<String> = header_lines
      .iter()
      .map(|l| l.trim())
      .filter(|l| l.starts_with(':'))
      .map(str::to_string)
      .collect();
    let body = &lines[close + 1..body_end];
    let Some(divider) = body.iter().rposition(|l| is_dash(l)) else {
      continue;
    };
    tests.push(CorpusTest {
      name,
      attributes,
      source: body[..divider].join("\n"),
      expected: body[divider + 1..].join("\n"),
    });
  }
  tests
}

/// A parsed S-expression node: optional field label, kind, children. Atom children (the
/// `identifier` in `(MISSING identifier)`, quoted tokens in `(UNEXPECTED 'x')`) are leaf
/// nodes with no children.
#[derive(Debug, PartialEq, Eq)]
struct Sexp {
  field: Option<String>,
  kind: String,
  children: Vec<Sexp>,
}

/// Parse one or more sibling sexps from `text` (expected trees are a single root; parsing
/// stays general). Tokens: parens, `field:` labels, atoms.
fn parse_sexp(text: &str) -> Option<Sexp> {
  let mut tokens = Vec::new();
  let mut current = String::new();
  for c in text.chars() {
    match c {
      '(' | ')' => {
        if !current.trim().is_empty() {
          tokens.push(current.trim().to_string());
        }
        current.clear();
        tokens.push(c.to_string());
      }
      c if c.is_whitespace() => {
        if !current.trim().is_empty() {
          tokens.push(current.trim().to_string());
        }
        current.clear();
      }
      c => current.push(c),
    }
  }
  if !current.trim().is_empty() {
    tokens.push(current.trim().to_string());
  }
  let mut position = 0usize;
  let root = parse_node(&tokens, &mut position, None)?;
  Some(root)
}

fn parse_node(tokens: &[String], position: &mut usize, field: Option<String>) -> Option<Sexp> {
  if tokens.get(*position)? != "(" {
    return None;
  }
  *position += 1;
  let kind = tokens.get(*position)?.clone();
  *position += 1;
  let mut children = Vec::new();
  let mut pending_field: Option<String> = None;
  while let Some(token) = tokens.get(*position) {
    match token.as_str() {
      ")" => {
        *position += 1;
        return Some(Sexp {
          field,
          kind,
          children,
        });
      }
      "(" => {
        let child = parse_node(tokens, position, pending_field.take())?;
        children.push(child);
      }
      atom if atom.ends_with(':') => {
        pending_field = Some(atom.trim_end_matches(':').to_string());
        *position += 1;
      }
      atom => {
        children.push(Sexp {
          field: pending_field.take(),
          kind: atom.to_string(),
          children: Vec::new(),
        });
        *position += 1;
      }
    }
  }
  None
}

/// The `tree-sitter test` comparison contract: kinds and structure must match exactly, and a
/// field label in the EXPECTED tree must match the actual child's field — but an expected
/// child written without a field accepts any field on the actual child (corpus authors may
/// omit fields; `to_sexp` always emits them).
fn sexp_matches(expected: &Sexp, actual: &Sexp) -> bool {
  if expected.kind != actual.kind {
    return false;
  }
  if let Some(field) = &expected.field {
    if actual.field.as_ref() != Some(field) {
      return false;
    }
  }
  expected.children.len() == actual.children.len()
    && expected
      .children
      .iter()
      .zip(&actual.children)
      .all(|(e, a)| sexp_matches(e, a))
}

fn corpus_roots() -> Vec<(PathBuf, SupportLang)> {
  let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../grammars");
  use SupportLang::*;
  let map: &[(&str, SupportLang)] = &[
    ("tree-sitter-bash/test/corpus", Bash),
    ("tree-sitter-c/test/corpus", C),
    ("tree-sitter-c-sharp/test/corpus", CSharp),
    ("tree-sitter-cpp/test/corpus", Cpp),
    ("tree-sitter-css/test/corpus", Css),
    ("tree-sitter-dart/test/corpus", Dart),
    ("tree-sitter-elixir/test/corpus", Elixir),
    ("tree-sitter-go/test/corpus", Go),
    ("tree-sitter-haskell/test/corpus", Haskell),
    ("tree-sitter-hcl/test/corpus", Hcl),
    ("tree-sitter-html/test/corpus", Html),
    ("tree-sitter-java/test/corpus", Java),
    ("tree-sitter-javascript/test/corpus", JavaScript),
    ("tree-sitter-json/test/corpus", Json),
    ("tree-sitter-kotlin-sg/test/corpus", Kotlin),
    ("tree-sitter-lua/test/corpus", Lua),
    ("tree-sitter-md/tree-sitter-markdown/test/corpus", Markdown),
    // tree-sitter-markdown-inline: no standalone surface here — the block grammar is what
    // SupportLang::Markdown compiles; the inline corpus stays vendored for provenance.
    ("tree-sitter-nix/corpus", Nix),
    ("tree-sitter-php/test/corpus", Php),
    ("tree-sitter-python/test/corpus", Python),
    ("tree-sitter-ruby/test/corpus", Ruby),
    ("tree-sitter-rust/test/corpus", Rust),
    ("tree-sitter-scala/test/corpus", Scala),
    ("tree-sitter-solidity/test/corpus", Solidity),
    ("tree-sitter-swift/test/corpus", Swift),
    ("tree-sitter-typescript/test/corpus", TypeScript),
    ("tree-sitter-yaml/test/corpus", Yaml),
  ];
  map
    .iter()
    .map(|(rel, lang)| (root.join(rel), *lang))
    .collect()
}

/// Dialect selection for `:language(...)` attributes in multi-grammar repos.
fn dialect_of(attr: &str) -> Option<SupportLang> {
  let inner = attr.strip_prefix(":language(")?.strip_suffix(')')?;
  match inner.trim() {
    "typescript" => Some(SupportLang::TypeScript),
    "tsx" => Some(SupportLang::Tsx),
    "javascript" => Some(SupportLang::JavaScript),
    "php" => Some(SupportLang::Php),
    "markdown" => Some(SupportLang::Markdown),
    _ => None,
  }
}

/// Corpus files excluded wholesale: (language, file basename, reason). These test a dialect
/// vorpal deliberately does not compile — not a mismatch, a shipping decision.
const EXCLUDED_FILES: &[(SupportLang, &str, &str)] = &[(
  SupportLang::Php,
  "interpolation.txt",
  "exercises the full-HTML php dialect; vorpal compiles LANGUAGE_PHP_ONLY (upstream ast-grep choice)",
)];

/// Known-mismatch allowlist: (corpus file basename, test name) → reason. Every entry must
/// KEEP failing; an entry that starts passing fails the suite until removed.
const ALLOWLIST: &[(&str, &str, &str)] = &[(
  "varsym.txt",
  "varsym: error: carrow",
  "explicit ERROR-tree expectation: error-recovery shape depends on the tree-sitter runtime \
   version, and ours differs from the CLI that generated upstream's expectation",
)];

#[test]
fn upstream_corpora_pass_against_the_compiled_parsers() {
  let mut total = 0usize;
  let mut passed = 0usize;
  let mut skipped = 0usize;
  let mut failures: Vec<String> = Vec::new();
  let mut allowlisted_seen: Vec<(String, String)> = Vec::new();

  for (dir, default_lang) in corpus_roots() {
    assert!(dir.is_dir(), "missing corpus dir {} — reimport upstream corpora", dir.display());
    let mut files: Vec<PathBuf> = walk_txt(&dir);
    files.sort();
    for file in files {
      let text = fs::read_to_string(&file).unwrap();
      let basename = file.file_name().unwrap().to_string_lossy().to_string();
      if let Some((_, _, _reason)) = EXCLUDED_FILES
        .iter()
        .find(|(lang, name, _)| *lang == default_lang && *name == basename)
      {
        skipped += parse_corpus(&text).len();
        continue;
      }
      for test in parse_corpus(&text) {
        total += 1;
        if test.attributes.iter().any(|a| a == ":skip" || a.starts_with(":platform(")) {
          skipped += 1;
          continue;
        }
        let mut lang = default_lang;
        if let Some(language_attr) = test.attributes.iter().find(|a| a.starts_with(":language(")) {
          match dialect_of(language_attr) {
            Some(dialect) => lang = dialect,
            None => {
              skipped += 1;
              continue;
            }
          }
        }
        let mut parser = tree_sitter::Parser::new();
        parser
          .set_language(&lang.get_ts_language())
          .unwrap_or_else(|err| panic!("{lang:?}: set_language: {err}"));
        let Some(tree) = parser.parse(&test.source, None) else {
          failures.push(format!("[{basename}] {}: parser returned no tree", test.name));
          continue;
        };
        let actual = tree.root_node().to_sexp();
        let ok = if test.attributes.iter().any(|a| a == ":error") {
          tree.root_node().has_error()
        } else {
          match (parse_sexp(&test.expected), parse_sexp(&actual)) {
            (Some(expected), Some(actual)) => sexp_matches(&expected, &actual),
            _ => false,
          }
        };
        let allowlisted = ALLOWLIST
          .iter()
          .any(|(f, n, _)| *f == basename && *n == test.name);
        match (ok, allowlisted) {
          (true, false) => passed += 1,
          (true, true) => allowlisted_seen.push((basename.clone(), test.name.clone())),
          (false, true) => skipped += 1, // known mismatch, documented in ALLOWLIST
          (false, false) => failures.push(format!(
            "[{basename}] {} ({lang:?}): tree mismatch",
            test.name
          )),
        }
      }
    }
  }

  println!("corpus: {passed} passed, {skipped} skipped, {} failed, {total} total", failures.len());
  assert!(
    allowlisted_seen.is_empty(),
    "allowlist entries now PASS — remove them:\n{allowlisted_seen:#?}"
  );
  assert!(
    failures.is_empty(),
    "{} corpus failures:\n{}",
    failures.len(),
    failures.join("\n")
  );
}

fn walk_txt(dir: &Path) -> Vec<PathBuf> {
  let mut out = Vec::new();
  let mut stack = vec![dir.to_path_buf()];
  while let Some(current) = stack.pop() {
    for entry in fs::read_dir(&current).into_iter().flatten().flatten() {
      let path = entry.path();
      if path.is_dir() {
        stack.push(path);
      } else if path.extension().is_some_and(|e| e == "txt") {
        out.push(path);
      }
    }
  }
  out
}
