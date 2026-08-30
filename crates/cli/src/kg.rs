//! Knowledge-graph subcommands (§3.6): the npm-shipped `vorpal` binary carries the full KG
//! surface — `index`, `graph`, `search`, and the `mcp` daemon — over the same library code as
//! the standalone `vorpal-index`/`vorpal-mcp` tools.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, anyhow};
use clap::{Args, ValueEnum};

use crate::config::ProjectConfig;


/// Default index location relative to the indexed tree / working directory. Hidden, so the
/// ignore-respecting walker never indexes the index itself.
const DEFAULT_INDEX_DIR: &str = ".vorpal/index";

/// Output surface for the query verbs: `text` is the byte-stable human rendering; `json` is
/// the records/pagination envelope shared with the MCP server.
#[derive(Copy, Clone, PartialEq, Eq, Default, ValueEnum)]
enum OutputFormat {
  #[default]
  Text,
  Json,
  /// Token-oriented columnar text: columns declared once, directories grouped.
  Toon,
  /// LEAN (LLM-Efficient Adaptive Notation) tabular profile: leanest measured page format.
  Lean,
  /// One durable id per line (eid, falling back to the dense id).
  Ids,
}

/// Cursor/limit flags shared by the paged query verbs. Pagination is a machine-surface
/// feature: both flags require `--format json` (text output stays complete and unchanged).
#[derive(Args)]
struct PageArg {
  /// (json) Records per page (default 100, max 1000).
  #[clap(long, value_name = "N")]
  limit: Option<u64>,
  /// (json) Opaque page cursor from a previous page's `nextCursor`.
  #[clap(long, value_name = "CURSOR")]
  cursor: Option<String>,
}

impl PageArg {
  fn reject_for_text(&self, format: OutputFormat) -> Result<()> {
    if format == OutputFormat::Text && (self.limit.is_some() || self.cursor.is_some()) {
      anyhow::bail!("--limit/--cursor page the records surfaces: add --format json|toon|ids");
    }
    Ok(())
  }
}

/// Render one page's already-serialized envelope in the chosen machine format.
fn emit_machine(format: OutputFormat, value: &serde_json::Value) -> Result<String> {
  let rows = value
    .get("records")
    .and_then(serde_json::Value::as_array)
    .map(Vec::as_slice)
    .unwrap_or(&[]);
  Ok(match format {
    OutputFormat::Json => format!("{}\n", serde_json::to_string_pretty(value)?),
    OutputFormat::Toon => vorpal_index::records::toon_from_values(rows),
    OutputFormat::Lean => vorpal_index::records::lean_from_values(rows),
    OutputFormat::Ids => vorpal_index::records::ids_from_values(rows),
    OutputFormat::Text => unreachable_text()?,
  })
}

fn unreachable_text() -> Result<String> {
  anyhow::bail!("text format renders through the library surfaces, not the records envelope")
}

#[derive(Args)]
pub struct IndexArg {
  /// Source directory to index.
  #[clap(default_value = ".")]
  src: PathBuf,
  /// Index directory (default: `<src>/.vorpal/index`).
  #[clap(long)]
  out: Option<PathBuf>,
  /// Content-authoritative cache validation: verify every replay against the file's current
  /// bytes (immune to preserved-mtime edits; reads every candidate file). Default is
  /// fast-stat, which trusts size+mtime outside the racy window.
  #[clap(long)]
  verify: bool,
}

#[derive(Copy, Clone, ValueEnum)]
enum GraphVerb {
  /// Direct callers of a symbol (incoming `calls` edges).
  Callers,
  /// Direct referrers of a symbol (incoming `references` edges).
  Refs,
  /// Files importing a symbol (incoming `imports` edges).
  Importers,
  /// Types implementing/extending a symbol (incoming `implements` edges).
  Implementors,
  /// Definitions using a type (incoming `of_type` edges).
  Typeusers,
  /// Nodes matching a name.
  Node,
  /// Relation-specific transitive traversal, each reached node with its path to the seed.
  Reachable,
  /// The defining source of a symbol, sliced from its indexed span (digest-verified).
  Snippet,
  /// What this index contains: kinds, relations, grades, and tier state, with counts.
  Schema,
  /// Definitions with no semantic in-edges anywhere (suppression-honest dead-code leads).
  Dead,
  /// Per-file parse-coverage overview (error bytes/ratio, worst first).
  Coverage,
  /// Blast radius of changed files: git-diff-seeded transitive inbound closure.
  Impact,
  /// What changed between two generations (files, nodes by durable eid, edge counts).
  Diff,
  /// Orientation summary: module mass, hubs by in-degree, entry-point candidates.
  Architecture,
}

