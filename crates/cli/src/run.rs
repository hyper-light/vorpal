use std::path::Path;
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::{Args, Parser, ValueEnum, builder::PossibleValue};
use ignore::WalkParallel;
use vorpal_config::{Fixer, Rule, parse_selector};
use vorpal_core::{MatchStrictness, Matcher, Pattern};
use vorpal_language::{Language, LanguageExt};

use crate::config::ProjectConfig;
use crate::lang::SgLang;
use crate::print::{
  ColoredPrinter, Diff, FileNamePrinter, Heading, InteractivePrinter, JSONPrinter, PrintProcessor,
  Printer,
};
use crate::remote::CountedProduce;
use crate::utils::ErrorContext as EC;
use crate::utils::{ContextArgs, InputArgs, MatchUnit, OutputArgs, Source, filter_source_pattern};
use crate::utils::{DebugFormat, FileTrace, RunTrace};
use crate::utils::{Items, PathWorker, StdInWorker, Worker};

fn lang_help() -> String {
  format!(
    "The language of the pattern. Supported languages are:\n{:?}",
    SgLang::all_langs()
  )
}

const LANG_HELP_LONG: &str = "The language of the pattern. For full language list, visit https://vorpal.github.io/reference/languages.html";

#[derive(Clone)]
pub(crate) struct Strictness(MatchStrictness);

impl Strictness {
  /// Stable wire name (the clap value name), for shipping `--strictness` to a remote agent.
  pub(crate) fn wire_name(&self) -> &'static str {
    use MatchStrictness as M;
    match self.0 {
      M::Cst => "cst",
      M::Smart => "smart",
      M::Ast => "ast",
      M::Relaxed => "relaxed",
      M::Signature => "signature",
      M::Template => "template",
    }
  }

  pub(crate) fn from_wire_name(name: &str) -> Option<Self> {
    Self::value_variants()
      .iter()
      .find(|v| v.wire_name() == name)
      .cloned()
  }
}
impl ValueEnum for Strictness {
  fn value_variants<'a>() -> &'a [Self] {
    use MatchStrictness as M;
    &[
      Strictness(M::Cst),
      Strictness(M::Smart),
      Strictness(M::Ast),
      Strictness(M::Relaxed),
      Strictness(M::Signature),
      Strictness(M::Template),
    ]
  }
  fn to_possible_value(&self) -> Option<PossibleValue> {
    use MatchStrictness as M;
    // The clap value name IS the wire name (single source): renaming one cannot silently break
    // the other's contract with `RunJob.strictness`.
    let help = match &self.0 {
      M::Cst => "Match all nodes exactly",
      M::Smart => "Match all nodes except source trivial nodes",
      M::Ast => "Match only ast nodes",
      M::Relaxed => "Match ast nodes except comments",
      M::Signature => "Match ast nodes except comments, without text",
      M::Template => "Similar to smart but match text only, node kinds are ignored",
    };
    Some(PossibleValue::new(self.wire_name()).help(help))
  }
}

#[derive(Args)]
#[group(required = true)]
pub(crate) struct MatcherArg {
  /// AST pattern to match.
  #[clap(short, long, value_name = "PATTERN")]
  pub(crate) pattern: Option<String>,
  /// AST kind to extract sub-part of pattern to match.
  ///
  /// selector defines the sub-syntax node kind that is the actual matcher of the pattern.
  /// See https://vorpal.github.io/guide/rule-config/atomic-rule.html#pattern-object.
  #[clap(
    long,
    value_name = "KIND",
    requires = "pattern",
    conflicts_with = "kind"
  )]
  pub(crate) selector: Option<String>,

  /// The strictness of the pattern.
  ///
  /// See https://vorpal.github.io/guide/rule-config/atomic-rule.html#strictness
  #[clap(
    long,
    value_name = "STRICTNESS",
    requires = "pattern",
    conflicts_with = "kind"
  )]
  pub(crate) strictness: Option<Strictness>,
  /// AST kind to match.
  ///
  /// It accepts ESQuery style selector.
  /// See https://vorpal.github.io/guide/rule-config/atomic-rule.html#esquery-style-kind
  #[clap(short, long, value_name = "KIND", conflicts_with = "pattern")]
  pub(crate) kind: Option<String>,
}

