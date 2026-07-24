//! Job-spec construction (coordinator) and reconstruction (agent).
//!
//! Fidelity rule (docs/REMOTE.md §1 `PrinterSpec`): anything a processor bakes into rendered
//! bytes — color, heading, report style, context — is resolved **on the coordinator** (it owns
//! the terminal and its tty/env state) and shipped as plain values; the agent reproduces the
//! exact processor from those values. Discovery inputs (`WalkParams`) mirror `InputArgs` so the
//! agent's `WalkParallel` is the same walk (I1 in agent mode).

use anyhow::{Context as _, Result, anyhow};
use codespan_reporting::term::termcolor::{Buffer, ColorChoice, WriteColor};

use vorpal_wire::{
  FlowParams, JobKind, JobSpec, JsonStyleSpec, Limits, NoIgnore as WireNoIgnore, PlatformSpec,
  PrinterSpec, ReportStyleSpec, RulePayload, RunJob, ScanJob, WalkParams,
};

use crate::lang::SgLang;
use crate::print::{
  CloudPrinter, ColoredPrinter, FileNamePrinter, Heading, JSONPrinter, JsonStyle, Platform,
  ReportStyle,
};
use crate::run::{RunArg, RunPrinterKind, Strictness};
use crate::scan::{ScanArg, ScanPrinterKind, ScanWithConfig};
use crate::utils::{ErrorContext as EC, IgnoreFile, InputArgs};

use super::fingerprint;
use super::rules_wire::{self, LangEnv};

// ---------------------------------------------------------------------------
// Enum mirrors (CLI ↔ wire)
// ---------------------------------------------------------------------------

pub(crate) fn json_style_to_wire(style: JsonStyle) -> JsonStyleSpec {
  match style {
    JsonStyle::Pretty => JsonStyleSpec::Pretty,
    JsonStyle::Stream => JsonStyleSpec::Stream,
    JsonStyle::Compact => JsonStyleSpec::Compact,
  }
}

pub(crate) fn json_style_from_wire(style: JsonStyleSpec) -> JsonStyle {
  match style {
    JsonStyleSpec::Pretty => JsonStyle::Pretty,
    JsonStyleSpec::Stream => JsonStyle::Stream,
    JsonStyleSpec::Compact => JsonStyle::Compact,
  }
}

pub(crate) fn report_style_to_wire(style: ReportStyle) -> ReportStyleSpec {
  match style {
    ReportStyle::Rich => ReportStyleSpec::Rich,
    ReportStyle::Medium => ReportStyleSpec::Medium,
    ReportStyle::Short => ReportStyleSpec::Short,
  }
}

pub(crate) fn report_style_from_wire(style: ReportStyleSpec) -> ReportStyle {
  match style {
    ReportStyleSpec::Rich => ReportStyle::Rich,
    ReportStyleSpec::Medium => ReportStyle::Medium,
    ReportStyleSpec::Short => ReportStyle::Short,
  }
}

pub(crate) fn platform_to_wire(platform: &Platform) -> PlatformSpec {
  match platform {
    Platform::GitHub => PlatformSpec::GitHub,
    Platform::Sarif => PlatformSpec::Sarif,
  }
}

pub(crate) fn platform_from_wire(platform: PlatformSpec) -> Platform {
  match platform {
    PlatformSpec::GitHub => Platform::GitHub,
    PlatformSpec::Sarif => Platform::Sarif,
  }
}

pub(crate) fn no_ignore_to_wire(file: &IgnoreFile) -> WireNoIgnore {
  match file {
    IgnoreFile::Hidden => WireNoIgnore::Hidden,
    IgnoreFile::Dot => WireNoIgnore::Dot,
    IgnoreFile::Exclude => WireNoIgnore::Exclude,
    IgnoreFile::Global => WireNoIgnore::Global,
    IgnoreFile::Parent => WireNoIgnore::Parent,
    IgnoreFile::Vcs => WireNoIgnore::Vcs,
  }
}

