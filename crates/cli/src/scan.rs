use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::Args;
use ignore::WalkParallel;
use vorpal_config::{
  CombinedScan, NO_SUPPRESS_ALL_ID, RuleCollection, RuleConfig, Severity, UNUSED_SUPPRESSION_ID,
  from_yaml_string,
};
use vorpal_core::{NodeMatch, tree_sitter::StrDoc};
use vorpal_language::SupportLang;

use crate::remote::CountedProduce;
use crate::config::{ProjectConfig, read_rule_file, with_rule_stats};
use crate::lang::SgLang;
use crate::print::{
  CloudPrinter, ColoredPrinter, Diff, FileNamePrinter, InteractivePrinter, JSONPrinter, Platform,
  PrintProcessor, Printer, ReportStyle, SimpleFile,
};
use crate::utils::RuleOverwrite;
use crate::utils::{
  ContextArgs, InputArgs, LangPrefilters, OutputArgs, OverwriteArgs, filter_file_rule,
};
use crate::utils::{ErrorContext as EC, MaxItemCounter};
use crate::utils::{FileTrace, ScanTrace};
use crate::utils::{Items, PathWorker, StdInWorker, Worker};

use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Args)]
pub struct ScanArg {
  /// Scan the codebase with the single rule located at the path RULE_FILE.
  ///
  /// It is useful to run single rule without project setup or vorpalconfig.yml.
  #[clap(short, long, value_name = "RULE_FILE")]
  rule: Option<PathBuf>,

  /// Scan the codebase with a rule defined by the provided RULE_TEXT.
  ///
  /// Use this argument if you want to test a rule without creating a YAML file on disk.
  /// You can run multiple rules by separating them with `---` in the RULE_TEXT.
  /// --inline-rules is incompatible with --rule.
  #[clap(long, conflicts_with = "rule", value_name = "RULE_TEXT")]
  inline_rules: Option<String>,

  /// Output warning/error messages in different formats.
  ///
  /// Supported formats: GitHub Action, SARIF (Static Analysis Results Interchange Format).
  #[clap(long, conflicts_with = "json", conflicts_with = "interactive")]
  pub(crate) format: Option<Platform>,

  #[clap(long, default_value = "rich", conflicts_with = "json")]
  pub(crate) report_style: ReportStyle,

  /// Include rule metadata in the json output.
  ///
  /// This flags requires --json output. Default is false.
  #[clap(long, requires = "json")]
  pub(crate) include_metadata: bool,

  /// severity related options
  #[clap(flatten)]
  overwrite: OverwriteArgs,

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

  /// Show at most NUM results and stop running once the limit is reached.
  ///
  /// Useful for big codebase to fail scan/search fast.
  #[clap(long, conflicts_with = "interactive", value_name = "NUM")]
  pub(crate) max_results: Option<u16>,
}

/// The non-interactive printer a scan resolves to (see [`ScanArg::printer_kind`]). Interactive
/// editing is a separate, local-only path handled where `Colored` is built.
pub(crate) enum ScanPrinterKind {
  FilesWithMatches,
  Cloud(Platform),
  Json(crate::print::JsonStyle),
  Colored,
}

impl ScanArg {
  // whether the scan includes all rules available in the project
  fn include_all_rules(&self) -> bool {
    self.overwrite.include_all_rules() && self.rule.is_none() && self.inline_rules.is_none()
  }

  /// Whether rules come from the project (vs `--rule`/`--inline-rules`, which compile against
  /// empty global utils) — decides if a remote job ships the project's util YAMLs.
  pub(crate) fn uses_project_rules(&self) -> bool {
    self.rule.is_none() && self.inline_rules.is_none()
  }

  /// Which non-interactive printer this scan selects. The **single source** of that selection:
  /// the local dispatch (`run_with_config`), the remote dispatch (`remote::scan_remote_dispatch`),
  /// and the wire `PrinterSpec` builder (`remote::spec`) all branch on this, so the processor the
  /// agent reconstructs cannot diverge from what a local run prints (docs/REMOTE.md §1). Adding a
  /// printer is then a compile error at each site until handled, not a silent divergence.
  pub(crate) fn printer_kind(&self) -> ScanPrinterKind {
    if self.output.files_with_matches {
      ScanPrinterKind::FilesWithMatches
    } else if let Some(format) = &self.format {
      ScanPrinterKind::Cloud(format.clone())
    } else if let Some(json) = self.output.json {
      ScanPrinterKind::Json(json)
    } else {
      ScanPrinterKind::Colored
    }
  }

