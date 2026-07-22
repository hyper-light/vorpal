mod args;
mod debug_query;
mod error_context;
mod inspect;
mod print_diff;
mod rule_overwrite;
mod worker;

pub use args::{ContextArgs, IgnoreFile, InputArgs, NoIgnore, OutputArgs, OverwriteArgs, WalkIgnore};
pub use debug_query::DebugFormat;
pub use error_context::{ErrorContext, exit_with_error};
pub use inspect::{FileTrace, Granularity, RuleTrace, RunTrace, ScanTrace};
pub use print_diff::DiffStyles;
pub use rule_overwrite::RuleOverwrite;
pub use worker::{
  ChannelCap, ItemSink, Items, MaxItemCounter, PathWorker, Produce, StdInWorker, Worker,
  run_producer,
};
pub(crate) use worker::filter_result;

use crate::lang::SgLang;

use anyhow::{Context, Result, anyhow};
use crossterm::{
  cursor::MoveTo,
  event::{self, Event, KeyCode, KeyModifiers},
  execute,
  terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
  terminal::{Clear, ClearType},
};
use smallvec::{SmallVec, smallvec};

use vorpal_config::{Rule, RuleCollection};
use vorpal_core::{Matcher, tree_sitter::StrDoc};
use vorpal_language::{Language, LanguageExt};

use std::fmt;
use std::fs::read_to_string;
use std::io::Write;
use std::io::stdout;
use std::path::{Path, PathBuf};
use std::str::FromStr;

pub(crate) type Vorpal = vorpal_core::Vorpal<StrDoc<SgLang>>;

fn read_char() -> Result<char> {
  loop {
    if let Event::Key(evt) = event::read()? {
      match evt.code {
        KeyCode::Tab => break Ok('\t'),
        KeyCode::Enter => break Ok('\n'),
        KeyCode::Char('c') if evt.modifiers.contains(KeyModifiers::CONTROL) => break Ok('q'),
        KeyCode::Char(c) => break Ok(c),
        _ => (),
      }
    }
  }
}

/// Prompts for user input on STDOUT
fn prompt_reply_stdout(prompt: &str) -> Result<char> {
  let mut stdout = std::io::stdout();
  write!(stdout, "{prompt}")?;
  stdout.flush()?;
  terminal::enable_raw_mode()?;
  let ret = read_char();
  terminal::disable_raw_mode()?;
  ret
}

// clear screen
pub fn clear() -> Result<()> {
  execute!(stdout(), Clear(ClearType::All), MoveTo(0, 0))?;
  Ok(())
  // https://github.com/console-rs/console/blob/be1c2879536c90ffc2b54938b5964084f5fef67d/src/common_term.rs#L56
  // print!("\r\x1b[2J\r\x1b[H");
}

pub fn run_in_alternate_screen<T>(f: impl FnOnce() -> Result<T>) -> Result<T> {
  execute!(stdout(), EnterAlternateScreen)?;
  clear()?;
  let ret = f();
  // it is possible f panics and leaves the terminal in alternate screen mode
  // this may not be worth handling, see #1499
  execute!(stdout(), LeaveAlternateScreen)?;
  ret
}

pub fn prompt(prompt_text: &str, letters: &str, default: Option<char>) -> Result<char> {
  loop {
    let input = prompt_reply_stdout(prompt_text)?;
    if let Some(default) = default
      && input == '\n'
    {
      return Ok(default);
    }
    if letters.contains(input) {
      return Ok(input);
    }
    eprintln!("Unrecognized command, try again?")
  }
}

#[derive(Debug)]
pub(crate) struct EmptyFile;

impl fmt::Display for EmptyFile {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str("File is empty")
  }
}

impl std::error::Error for EmptyFile {}

/// Where file content comes from: the local filesystem (today's path) or memory (content that
/// arrived over the wire in remote streaming mode, §3.3 of `docs/REMOTE.md`). `read_file` is the
/// single byte-acquisition chokepoint of the match pipeline; everything downstream of it is
/// filesystem-agnostic.
///
/// `Memory` owns its `String` so it is consumed straight into the parser (`lang.grep`) with no
/// re-copy — a stream-mode scan of a billion-LOC corpus must not memcpy every file twice.
pub enum Source {
  Fs,
  Memory(String),
}

pub(crate) fn read_file(path: &Path) -> Result<String> {
  let file_content =
    read_to_string(path).with_context(|| format!("Cannot read file {}", path.to_string_lossy()))?;
  validate_content(file_content)
}