impl MatcherArg {
  fn build_matcher(&self, lang: SgLang) -> Result<Rule> {
    if let Some(kind) = &self.kind {
      parse_selector(kind, lang).context(EC::ParseSelector)
    } else if let Some(pattern) = &self.pattern {
      self.build_pattern(pattern, lang).map(Rule::Pattern)
    } else {
      Err(anyhow::anyhow!("Impossible code path"))
    }
  }

  fn build_pattern(&self, pattern: &str, lang: SgLang) -> Result<Pattern> {
    let pattern = if let Some(sel) = &self.selector {
      Pattern::contextual(pattern, sel, lang)
    } else {
      Pattern::try_new(pattern, lang)
    }
    .context(EC::ParsePattern)?;
    if let Some(strictness) = &self.strictness {
      Ok(pattern.with_strictness(strictness.0.clone()))
    } else {
      Ok(pattern)
    }
  }
}

#[derive(Parser)]
pub struct RunArg {
  // search pattern related options
  #[clap(flatten)]
  pub(crate) matcher: MatcherArg,

  /// String to replace the matched AST node.
  #[clap(short, long, value_name = "FIX", required_if_eq("update_all", "true"))]
  pub(crate) rewrite: Option<String>,

  /// The language of the pattern query.
  #[clap(short, long, help(lang_help()), long_help=LANG_HELP_LONG)]
  pub(crate) lang: Option<SgLang>,

  /// Print query pattern's tree-sitter AST. Requires lang be set explicitly.
  #[clap(
      long,
      requires = "lang",
      value_name="format",
      num_args(0..=1),
      require_equals = true,
      default_missing_value = "pattern"
  )]
  debug_query: Option<DebugFormat>,

  /// input related options
  #[clap(flatten)]
  pub(crate) input: InputArgs,

  /// output related options
  #[clap(flatten)]
  pub(crate) output: OutputArgs,

  /// context related options
  #[clap(flatten)]
  pub(crate) context: ContextArgs,

  /// remote fan-out options (see docs/REMOTE.md)
  #[clap(flatten)]
  pub(crate) remote: crate::remote::RemoteArgs,

  /// Controls whether to print the file name as heading.
  ///
  /// If heading is used, the file name will be printed as heading before all matches of that file.
  /// If heading is not used, vorpal will print the file path before each match as prefix.
  /// The default value `auto` is to use heading when printing to a terminal
  /// and to disable heading when piping to another program or redirected to files.
  #[clap(long, default_value = "auto", value_name = "WHEN")]
  pub(crate) heading: Heading,
}

impl RunArg {
  /// Agent-side (`__agent`) construction from a wire job: matcher inputs and walk inputs only;
  /// printing decisions arrive resolved via the shipped `PrinterSpec` (docs/REMOTE.md §1).
  #[allow(clippy::too_many_arguments)]
  pub(crate) fn for_remote_agent(
    pattern: Option<String>,
    selector: Option<String>,
    strictness: Option<Strictness>,
    kind: Option<String>,
    rewrite: Option<String>,
    lang: Option<SgLang>,
    input: InputArgs,
    output: OutputArgs,
    context: ContextArgs,
  ) -> Self {
    Self {
      matcher: MatcherArg {
        pattern,
        selector,
        strictness,
        kind,
      },
      rewrite,
      lang,
      debug_query: None,
      input,
      output,
      context,
      remote: Default::default(),
      heading: Heading::Never,
    }
  }

  fn build_matcher(&self, lang: SgLang) -> Result<Rule> {
    self.matcher.build_matcher(lang)
  }