  /// Agent-side (`__agent`) construction: only walk inputs and the local result cap matter here —
  /// rules and printer decisions arrive resolved via the wire job (docs/REMOTE.md §1).
  pub(crate) fn for_remote_agent(
    input: InputArgs,
    output: OutputArgs,
    context: ContextArgs,
    max_results: Option<u16>,
  ) -> Self {
    Self {
      rule: None,
      inline_rules: None,
      format: None,
      report_style: ReportStyle::Rich,
      include_metadata: false,
      overwrite: OverwriteArgs {
        filter: None,
        error: None,
        warning: None,
        info: None,
        hint: None,
        off: None,
      },
      input,
      output,
      context,
      remote: Default::default(),
      max_results,
    }
  }
}

pub fn run_with_config(arg: ScanArg, project: Result<ProjectConfig>) -> Result<ExitCode> {
  let project_trace = arg.output.inspect.project_trace();
  project_trace.print_project(&project)?;
  if arg.remote.is_remote() {
    // Remote fan-out has its own printer dispatch: every non-interactive printer's output is a
    // relocatable fragment; interactive editing is local-only and rejected there.
    return crate::remote::scan_remote_dispatch(arg, project);
  }
  let context = arg.context.get();
  match arg.printer_kind() {
    ScanPrinterKind::FilesWithMatches => {
      let printer = FileNamePrinter::stdout(arg.output.color);
      run_scan(arg, printer, project)
    }
    ScanPrinterKind::Cloud(format) => {
      let printer = CloudPrinter::stdout(format);
      run_scan(arg, printer, project)
    }
    ScanPrinterKind::Json(json) => {
      let printer = JSONPrinter::stdout(json).include_metadata(arg.include_metadata);
      run_scan(arg, printer, project)
    }
    ScanPrinterKind::Colored => {
      let printer = ColoredPrinter::stdout(arg.output.color)
        .style(arg.report_style)
        .context(context);
      // Interactive editing is the one local-only branch (it edits files); remote rejects it.
      if arg.output.needs_interactive() {
        let from_stdin = arg.input.stdin;
        let printer = InteractivePrinter::new(printer, arg.output.update_all, from_stdin)?;
        run_scan(arg, printer, project)
      } else {
        run_scan(arg, printer, project)
      }
    }
  }
}

fn run_scan<P: Printer + 'static>(
  arg: ScanArg,
  printer: P,
  project: Result<ProjectConfig>,
) -> Result<ExitCode> {
  if arg.input.stdin {
    let worker = ScanStdin::try_new(arg)?;
    // TODO: report a soft error if rules have different languages
    worker.run_std_in(printer)
  } else {
    let worker = ScanWithConfig::try_new(arg, project)?;
    worker.run_path(printer)
  }
}

pub(crate) struct ScanWithConfig {
  arg: ScanArg,
  configs: RuleCollection<SgLang>,
  /// §12 pre-parse gate, built once from every rule's required literals.
  prefilters: LangPrefilters,
  unused_suppression_rule: RuleConfig<SgLang>,
  no_suppress_all_rule: RuleConfig<SgLang>,
  trace: ScanTrace,
  proj_dir: PathBuf,
  // TODO: remove this
  error_count: AtomicUsize,
  max_item_counter: Option<MaxItemCounter>,
}
impl ScanWithConfig {
  pub(crate) fn try_new(arg: ScanArg, project: Result<ProjectConfig>) -> Result<Self> {
    let overwrite = RuleOverwrite::new(&arg.overwrite)?;
    let unused_suppression_rule = unused_suppression_rule_config(&arg, &overwrite);
    let no_suppress_all_rule = no_suppress_all_rule_config(&overwrite);
    let mut proj_dir = PathBuf::from(".");
    let (configs, rule_trace) = if let Some(path) = &arg.rule {
      let rules = read_rule_file(path, &Default::default())
        .and_then(|configs| overwrite.process_configs(configs))?;
      proj_dir = path.parent().unwrap_or(Path::new(".")).to_path_buf();
      with_rule_stats(rules)?
    } else if let Some(text) = &arg.inline_rules {
      let configs = from_yaml_string(text, &Default::default())
        .with_context(|| EC::ParseRule("INLINE_RULES".into()))?;
      let rules = overwrite.process_configs(configs)?;
      with_rule_stats(rules)?
    } else {
      // NOTE: only query project here since -r does not need project
      let project_config = project?;
      proj_dir = project_config.project_dir.clone();
      project_config.find_rules(overwrite)?
    };
    let trace = arg.output.inspect.scan_trace(rule_trace);
    trace.print_rules(&configs)?;
    let absolute_proj_dir = proj_dir
      .canonicalize()
      .or_else(|_| std::env::current_dir())?;
    let max_item_counter = arg.max_results.map(MaxItemCounter::new);
    let prefilters = LangPrefilters::build(&configs);
    Ok(Self {
      arg,
      configs,
      prefilters,
      unused_suppression_rule,
      no_suppress_all_rule,
      trace,
      proj_dir: absolute_proj_dir,
      error_count: AtomicUsize::new(0),
      max_item_counter,
    })
  }
}
impl Worker for ScanWithConfig {
  fn consume_items<P: Printer>(
    &self,
    items: Items<P::Processed>,
    mut printer: P,
  ) -> Result<ExitCode> {
    printer.before_print()?;
    for item in items {
      printer.process(item)?;
    }
    printer.after_print()?;
    self.trace.print()?;
    let error_count = self.error_count.load(Ordering::Acquire);
    if error_count > 0 {
      Err(anyhow::anyhow!(EC::DiagnosticError(error_count)))
    } else {
      Ok(ExitCode::SUCCESS)
    }
  }
}

