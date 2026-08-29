//! Repository-intelligence API (IMPROVEMENTS §9): the persistent index/graph/search surface
//! exposed to Node, alongside the inherited structural matcher. Same contracts as the
//! Python bindings and the CLI: rendered strings share the CLI's formats; `indexNode`
//! returns structured fields; ambiguous graph names return candidate listings unless
//! `all: true` merges explicitly.

use napi::bindgen_prelude::*;
use napi_derive::napi;

fn to_napi_err(err: Box<dyn std::error::Error>) -> Error {
  Error::from_reason(err.to_string())
}

/// Build or refresh the index for `src` (default output: `<src>/.vorpal/index`); returns
/// the CLI's one-line report.
#[napi]
pub fn index_build(src: String, out: Option<String>) -> Result<String> {
  let src = std::path::Path::new(&src);
  let out = out
    .map(std::path::PathBuf::from)
    .unwrap_or_else(|| src.join(".vorpal/index"));
  let report = vorpal_index::build_index(src, &out).map_err(to_napi_err)?;
  Ok(if report.reused {
    format!("unchanged — reused existing index ({} nodes)", report.nodes)
  } else {
    format!(
      "parsed {} files ({} replayed from cache) → {} nodes; refs: {} resolved, {} ambiguous, {} external, {} masked",
      report.indexed,
      report.skipped,
      report.nodes,
      report.resolved,
      report.ambiguous,
      report.external,
      report.masked
    )
  })
}

/// Hybrid search; `explain` appends `(id N; channel#rank …)` provenance per line.
#[napi]
pub fn index_search(
  index_dir: String,
  query: String,
  k: Option<u32>,
  explain: Option<bool>,
) -> Result<String> {
  let dir = std::path::Path::new(&index_dir);
  let k = k.unwrap_or(10) as usize;
  if explain.unwrap_or(false) {
    vorpal_index::search_index_explained(dir, &query, k).map_err(to_napi_err)
  } else {
    vorpal_index::search_index(dir, &query, k).map_err(to_napi_err)
  }
}

/// Selector arguments for `indexGraph`.
#[napi(object)]
#[derive(Default)]
pub struct GraphOptions {
  /// Refine: definition file path must end with this suffix.
  pub path: Option<String>,
  /// Refine: symbol kind (function, method, struct, field, …).
  pub kind: Option<String>,
  /// Query exactly this node id.
  pub id: Option<i64>,
  /// Merge results across all same-named definitions.
  pub all: Option<bool>,
  /// Append node ids to result lines.
  pub ids: Option<bool>,
}

/// Graph query with the shared symbol-selector contract.
#[napi]
pub fn index_graph(
  index_dir: String,
  verb: String,
  name: String,
  options: Option<GraphOptions>,
) -> Result<String> {
  let options = options.unwrap_or_default();
  let target = vorpal_index::GraphTarget {
    name,
    id: options.id.and_then(|v| u64::try_from(v).ok()),
    // Durable ids arrive through the name as `eid:<hex>` (parsed by the selector).
    external_id: None,
    path_suffix: options.path,
    kind: options.kind,
    merge_all: options.all.unwrap_or(false),
    show_ids: options.ids.unwrap_or(false),
  };
  vorpal_index::graph_query_selected(std::path::Path::new(&index_dir), &verb, &target)
    .map_err(to_napi_err)
}

/// One node's structured fields.
#[napi(object)]
pub struct NodeInfo {
  pub id: i64,
  pub name: String,
  pub kind: String,
  pub path: String,
  pub signature: String,
  pub exported: bool,
  /// Definition byte range in `path`; `[0, 0]` when unknown.
  pub span_start: u32,
  pub span_end: u32,
}