/// Apply the exact size/emptiness gates `read_file` applies, so in-memory content is accepted or
/// rejected byte-for-byte like local content (discovery parity would otherwise silently drift).
fn validate_content(file_content: String) -> Result<String> {
  // skip large files or empty file
  if file_too_large(&file_content) {
    Err(anyhow!("File is too large"))
  } else if file_content.is_empty() {
    Err(EmptyFile.into())
  } else {
    Ok(file_content)
  }
}

fn read_source(path: &Path, source: Source) -> Result<String> {
  match source {
    Source::Fs => read_file(path),
    // Already owned — validate in place, no second allocation.
    Source::Memory(content) => validate_content(content),
  }
}

fn collect_file_stats(
  path: &Path,
  lang: SgLang,
  configs: &RuleCollection<SgLang>,
  rule_stats: &ScanTrace,
) -> Result<()> {
  let rules = configs.get_rule_from_lang(path, lang);
  rule_stats.print_file(path, lang, &rules)?;
  Ok(())
}

/// The in-source suppression marker (`vorpal-ignore` comments): a file containing it must
/// always be scanned — suppression bookkeeping (used/unused directives) is part of scan
/// output even when no rule matches the file.
const SUPPRESSION_MARKER: &str = "vorpal-ignore";

/// Per-language OR-of-rules prefilters for the scan path (§12): a file parses only when some
/// rule of one of its languages may still match by literal evidence. A rule without
/// extractable literals keeps its whole language always-parsing (vacuously matching
/// prefilter), so skipping stays a pure necessary condition; a language with no rules at all
/// can never produce a match and is skipped outright. Files carrying the suppression marker
/// are always scanned regardless, so `vorpal-ignore` accounting never misses a directive.
pub struct LangPrefilters {
  by_lang: std::collections::HashMap<SgLang, Vec<Prefilter>>,
  suppression: memchr::memmem::Finder<'static>,
}

impl LangPrefilters {
  pub fn build(configs: &RuleCollection<SgLang>) -> Self {
    let mut by_lang: std::collections::HashMap<SgLang, Vec<Prefilter>> =
      std::collections::HashMap::new();
    configs.for_each_rule(|rule| {
      by_lang
        .entry(rule.language)
        .or_default()
        .push(Prefilter::from_literals(rule.matcher.required_literals()));
    });
    Self {
      by_lang,
      suppression: memchr::memmem::Finder::new(SUPPRESSION_MARKER.as_bytes()).into_owned(),
    }
  }

  /// Whether any rule of `lang` may match `content` — vacuously true for literal-less rules.
  fn lang_may_match(&self, lang: SgLang, content: &str) -> bool {
    match self.by_lang.get(&lang) {
      Some(prefilters) => prefilters.iter().any(|p| p.may_match(content)),
      // No rules for this language: nothing can ever match it.
      None => false,
    }
  }

  /// Whether the file (root language + any injectable languages) can possibly affect scan
  /// output — by matching a rule, or by carrying suppression directives that must be tracked.
  pub fn may_match(&self, lang: SgLang, content: &str) -> bool {
    if self.suppression.find(content.as_bytes()).is_some() {
      return true;
    }
    if self.lang_may_match(lang, content) {
      return true;
    }
    lang
      .injectable_sg_langs()
      .into_iter()
      .flatten()
      .any(|injected| self.lang_may_match(injected, content))
  }
}

pub fn filter_file_rule(
  path: &Path,
  configs: &RuleCollection<SgLang>,
  trace: &ScanTrace,
  prefilters: Option<&LangPrefilters>,
) -> Result<SmallVec<[Vorpal; 1]>> {
  filter_source_rule(path, Source::Fs, configs, trace, prefilters)
}