// we should only suggest unused suppression if scan includes all rules
// otherwise, keep silent about unused suppressions because they may used by other rules
// this is a "smart" heuristic but user always can override it
fn default_unused_suppression_rule_severity(arg: &ScanArg) -> Severity {
  if arg.include_all_rules() {
    Severity::Hint
  } else {
    Severity::Off
  }
}

fn no_suppress_all_rule_config(overwrite: &RuleOverwrite) -> RuleConfig<SgLang> {
  let severity = overwrite
    .find(NO_SUPPRESS_ALL_ID)
    .severity
    .unwrap_or(Severity::Off);
  CombinedScan::no_suppress_all_config(severity, SupportLang::Rust.into())
}

fn unused_suppression_rule_config(arg: &ScanArg, overwrite: &RuleOverwrite) -> RuleConfig<SgLang> {
  let severity = overwrite
    .find(UNUSED_SUPPRESSION_ID)
    .severity
    .unwrap_or_else(|| default_unused_suppression_rule_severity(arg));
  CombinedScan::unused_config(severity, SupportLang::Rust.into())
}

impl PathWorker for ScanWithConfig {
  fn get_trace(&self) -> &FileTrace {
    &self.trace.inner.file_trace
  }
  fn build_walk(&self) -> Result<WalkParallel> {
    let mut langs = HashSet::new();
    self.configs.for_each_rule(|rule| {
      langs.insert(rule.language);
    });
    self.arg.input.walk_langs(langs.into_iter())
  }
  fn produce_item<P: Printer>(
    &self,
    path: &Path,
    processor: &P::Processor,
  ) -> Result<Vec<P::Processed>> {
    Ok(self.produce_counted::<P>(path, processor)?.into_iter().map(|(f, _)| f).collect())
  }

  fn should_stop(&self) -> bool {
    match &self.max_item_counter {
      Some(max) => max.reached_max(),
      None => false,
    }
  }
}

impl crate::remote::CountedProduce for ScanWithConfig {
  fn produce_counted<P: Printer>(
    &self,
    path: &Path,
    processor: &P::Processor,
  ) -> Result<Vec<(P::Processed, u32)>> {
    let items = filter_file_rule(path, &self.configs, &self.trace, Some(&self.prefilters))?;
    if items.is_empty() {
      return Ok(vec![]);
    }
    // use path relative to project director
    let abs_path = path.canonicalize()?;
    let normalized_path = abs_path.strip_prefix(&self.proj_dir).unwrap_or(path);
    let ret = self.render_items::<P>(path, normalized_path, items, processor)?;
    if !ret.is_empty() {
      // Scan feeds the index too (§3.4): rule matches bank the file's extraction product.
      let _ = vorpal_index::warm_product_cache(path);
    }
    Ok(ret)
  }
}