impl GraphVerb {
  fn as_str(self) -> &'static str {
    match self {
      GraphVerb::Callers => "callers",
      GraphVerb::Refs => "refs",
      GraphVerb::Importers => "importers",
      GraphVerb::Implementors => "implementors",
      GraphVerb::Typeusers => "typeusers",
      GraphVerb::Node => "node",
      GraphVerb::Reachable => "reachable",
      GraphVerb::Snippet => "snippet",
      GraphVerb::Schema => "schema",
      GraphVerb::Dead => "dead",
      GraphVerb::Coverage => "coverage",
      GraphVerb::Impact => "impact",
      GraphVerb::Diff => "diff",
      GraphVerb::Architecture => "architecture",
    }
  }
}

#[derive(Args)]
pub struct GraphArg {
  /// Which relation to query.
  #[clap(value_enum)]
  verb: GraphVerb,
  /// Exact symbol name (not used by `schema`).
  name: Option<String>,
  /// Refine to definitions whose file path ends with this suffix.
  #[clap(long, value_name = "SUFFIX")]
  path: Option<String>,
  /// Refine to one symbol kind (function, method, struct, field, …).
  #[clap(long, value_name = "KIND")]
  kind: Option<String>,
  /// Query exactly this node id (from `graph node <name>` or an ambiguity listing).
  #[clap(long, value_name = "ID")]
  id: Option<u64>,
  /// Durable external id (32 hex chars from `graph node` output) — survives rebuilds.
  /// Also accepted anywhere a name is, as `eid:<hex>`.
  #[clap(long, value_name = "HEX")]
  eid: Option<String>,
  /// Merge results across ALL same-named definitions (the pre-selector behavior).
  #[clap(long)]
  all: bool,
  /// (reachable) Traversal direction: `in` = everything reaching the symbol (transitive
  /// callers), `out` = everything it reaches, `both` = the undirected closure (hops may
  /// alternate orientation). Default `in`.
  #[clap(long, value_name = "in|out|both", default_value = "in")]
  direction: String,
  /// (reachable) Comma-separated edge types to follow (calls, references, imports,
  /// implements, of_type, defines, has_method, has_field, overrides). Default `calls`.
  #[clap(long, value_name = "RELS", default_value = "calls")]
  relations: String,
  /// (reachable) Maximum hops (0 = unbounded).
  #[clap(long, value_name = "N", default_value_t = 0)]
  depth: u32,
  /// (reachable) Only traverse edges at this resolution grade or better
  /// (exact | constrained | heuristic). Absent = structural edges included.
  #[clap(long, value_name = "GRADE")]
  min_grade: Option<String>,
  /// (node) List nodes whose name matches this regex instead of an exact name.
  #[clap(long, value_name = "REGEX")]
  pattern: Option<String>,
  /// (architecture) Rows per section.
  #[clap(long, value_name = "N", default_value_t = 20)]
  top: usize,
  /// (diff) Older generation: content id, path, or `prev` (default).
  #[clap(long, value_name = "GEN", default_value = "prev")]
  from: String,
  /// (diff) Newer generation: content id, path, or `CURRENT` (default).
  #[clap(long, value_name = "GEN", default_value = "CURRENT")]
  to: String,
  /// (impact) Diff base: everything the branch/worktree changes relative to this ref
  /// (merge-base semantics). Absent = uncommitted changes only.
  #[clap(long, value_name = "REF")]
  since: Option<String>,
  /// (impact) The indexed source root (a git repo).
  #[clap(long, value_name = "DIR", default_value = ".")]
  src: PathBuf,
  /// (dead) Refine to definitions whose file path starts with this prefix.
  #[clap(long, value_name = "PREFIX")]
  prefix: Option<String>,
  /// (dead) Only exported definitions.
  #[clap(long)]
  exported: bool,
  /// (dead) Exclude test-classified paths.
  #[clap(long)]
  no_tests: bool,
  /// (snippet) Whole context lines to include around the definition span.
  #[clap(long, value_name = "N", default_value_t = 0)]
  context: usize,
  /// (snippet) Byte cap per snippet body (clamped to 64..262144).
  #[clap(long, value_name = "BYTES", default_value_t = 16384)]
  max_bytes: usize,
  /// Append each result's node id (stable within this index generation).
  #[clap(long)]
  ids: bool,
  /// Output format: byte-stable text (default) or the paged records envelope.
  #[clap(long, value_enum, default_value_t)]
  format: OutputFormat,
  #[clap(flatten)]
  page: PageArg,
  /// Index directory (default: `./.vorpal/index`).
  #[clap(long)]
  index: Option<PathBuf>,
}