/// `filter_file_rule` generalized over the byte source; `Source::Fs` is bit-identical to the
/// original, `Source::Memory` runs the same pipeline on remotely-streamed content.
pub fn filter_source_rule(
  path: &Path,
  source: Source,
  configs: &RuleCollection<SgLang>,
  trace: &ScanTrace,
  prefilters: Option<&LangPrefilters>,
) -> Result<SmallVec<[Vorpal; 1]>> {
  let Some(lang) = SgLang::from_path(path) else {
    return Ok(smallvec![]);
  };
  let file_content = read_source(path, source)?;
  // §12 pre-parse gate: skip files no rule can match by literal evidence. The file still gets
  // its per-file rule-count trace entry (`--inspect` parity with the pre-prefilter pipeline);
  // only the parse is skipped.
  if let Some(prefilters) = prefilters {
    if !prefilters.may_match(lang, &file_content) {
      collect_file_stats(path, lang, configs, trace)?;
      return Ok(smallvec![]);
    }
  }
  let grep = lang.grep(file_content);
  collect_file_stats(path, lang, configs, trace)?;
  let mut ret = smallvec![grep.clone()];
  if lang.injectable_languages().is_some() {
    let sub_roots = grep.get_injections(|s| SgLang::from_str(s).ok());
    for root in sub_roots {
      collect_file_stats(path, *root.lang(), configs, trace).ok();
      ret.push(root);
    }
  }
  Ok(ret)
}

/// Cap on prebuilt finders per matcher: the longest (most selective) literals win the slots.
const MAX_PREFILTER_LITERALS: usize = 8;

/// Pre-parse literal filter (§12): SIMD substring finders (memchr/memmem) over a matcher's
/// required literals, checked against the raw bytes with per-token AND semantics. A file
/// missing any required literal cannot match, so it is skipped before tree-sitter ever runs.
/// Purely a necessary condition — matching semantics are unchanged.
pub struct Prefilter {
  finders: Vec<memchr::memmem::Finder<'static>>,
}

impl Prefilter {
  pub fn for_rule(rule: &Rule) -> Self {
    Self::from_literals(rule.required_literals())
  }

  fn from_literals(mut literals: Vec<&str>) -> Self {
    literals.sort_unstable();
    literals.dedup();
    literals.sort_by_key(|l| std::cmp::Reverse(l.len()));
    let finders = literals
      .into_iter()
      .take(MAX_PREFILTER_LITERALS)
      .map(|l| memchr::memmem::Finder::new(l.as_bytes()).into_owned())
      .collect();
    Self { finders }
  }


  /// True when every required literal occurs in `content` (vacuously true with no literals).
  pub fn may_match(&self, content: &str) -> bool {
    self
      .finders
      .iter()
      .all(|finder| finder.find(content.as_bytes()).is_some())
  }
}

// sub_matchers are the injected languages
// e.g. js/css in html
#[cfg(test)]
pub fn filter_file_pattern<'a>(
  path: &Path,
  lang: SgLang,
  root_matcher: Option<&'a Rule>,
  sub_matchers: &'a [(SgLang, Rule)],
) -> Result<SmallVec<[MatchUnit<&'a Rule>; 1]>> {
  filter_source_pattern(path, Source::Fs, lang, root_matcher, sub_matchers)
}

/// `filter_file_pattern` generalized over the byte source (see [`Source`]). The pattern `run`
/// commands drive this directly through `filter_source_pattern`.
pub fn filter_source_pattern<'a>(
  path: &Path,
  source: Source,
  lang: SgLang,
  root_matcher: Option<&'a Rule>,
  sub_matchers: &'a [(SgLang, Rule)],
) -> Result<SmallVec<[MatchUnit<&'a Rule>; 1]>> {
  let file_content = read_source(path, source)?;
  // §12 fast path: consult required literals BEFORE parsing. Injected-language matchers are
  // checked against the whole file (injected regions are substrings of it — sound). A file no
  // candidate matcher can match never reaches tree-sitter.
  let root_filter = root_matcher.map(Prefilter::for_rule);
  let sub_filters: Vec<Prefilter> = sub_matchers
    .iter()
    .map(|(_, matcher)| Prefilter::for_rule(matcher))
    .collect();
  let root_viable = root_filter
    .as_ref()
    .is_some_and(|filter| filter.may_match(&file_content));
  let any_sub_viable = sub_filters
    .iter()
    .any(|filter| filter.may_match(&file_content));
  if !root_viable && !any_sub_viable {
    return Ok(smallvec![]);
  }
  let grep = lang.grep(&file_content);
  let mut ret = smallvec![];
  if root_viable {
    if let Some(matcher) = root_matcher {
      ret.push(MatchUnit {
        grep: grep.clone(),
        path: path.to_path_buf(),
        matcher,
      });
    }
  }
  let injections = grep.get_injections(|s| SgLang::from_str(s).ok());
  let sub_units = injections.into_iter().filter_map(|inner| {
    let index = sub_matchers.iter().position(|i| *inner.lang() == i.0)?;
    if !sub_filters[index].may_match(&file_content) {
      return None;
    }
    Some(MatchUnit {
      grep: inner,
      path: path.to_path_buf(),
      matcher: &sub_matchers[index].1,
    })
  });
  ret.extend(sub_units);
  Ok(ret)
}