impl ScanWithConfig {
  /// The scan/render loop shared by the filesystem path (`produce_item`) and the remote streaming
  /// path (`produce_item_from_content`): identical matching, suppression, `--max-results`
  /// claiming, severity accounting, and rendering. Each rendered fragment is returned with the
  /// **number of matches** it contains, so an agent can report an accurate `Rendered.match_count`
  /// (docs/REMOTE.md §3.1); local callers drop the count.
  fn render_items<P: Printer>(
    &self,
    path: &Path,
    normalized_path: &Path,
    items: smallvec::SmallVec<[crate::utils::Vorpal; 1]>,
    processor: &P::Processor,
  ) -> Result<Vec<(P::Processed, u32)>> {
    let mut error_count = 0usize;
    let mut ret = vec![];
    for grep in items {
      let file_content = grep.source();
      let rules = self
        .configs
        .get_rule_from_lang(normalized_path, *grep.lang());
      let mut combined = CombinedScan::new(rules);
      combined.set_unused_suppression_rule(&self.unused_suppression_rule);
      combined.set_no_suppress_all_rule(&self.no_suppress_all_rule);
      let interactive = self.arg.output.needs_interactive();
      // exclude_fix rule because we already have diff inspection before
      let scanned = combined.scan(&grep, /* separate_fix*/ interactive);
      if interactive {
        let diffs = scanned.diffs;
        let count = diffs.len() as u32;
        let processed = match_rule_diff_on_file(path, diffs, processor)?;
        ret.push((processed, count));
      }
      for (rule, matches) in scanned.matches {
        // Atomically reserve slots for matches, truncating if needed
        let matches: Vec<_> = if let Some(counter) = &self.max_item_counter {
          let wanted = matches.len();
          // Atomically claim as many slots as we can (up to wanted)
          let claimed = counter.claim(wanted);
          if claimed == 0 {
            break;
          }
          matches.into_iter().take(claimed).collect()
        } else {
          matches
        };
        if matches.is_empty() {
          continue;
        }
        let match_count = matches.len();
        if matches!(rule.severity, Severity::Error) {
          error_count = error_count.saturating_add(match_count);
        }
        let processed = match_rule_on_file(path, matches, rule, file_content, processor)?;
        ret.push((processed, match_count as u32));
      }
    }
    self.error_count.fetch_add(error_count, Ordering::AcqRel);
    Ok(ret)
  }

  /// Remote streaming mode (§3.3): run the exact scan pipeline on content that arrived over the
  /// wire. `normalized_path` is the project-relative path for rule `files:`/`ignores:` glob
  /// matching, computed lexically by the coordinator (no filesystem access here).
  pub(crate) fn produce_item_from_content<P: Printer>(
    &self,
    display_path: &Path,
    normalized_path: &Path,
    content: String,
    processor: &P::Processor,
  ) -> Result<Vec<P::Processed>> {
    let items = crate::utils::filter_source_rule(
      display_path,
      crate::utils::Source::Memory(content),
      &self.configs,
      &self.trace,
      Some(&self.prefilters),
    )?;
    Ok(
      self
        .render_items::<P>(display_path, normalized_path, items, processor)?
        .into_iter()
        .map(|(f, _)| f)
        .collect(),
    )
  }

  /// Agent-side construction from a wire job: the rule set arrives pre-resolved (post-overwrite)
  /// and pre-compiled by the caller; everything else mirrors `try_new`.
  pub(crate) fn from_remote_parts(
    arg: ScanArg,
    configs: RuleCollection<SgLang>,
    unused_suppression_rule: RuleConfig<SgLang>,
    no_suppress_all_rule: RuleConfig<SgLang>,
    proj_dir: PathBuf,
  ) -> Self {
    let max_item_counter = arg.max_results.map(MaxItemCounter::new);
    let prefilters = LangPrefilters::build(&configs);
    let trace = arg.output.inspect.scan_trace(crate::utils::RuleTrace {
      file_trace: Default::default(),
      effective_rule_count: configs.total_rule_count(),
      skipped_rule_count: 0,
    });
    Self {
      arg,
      configs,
      prefilters,
      unused_suppression_rule,
      no_suppress_all_rule,
      trace,
      proj_dir,
      error_count: AtomicUsize::new(0),
      max_item_counter,
    }
  }

  pub(crate) fn scan_arg(&self) -> &ScanArg {
    &self.arg
  }

  pub(crate) fn rule_collection(&self) -> &RuleCollection<SgLang> {
    &self.configs
  }