  // do not unwrap pattern here, we should allow non-pattern to be debugged as tree
  fn debug_pattern_if_needed(&self, rule_ret: &Result<Rule>, lang: SgLang) {
    let Some(debug_query) = &self.debug_query else {
      return;
    };
    let colored = self.output.color.should_use_color();
    if !matches!(debug_query, DebugFormat::Pattern) {
      if let Some(pattern) = &self.matcher.pattern {
        debug_query.debug_tree(pattern, lang, colored);
      }
    } else if let Ok(Rule::Pattern(pattern)) = rule_ret {
      debug_query.debug_pattern(pattern, lang, colored);
    }
  }
}

// Every run will include Search or Replace
// Search or Replace by arguments `pattern` and `rewrite` passed from CLI
pub fn run_with_pattern(arg: RunArg, project: Result<ProjectConfig>) -> Result<ExitCode> {
  let proj = arg.output.inspect.project_trace();
  proj.print_project(&project)?;
  if arg.remote.is_remote() {
    // Remote fan-out has its own printer dispatch (interactive editing is local-only).
    return crate::remote::run_remote_dispatch(arg, project);
  }
  let context = arg.context.get();
  match arg.printer_kind() {
    RunPrinterKind::FilesWithMatches => {
      let printer = FileNamePrinter::stdout(arg.output.color);
      run_pattern_with_printer(arg, printer)
    }
    RunPrinterKind::Json(json) => {
      let printer = JSONPrinter::stdout(json).context(context);
      run_pattern_with_printer(arg, printer)
    }
    RunPrinterKind::Colored => {
      let printer = ColoredPrinter::stdout(arg.output.color)
        .heading(arg.heading)
        .context(context);
      // Interactive editing is the one local-only branch; remote rejects it.
      if arg.output.needs_interactive() {
        let from_stdin = arg.input.stdin;
        let printer = InteractivePrinter::new(printer, arg.output.update_all, from_stdin)?;
        run_pattern_with_printer(arg, printer)
      } else {
        run_pattern_with_printer(arg, printer)
      }
    }
  }
}

/// The non-interactive printer a search resolves to. Single source of the selection shared by the
/// local dispatch, the remote dispatch, and the wire `PrinterSpec` builder (see
/// [`crate::scan::ScanPrinterKind`] for the rationale).
pub(crate) enum RunPrinterKind {
  FilesWithMatches,
  Json(crate::print::JsonStyle),
  Colored,
}

impl RunArg {
  pub(crate) fn printer_kind(&self) -> RunPrinterKind {
    if self.output.files_with_matches {
      RunPrinterKind::FilesWithMatches
    } else if let Some(json) = self.output.json {
      RunPrinterKind::Json(json)
    } else {
      RunPrinterKind::Colored
    }
  }
}

fn run_pattern_with_printer(arg: RunArg, printer: impl Printer + 'static) -> Result<ExitCode> {
  let trace = arg.output.inspect.run_trace();
  if arg.input.stdin {
    RunWithSpecificLang::new(arg, trace)?.run_std_in(printer)
  } else if arg.lang.is_some() {
    RunWithSpecificLang::new(arg, trace)?.run_path(printer)
  } else {
    RunWithInferredLang { arg, trace }.run_path(printer)
  }
}

pub(crate) struct RunWithInferredLang {
  pub(crate) arg: RunArg,
  pub(crate) trace: RunTrace,
}
impl Worker for RunWithInferredLang {
  fn consume_items<P: Printer>(
    &self,
    items: Items<P::Processed>,
    mut printer: P,
  ) -> Result<ExitCode> {
    let printer = &mut printer;
    let mut has_matches = false;
    printer.before_print()?;
    for item in items {
      printer.process(item)?;
      has_matches = true
    }
    printer.after_print()?;
    self.trace.print()?;
    Ok(ExitCode::from(if has_matches { 0 } else { 1 }))
  }
}

impl RunWithInferredLang {
  pub(crate) fn run_arg(&self) -> &RunArg {
    &self.arg
  }
}

impl PathWorker for RunWithInferredLang {
  fn build_walk(&self) -> Result<WalkParallel> {
    self.arg.input.walk()
  }
  fn get_trace(&self) -> &FileTrace {
    &self.trace.inner
  }

