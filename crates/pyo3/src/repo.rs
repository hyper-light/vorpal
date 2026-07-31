//! Repository-intelligence API (IMPROVEMENTS §9): the persistent index/graph/search surface
//! exposed to Python, alongside the inherited structural matcher.
//!
//! Deliberately thin and stable: rendered-string results share the CLI's exact formats (one
//! contract everywhere), while `index_node` returns a structured [`NodeInfo`] for callers
//! that need fields. Handles are directory paths — the index's on-disk artifacts are the
//! durable identity; every call revalidates against them (mmap cold-open is milliseconds).

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;

fn to_py_err(err: Box<dyn std::error::Error>) -> PyErr {
  PyRuntimeError::new_err(err.to_string())
}

/// Build or refresh the index for `src` (default output: `<src>/.vorpal/index`), returning
/// the same one-line report the CLI prints.
#[pyfunction]
#[pyo3(signature = (src, out=None))]
pub fn index_build(src: &str, out: Option<&str>) -> PyResult<String> {
  let src = std::path::Path::new(src);
  let out = out
    .map(std::path::PathBuf::from)
    .unwrap_or_else(|| src.join(".vorpal/index"));
  let report = vorpal_index::build_index(src, &out).map_err(to_py_err)?;
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

/// Hybrid search over a persisted index. `explain=True` appends `(id N; channel#rank …)`
/// ranking provenance per line.
#[pyfunction]
#[pyo3(signature = (index_dir, query, k=10, explain=false))]
pub fn index_search(index_dir: &str, query: &str, k: usize, explain: bool) -> PyResult<String> {
  let dir = std::path::Path::new(index_dir);
  if explain {
    vorpal_index::search_index_explained(dir, query, k).map_err(to_py_err)
  } else {
    vorpal_index::search_index(dir, query, k).map_err(to_py_err)
  }
}

/// Graph query with the shared symbol-selector contract: ambiguous names return candidate
/// listings; refine with `path`/`kind`/`id`, or `all=True` to merge namesakes.
#[pyfunction]
#[pyo3(signature = (index_dir, verb, name, path=None, kind=None, id=None, all=false, ids=false))]
#[allow(clippy::too_many_arguments)]
pub fn index_graph(
  index_dir: &str,
  verb: &str,
  name: &str,
  path: Option<String>,
  kind: Option<String>,
  id: Option<u64>,
  all: bool,
  ids: bool,
) -> PyResult<String> {
  let target = vorpal_index::GraphTarget {
    name: name.to_string(),
    id,
    // Durable ids arrive through the name as `eid:<hex>` (parsed by the selector).
    external_id: None,
    path_suffix: path,
    kind,
    merge_all: all,
    show_ids: ids,
  };
  vorpal_index::graph_query_selected(std::path::Path::new(index_dir), verb, &target)
    .map_err(to_py_err)
}

/// One node's structured fields — the typed complement to the rendered surfaces.
#[pyclass(get_all)]
pub struct NodeInfo {
  pub id: u64,
  pub name: String,
  pub kind: String,
  pub path: String,
  pub signature: String,
  pub exported: bool,
  /// Definition byte range in `path`; `(0, 0)` when unknown.
  pub span: (u32, u32),
}

/// Fetch one node by id.
#[pyfunction]
pub fn index_node(index_dir: &str, id: u64) -> PyResult<NodeInfo> {
  let kg =
    vorpal_kg::Kg::load(std::path::Path::new(index_dir)).map_err(|e| to_py_err(Box::new(e)))?;
  let view = kg
    .node(vorpal_kg::NodeId::new(id))
    .ok_or_else(|| PyRuntimeError::new_err(format!("no node with id {id}")))?;
  Ok(NodeInfo {
    id,
    name: view.name.to_string(),
    kind: format!("{:?}", view.kind),
    path: view.path.to_string(),
    signature: view.signature.to_string(),
    exported: view.exported,
    span: view.span,
  })
}