#[derive(Args)]
pub struct SearchArg {
  /// Free-text query — hybrid retrieval fusing exact/token name matches, lexical-embedding
  /// similarity, and graph in-degree (RRF). With --code, an ast-grep PATTERN instead.
  query: String,
  /// Structural mode: treat the query as an ast-grep pattern, run it over the generation's
  /// own (digest-verified) files, and rank enclosing definitions by semantic in-degree.
  /// C/C++ call patterns need statement form (`kfree($A);`) — bare calls parse as
  /// declarations.
  #[clap(long)]
  code: bool,
  /// Max results.
  #[clap(short, default_value_t = 10)]
  k: usize,
  /// Filter: definition file path must start with this prefix (package/subtree scoping).
  #[clap(long, value_name = "PREFIX")]
  prefix: Option<String>,
  /// Filter: definition file path must end with this suffix.
  #[clap(long, value_name = "SUFFIX")]
  path: Option<String>,
  /// Filter: symbol kind (function, method, struct, field, …).
  #[clap(long, value_name = "KIND")]
  kind: Option<String>,
  /// Filter: language name or alias (rust, py, ts, …).
  #[clap(long, value_name = "LANG")]
  lang: Option<String>,
  /// Filter: only exported definitions.
  #[clap(long)]
  exported: bool,
  /// Filter: exclude test-classified paths (tests/, __tests__/, *_test.*, test_*.py, …).
  #[clap(long)]
  no_tests: bool,
  /// Output format: byte-stable text (default) or the paged records envelope.
  #[clap(long, value_enum, default_value_t)]
  format: OutputFormat,
  #[clap(flatten)]
  page: PageArg,
  /// Index directory (default: `./.vorpal/index`).
  #[clap(long)]
  index: Option<PathBuf>,
}

#[derive(Args)]
pub struct McpArg {
  /// Index directory the daemon serves (default: `./.vorpal/index`).
  #[clap(long)]
  index: Option<PathBuf>,
  /// Tool profile: `scout` (read-only navigation), `analysis` (+ traversal/evidence/health),
  /// `full` (everything).
  #[clap(long, default_value = "full")]
  profile: String,
  /// Disable the proactive background rebuild (D1): the index then refreshes lazily on the
  /// first query after a change instead of as soon as the tree goes quiet.
  #[clap(long)]
  no_watch_rebuild: bool,
  /// Serve every enrolled project from this one daemon (registry: `vorpal mcp allow`).
  #[clap(long)]
  projects: bool,
  #[clap(subcommand)]
  action: Option<McpAction>,
}

/// Registry management — the HUMAN-ONLY enrollment surface. These commands exist exactly so
/// that the MCP protocol never has to (and never can) touch the registry: a confirmation
/// delivered through MCP would be answered by the same agent that may have been influenced.
#[derive(clap::Subcommand)]
pub enum McpAction {
  /// Enroll a source root so `vorpal mcp --projects` may serve it.
  Allow {
    /// Source directory to enroll.
    path: PathBuf,
    /// Project name (default: the directory name).
    #[clap(long)]
    name: Option<String>,
    /// Index root (default: `<path>/.vorpal/index`).
    #[clap(long)]
    index: Option<PathBuf>,
  },
  /// Remove an enrolled project by name.
  Deny { name: String },
  /// List enrolled projects.
  Projects,
  /// Write this machine's MCP client configs to launch vorpal (idempotent; backups taken).
  Install {
    /// Which client to configure.
    #[clap(long, value_enum, default_value = "all")]
    client: crate::mcp_install::Client,
    /// Command to write into the config (default: this executable's absolute path).
    #[clap(long)]
    command: Option<String>,
    /// Print what would be written without touching anything.
    #[clap(long)]
    dry_run: bool,
  },
}

fn index_dir(explicit: Option<PathBuf>) -> PathBuf {
  explicit.unwrap_or_else(|| PathBuf::from(DEFAULT_INDEX_DIR))
}

fn boxed(err: Box<dyn std::error::Error>) -> anyhow::Error {
  anyhow!(err.to_string())
}