  fn produce_item<P: Printer>(
    &self,
    path: &Path,
    processor: &P::Processor,
  ) -> Result<Vec<P::Processed>> {
    Ok(
      self
        .produce_counted::<P>(path, processor)?
        .into_iter()
        .map(|(f, _)| f)
        .collect(),
    )
  }
}

impl crate::remote::CountedProduce for RunWithInferredLang {
  fn produce_counted<P: Printer>(
    &self,
    path: &Path,
    processor: &P::Processor,
  ) -> Result<Vec<(P::Processed, u32)>> {
    let ret = self.produce_from_source::<P>(path, Source::Fs, processor)?;
    if !ret.is_empty() {
      // Search feeds the index (§3.4): this file was walked, read, and matched — bank its
      // extraction product so the next `vorpal index` replays it instead of re-parsing.
      // Best-effort by design: search output must never depend on cache writability.
      let _ = vorpal_index::warm_product_cache(path);
    }
    Ok(ret)
  }
}

impl RunWithInferredLang {
  fn produce_from_source<P: Printer>(
    &self,
    path: &Path,
    source: Source,
    processor: &P::Processor,
  ) -> Result<Vec<(P::Processed, u32)>> {
    let Some(lang) = SgLang::from_path(path) else {
      return Ok(vec![]);
    };
    self.trace.print_file(path, lang)?;
    let matcher = self.arg.build_matcher(lang)?;
    // match sub region
    let sub_langs = lang.injectable_sg_langs().into_iter().flatten();
    let sub_matchers = sub_langs
      .filter_map(|l| {
        let maybe_pattern = self.arg.build_matcher(l);
        maybe_pattern.ok().map(|pattern| (l, pattern))
      })
      .collect::<Vec<_>>();

    let items = filter_source_pattern(path, source, lang, Some(&matcher), &sub_matchers)?;
    let mut ret = Vec::with_capacity(items.len());
    let rewrite_str = self.arg.rewrite.as_ref();

    for unit in items {
      let i_lang = unit.grep.lang();
      let rewrite = rewrite_str
        .map(|s| Fixer::from_str(s, i_lang))
        .transpose()
        .unwrap_or_else(|e| {
          eprintln!("⚠️  Rewriting was skipped because pattern fails to parse. Error detail:");
          eprintln!("╰▻ {e}");
          None
        });
      let Some(processed) = match_one_file(processor, &unit, &rewrite)? else {
        continue;
      };
      ret.push(processed);
    }
    Ok(ret)
  }

  /// Remote streaming mode: the same pattern pipeline over wire-delivered content (§3.3).
  pub(crate) fn produce_item_from_content<P: Printer>(
    &self,
    path: &Path,
    content: String,
    processor: &P::Processor,
  ) -> Result<Vec<P::Processed>> {
    Ok(
      self
        .produce_from_source::<P>(path, Source::Memory(content), processor)?
        .into_iter()
        .map(|(f, _)| f)
        .collect(),
    )
  }
}

pub(crate) struct RunWithSpecificLang {
  arg: RunArg,
  rule: Rule,
  rewrite: Option<Fixer>,
  stats: RunTrace,
}

impl RunWithSpecificLang {
  pub(crate) fn new(arg: RunArg, stats: RunTrace) -> Result<Self> {
    let lang = arg.lang.ok_or(anyhow::anyhow!(EC::LanguageNotSpecified))?;
    // do not unwrap result here
    let pattern_ret = arg.build_matcher(lang);
    arg.debug_pattern_if_needed(&pattern_ret, lang);
    let rewrite = if let Some(s) = &arg.rewrite {
      Some(Fixer::from_str(s, &lang).context(EC::ParsePattern)?)
    } else {
      None
    };
    Ok(Self {
      arg,
      rule: pattern_ret?,
      rewrite,
      stats,
    })
  }

  pub(crate) fn run_arg(&self) -> &RunArg {
    &self.arg
  }
}

