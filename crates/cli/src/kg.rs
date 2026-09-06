//! Knowledge-graph subcommands (§3.6): the npm-shipped `vorpal` binary carries the full KG
//! surface — `index`, `graph`, `search`, and the `mcp` daemon — over the same library code as
//! the standalone `vorpal-index`/`vorpal-mcp` tools.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, anyhow};
use clap::{Args, ValueEnum};

use crate::config::ProjectConfig;
use crate::lang::CustomLang;


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
  /// Direct callers of a symbol (incoming `calls` edges), each with its call site.
  Callers,
  /// Direct callees of a symbol (outgoing `calls` edges), each with the call site inside
  /// the symbol's own body.
  Callees,
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
  /// Outgoing data-flow rows for a definition: which arguments flow into which callees.
  Flows,
  /// Near-clones of a definition (`similar_to` edges; confidence = estimated similarity).
  Similar,
  /// Runtime-observed calls for a definition (from ingested traces), both directions.
  Observed,
}

impl GraphVerb {
  fn as_str(self) -> &'static str {
    match self {
      GraphVerb::Callers => "callers",
      GraphVerb::Callees => "callees",
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
      GraphVerb::Flows => "flows",
      GraphVerb::Similar => "similar",
      GraphVerb::Observed => "observed",
    }
  }
}

/// `vorpal query '<text>'` (G-M4): the Cypher-shaped read-only query language.
#[derive(Args)]
pub struct QueryArg {
  /// Query text, e.g.
  /// 'MATCH (f:Function)-[:calls*1..3]->(g {name: "resolve_target"}) RETURN f.name LIMIT 20'
  text: String,
  /// Index directory (default .vorpal/index).
  #[clap(long, value_name = "DIR")]
  index: Option<PathBuf>,
  /// Output: text table or the QueryResult JSON document.
  #[clap(long, value_enum, default_value_t = OutputFormat::Text)]
  format: OutputFormat,
}