  /// Resolved severities of the two synthesized suppression rules — part of the post-overwrite
  /// rule semantics a remote agent must reproduce.
  pub(crate) fn suppression_severities(&self) -> (Severity, Severity) {
    (
      self.unused_suppression_rule.severity.clone(),
      self.no_suppress_all_rule.severity.clone(),
    )
  }

  pub(crate) fn project_dir(&self) -> &Path {
    &self.proj_dir
  }

  /// Fold a remote node's error-severity match count into the same counter local scanning uses,
  /// so the exit-code decision in `consume_items` is identical (§3.4).
  pub(crate) fn add_remote_error_count(&self, count: usize) {
    self.error_count.fetch_add(count, Ordering::AcqRel);
  }

  pub(crate) fn local_error_count(&self) -> usize {
    self.error_count.load(Ordering::Acquire)
  }
}

struct ScanStdin {
  rules: Vec<RuleConfig<SgLang>>,
  // TODO: remove this
  error_count: AtomicUsize,
  max_diagnostics_shown: Option<usize>,
}
impl ScanStdin {
  fn try_new(arg: ScanArg) -> Result<Self> {
    let overwrite = RuleOverwrite::new(&arg.overwrite)?;
    let global_rules = Default::default();
    let rules = if let Some(path) = &arg.rule {
      read_rule_file(path, &global_rules).and_then(|configs| overwrite.process_configs(configs))?
    } else if let Some(text) = &arg.inline_rules {
      let configs = from_yaml_string(text, &global_rules)
        .with_context(|| EC::ParseRule("INLINE_RULES".into()))?;
      overwrite.process_configs(configs)?
    } else {
      return Err(anyhow::anyhow!(EC::RuleNotSpecified));
    };
    Ok(Self {
      rules,
      error_count: AtomicUsize::new(0),
      max_diagnostics_shown: arg.max_results.map(usize::from),
    })
  }
}

impl Worker for ScanStdin {
  fn consume_items<P: Printer>(
    &self,
    items: Items<P::Processed>,
    mut printer: P,
  ) -> Result<ExitCode> {
    printer.before_print()?;
    for item in items {
      printer.process(item)?;
    }
    printer.after_print()?;
    let error_count = self.error_count.load(Ordering::Acquire);
    if error_count > 0 {
      Err(anyhow::anyhow!(EC::DiagnosticError(error_count)))
    } else {
      Ok(ExitCode::SUCCESS)
    }
  }
}

impl StdInWorker for ScanStdin {
  fn parse_stdin<P: Printer>(
    &self,
    src: String,
    processor: &P::Processor,
  ) -> Result<Vec<P::Processed>> {
    use vorpal_core::tree_sitter::LanguageExt;
    let lang = self.rules[0].language;
    let combined = CombinedScan::new(self.rules.iter().collect());
    let grep = lang.grep(src);
    let path = Path::new("STDIN");
    let file_content = grep.source();
    // do not separate_fix rule in stdin mode
    let scanned = combined.scan(&grep, false);
    let mut error_count = 0usize;
    let mut diagnostic_count = 0usize;
    let mut ret = vec![];
    for (rule, matches) in scanned.matches {
      // Truncate matches if max_diagnostics_shown is set
      let matches: Vec<_> = if let Some(max) = self.max_diagnostics_shown {
        let remaining = max.saturating_sub(diagnostic_count);
        if remaining == 0 {
          break;
        }
        matches.into_iter().take(remaining).collect()
      } else {
        matches
      };
      if matches.is_empty() {
        continue;
      }
      let match_count = matches.len();
      diagnostic_count += match_count;
      if matches!(rule.severity, Severity::Error) {
        error_count = error_count.saturating_add(match_count);
      }
      let processed = match_rule_on_file(path, matches, rule, file_content, processor)?;
      ret.push(processed);
    }
    self.error_count.fetch_add(error_count, Ordering::AcqRel);
    Ok(ret)
  }
}
fn match_rule_diff_on_file<T>(
  path: &Path,
  matches: Vec<(&RuleConfig<SgLang>, NodeMatch<StrDoc<SgLang>>)>,
  processor: &impl PrintProcessor<T>,
) -> Result<T> {
  let diffs = matches
    .into_iter()
    .filter_map(|(rule, m)| {
      let fixers = &rule.fixer;
      let diff = Diff::multiple(m, &rule.matcher, fixers)?;
      Some((diff, rule))
    })
    .collect();
  let processed = processor.print_rule_diffs(diffs, path)?;
  Ok(processed)
}