pub(crate) fn no_ignore_from_wire(file: WireNoIgnore) -> IgnoreFile {
  match file {
    WireNoIgnore::Hidden => IgnoreFile::Hidden,
    WireNoIgnore::Dot => IgnoreFile::Dot,
    WireNoIgnore::Exclude => IgnoreFile::Exclude,
    WireNoIgnore::Global => IgnoreFile::Global,
    WireNoIgnore::Parent => IgnoreFile::Parent,
    WireNoIgnore::Vcs => IgnoreFile::Vcs,
  }
}

fn severity_name(severity: &vorpal_config::Severity) -> Result<String> {
  match serde_json::to_value(severity)? {
    serde_json::Value::String(s) => Ok(s),
    other => Err(anyhow!("severity serialized to non-string {other}")),
  }
}

pub(crate) fn severity_from_name(name: &str) -> Result<vorpal_config::Severity> {
  serde_json::from_value(serde_json::Value::String(name.to_string()))
    .with_context(|| format!("unknown severity `{name}`"))
}

// ---------------------------------------------------------------------------
// Coordinator side: build the job
// ---------------------------------------------------------------------------

fn walk_params(input: &InputArgs) -> Result<WalkParams> {
  let paths = input
    .paths
    .iter()
    .map(|p| {
      p.to_str().map(str::to_owned).ok_or_else(|| {
        anyhow!(EC::RemoteInvalid(format!(
          "non-UTF-8 path {p:?} cannot cross the wire"
        )))
      })
    })
    .collect::<Result<Vec<_>>>()?;
  Ok(WalkParams {
    paths,
    globs: input.globs.clone(),
    no_ignore: input.no_ignore.iter().map(no_ignore_to_wire).collect(),
    follow_links: input.follow,
    threads: input.threads as u32,
  })
}

/// Resolve every terminal-dependent decision the colored/file-name processors bake into bytes.
/// `buffer_color` mirrors `StandardStream::stdout(choice).supports_color()` (what
/// `get_processor` reads); `styles_colored` mirrors `PrintStyles::from(choice)`.
fn resolve_color(color: crate::print::ColorArg) -> (bool, bool) {
  let choice: ColorChoice = color.into();
  let buffer_color =
    codespan_reporting::term::termcolor::StandardStream::stdout(choice).supports_color();
  let styles_colored = color.should_use_color();
  (buffer_color, styles_colored)
}

fn scan_printer_spec(arg: &ScanArg) -> PrinterSpec {
  // Branch on the same classifier the local/remote dispatch use, so the reconstructed processor
  // matches the printer the coordinator would build (docs/REMOTE.md §1).
  match arg.printer_kind() {
    ScanPrinterKind::FilesWithMatches => {
      let (buffer_color, styles_colored) = resolve_color(arg.output.color);
      PrinterSpec::FileName {
        buffer_color,
        styles_colored,
      }
    }
    ScanPrinterKind::Cloud(format) => PrinterSpec::Cloud {
      platform: platform_to_wire(&format),
    },
    ScanPrinterKind::Json(json) => PrinterSpec::Json {
      style: json_style_to_wire(json),
      // scan constructs its JSON printer without context (matches `run_with_config`)
      context: (0, 0),
      include_metadata: arg.include_metadata,
    },
    ScanPrinterKind::Colored => {
      let (buffer_color, styles_colored) = resolve_color(arg.output.color);
      PrinterSpec::Colored {
        buffer_color,
        styles_colored,
        // scan never sets heading; the printer default is Auto, resolved here on the coordinator
        heading: Heading::Auto.should_print(),
        report_style: report_style_to_wire(arg.report_style),
        context: arg.context.get(),
      }
    }
  }
}