/// The extraction environment a project configures (F-M3): each custom language's declared
/// `outlineRules` file becomes a rule source whose origin is the path exactly as written in
/// the config — relative, so the rules digest is machine-independent. An unreadable rules
/// file is a hard error naming it; a custom language declaring none is reported (pattern-only,
/// not indexed), never silently skipped.
fn extraction_env_from_project(
  project: Option<&ProjectConfig>,
) -> Result<vorpal_index::ExtractionEnv> {
  let mut env = vorpal_index::ExtractionEnv::default();
  let Some(project) = project else {
    return Ok(env);
  };
  let Some(customs) = project.custom_languages.as_ref() else {
    return Ok(env);
  };
  let mut pattern_only: Vec<&str> = Vec::new();
  for (name, lang) in customs {
    if lang.outline_rules.is_none() && lang.ref_spec.is_none() {
      pattern_only.push(name);
      continue;
    }
    if let Some(declared) = lang.outline_rules.as_ref() {
      let path = project.project_dir.join(declared);
      let yaml = std::fs::read_to_string(&path).with_context(|| {
        format!("reading outline rules for custom language '{name}': {}", path.display())
      })?;
      env.outline_sources.push(vorpal_index::RuleSource {
        origin: declared.to_string_lossy().into_owned(),
        yaml,
      });
    }
    if let Some(declared) = lang.ref_spec.as_ref() {
      let path = project.project_dir.join(declared);
      let yaml = std::fs::read_to_string(&path).with_context(|| {
        format!("reading ref spec for custom language '{name}': {}", path.display())
      })?;
      env.ref_spec_sources.push(vorpal_index::RuleSource {
        origin: declared.to_string_lossy().into_owned(),
        yaml,
      });
    }
    if let Some(canary) = lang.canary.as_ref() {
      env.canaries.push(vorpal_index::DynamicCanary {
        lang: name.clone(),
        path: canary.path.clone(),
        source: canary.source.clone(),
        min_items: canary.min_items,
        min_refs: canary.min_refs,
      });
    }
  }
  // languageInjections shape index extraction (C3a) — fold the exact config bytes into the
  // rules digest so editing an injection rule re-keys products like an outline-rule edit.
  if !project.language_injections.is_empty() {
    let yaml = serde_yaml::to_string(&project.language_injections)
      .context("serializing languageInjections for the extraction identity")?;
    env.injection_config = Some(vorpal_index::RuleSource {
      origin: "vorpalconfig.yml#languageInjections".into(),
      yaml,
    });
  }
  if !pattern_only.is_empty() {
    pattern_only.sort_unstable();
    println!(
      "note: {} custom language(s) declare no outlineRules and are pattern-only, not indexed: {}",
      pattern_only.len(),
      pattern_only.join(", ")
    );
  }
  Ok(env)
}

pub fn run_index(arg: IndexArg, project: Result<ProjectConfig>) -> Result<ExitCode> {
  let out = arg.out.unwrap_or_else(|| arg.src.join(DEFAULT_INDEX_DIR));
  let mode = if arg.verify {
    vorpal_index::CacheMode::Verified
  } else {
    vorpal_index::CacheMode::default()
  };
  // Custom/dynamic languages were registered at CLI setup (the one-shot dlopen); here their
  // configured outline rules extend extraction (F-M3). No project config = bundled behavior.
  let env = extraction_env_from_project(project.ok().as_ref())?;
  let report =
    vorpal_index::build_index_env(&arg.src, &out, mode, Default::default(), &env)
      .map_err(boxed)
      .with_context(|| format!("indexing {}", arg.src.display()))?;
  if report.reused {
    println!("unchanged — reused existing index ({} nodes)", report.nodes);
  } else {
    println!(
      "parsed {} files ({} replayed from cache) → {} nodes; refs: {} resolved, {} ambiguous, {} external, {} masked",
      report.indexed,
      report.skipped,
      report.nodes,
      report.resolved,
      report.ambiguous,
      report.external,
      report.masked
    );
    if report.error_files > 0 {
      println!(
        "note: {} files had parse errors ({} ERROR nodes total; some definitions may be \
         missing) — tree-sitter could not fully parse them",
        report.error_files, report.error_nodes
      );
    }
  }
  if !report.unverified_langs.is_empty() {
    println!(
      "note: {} dynamic language(s) indexed without a canary (best-effort, unverified): {} —        add `canary:` to their custom language config",
      report.unverified_langs.len(),
      report.unverified_langs.join(", ")
    );
  }
  if report.cache_mode != "fast-stat" {
    println!("cache mode: {}", report.cache_mode);
  }
  println!("index: {}", out.display());
  Ok(ExitCode::SUCCESS)
}