pub fn run_query(arg: QueryArg) -> Result<ExitCode> {
  if !matches!(arg.format, OutputFormat::Text | OutputFormat::Json) {
    return Err(anyhow!("`query` renders --format text or json"));
  }
  let dir = index_dir(arg.index);
  let kg = vorpal_index::Kg::load(&dir)
    .map_err(|err| anyhow!(err.to_string()))
    .with_context(|| missing_index_hint(&dir))?;
  let result = match vorpal_query::run(&kg, &arg.text) {
    Ok(result) => result,
    Err(err) => {
      // Query mistakes are user-facing teaching errors, not stack noise: print the typed
      // message (it names the byte offset / boundary / ceiling) and exit nonzero.
      eprintln!("{err}");
      return Ok(ExitCode::FAILURE);
    }
  };
  match arg.format {
    OutputFormat::Json => println!("{}", serde_json::to_string_pretty(&result)?),
    _ => {
      println!("{}", result.columns.join(" | "));
      for row in &result.rows {
        println!(
          "{}",
          row.iter().map(ToString::to_string).collect::<Vec<_>>().join(" | ")
        );
      }
      let shown = result.rows.len() as u64;
      if shown != result.total_rows {
        println!("({shown} of {} rows)", result.total_rows);
      } else {
        println!("({shown} row{})", if shown == 1 { "" } else { "s" });
      }
    }
  }
  Ok(ExitCode::SUCCESS)
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
  /// implements, of_type, defines, has_method, has_field, overrides, data_flows,
  /// changes_with, similar_to, requests, notifies).
  /// Default `calls`.
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
  /// similarity, and graph in-degree (RRF). Two or more double-quoted phrases joined by
  /// literal AND (`'"retry logic" AND "connection pool"'`) intersect per-phrase results
  /// (conjunction, min-of-scores). With --code, an ast-grep PATTERN instead.
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
  /// Show the base fused ordering and the encoder-reranked ordering side by side —
  /// ONE search, two views (requires the advanced embedder: `vorpal enable` or
  /// `encoderDir`). Text output only.
  #[clap(long)]
  ranked: bool,
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

pub(crate) fn index_dir(explicit: Option<PathBuf>) -> PathBuf {
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
/// Union every enrolled project's custom-language declarations into ONE registration map
/// (D4 v2): library paths absolutize per project; an extension may have one owner; an
/// extension routing to a builtin grammar refuses (shadowing cannot be consented to
/// per-project); one language name must mean one definition. Every refusal names the
/// projects involved.
fn union_custom_languages(
  declared: Vec<(String, PathBuf, std::collections::HashMap<String, CustomLang>)>,
) -> Result<std::collections::HashMap<String, CustomLang>> {
  let mut union: std::collections::HashMap<String, (CustomLang, String)> = Default::default();
  let mut claimed_ext: std::collections::HashMap<String, (String, String)> = Default::default();
  for (project, project_dir, customs) in declared {
    let mut customs: Vec<(String, CustomLang)> = customs.into_iter().collect();
    customs.sort_by(|a, b| a.0.cmp(&b.0)); // deterministic refusal order
    for (lang_name, custom) in customs {
      let custom = absolutized_custom(custom, &project_dir);
      for ext in &custom.extensions {
        let probe = format!("probe.{ext}");
        // Customs are not yet registered, so the registry router answers builtins only.
        if matches!(
          vorpal_lang_registry::from_path(std::path::Path::new(&probe)),
          Some(vorpal_lang_registry::SgLang::Builtin(_))
        ) {
          return Err(anyhow!(
            "project '{project}': custom language '{lang_name}' claims extension '.{ext}', \
             which routes to a builtin grammar — shadowing a builtin cannot be consented \
             to per-project; multi-project serving refuses it"
          ));
        }
        if let Some((other_lang, other_project)) =
          claimed_ext.insert(ext.clone(), (lang_name.clone(), project.clone()))
        {
          if other_lang != lang_name {
            return Err(anyhow!(
              "extension '.{ext}' is claimed by custom language '{lang_name}' \
               (project '{project}') and '{other_lang}' (project '{other_project}') — \
               multi-project serving needs one owner per extension"
            ));
          }
        }
      }
      match union.entry(lang_name.clone()) {
        std::collections::hash_map::Entry::Vacant(slot) => {
          slot.insert((custom, project.clone()));
        }
        std::collections::hash_map::Entry::Occupied(existing) => {
          let (registered, first_project) = existing.get();
          if !same_custom(registered, &custom) {
            return Err(anyhow!(
              "custom language '{lang_name}' is declared differently by projects \
               '{first_project}' and '{project}' (library/symbol/extensions differ) — \
               multi-project serving needs one definition per language name"
            ));
          }
        }
      }
    }
  }
  Ok(union.into_iter().map(|(lang, (custom, _))| (lang, custom)).collect())
}

/// Resolve a custom language's library paths against its project dir, so the union
/// registration's shared base is inert (Path::join with an absolute path yields it).
fn absolutized_custom(mut custom: CustomLang, project_dir: &Path) -> CustomLang {
  use vorpal_dynamic::LibraryPath;
  custom.library_path = match custom.library_path {
    LibraryPath::Single(path) => LibraryPath::Single(project_dir.join(path)),
    LibraryPath::Platform(map) => LibraryPath::Platform(
      map.into_iter().map(|(target, path)| (target, project_dir.join(path))).collect(),
    ),
  };
  custom
}

/// Two declarations describe the same registration (name-collision check).
fn same_custom(a: &CustomLang, b: &CustomLang) -> bool {
  use vorpal_dynamic::LibraryPath;
  let libs_equal = match (&a.library_path, &b.library_path) {
    (LibraryPath::Single(x), LibraryPath::Single(y)) => x == y,
    (LibraryPath::Platform(x), LibraryPath::Platform(y)) => x == y,
    _ => false,
  };
  libs_equal
    && a.language_symbol == b.language_symbol
    && a.extensions == b.extensions
    && a.expando_char == b.expando_char
    && a.meta_var_char == b.meta_var_char
}

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
  // Fault economics for the one-shot build: keep freed pages resident until exit (the
  // standalone indexer has always done this; the CLI's `index` never did — 2.31 M minor
  // faults and ~4 s of avoidable system CPU per kernel build, BENCHMARKS.md 2026-09-06).
  vorpal_index::retain_dirty_pages_for_batch_run();
  let out = arg.out.unwrap_or_else(|| arg.src.join(DEFAULT_INDEX_DIR));
  let mode = if arg.verify {
    vorpal_index::CacheMode::Verified
  } else {
    vorpal_index::CacheMode::default()
  };
  let project = project.ok();
  // `semanticTier` in vorpalconfig.yml selects the index's embedding tier; the selection
  // file at the index root is the single cross-process truth every warm reads (absent
  // key = keep the existing selection; absent file = lexical).
  if let Some(tier) = project.as_ref().and_then(|p| p.semantic_tier.as_deref()) {
    let tier = match tier {
      "lexical" => vorpal_index::SemanticTier::Lexical,
      "learned" => vorpal_index::SemanticTier::Learned,
      other => anyhow::bail!("vorpalconfig.yml semanticTier wants lexical|learned, got '{other}'"),
    };
    vorpal_index::write_tier_selection(&out, tier).map_err(boxed)?;
  }
  // `encoderDir` opts this index into the Stage-6 encoder reranker: the selection
  // file at the root is the cross-process truth (absent key = keep the existing
  // selection); relative paths resolve against the project dir, and the directory
  // must already exist — vorpal never downloads models.
  if let Some(dir) = project.as_ref().and_then(|p| p.encoder_dir.as_deref()) {
    let model_dir = {
      let path = std::path::Path::new(dir);
      if path.is_absolute() {
        path.to_path_buf()
      } else {
        project
          .as_ref()
          .map(|p| p.project_dir.join(path))
          .unwrap_or_else(|| path.to_path_buf())
      }
    };
    if !model_dir.is_dir() {
      anyhow::bail!(
        "vorpalconfig.yml encoderDir names a missing directory: {}",
        model_dir.display()
      );
    }
    vorpal_index::write_encoder_selection(&out, &model_dir)
      .map_err(|e| anyhow::anyhow!("writing encoder.dir: {e}"))?;
  }
  // Custom/dynamic languages were registered at CLI setup (the one-shot dlopen); here their
  // configured outline rules extend extraction (F-M3). No project config = bundled behavior.
  let env = extraction_env_from_project(project.as_ref())?;
  let report =
    vorpal_index::build_index_env(&arg.src, &out, mode, Default::default(), &env)
      .map_err(boxed)
      .with_context(|| format!("indexing {}", arg.src.display()))?;
  if report.reused {
    if report.indexed > 0 {
      // The stamp-only cutoff: files re-extracted and proven extraction-identical, stamps
      // refreshed, graph carried forward byte-identically.
      println!(
        "content-unchanged — restamped {} file(s), reused graph ({} nodes)",
        report.indexed, report.nodes
      );
    } else {
      println!("unchanged — reused existing index ({} nodes)", report.nodes);
    }
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
    match &report.cochange_note {
      Some(note) => println!("note: {note}"),
      None => println!("co-change: {} file pairs from git history", report.cochange_edges),
    }
    match &report.similar_note {
      Some(note) => println!("near-clones: {note}"),
      None => println!("near-clones: {} similar_to pairs from token sketches", report.similar_edges),
    }
    if report.request_sites > 0 {
      println!(
        "requests: {} of {} request/emit sites linked to routes/channels",
        report.request_edges, report.request_sites
      );
    }
    if let Some(note) = &report.request_note {
      println!("requests: {note}");
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

  if matches!(arg.verb, GraphVerb::Flows) {
    let kg = vorpal_index::Kg::load(&dir)
      .map_err(|err| anyhow::anyhow!(err.to_string()))
      .with_context(|| missing_index_hint(&dir))?;
    let gen_dir = vorpal_index::resolve_index_dir(&dir);
    let name = arg
      .name
      .clone()
      .ok_or_else(|| anyhow!("`graph flows` needs a symbol name"))?;
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
      path_suffix: arg.path.clone(),
      kind: arg.kind.clone(),
      merge_all: arg.all,
      show_ids: arg.ids,
    };
    let (records, sidecar_present) =
      vorpal_index::records::flow_records(&kg, &gen_dir, &target).map_err(anyhow::Error::msg)?;
    match arg.format {
      OutputFormat::Text => {
        if !sidecar_present {
          println!(
            "no data-flow sidecar in this generation (built before flows existed) — rebuild              the index to record flows"
          );
        } else if records.is_empty() {
          println!("no outgoing data flows recorded for this selection");
        }
        for r in &records {
          println!(
            "{} --arg#{}({}{})--> {} param#{} [{}]",
            r.from_name,
            r.arg_index,
            r.class,
            r.expr.as_deref().map(|e| format!(" {e}")).unwrap_or_default(),
            r.to_name,
            if r.param_index == u16::MAX { "?".to_string() } else { r.param_index.to_string() },
            r.to_path
          );
        }
      }
      _ => {
        let value = serde_json::json!({
          "sidecarPresent": sidecar_present,
          "records": records,
        });
        match arg.format {
          OutputFormat::Json => println!("{}", serde_json::to_string_pretty(&value)?),
          _ => print!("{}", emit_machine(arg.format, &value)?),
        }
      }
    }
    return Ok(ExitCode::SUCCESS);
  }

  if matches!(arg.verb, GraphVerb::Observed) {
    let kg = vorpal_index::Kg::load(&dir)
      .map_err(|err| anyhow::anyhow!(err.to_string()))
      .with_context(|| missing_index_hint(&dir))?;
    let gen_dir = vorpal_index::resolve_index_dir(&dir);
    let name = arg
      .name
      .clone()
      .ok_or_else(|| anyhow!("`graph observed` needs a symbol name"))?;
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
      path_suffix: arg.path.clone(),
      kind: arg.kind.clone(),
      merge_all: arg.all,
      show_ids: arg.ids,
    };
    let (records, sidecar_present) =
      vorpal_index::records::observed_records(&kg, &gen_dir, &target).map_err(anyhow::Error::msg)?;
    match arg.format {
      OutputFormat::Text => {
        if !sidecar_present {
          println!(
            "no observed-calls sidecar for this generation — ingest runtime traces with \
             `vorpal-index ingest-traces <index> <folded-stacks>` (a rebuild invalidates it)"
          );
        } else if records.is_empty() {
          println!("no observed calls recorded for this selection");
        }
        for r in &records {
          println!(
            "{} {} x{} {}{}",
            if r.direction == "in" { "<-observed-" } else { "-observed->" },
            r.counterpart_name,
            r.count,
            r.counterpart_path,
            if r.in_static_graph { "" } else { "  (not in the static graph)" }
          );
        }
      }
      _ => {
        let value = serde_json::json!({
          "sidecarPresent": sidecar_present,
          "records": records,
        });
        match arg.format {
          OutputFormat::Json => println!("{}", serde_json::to_string_pretty(&value)?),
          _ => print!("{}", emit_machine(arg.format, &value)?),
        }
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
      let gen_dir = vorpal_index::resolve_index_dir(&dir);
      vorpal_index::reachable_query_on(&kg, Some(&gen_dir), &target, *direction, relations, *max_depth, *min_confidence)
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
              Some(vorpal_index::resolve_index_dir(&dir)).as_deref(),
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
        // Rows carry the call site, exactly as the MCP `graph` tool's do — the shell fast
        // path answers "who calls X" / "what does X call" in one command.
        (None, verb) => vorpal_index::records::selected_value(
          vorpal_index::records::related_records_with_sites(
            &kg,
            Some(vorpal_index::resolve_index_dir(&dir)).as_deref(),
            verb.as_str(),
            &target,
            None,
          )
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
  if arg.ranked {
    if !matches!(arg.format, OutputFormat::Text) {
      anyhow::bail!("--ranked is a side-by-side text view; drop --format");
    }
    let searcher = vorpal_index::open_searcher(&dir)
      .map_err(boxed)
      .with_context(|| missing_index_hint(&dir))?;
    let (base, reranked) = searcher
      .records_ranked(&arg.query, arg.k.max(1), &filter)
      .map_err(boxed)?;
    render_ranked_columns(&arg.query, &base, reranked.as_deref(), searcher.encoder_status());
    return Ok(ExitCode::SUCCESS);
  }
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
      let report = vorpal_index::search_report_filtered(&dir, &arg.query, arg.k, &filter)
        .map_err(boxed)
        .with_context(|| missing_index_hint(&dir))?;
      let mut value = vorpal_index::records::paged_value(
        &report.hits,
        arg.page.cursor.as_deref(),
        arg.page.limit,
        "hits",
      )
      .map_err(anyhow::Error::msg)?;
      if let Some(mp) = &report.multi_phrase {
        value["multiPhrase"] = serde_json::to_value(mp).map_err(anyhow::Error::msg)?;
      }
      print!("{}", emit_machine(machine, &value)?);
    }
  }
  Ok(ExitCode::SUCCESS)
}

/// The `--ranked` side-by-side view: the SAME search's fused ordering next to the
/// encoder-reranked one, with per-hit movement markers (↑n moved up n places, ↓n
/// down, · unchanged, + newly visible at this depth). Both views come from one
/// channel pass — only the encoder step separates them.
fn render_ranked_columns(
  query: &str,
  base: &[vorpal_index::records::SearchHitRecord],
  reranked: Option<&[vorpal_index::records::SearchHitRecord]>,
  encoder_status: Option<&str>,
) {
  let cell = |hit: &vorpal_index::records::SearchHitRecord| -> String {
    let basename = hit.node.path.rsplit('/').next().unwrap_or(&hit.node.path);
    let mut text = format!("{}  ({basename})", hit.node.name);
    const WIDTH: usize = 46;
    if text.chars().count() > WIDTH {
      text = text.chars().take(WIDTH - 1).collect::<String>() + "…";
    }
    text
  };
  if base.is_empty() {
    println!("(no results for '{query}')");
    return;
  }
  let Some(reranked) = reranked else {
    for (rank, hit) in base.iter().enumerate() {
      println!("{:>2}  {}", rank + 1, cell(hit));
    }
    match encoder_status {
      // A selection exists but could not be honored — the stated reason.
      Some(status) => println!("\n(reranked view unavailable — {status})"),
      None => println!(
        "\n(reranked view unavailable: no encoder enabled — `vorpal enable semantic-f16` \
         or `encoderDir` in vorpalconfig.yml — or the query is a conjunction, which \
         keeps its own ranking)"
      ),
    }
    return;
  };
  let header = format!("{:>2}  {:<48}{}", "#", "fused", "reranked (encoder)");
  println!("{header}");
  for rank in 0..base.len().max(reranked.len()) {
    let left = base.get(rank).map(&cell).unwrap_or_default();
    let right = match reranked.get(rank) {
      Some(hit) => {
        let marker = match base.iter().position(|b| b.node.id == hit.node.id) {
          Some(was) if was > rank => format!("↑{}", was - rank),
          Some(was) if was < rank => format!("↓{}", rank - was),
          Some(_) => "·".to_string(),
          None => "+".to_string(),
        };
        format!("{marker:<3} {}", cell(hit))
      }
      None => String::new(),
    };
    println!("{:>2}  {left:<48}{right}", rank + 1);
  }
}

pub fn run_mcp(arg: McpArg, project: Result<ProjectConfig>) -> Result<ExitCode> {
  if let Some(action) = arg.action {
    return run_mcp_action(action);
  }
  let profile = vorpal_mcp::Profile::parse(&arg.profile)
    .ok_or_else(|| anyhow!("--profile must be full, analysis, or scout"))?;
  if arg.projects {
    // Per-project custom languages (D4 v2): union-register every enrolled project's
    // dynamic grammars in THIS process, at launch — the serving loop still can never
    // dlopen — and hand each project its own extraction environment.
    let mut envs = std::collections::BTreeMap::new();
    let mut declared: Vec<(String, PathBuf, std::collections::HashMap<String, CustomLang>)> =
      Vec::new();
    for (name, src, _index) in vorpal_mcp::enrolled_projects()? {
      let Some(config) = ProjectConfig::load_unregistered(&src)
        .with_context(|| format!("loading project '{name}' config at {}", src.display()))?
      else {
        continue; // no config file: builtin grammars, default env
      };
      if config.language_globs.is_some() {
        return Err(anyhow!(
          "project '{name}' declares languageGlobs, which rebind builtin file routing \
           process-wide — multi-project serving refuses them (run that project as a \
           single-project daemon: `vorpal mcp --index …`)"
        ));
      }
      declared.push((
        name.clone(),
        config.project_dir.clone(),
        config.custom_languages.clone().unwrap_or_default(),
      ));
      envs.insert(name.clone(), extraction_env_from_project(Some(&config))?);
      // Injectable registrations are per-env on the index path (C3a); the global
      // injectable set only affects run/scan, which this daemon never serves.
    }
    let merged = union_custom_languages(declared)?;
    if !merged.is_empty() {
      // Library paths were absolutized per project inside the union; the base is inert.
      vorpal_lang_registry::SgLang::register_custom_language(std::path::Path::new("/"), merged)
        .map_err(|err| anyhow!("union custom-language registration failed: {err}"))?;
    }
    vorpal_mcp::serve_stdio_projects_with_envs(profile, envs)?;
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

pub(crate) fn missing_index_hint(dir: &Path) -> String {
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

#[cfg(test)]
mod union_tests {
  use super::*;
  use std::collections::HashMap;
  use vorpal_dynamic::LibraryPath;

  fn custom(lib: &str, exts: &[&str]) -> CustomLang {
    CustomLang {
      library_path: LibraryPath::Single(PathBuf::from(lib)),
      language_symbol: None,
      meta_var_char: None,
      expando_char: None,
      extensions: exts.iter().map(|e| e.to_string()).collect(),
      outline_rules: None,
      ref_spec: None,
      canary: None,
    }
  }

  #[test]
  fn union_absolutizes_and_shares_identical_declarations() {
    let merged = union_custom_languages(vec![
      (
        "a".into(),
        PathBuf::from("/proj/a"),
        HashMap::from([("zed".to_string(), custom("libs/zed.so", &["zed"]))]),
      ),
      (
        // The SAME declaration from another project dir differs after absolutization —
        // identical only when the resolved paths agree.
        "b".into(),
        PathBuf::from("/proj/a"),
        HashMap::from([("zed".to_string(), custom("libs/zed.so", &["zed"]))]),
      ),
    ])
    .expect("identical declarations merge");
    assert_eq!(merged.len(), 1);
    match &merged["zed"].library_path {
      LibraryPath::Single(path) => assert_eq!(path, &PathBuf::from("/proj/a/libs/zed.so")),
      LibraryPath::Platform(_) => panic!("single-path declaration must stay single"),
    }
  }

  #[test]
  fn union_refuses_conflicts_by_name() {
    let refusals = [
      // Same name, different libraries (different project dirs absolutize apart).
      union_custom_languages(vec![
        (
          "a".into(),
          PathBuf::from("/proj/a"),
          HashMap::from([("zed".to_string(), custom("libs/zed.so", &["zed"]))]),
        ),
        (
          "b".into(),
          PathBuf::from("/proj/b"),
          HashMap::from([("zed".to_string(), custom("libs/zed.so", &["zed"]))]),
        ),
      ]),
      // Same extension, two owners.
      union_custom_languages(vec![
        (
          "a".into(),
          PathBuf::from("/proj/a"),
          HashMap::from([("zed".to_string(), custom("z.so", &["zz"]))]),
        ),
        (
          "b".into(),
          PathBuf::from("/proj/b"),
          HashMap::from([("qux".to_string(), custom("q.so", &["zz"]))]),
        ),
      ]),
      // Shadowing a builtin extension.
      union_custom_languages(vec![(
        "a".into(),
        PathBuf::from("/proj/a"),
        HashMap::from([("pyx".to_string(), custom("p.so", &["py"]))]),
      )]),
    ];
    let messages: Vec<String> = refusals
      .into_iter()
      .map(|r| match r {
        Err(err) => err.to_string(),
        Ok(_) => panic!("must refuse"),
      })
      .collect();
    assert!(messages[0].contains("declared differently"), "{}", messages[0]);
    assert!(messages[0].contains("'a'") && messages[0].contains("'b'"), "{}", messages[0]);
    assert!(messages[1].contains("one owner per extension"), "{}", messages[1]);
    assert!(messages[2].contains("routes to a builtin"), "{}", messages[2]);
  }
}
