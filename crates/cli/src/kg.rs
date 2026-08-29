//! Knowledge-graph subcommands (§3.6): the npm-shipped `vorpal` binary carries the full KG
//! surface — `index`, `graph`, `search`, and the `mcp` daemon — over the same library code as
//! the standalone `vorpal-index`/`vorpal-mcp` tools.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, anyhow};
use clap::{Args, ValueEnum};

mod emit;

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
      anyhow::bail!("--limit/--cursor page the records surface: add --format json");
    }
    Ok(())
  }
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
    }
  }
}

#[derive(Args)]
pub struct GraphArg {
  /// Which relation to query.
  #[clap(value_enum)]
  verb: GraphVerb,
  /// Exact symbol name.
  name: String,
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
  /// callers), `out` = everything it reaches. Default `in`.
  #[clap(long, value_name = "in|out", default_value = "in")]
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
  /// similarity, and graph in-degree (RRF).
  query: String,
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
}

fn index_dir(explicit: Option<PathBuf>) -> PathBuf {
  explicit.unwrap_or_else(|| PathBuf::from(DEFAULT_INDEX_DIR))
}

fn boxed(err: Box<dyn std::error::Error>) -> anyhow::Error {
  anyhow!(err.to_string())
}

pub fn run_index(arg: IndexArg) -> Result<ExitCode> {
  let out = arg.out.unwrap_or_else(|| arg.src.join(DEFAULT_INDEX_DIR));
  let mode = if arg.verify {
    vorpal_index::CacheMode::Verified
  } else {
    vorpal_index::CacheMode::default()
  };
  let report = vorpal_index::build_index_with(&arg.src, &out, mode)
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
  if report.cache_mode != "fast-stat" {
    println!("cache mode: {}", report.cache_mode);
  }
  println!("index: {}", out.display());
  Ok(ExitCode::SUCCESS)
}

pub fn run_graph(arg: GraphArg) -> Result<ExitCode> {
  arg.page.reject_for_text(arg.format)?;
  let dir = index_dir(arg.index);
  let eid = match arg.eid.as_deref() {
    Some(hex) => Some(
      u128::from_str_radix(hex, 16)
        .map_err(|_| anyhow::anyhow!("malformed external id '{hex}' (expect 32 hex chars)"))?,
    ),
    None => None,
  };
  let target = vorpal_index::GraphTarget {
    name: arg.name,
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
      other => anyhow::bail!("--direction must be `in` or `out`, got '{other}'"),
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
    (OutputFormat::Json, _) => {
      let kg = vorpal_index::Kg::load(&dir)
        .map_err(|err| anyhow::anyhow!(err.to_string()))
        .with_context(|| missing_index_hint(&dir))?;
      let cursor = arg.page.cursor.as_deref();
      let mut json = match (&traversal, arg.verb) {
        (Some((direction, relations, max_depth, min_confidence)), _) => emit::selected_json(
          vorpal_index::records::reach_records(
            &kg,
            &target,
            *direction,
            relations,
            *max_depth,
            *min_confidence,
          )
          .map_err(anyhow::Error::msg)?,
          cursor,
          arg.page.limit,
        )?,
        (None, GraphVerb::Node) => emit::records_json(
          &vorpal_index::records::listing_records(&kg, &target).map_err(anyhow::Error::msg)?,
          cursor,
          arg.page.limit,
        )?,
        (None, verb) => emit::selected_json(
          vorpal_index::records::related_records(&kg, verb.as_str(), &target)
            .map_err(anyhow::Error::msg)?,
          cursor,
          arg.page.limit,
        )?,
      };
      json.push('\n');
      json
    }
  };
  print!("{output}");
  Ok(ExitCode::SUCCESS)
}

pub fn run_search(arg: SearchArg) -> Result<ExitCode> {
  arg.page.reject_for_text(arg.format)?;
  let dir = index_dir(arg.index);
  let filter = vorpal_index::SearchFilter {
    path_prefix: arg.prefix,
    path_suffix: arg.path,
    kind: arg.kind,
    lang: arg.lang,
    exported_only: arg.exported,
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
    OutputFormat::Json => {
      let records = vorpal_index::search_records_filtered(&dir, &arg.query, arg.k, &filter)
        .map_err(boxed)
        .with_context(|| missing_index_hint(&dir))?;
      let mut json = emit::records_json(&records, arg.page.cursor.as_deref(), arg.page.limit)?;
      json.push('\n');
      print!("{json}");
    }
  }
  Ok(ExitCode::SUCCESS)
}

pub fn run_mcp(arg: McpArg) -> Result<ExitCode> {
  vorpal_mcp::serve_stdio(index_dir(arg.index))?;
  Ok(ExitCode::SUCCESS)
}

fn missing_index_hint(dir: &Path) -> String {
  format!(
    "querying index at {} (build one first: `vorpal index <src>`)",
    dir.display()
  )
}