pub fn run_graph(arg: GraphArg) -> Result<ExitCode> {
  arg.page.reject_for_text(arg.format)?;
  let dir = index_dir(arg.index);

  if matches!(arg.verb, GraphVerb::Schema) {
    let kg = vorpal_index::Kg::load(&dir)
      .map_err(|err| anyhow::anyhow!(err.to_string()))
      .with_context(|| missing_index_hint(&dir))?;
    let gen_dir = vorpal_index::resolve_index_dir(&dir);
    let report = vorpal_index::records::schema_report(&kg, Some(&gen_dir));
    match arg.format {
      OutputFormat::Text => print!("{}", vorpal_index::records::render_schema(&report)),
      OutputFormat::Json => println!("{}", serde_json::to_string_pretty(&report)?),
      OutputFormat::Toon | OutputFormat::Lean | OutputFormat::Ids => {
        anyhow::bail!("schema is a single report — use --format text or json")
      }
    }
    return Ok(ExitCode::SUCCESS);
  }

  if matches!(arg.verb, GraphVerb::Architecture) {
    let kg = vorpal_index::Kg::load(&dir)
      .map_err(|err| anyhow::anyhow!(err.to_string()))
      .with_context(|| missing_index_hint(&dir))?;
    let gen_dir = vorpal_index::resolve_index_dir(&dir);
    let report =
      vorpal_index::records::architecture_report(&kg, Some(&gen_dir), arg.top.clamp(1, 500));
    match arg.format {
      OutputFormat::Text => print!("{}", vorpal_index::records::render_architecture(&report)),
      OutputFormat::Json => println!("{}", serde_json::to_string_pretty(&report)?),
      OutputFormat::Toon | OutputFormat::Lean | OutputFormat::Ids => {
        anyhow::bail!("architecture is a single report — use --format text or json")
      }
    }
    return Ok(ExitCode::SUCCESS);
  }

  if matches!(arg.verb, GraphVerb::Diff) {
    let from_dir = vorpal_index::gendiff::resolve_generation(&dir, &arg.from)
      .map_err(anyhow::Error::msg)?;
    let to_dir = vorpal_index::gendiff::resolve_generation(&dir, &arg.to)
      .map_err(anyhow::Error::msg)?;
    let from_kg = vorpal_index::Kg::load(&from_dir).map_err(|err| anyhow!(err.to_string()))?;
    let to_kg = vorpal_index::Kg::load(&to_dir).map_err(|err| anyhow!(err.to_string()))?;
    let label = |dir: &std::path::Path| {
      dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
    };
    let diff = vorpal_index::gendiff::diff(&from_kg, &to_kg, &label(&from_dir), &label(&to_dir));
    let page = match arg.format {
      OutputFormat::Text => vorpal_index::records::PageRequest { cursor: None, limit: Some(200) },
      _ => vorpal_index::records::PageRequest {
        cursor: arg.page.cursor.as_deref(),
        limit: arg.page.limit,
      },
    };
    let report = vorpal_index::records::diff_page(&from_kg, &to_kg, diff, page)
      .map_err(anyhow::Error::msg)?;
    match arg.format {
      OutputFormat::Text => print!("{}", vorpal_index::records::render_diff(&report)),
      machine => print!("{}", emit_machine(machine, &serde_json::to_value(&report)?)?),
    }
    return Ok(ExitCode::SUCCESS);
  }

  if matches!(arg.verb, GraphVerb::Impact) {
    let kg = vorpal_index::Kg::load(&dir)
      .map_err(|err| anyhow::anyhow!(err.to_string()))
      .with_context(|| missing_index_hint(&dir))?;
    let changed = vorpal_index::impact::changed_paths(&arg.src, arg.since.as_deref())
      .map_err(anyhow::Error::msg)?;
    let (seeds, missing) = vorpal_index::impact::seeds_for_paths(&kg, &arg.src, &changed);
    let mut relations = Vec::new();
    for rel in arg.relations.split(',').filter(|r| !r.trim().is_empty()) {
      relations.push(
        vorpal_index::EdgeType::from_name(rel.trim())
          .ok_or_else(|| anyhow::anyhow!("unknown relation '{rel}'"))?,
      );
    }
    let max_depth = (arg.depth > 0).then_some(arg.depth);
    let min_confidence =
      vorpal_index::min_confidence_for_grade(arg.min_grade.as_deref()).map_err(boxed)?;
    let page = match arg.format {
      OutputFormat::Text => vorpal_index::records::PageRequest { cursor: None, limit: Some(200) },
      _ => vorpal_index::records::PageRequest {
        cursor: arg.page.cursor.as_deref(),
        limit: arg.page.limit,
      },
    };
    let report = vorpal_index::records::impact_page(
      &kg,
      &seeds,
      &relations,
      max_depth,
      min_confidence,
      (changed.len(), missing),
      page,
    )
    .map_err(anyhow::Error::msg)?;
    match arg.format {
      OutputFormat::Text => print!("{}", vorpal_index::records::render_impact(&report)),
      machine => {
        let mut value = serde_json::json!({
          "outcome": "hits",
          "records": serde_json::to_value(&report.records)?,
          "total": report.total,
          "truncated": report.end < report.total,
          "changedFiles": report.changed_files,
          "missingFiles": report.missing_files,
          "seeds": report.seeds,
        });
        if report.end < report.total {
          value["nextCursor"] = serde_json::json!(format!("o:{}", report.end));
        }
        print!("{}", emit_machine(machine, &value)?);
      }
    }
    return Ok(ExitCode::SUCCESS);
  }

  if matches!(arg.verb, GraphVerb::Coverage) {
    let gen_dir = vorpal_index::resolve_index_dir(&dir);
    if !gen_dir.join("manifest.bin").exists() {
      anyhow::bail!(missing_index_hint(&dir));
    }
    let report = vorpal_index::records::coverage_records(Some(&gen_dir));
    match arg.format {
      OutputFormat::Text => print!("{}", vorpal_index::records::render_coverage(&report)),
      machine => {
        let mut value = vorpal_index::records::paged_value(
          &report.records,
          arg.page.cursor.as_deref(),
          arg.page.limit,
          "hits",
        )
        .map_err(anyhow::Error::msg)?;
        value["totalFiles"] = report.total_files.into();
        value["damagedFiles"] = report.damaged_files.into();
        value["totalErrorBytes"] = report.total_error_bytes.into();
        print!("{}", emit_machine(machine, &value)?);
      }
    }
    return Ok(ExitCode::SUCCESS);
  }

  if matches!(arg.verb, GraphVerb::Dead) {
    let kg = vorpal_index::Kg::load(&dir)
      .map_err(|err| anyhow::anyhow!(err.to_string()))
      .with_context(|| missing_index_hint(&dir))?;
    let gen_dir = vorpal_index::resolve_index_dir(&dir);
    let filter = vorpal_index::records::DeadFilter {
      kind: arg.kind,
      path_prefix: arg.prefix,
      path_suffix: arg.path,
      exported_only: arg.exported,
      exclude_tests: arg.no_tests,
    };
    match arg.format {
      OutputFormat::Text => {
        // Text = the first 200 candidates (whole-scan totals in the head).
        let report = vorpal_index::records::dead_records_page(
          &kg,
          Some(&gen_dir),
          &filter,
          vorpal_index::records::PageRequest { cursor: None, limit: Some(200) },
        )
        .map_err(anyhow::Error::msg)?;
        print!("{}", vorpal_index::records::render_dead(&report));
      }
      machine => {
        let report = vorpal_index::records::dead_records_page(
          &kg,
          Some(&gen_dir),
          &filter,
          vorpal_index::records::PageRequest {
            cursor: arg.page.cursor.as_deref(),
            limit: arg.page.limit,
          },
        )
        .map_err(anyhow::Error::msg)?;
        let mut value = serde_json::json!({
          "outcome": "hits",
          "records": serde_json::to_value(&report.records)?,
          "total": report.total,
          "truncated": report.end < report.total,
          "suppressedReferenced": report.suppressed_referenced,
          "suppressedDamaged": report.suppressed_damaged,
          "nameSuppression": report.name_suppression,
        });
        if report.end < report.total {
          value["nextCursor"] = serde_json::json!(format!("o:{}", report.end));
        }
        print!("{}", emit_machine(machine, &value)?);
      }
    }
    return Ok(ExitCode::SUCCESS);
  }

  if let Some(pattern) = arg.pattern {
    if !matches!(arg.verb, GraphVerb::Node) {
      anyhow::bail!("--pattern is a listing: use `graph node --pattern <regex>`");
    }
    let kg = vorpal_index::Kg::load(&dir)
      .map_err(|err| anyhow::anyhow!(err.to_string()))
      .with_context(|| missing_index_hint(&dir))?;
    match arg.format {
      OutputFormat::Text => print!(
        "{}",
        vorpal_index::pattern_query_on(&kg, &pattern, 200).map_err(boxed)?
      ),
      machine => {
        let records =
          vorpal_index::records::pattern_records(&kg, &pattern).map_err(anyhow::Error::msg)?;
        let value = vorpal_index::records::paged_value(
          &records,
          arg.page.cursor.as_deref(),
          arg.page.limit,
          "hits",
        )
        .map_err(anyhow::Error::msg)?;
        print!("{}", emit_machine(machine, &value)?);
      }
    }
    return Ok(ExitCode::SUCCESS);
  }

  let name = arg
    .name
    .ok_or_else(|| anyhow!("`graph {}` needs a symbol name", arg.verb.as_str()))?;
  let eid = match arg.eid.as_deref() {
    Some(hex) => Some(
      u128::from_str_radix(hex, 16)
        .map_err(|_| anyhow::anyhow!("malformed external id '{hex}' (expect 32 hex chars)"))?,
    ),
    None => None,
  };
  let target = vorpal_index::GraphTarget {
    name,
    id: arg.id,
    external_id: eid,
    path_suffix: arg.path,
    kind: arg.kind,
    merge_all: arg.all,
    show_ids: arg.ids,
  };
  // Reachable parses its traversal flags in both formats; the other verbs only need them
  // rendered or recorded.
  let traversal = if matches!(arg.verb, GraphVerb::Reachable) {
    let direction = match arg.direction.as_str() {
      "in" => vorpal_index::Direction::In,
      "out" => vorpal_index::Direction::Out,
      "both" => vorpal_index::Direction::Both,
      other => anyhow::bail!("--direction must be `in`, `out`, or `both`, got '{other}'"),
    };
    let mut relations = Vec::new();
    for rel in arg.relations.split(',').filter(|r| !r.trim().is_empty()) {
      relations.push(
        vorpal_index::EdgeType::from_name(rel.trim())
          .ok_or_else(|| anyhow::anyhow!("unknown relation '{rel}'"))?,
      );
    }
    let max_depth = (arg.depth > 0).then_some(arg.depth);
    let min_confidence =
      vorpal_index::min_confidence_for_grade(arg.min_grade.as_deref()).map_err(boxed)?;
    Some((direction, relations, max_depth, min_confidence))
  } else {
    None
  };

  if matches!(arg.verb, GraphVerb::Snippet) {
    let kg = vorpal_index::Kg::load(&dir)
      .map_err(|err| anyhow::anyhow!(err.to_string()))
      .with_context(|| missing_index_hint(&dir))?;
    let gen_dir = vorpal_index::resolve_index_dir(&dir);
    let max_bytes = arg.max_bytes.clamp(64, 262_144);
    let output = match arg.format {
      OutputFormat::Text => vorpal_index::snippet_query_on(
        &kg,
        Some(&gen_dir),
        &target,
        arg.context,
        max_bytes,
      )
      .map_err(snippet_error)?,
      machine => {
        let selected = vorpal_index::records::snippet_records(
          &kg,
          Some(&gen_dir),
          &target,
          arg.context,
          max_bytes,
        )
        .map_err(snippet_error)?;
        let value = vorpal_index::records::selected_value(
          selected,
          arg.page.cursor.as_deref(),
          arg.page.limit,
        )
        .map_err(anyhow::Error::msg)?;
        emit_machine(machine, &value)?
      }
    };
    print!("{output}");
    return Ok(ExitCode::SUCCESS);
  }

  let output = match (arg.format, &traversal) {
    (OutputFormat::Text, Some((direction, relations, max_depth, min_confidence))) => {
      let kg = vorpal_index::Kg::load(&dir)
        .map_err(|err| anyhow::anyhow!(err.to_string()))
        .with_context(|| missing_index_hint(&dir))?;
      vorpal_index::reachable_query_on(&kg, &target, *direction, relations, *max_depth, *min_confidence)
        .map_err(boxed)?
    }
    (OutputFormat::Text, None) => vorpal_index::graph_query_selected(&dir, arg.verb.as_str(), &target)
      .map_err(boxed)
      .with_context(|| missing_index_hint(&dir))?,
    (machine, _) => {
      let kg = vorpal_index::Kg::load(&dir)
        .map_err(|err| anyhow::anyhow!(err.to_string()))
        .with_context(|| missing_index_hint(&dir))?;
      let cursor = arg.page.cursor.as_deref();
      let value = match (&traversal, arg.verb) {
        (Some((direction, relations, max_depth, min_confidence)), _) => {
          vorpal_index::records::selected_page_value(
            vorpal_index::records::reach_records_page(
              &kg,
              &target,
              *direction,
              relations,
              *max_depth,
              *min_confidence,
              vorpal_index::records::PageRequest {
                cursor,
                limit: arg.page.limit,
              },
            )
            .map_err(anyhow::Error::msg)?,
            cursor,
            arg.page.limit,
          )
          .map_err(anyhow::Error::msg)?
        }
        (None, GraphVerb::Node) => vorpal_index::records::paged_value(
          &vorpal_index::records::listing_records(&kg, &target).map_err(anyhow::Error::msg)?,
          cursor,
          arg.page.limit,
          "hits",
        )
        .map_err(anyhow::Error::msg)?,
        (None, verb) => vorpal_index::records::selected_value(
          vorpal_index::records::related_records(&kg, verb.as_str(), &target)
            .map_err(anyhow::Error::msg)?,
          cursor,
          arg.page.limit,
        )
        .map_err(anyhow::Error::msg)?,
      };
      emit_machine(machine, &value)?
    }
  };
  print!("{output}");
  Ok(ExitCode::SUCCESS)
}