/// Fetch one node by id.
#[napi]
pub fn index_node(index_dir: String, id: i64) -> Result<NodeInfo> {
  let raw = u64::try_from(id).map_err(|_| Error::from_reason("id must be non-negative"))?;
  let kg =
    vorpal_kg::Kg::load(std::path::Path::new(&index_dir)).map_err(|e| to_napi_err(Box::new(e)))?;
  let view = kg
    .node(vorpal_kg::NodeId::new(raw))
    .ok_or_else(|| Error::from_reason(format!("no node with id {id}")))?;
  Ok(NodeInfo {
    id,
    name: view.name.to_string(),
    kind: format!("{:?}", view.kind),
    path: view.path.to_string(),
    signature: view.signature.to_string(),
    exported: view.exported,
    span_start: view.span.0,
    span_end: view.span.1,
  })
}

/// Typed build report — the structured twin of `indexBuild`'s one-liner.
// The napi export glue is what consumes these; it is inert under cfg(test), so the test
// compilation alone sees them as dead.
#[cfg_attr(test, allow(dead_code))]
#[napi(object)]
pub struct BuildReport {
  pub indexed: i64,
  pub skipped: i64,
  pub nodes: i64,
  pub resolved: i64,
  pub ambiguous: i64,
  pub external: i64,
  pub masked: i64,
  pub reused: bool,
}

/// `indexBuild`, returning the typed report instead of the rendered line.
#[cfg_attr(test, allow(dead_code))]
#[napi]
pub fn index_build_report(src: String, out: Option<String>) -> Result<BuildReport> {
  let src = std::path::Path::new(&src);
  let out = out
    .map(std::path::PathBuf::from)
    .unwrap_or_else(|| src.join(".vorpal/index"));
  let report = vorpal_index::build_index(src, &out).map_err(to_napi_err)?;
  Ok(BuildReport {
    indexed: report.indexed as i64,
    skipped: report.skipped as i64,
    nodes: report.nodes as i64,
    resolved: report.resolved as i64,
    ambiguous: report.ambiguous as i64,
    external: report.external as i64,
    masked: report.masked as i64,
    reused: report.reused,
  })
}

/// Traversal arguments for `Index.reachable`, extending the selector options.
#[napi(object)]
#[derive(Default)]
pub struct ReachOptions {
  /// Edge types to follow (default ["calls"]).
  pub relations: Option<Vec<String>>,
  /// Maximum hops (0 or absent = unbounded).
  pub max_depth: Option<u32>,
  /// Only traverse edges at this resolution grade or better (exact | constrained |
  /// heuristic; absent = include structural edges too).
  pub min_grade: Option<String>,
  /// Refine: seed's definition file path must end with this suffix.
  pub path: Option<String>,
  /// Refine: seed's symbol kind.
  pub kind: Option<String>,
  /// Seed exactly this node id.
  pub id: Option<i64>,
  /// Merge across all same-named seeds instead of listing candidates.
  pub all: Option<bool>,
}

fn selected_to_value<T: serde::Serialize>(
  selected: vorpal_index::records::Selected<T>,
) -> Result<serde_json::Value> {
  Ok(match selected {
    vorpal_index::records::Selected::NoMatch => {
      serde_json::json!({"outcome": "no-match", "records": []})
    }
    vorpal_index::records::Selected::Ambiguous(candidates) => serde_json::json!({
      "outcome": "ambiguous",
      "records": serde_json::to_value(candidates).map_err(|e| Error::from_reason(e.to_string()))?,
    }),
    vorpal_index::records::Selected::Hits(hits) => serde_json::json!({
      "outcome": "hits",
      "records": serde_json::to_value(hits).map_err(|e| Error::from_reason(e.to_string()))?,
    }),
  })
}

fn selector_target(name: String, options: &GraphOptions) -> vorpal_index::GraphTarget {
  vorpal_index::GraphTarget {
    name,
    id: options.id.and_then(|v| u64::try_from(v).ok()),
    // Durable ids arrive through the name as `eid:<hex>` (parsed by the selector).
    external_id: None,
    path_suffix: options.path.clone(),
    kind: options.kind.clone(),
    merge_all: options.all.unwrap_or(false),
    show_ids: true,
  }
}