fn run_printer_spec(arg: &RunArg) -> PrinterSpec {
  match arg.printer_kind() {
    RunPrinterKind::FilesWithMatches => {
      let (buffer_color, styles_colored) = resolve_color(arg.output.color);
      PrinterSpec::FileName {
        buffer_color,
        styles_colored,
      }
    }
    RunPrinterKind::Json(json) => PrinterSpec::Json {
      style: json_style_to_wire(json),
      context: arg.context.get(),
      include_metadata: false,
    },
    RunPrinterKind::Colored => {
      let (buffer_color, styles_colored) = resolve_color(arg.output.color);
      PrinterSpec::Colored {
        buffer_color,
        styles_colored,
        heading: arg.heading.should_print(),
        // run's colored printer keeps the default (rich) display style
        report_style: ReportStyleSpec::Rich,
        context: arg.context.get(),
      }
    }
  }
}

pub(crate) fn build_scan_job(
  worker: &ScanWithConfig,
  lang_env: &LangEnv,
  globals_yaml: Vec<String>,
) -> Result<JobSpec> {
  let arg = worker.scan_arg();
  let rules: RulePayload = rules_wire::encode_scan_rules(worker.rule_collection(), globals_yaml)?;
  let (unused, no_suppress) = worker.suppression_severities();
  let proj_dir = worker
    .project_dir()
    .to_str()
    .ok_or_else(|| {
      anyhow!(EC::RemoteInvalid(
        "non-UTF-8 project dir cannot cross the wire".into()
      ))
    })?
    .to_string();
  Ok(JobSpec {
    job_id: std::process::id() as u64,
    kind: JobKind::Scan(ScanJob {
      rules,
      unused_suppression_severity: severity_name(&unused)?,
      no_suppress_all_severity: severity_name(&no_suppress)?,
      proj_dir,
    }),
    walk: walk_params(&arg.input)?,
    printer: scan_printer_spec(arg),
    result_encoding: vorpal_wire::ResultEncoding::Rendered,
    limits: Limits {
      local_max_results: arg.max_results.map(u32::from),
      ..Limits::default()
    },
    flow: FlowParams::default(),
    lang_env: lang_env.encode()?,
    expected_grammar_fingerprint: fingerprint::grammar_fingerprint(),
  })
}

pub(crate) fn build_run_job(arg: &RunArg, lang_env: &LangEnv) -> Result<JobSpec> {
  Ok(JobSpec {
    job_id: std::process::id() as u64,
    kind: JobKind::Run(RunJob {
      pattern: arg.matcher.pattern.clone(),
      selector: arg.matcher.selector.clone(),
      strictness: arg
        .matcher
        .strictness
        .as_ref()
        .map(|s| s.wire_name().to_string()),
      kind: arg.matcher.kind.clone(),
      rewrite: arg.rewrite.clone(),
      lang: arg.lang.map(|l| l.to_string()),
    }),
    walk: walk_params(&arg.input)?,
    printer: run_printer_spec(arg),
    result_encoding: vorpal_wire::ResultEncoding::Rendered,
    limits: Limits::default(),
    flow: FlowParams::default(),
    lang_env: lang_env.encode()?,
    expected_grammar_fingerprint: fingerprint::grammar_fingerprint(),
  })
}

// ---------------------------------------------------------------------------
// Agent side: rebuild args and printers
// ---------------------------------------------------------------------------

pub(crate) fn input_args_from_walk(walk: &WalkParams) -> InputArgs {
  InputArgs {
    paths: walk.paths.iter().map(Into::into).collect(),
    follow: walk.follow_links,
    no_ignore: walk
      .no_ignore
      .iter()
      .copied()
      .map(no_ignore_from_wire)
      .collect(),
    stdin: false,
    globs: walk.globs.clone(),
    threads: walk.threads as usize,
  }
}

fn neutral_output_args() -> crate::utils::OutputArgs {
  crate::utils::OutputArgs {
    interactive: false,
    update_all: false,
    files_with_matches: false,
    json: None,
    color: crate::print::ColorArg::Never,
    inspect: Default::default(),
  }
}