pub fn run_search(arg: SearchArg) -> Result<ExitCode> {
  arg.page.reject_for_text(arg.format)?;
  let dir = index_dir(arg.index);
  if arg.code {
    let kg = vorpal_index::Kg::load(&dir)
      .map_err(|err| anyhow::anyhow!(err.to_string()))
      .with_context(|| missing_index_hint(&dir))?;
    let gen_dir = vorpal_index::resolve_index_dir(&dir);
    let report = vorpal_index::records::code_search(
      &kg,
      Some(&gen_dir),
      &arg.query,
      arg.lang.as_deref(),
      arg.prefix.as_deref(),
      arg.k.max(1),
    )
    .map_err(anyhow::Error::msg)?;
    match arg.format {
      OutputFormat::Text => print!("{}", vorpal_index::records::render_code_search(&report)),
      machine => {
        let value = vorpal_index::records::paged_value(
          &report.records,
          arg.page.cursor.as_deref(),
          arg.page.limit,
          "hits",
        )
        .map_err(anyhow::Error::msg)?;
        let mut value = value;
        value["staleFiles"] = report.stale_files.into();
        value["unreadableFiles"] = report.unreadable_files.into();
        value["scannedFiles"] = report.scanned_files.into();
        value["totalMatches"] = report.total_matches.into();
        print!("{}", emit_machine(machine, &value)?);
      }
    }
    return Ok(ExitCode::SUCCESS);
  }
  let filter = vorpal_index::SearchFilter {
    path_prefix: arg.prefix,
    path_suffix: arg.path,
    kind: arg.kind,
    lang: arg.lang,
    exported_only: arg.exported,
    exclude_tests: arg.no_tests,
  };
  match arg.format {
    OutputFormat::Text => {
      let rendered = if filter.is_empty() {
        vorpal_index::search_index(&dir, &arg.query, arg.k)
      } else {
        vorpal_index::search_index_filtered(&dir, &arg.query, arg.k, &filter)
      }
      .map_err(boxed)
      .with_context(|| missing_index_hint(&dir))?;
      if rendered.is_empty() {
        println!("(no results for '{}')", arg.query);
      } else {
        print!("{rendered}");
      }
    }
    machine => {
      let records = vorpal_index::search_records_filtered(&dir, &arg.query, arg.k, &filter)
        .map_err(boxed)
        .with_context(|| missing_index_hint(&dir))?;
      let value = vorpal_index::records::paged_value(
        &records,
        arg.page.cursor.as_deref(),
        arg.page.limit,
        "hits",
      )
      .map_err(anyhow::Error::msg)?;
      print!("{}", emit_machine(machine, &value)?);
    }
  }
  Ok(ExitCode::SUCCESS)
}

