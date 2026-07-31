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
