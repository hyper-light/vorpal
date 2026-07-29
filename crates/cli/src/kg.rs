//! Knowledge-graph subcommands (§3.6): the npm-shipped `vorpal` binary carries the full KG
//! surface — `index`, `graph`, `search`, and the `mcp` daemon — over the same library code as
//! the standalone `vorpal-index`/`vorpal-mcp` tools.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, anyhow};
use clap::{Args, ValueEnum};

/// Default index location relative to the indexed tree / working directory. Hidden, so the
/// ignore-respecting walker never indexes the index itself.
const DEFAULT_INDEX_DIR: &str = ".vorpal/index";

#[derive(Args)]
pub struct IndexArg {
  /// Source directory to index.
  #[clap(default_value = ".")]
  src: PathBuf,
  /// Index directory (default: `<src>/.vorpal/index`).
  #[clap(long)]
  out: Option<PathBuf>,
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
  /// Merge results across ALL same-named definitions (the pre-selector behavior).
  #[clap(long)]
  all: bool,
  /// Append each result's node id (stable within this index generation).
  #[clap(long)]
  ids: bool,
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
  let report = vorpal_index::build_index(&arg.src, &out)
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
  println!("index: {}", out.display());
  Ok(ExitCode::SUCCESS)
}

pub fn run_graph(arg: GraphArg) -> Result<ExitCode> {
  let dir = index_dir(arg.index);
  let target = vorpal_index::GraphTarget {
    name: arg.name,
    id: arg.id,
    path_suffix: arg.path,
    kind: arg.kind,
    merge_all: arg.all,
    show_ids: arg.ids,
  };
  let rendered = vorpal_index::graph_query_selected(&dir, arg.verb.as_str(), &target)
    .map_err(boxed)
    .with_context(|| missing_index_hint(&dir))?;
  print!("{rendered}");
  Ok(ExitCode::SUCCESS)
}

pub fn run_search(arg: SearchArg) -> Result<ExitCode> {
  let dir = index_dir(arg.index);
  let rendered = vorpal_index::search_index(&dir, &arg.query, arg.k)
    .map_err(boxed)
    .with_context(|| missing_index_hint(&dir))?;
  if rendered.is_empty() {
    println!("(no results for '{}')", arg.query);
  } else {
    print!("{rendered}");
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