pub fn run_mcp(arg: McpArg, project: Result<ProjectConfig>) -> Result<ExitCode> {
  if let Some(action) = arg.action {
    return run_mcp_action(action);
  }
  let profile = vorpal_mcp::Profile::parse(&arg.profile)
    .ok_or_else(|| anyhow!("--profile must be full, analysis, or scout"))?;
  if arg.projects {
    vorpal_mcp::serve_stdio_projects(profile)?;
    return Ok(ExitCode::SUCCESS);
  }
  // Custom languages were registered (the one-shot dlopen) at CLI setup, before serving
  // begins; the daemon itself can never load code. Its rebuilds run under the same
  // extraction environment `vorpal index` uses.
  let env = extraction_env_from_project(project.ok().as_ref())?;
  vorpal_mcp::serve_stdio_opts(index_dir(arg.index), profile, env, !arg.no_watch_rebuild)?;
  Ok(ExitCode::SUCCESS)
}

fn run_mcp_action(action: McpAction) -> Result<ExitCode> {
  match action {
    McpAction::Allow { path, name, index } => {
      let (name, entry, file) =
        vorpal_mcp::registry::enroll(&path, name.as_deref(), index.as_deref())
          .map_err(|err| anyhow!(err))?;
      println!(
        "enrolled '{name}': src={} index={} ({})",
        entry.src.display(),
        entry.index.display(),
        file.display()
      );
    }
    McpAction::Deny { name } => {
      let file = vorpal_mcp::registry::remove(&name).map_err(|err| anyhow!(err))?;
      println!("removed '{name}' ({})", file.display());
    }
    McpAction::Install {
      client,
      command,
      dry_run,
    } => {
      crate::mcp_install::run_install(client, command.as_deref(), dry_run)?;
    }
    McpAction::Projects => {
      let projects = vorpal_mcp::registry::load().map_err(|err| anyhow!(err))?;
      if projects.is_empty() {
        println!("no projects enrolled (enroll one: `vorpal mcp allow <path>`)");
      }
      for (name, entry) in projects {
        println!("{name}  src={}  index={}", entry.src.display(), entry.index.display());
      }
    }
  }
  Ok(ExitCode::SUCCESS)
}

fn missing_index_hint(dir: &Path) -> String {
  format!(
    "querying index at {} (build one first: `vorpal index <src>`)",
    dir.display()
  )
}

fn snippet_error(err: vorpal_index::records::SnippetError) -> anyhow::Error {
  match err {
    vorpal_index::records::SnippetError::Stale(message)
    | vorpal_index::records::SnippetError::Other(message) => anyhow!(message),
  }
}