/// A pinned index session (IMPROVEMENTS #8): `Index.open(dir)` resolves the live generation
/// once, and every query on the object answers from exactly that generation — a rebuild
/// landing mid-session can never split ids or spans across index states. Results are plain
/// JS objects sharing the vorpal-index record schema (the same fields MCP's
/// `structuredContent` serializes).
///
/// Contract notes (mirrors the Python `Index`): the session reads the pinned generation's
/// immutable artifacts, so a newer index on disk is invisible until you `open()` again;
/// the object is read-only after open; `open` fails loudly on unreadable artifacts; methods
/// return complete typed arrays — iterate or slice natively (cursor pagination is the MCP
/// wire concern).
#[napi]
pub struct Index {
  kg: vorpal_kg::Kg,
  generation_dir: std::path::PathBuf,
  generation: String,
}

#[napi]
impl Index {
  /// Open `index_dir`, pinning its CURRENT generation for the session's lifetime.
  #[napi(factory)]
  pub fn open(index_dir: String) -> Result<Index> {
    let root = std::path::Path::new(&index_dir);
    let generation_dir = vorpal_kg::resolve_index_dir(root);
    let kg = vorpal_kg::Kg::load(&generation_dir).map_err(|e| to_napi_err(Box::new(e)))?;
    let generation = generation_dir
      .file_name()
      .and_then(|name| name.to_str())
      .unwrap_or("")
      .to_string();
    Ok(Index {
      kg,
      generation_dir,
      generation,
    })
  }

  /// The pinned generation's content id ("" for a legacy flat index).
  #[napi(getter)]
  pub fn generation(&self) -> String {
    self.generation.clone()
  }

  /// One node's typed record, or null.
  #[napi]
  pub fn node(&self, id: i64) -> Result<serde_json::Value> {
    let raw = u64::try_from(id).map_err(|_| Error::from_reason("id must be non-negative"))?;
    match vorpal_index::records::node_record(&self.kg, vorpal_kg::NodeId::new(raw)) {
      Some(record) => serde_json::to_value(record).map_err(|e| Error::from_reason(e.to_string())),
      None => Ok(serde_json::Value::Null),
    }
  }

  /// Typed candidate listing for a selector: every match is the answer.
  #[napi]
  pub fn nodes(&self, name: String, options: Option<GraphOptions>) -> Result<serde_json::Value> {
    let options = options.unwrap_or_default();
    let records =
      vorpal_index::records::listing_records(&self.kg, &selector_target(name, &options))
        .map_err(Error::from_reason)?;
    serde_json::to_value(records).map_err(|e| Error::from_reason(e.to_string()))
  }

  /// Typed edge query (callers/references/importers/implementors/typeusers):
  /// `{outcome: "hits"|"ambiguous"|"no-match", records: [...]}`, each hit with its grade.
  #[napi]
  pub fn related(
    &self,
    verb: String,
    name: String,
    options: Option<GraphOptions>,
  ) -> Result<serde_json::Value> {
    let options = options.unwrap_or_default();
    let selected = vorpal_index::records::related_records(
      &self.kg,
      &verb,
      &selector_target(name, &options),
    )
    .map_err(Error::from_reason)?;
    selected_to_value(selected)
  }