const MAX_FILE_SIZE: usize = 3_000_000;
const MAX_LINE_COUNT: usize = 200_000;

// skip files that are too large in size AND have too many lines
fn file_too_large(file_content: &str) -> bool {
  // the && operator is intentional here to include more files
  file_content.len() > MAX_FILE_SIZE && file_content.lines().count() > MAX_LINE_COUNT
}

/// A single atomic unit where matches happen.
/// It contains the file path, sg instance and matcher.
/// An analogy to compilation unit in C programming language.
pub struct MatchUnit<M: Matcher> {
  pub path: PathBuf,
  pub grep: Vorpal,
  pub matcher: M,
}

#[cfg(test)]
mod test {
  use super::*;
  use vorpal_language::SupportLang;

  #[test]
  fn test_html_embedding() {
    let root =
      SgLang::Builtin(SupportLang::Html).grep("<script lang=typescript>alert(123)</script>");
    let docs = root.get_injections(|s| SgLang::from_str(s).ok());
    assert_eq!(docs.len(), 1);
    let script = docs[0].root().child(0).expect("should exist");
    assert_eq!(script.kind(), "expression_statement");
  }

  #[test]
  fn test_html_embedding_lang_not_found() {
    let root = SgLang::Builtin(SupportLang::Html).grep("<script lang=xxx>alert(123)</script>");
    let docs = root.get_injections(|s| SgLang::from_str(s).ok());
    assert_eq!(docs.len(), 0);
  }

  #[test]
  fn prefilter_ignores_constraint_literals_on_possibly_unbound_vars() {
    use vorpal_config::from_yaml_string;
    use vorpal_language::SupportLang;
    // `bar()` matches without binding $A, so the constraint's literal is NOT a necessary
    // condition; a prefilter that unions constraint literals would silently skip this file.
    let yaml = "id: t\nlanguage: Rust\nrule: {any: [{pattern: 'foo($A)'}, {pattern: 'bar()'}]}\nconstraints: {A: {regex: zzz_special}}";
    let configs = from_yaml_string(yaml, &Default::default()).expect("rule parses");
    let collection = RuleCollection::try_new(configs).expect("collection builds");
    let prefilters = LangPrefilters::build(&collection);
    let lang = SgLang::Builtin(SupportLang::Rust);
    assert!(
      prefilters.may_match(lang, "fn t() { bar(); }"),
      "constraint literals must not gate files where the constrained var can be unbound"
    );
  }

  #[test]
  fn test_prefilter_gates_on_required_literals() {
    use vorpal_core::Pattern;
    let lang = SgLang::Builtin(SupportLang::Rust);
    let filter = Prefilter::for_rule(&Rule::Pattern(Pattern::new("special_sentinel($A)", lang)));
    assert!(filter.may_match("fn x() { special_sentinel(1); }"));
    // Per-token AND is formatting-insensitive.
    assert!(filter.may_match("special_sentinel ( 1 )"));
    assert!(!filter.may_match("fn x() { other(1); }"));
    // A metavariable-only pattern requires nothing and gates nothing.
    let all_pass = Prefilter::for_rule(&Rule::Pattern(Pattern::new("$A", lang)));
    assert!(all_pass.may_match("anything at all"));
  }

  #[test]
  fn test_filter_file_pattern_skips_unmatchable_files() {
    use vorpal_core::Pattern;
    let dir = tempfile::tempdir().unwrap();
    let hit = dir.path().join("hit.rs");
    let miss = dir.path().join("miss.rs");
    std::fs::write(
      &hit,
      "fn special_sentinel() {}\nfn go() {\n    special_sentinel();\n}\n",
    )
    .unwrap();
    std::fs::write(&miss, "fn go() {\n    other();\n}\n").unwrap();
    let lang = SgLang::Builtin(SupportLang::Rust);
    let rule = Rule::Pattern(Pattern::new("special_sentinel()", lang));
    // The literal-bearing file parses and yields a unit; the other is skipped pre-parse.
    let units = filter_file_pattern(&hit, lang, Some(&rule), &[]).unwrap();
    assert_eq!(units.len(), 1);
    let units = filter_file_pattern(&miss, lang, Some(&rule), &[]).unwrap();
    assert!(units.is_empty());
  }
}