impl Worker for RunWithSpecificLang {
  fn consume_items<P: Printer>(
    &self,
    items: Items<P::Processed>,
    mut printer: P,
  ) -> Result<ExitCode> {
    printer.before_print()?;
    let mut has_matches = false;
    for item in items {
      printer.process(item)?;
      has_matches = true;
    }
    printer.after_print()?;
    self.stats.print()?;
    if !has_matches
      && let Rule::Pattern(pattern) = &self.rule
      && pattern.has_error()
    {
      return Err(anyhow::anyhow!(EC::PatternHasError));
    }
    Ok(ExitCode::from(if has_matches { 0 } else { 1 }))
  }
}

impl PathWorker for RunWithSpecificLang {
  fn build_walk(&self) -> Result<WalkParallel> {
    let lang = self.arg.lang.expect("must present");
    self.arg.input.walk_lang(lang)
  }
  fn get_trace(&self) -> &FileTrace {
    &self.stats.inner
  }
  fn produce_item<P: Printer>(
    &self,
    path: &Path,
    processor: &P::Processor,
  ) -> Result<Vec<P::Processed>> {
    Ok(
      self
        .produce_counted::<P>(path, processor)?
        .into_iter()
        .map(|(f, _)| f)
        .collect(),
    )
  }
}

impl crate::remote::CountedProduce for RunWithSpecificLang {
  fn produce_counted<P: Printer>(
    &self,
    path: &Path,
    processor: &P::Processor,
  ) -> Result<Vec<(P::Processed, u32)>> {
    let ret = self.produce_from_source::<P>(path, Source::Fs, processor)?;
    if !ret.is_empty() {
      // Search feeds the index (§3.4): bank the matched file's extraction product.
      let _ = vorpal_index::warm_product_cache(path);
    }
    Ok(ret)
  }
}

impl RunWithSpecificLang {
  fn produce_from_source<P: Printer>(
    &self,
    path: &Path,
    source: Source,
    processor: &P::Processor,
  ) -> Result<Vec<(P::Processed, u32)>> {
    let arg = &self.arg;
    let pattern = &self.rule;
    let lang = arg.lang.expect("must present");
    let Some(path_lang) = SgLang::from_path(path) else {
      return Ok(vec![]);
    };
    self.stats.print_file(path, path_lang)?;
    let (root_matcher, sub_matchers) = if path_lang == lang {
      (Some(pattern), vec![])
    } else {
      // rebuild arg here, unfortunately
      let rule = arg.build_matcher(lang)?;
      (None, vec![(lang, rule)])
    };
    let filtered = filter_source_pattern(path, source, path_lang, root_matcher, &sub_matchers)?;
    let mut ret = Vec::with_capacity(filtered.len());
    for unit in filtered {
      let Some(processed) = match_one_file(processor, &unit, &self.rewrite)? else {
        continue;
      };
      ret.push(processed);
    }
    Ok(ret)
  }

  /// Remote streaming mode: the same pattern pipeline over wire-delivered content (§3.3).
  pub(crate) fn produce_item_from_content<P: Printer>(
    &self,
    path: &Path,
    content: String,
    processor: &P::Processor,
  ) -> Result<Vec<P::Processed>> {
    Ok(
      self
        .produce_from_source::<P>(path, Source::Memory(content), processor)?
        .into_iter()
        .map(|(f, _)| f)
        .collect(),
    )
  }
}