fn neutral_context_args() -> crate::utils::ContextArgs {
  crate::utils::ContextArgs {
    before: 0,
    after: 0,
    context: 0,
  }
}

/// Rebuild the `ScanArg` an agent-side `ScanWithConfig` needs. Only walk inputs, the local
/// max-results cap, and inert defaults — printing decisions live in the shipped `PrinterSpec`.
pub(crate) fn scan_arg_from_job(job: &JobSpec) -> Result<ScanArg> {
  let max_results = match job.limits.local_max_results {
    Some(n) => Some(
      u16::try_from(n).map_err(|_| anyhow!("local max-results {n} exceeds the CLI's u16 range"))?,
    ),
    None => None,
  };
  Ok(ScanArg::for_remote_agent(
    input_args_from_walk(&job.walk),
    neutral_output_args(),
    neutral_context_args(),
    max_results,
  ))
}

pub(crate) fn run_arg_from_job(job: &JobSpec, run: &RunJob) -> Result<RunArg> {
  let strictness = match &run.strictness {
    Some(name) => Some(
      Strictness::from_wire_name(name)
        .ok_or_else(|| anyhow!("unknown strictness `{name}` in job"))?,
    ),
    None => None,
  };
  let lang = match &run.lang {
    Some(name) => Some(
      name
        .parse::<SgLang>()
        .map_err(|e| anyhow!("unknown language `{name}` in job: {e}"))?,
    ),
    None => None,
  };
  Ok(RunArg::for_remote_agent(
    run.pattern.clone(),
    run.selector.clone(),
    strictness,
    run.kind.clone(),
    run.rewrite.clone(),
    lang,
    input_args_from_walk(&job.walk),
    neutral_output_args(),
    neutral_context_args(),
  ))
}

/// The four remotable printers, reconstructed with the coordinator's resolved decisions. The
/// agent only ever uses `get_processor()`; the writer half is a sink and is never written to.
pub(crate) enum AgentPrinter {
  Json(JSONPrinter<std::io::Sink>),
  Colored(ColoredPrinter<Buffer>),
  FileName(FileNamePrinter<Buffer>),
  Cloud(CloudPrinter<std::io::Sink>),
}

/// A writer whose `supports_color()` matches the coordinator's, so `get_processor` bakes the same
/// color decision into fragments.
fn color_carrier(buffer_color: bool) -> Buffer {
  if buffer_color {
    Buffer::ansi()
  } else {
    Buffer::no_color()
  }
}

fn style_choice(styles_colored: bool) -> ColorChoice {
  if styles_colored {
    ColorChoice::AlwaysAnsi
  } else {
    ColorChoice::Never
  }
}

pub(crate) fn printer_from_spec(spec: &PrinterSpec) -> AgentPrinter {
  match *spec {
    PrinterSpec::Json {
      style,
      context,
      include_metadata,
    } => AgentPrinter::Json(
      JSONPrinter::new(std::io::sink(), json_style_from_wire(style))
        .context(context)
        .include_metadata(include_metadata),
    ),
    PrinterSpec::Colored {
      buffer_color,
      styles_colored,
      heading,
      report_style,
      context,
    } => AgentPrinter::Colored(
      ColoredPrinter::new(color_carrier(buffer_color))
        .color(style_choice(styles_colored))
        .style(report_style_from_wire(report_style))
        .heading(if heading {
          Heading::Always
        } else {
          Heading::Never
        })
        .context(context),
    ),
    PrinterSpec::FileName {
      buffer_color,
      styles_colored,
    } => AgentPrinter::FileName(
      FileNamePrinter::new(color_carrier(buffer_color)).color(style_choice(styles_colored)),
    ),
    PrinterSpec::Cloud { platform } => AgentPrinter::Cloud(CloudPrinter::new(
      std::io::sink(),
      platform_from_wire(platform),
    )),
  }
}