  /// Typed relation-restricted traversal: BFS steps with depth, parent (`via`), relation,
  /// and grade — the same contract as the CLI/MCP `reachable`.
  #[napi]
  pub fn reachable(
    &self,
    name: String,
    direction: String,
    options: Option<ReachOptions>,
  ) -> Result<serde_json::Value> {
    let options = options.unwrap_or_default();
    let dir = match direction.as_str() {
      "in" => vorpal_kg::Direction::In,
      "out" => vorpal_kg::Direction::Out,
      other => {
        return Err(Error::from_reason(format!(
          "direction must be \"in\" or \"out\", got '{other}'"
        )));
      }
    };
    let relations = match &options.relations {
      None => vec![vorpal_kg::EdgeType::CALLS],
      Some(names) if names.is_empty() => vec![vorpal_kg::EdgeType::CALLS],
      Some(names) => {
        let mut out = Vec::with_capacity(names.len());
        for name in names {
          out.push(
            vorpal_kg::EdgeType::from_name(name)
              .ok_or_else(|| Error::from_reason(format!("unknown relation '{name}'")))?,
          );
        }
        out
      }
    };
    let min_confidence =
      vorpal_index::min_confidence_for_grade(options.min_grade.as_deref()).map_err(to_napi_err)?;
    let target = vorpal_index::GraphTarget {
      name,
      id: options.id.and_then(|v| u64::try_from(v).ok()),
      external_id: None,
      path_suffix: options.path.clone(),
      kind: options.kind.clone(),
      merge_all: options.all.unwrap_or(false),
      show_ids: true,
    };
    let selected = vorpal_index::records::reach_records(
      &self.kg,
      &target,
      dir,
      &relations,
      options.max_depth.filter(|&d| d > 0),
      min_confidence,
    )
    .map_err(Error::from_reason)?;
    selected_to_value(selected)
  }

  /// Typed evidence (`why`): edge form (`toId`) or absence form (`name`).
  #[napi]
  pub fn why(
    &self,
    from_id: i64,
    to_id: Option<i64>,
    name: Option<String>,
  ) -> Result<serde_json::Value> {
    if to_id.is_none() && name.is_none() {
      return Err(Error::from_reason(
        "pass toId (edge evidence) or name (absence evidence)",
      ));
    }
    let from = u64::try_from(from_id).map_err(|_| Error::from_reason("fromId must be non-negative"))?;
    let to = match to_id {
      Some(v) => Some(u64::try_from(v).map_err(|_| Error::from_reason("toId must be non-negative"))?),
      None => None,
    };
    let records =
      vorpal_index::records::evidence_records(&self.kg, from, to, name.as_deref());
    serde_json::to_value(records).map_err(|e| Error::from_reason(e.to_string()))
  }

  /// Typed hybrid search over the pinned generation: hits with score and per-channel
  /// ranking provenance. Structured filters (IMPROVEMENTS #9) apply to every channel
  /// before ranking, so `k` results means `k` matching results.
  #[napi]
  pub fn search(
    &self,
    query: String,
    k: Option<u32>,
    options: Option<SearchOptions>,
  ) -> Result<serde_json::Value> {
    let options = options.unwrap_or_default();
    let filter = vorpal_index::SearchFilter {
      path_prefix: options.prefix,
      path_suffix: options.path,
      kind: options.kind,
      lang: options.lang,
      exported_only: options.exported.unwrap_or(false),
    };
    // The pinned generation dir IS the index dir here (resolve is idempotent), so a rebuild
    // landing mid-session cannot swap the ranking's graph or ANN tier under us.
    let records = vorpal_index::search_records_filtered(
      &self.generation_dir,
      &query,
      k.unwrap_or(10) as usize,
      &filter,
    )
    .map_err(to_napi_err)?;
    serde_json::to_value(records).map_err(|e| Error::from_reason(e.to_string()))
  }
}

/// Structured search filters for `Index.search` (IMPROVEMENTS #9).
#[napi(object)]
#[derive(Default)]
pub struct SearchOptions {
  /// Definition file path must end with this suffix.
  pub path: Option<String>,
  /// Definition file path must start with this prefix (package/subtree scoping).
  pub prefix: Option<String>,
  /// Symbol kind (function, method, struct, …).
  pub kind: Option<String>,
  /// Language name or alias (rust, py, ts, …).
  pub lang: Option<String>,
  /// Only exported definitions.
  pub exported: Option<bool>,
}