impl StdInWorker for RunWithSpecificLang {
  fn parse_stdin<P: Printer>(
    &self,
    src: String,
    processor: &P::Processor,
  ) -> Result<Vec<P::Processed>> {
    let lang = self.arg.lang.expect("must present");
    let grep = lang.grep(src);
    let root = grep.root();
    let mut matches = root.find_all(&self.rule).peekable();
    if matches.peek().is_none() {
      return Ok(vec![]);
    }
    let rewrite = &self.rewrite;
    let path = Path::new("STDIN");
    let processed = if let Some(rewrite) = rewrite {
      let diffs = matches.map(|m| Diff::generate(m, &self.rule, rewrite));
      processor.print_diffs(diffs.collect(), path)?
    } else {
      processor.print_matches(matches.collect(), path)?
    };
    Ok(vec![processed])
  }
}
/// Render one file's matches. Returns the rendered fragment paired with the **match count** it
/// contains (`None` when the file has no matches), so a remote agent can report an accurate
/// `Rendered.match_count`; local callers ignore the count.
fn match_one_file<T, P: PrintProcessor<T>>(
  processor: &P,
  match_unit: &MatchUnit<impl Matcher>,
  rewrite: &Option<Fixer>,
) -> Result<Option<(T, u32)>> {
  let MatchUnit {
    path,
    grep,
    matcher,
  } = match_unit;

  let root = grep.root();
  let matches: Vec<_> = root.find_all(matcher).collect();
  if matches.is_empty() {
    return Ok(None);
  }
  let count = matches.len() as u32;
  let ret = if let Some(rewrite) = rewrite {
    let diffs = matches
      .into_iter()
      .map(|m| Diff::generate(m, matcher, rewrite));
    processor.print_diffs(diffs.collect(), path)?
  } else {
    processor.print_matches(matches, path)?
  };
  Ok(Some((ret, count)))
}

#[cfg(test)]
mod test {
  use super::*;
  use crate::print::ColorArg;
  use std::path::PathBuf;
  use vorpal_language::SupportLang;

  fn default_matcher_arg() -> MatcherArg {
    MatcherArg {
      pattern: None,
      strictness: None,
      selector: None,
      kind: None,
    }
  }

  fn default_run_arg() -> RunArg {
    RunArg {
      matcher: default_matcher_arg(),
      rewrite: None,
      lang: None,
      heading: Heading::Never,
      debug_query: None,
      input: InputArgs {
        no_ignore: vec![],
        stdin: false,
        follow: false,
        paths: vec![PathBuf::from(".")],
        globs: vec![],
        threads: 0,
      },
      output: OutputArgs {
        color: ColorArg::Never,
        interactive: false,
        json: None,
        files_with_matches: false,
        update_all: false,
        inspect: Default::default(),
      },
      context: ContextArgs {
        before: 0,
        after: 0,
        context: 0,
      },
      remote: Default::default(),
    }
  }

  #[test]
  fn test_run_with_pattern() {
    let arg = RunArg {
      matcher: MatcherArg {
        pattern: Some("console.log".to_string()),
        ..default_matcher_arg()
      },
      ..default_run_arg()
    };
    let proj = Err(anyhow::anyhow!("no project"));
    assert!(run_with_pattern(arg, proj).is_ok())
  }

  #[test]
  fn test_run_with_strictness() {
    let arg = RunArg {
      matcher: MatcherArg {
        pattern: Some("console.log".to_string()),
        strictness: Some(Strictness(MatchStrictness::Ast)),
        ..default_matcher_arg()
      },
      ..default_run_arg()
    };
    let proj = Err(anyhow::anyhow!("no project"));
    assert!(run_with_pattern(arg, proj).is_ok())
  }

  #[test]
  fn test_run_with_specific_lang() {
    let arg = RunArg {
      matcher: MatcherArg {
        pattern: Some("Some(result)".to_string()),
        ..default_matcher_arg()
      },
      lang: Some(SupportLang::Rust.into()),
      ..default_run_arg()
    };
    let proj = Err(anyhow::anyhow!("no project"));
    assert!(run_with_pattern(arg, proj).is_ok())
  }

  #[test]
  fn test_run_with_invalid_kind() {
    let arg = RunArg {
      matcher: MatcherArg {
        kind: Some("gibberish".to_string()),
        ..default_matcher_arg()
      },
      lang: Some(SupportLang::Rust.into()),
      ..default_run_arg()
    };
    let proj = Err(anyhow::anyhow!("no project"));
    assert!(run_with_pattern(arg, proj).is_err())
  }
}