fn match_rule_on_file<T>(
  path: &Path,
  matches: Vec<NodeMatch<StrDoc<SgLang>>>,
  rule: &RuleConfig<SgLang>,
  file_content: &str,
  processor: &impl PrintProcessor<T>,
) -> Result<T> {
  let file = SimpleFile::new(path.to_string_lossy(), file_content);
  let processed = if let Some(fixer) = &rule.fixer.first() {
    let diffs = matches
      .into_iter()
      .map(|m| (Diff::generate(m, &rule.matcher, fixer), rule))
      .collect();
    processor.print_rule_diffs(diffs, path)?
  } else {
    processor.print_rule(matches, file, rule)?
  };
  Ok(processed)
}

#[cfg(test)]
mod test {
  use super::*;
  use crate::print::ColorArg;
  use std::fs::File;
  use std::io::Write;
  use tempfile::TempDir;

  const RULE: &str = r#"
id: test
message: Add your rule message here....
severity: error # error, warning, hint, info
language: Rust
rule:
  pattern: Some(123)
"#;

  // TODO: unify with verify::test
  pub fn create_test_files<'a>(
    names_and_contents: impl IntoIterator<Item = (&'a str, &'a str)>,
  ) -> TempDir {
    let dir = TempDir::new().unwrap();
    for (name, contents) in names_and_contents {
      let path = dir.path().join(name);
      let mut file = File::create(path.clone()).unwrap();
      file.write_all(contents.as_bytes()).unwrap();
      file.sync_all().unwrap();
    }
    dir
  }

  fn default_scan_arg() -> ScanArg {
    ScanArg {
      rule: None,
      inline_rules: None,
      report_style: ReportStyle::Rich,
      include_metadata: false,
      input: InputArgs {
        no_ignore: vec![],
        paths: vec![PathBuf::from(".")],
        stdin: false,
        follow: false,
        globs: vec![],
        threads: 0,
      },
      overwrite: OverwriteArgs {
        filter: None,
        error: None,
        warning: None,
        info: None,
        hint: None,
        off: None,
      },
      output: OutputArgs {
        interactive: false,
        json: None,
        files_with_matches: false,
        update_all: false,
        color: ColorArg::Never,
        inspect: Default::default(),
      },
      context: ContextArgs {
        before: 0,
        after: 0,
        context: 0,
      },
      remote: Default::default(),
      format: None,
      max_results: None,
    }
  }

  #[test]
  fn test_run_with_config() {
    let dir = create_test_files([("vorpalconfig.yml", "ruleDirs: [rules]")]);
    std::fs::create_dir_all(dir.path().join("rules")).unwrap();
    let mut file = File::create(dir.path().join("rules/test.yml")).unwrap();
    file.write_all(RULE.as_bytes()).unwrap();
    let mut file = File::create(dir.path().join("test.rs")).unwrap();
    file
      .write_all("fn test() { Some(123) }".as_bytes())
      .unwrap();
    file.sync_all().unwrap();
    let project_config = ProjectConfig::setup(Some(dir.path().join("vorpalconfig.yml"))).unwrap();
    let arg = default_scan_arg();
    assert!(run_with_config(arg, project_config).is_ok());
  }

  #[test]
  fn test_scan_with_inline_rules() {
    let inline_rules = "{id: test, language: ts, rule: {pattern: readFileSync}}".to_string();
    let arg = ScanArg {
      inline_rules: Some(inline_rules),
      ..default_scan_arg()
    };
    assert!(run_with_config(arg, Err(anyhow::anyhow!("not found"))).is_ok());
  }

  #[test]
  fn test_scan_with_inline_rules_diff() {
    let inline_rules =
      "{id: test, language: ts, rule: {pattern: readFileSync}, fix: 'nnn'}".to_string();
    let arg = ScanArg {
      inline_rules: Some(inline_rules),
      ..default_scan_arg()
    };
    assert!(run_with_config(arg, Err(anyhow::anyhow!("not found"))).is_ok());
  }

  // baseline test for coverage
  #[test]
  fn test_scan_with_inline_rules_error() {
    let inline_rules = "nonsense".to_string();
    let arg = ScanArg {
      inline_rules: Some(inline_rules),
      ..default_scan_arg()
    };
    let err = run_with_config(arg, Err(anyhow::anyhow!("not found"))).expect_err("should error");
    assert!(err.is::<EC>());
    assert_eq!(err.to_string(), "Cannot parse rule INLINE_RULES");
  }
}
