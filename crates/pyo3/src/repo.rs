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
pub fn index_build(py: Python<'_>, src: &str, out: Option<&str>) -> PyResult<String> {
  let src = src.to_string();
  let out = out.map(str::to_string);
  // Release the GIL for the whole (blocking, CPU/IO-heavy) build so the async facade in
  // vorpal_py (`await vorpal.build(...)`) actually yields the event loop instead of pinning
  // it. PyErr is not `Ungil`, so the closure returns Result<_, String> and the error is
  // rebuilt into a PyErr after the GIL is reacquired.
  let result: Result<String, String> = py.detach(move || {
    let src = std::path::Path::new(&src);
    let out = out
      .map(std::path::PathBuf::from)
      .unwrap_or_else(|| src.join(".vorpal/index"));
    let report = vorpal_index::build_index(src, &out).map_err(|e| e.to_string())?;
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
  });
  result.map_err(PyRuntimeError::new_err)
}

/// Hybrid search over a persisted index. `explain=True` appends `(id N; channel#rank …)`
/// ranking provenance per line.
#[pyfunction]
#[pyo3(signature = (index_dir, query, k=10, explain=false))]
pub fn index_search(
  py: Python<'_>,
  index_dir: &str,
  query: &str,
  k: usize,
  explain: bool,
) -> PyResult<String> {
  let index_dir = index_dir.to_string();
  let query = query.to_string();
  let result: Result<String, String> = py.detach(move || {
    let dir = std::path::Path::new(&index_dir);
    if explain {
      vorpal_index::search_index_explained(dir, &query, k).map_err(|e| e.to_string())
    } else {
      vorpal_index::search_index(dir, &query, k).map_err(|e| e.to_string())
    }
  });
  result.map_err(PyRuntimeError::new_err)
}

/// Graph query with the shared symbol-selector contract: ambiguous names return candidate
/// listings; refine with `path`/`kind`/`id`, or `all=True` to merge namesakes.
#[pyfunction]
#[pyo3(signature = (index_dir, verb, name, path=None, kind=None, id=None, all=false, ids=false))]
#[allow(clippy::too_many_arguments)]
pub fn index_graph(
  py: Python<'_>,
  index_dir: &str,
  verb: &str,
  name: &str,
  path: Option<String>,
  kind: Option<String>,
  id: Option<u64>,
  all: bool,
  ids: bool,
) -> PyResult<String> {
  let index_dir = index_dir.to_string();
  let verb = verb.to_string();
  let name = name.to_string();
  let result: Result<String, String> = py.detach(move || {
    let target = vorpal_index::GraphTarget {
      name,
      id,
      // Durable ids arrive through the name as `eid:<hex>` (parsed by the selector).
      external_id: None,
      path_suffix: path,
      kind,
      merge_all: all,
      show_ids: ids,
    };
    vorpal_index::graph_query_selected(std::path::Path::new(&index_dir), &verb, &target)
      .map_err(|e| e.to_string())
  });
  result.map_err(PyRuntimeError::new_err)
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
pub fn index_node(py: Python<'_>, index_dir: &str, id: u64) -> PyResult<NodeInfo> {
  let index_dir = index_dir.to_string();
  // The mmap open + lookup runs GIL-free; only the owned fields cross back.
  let result: Result<NodeInfo, String> = py.detach(move || {
    let kg = vorpal_kg::Kg::load(std::path::Path::new(&index_dir)).map_err(|e| e.to_string())?;
    let view = kg
      .node(vorpal_kg::NodeId::new(id))
      .ok_or_else(|| format!("no node with id {id}"))?;
    Ok(NodeInfo {
      id,
      name: view.name.to_string(),
      kind: format!("{:?}", view.kind),
      path: view.path.to_string(),
      signature: view.signature.to_string(),
      exported: view.exported,
      span: view.span,
    })
  });
  result.map_err(PyRuntimeError::new_err)
}

/// Typed build report — the structured twin of [`index_build`]'s one-liner.
#[pyclass(get_all)]
pub struct BuildReport {
  pub indexed: u64,
  pub skipped: u64,
  pub nodes: u64,
  pub resolved: u64,
  pub ambiguous: u64,
  pub external: u64,
  pub masked: u64,
  pub reused: bool,
}

/// Serialize any vorpal-index record to a native Python object (dicts/lists/scalars).
fn record_to_py<T: serde::Serialize>(py: Python<'_>, record: &T) -> PyResult<Py<PyAny>> {
  Ok(
    pythonize::pythonize(py, record)
      .map_err(|err| PyRuntimeError::new_err(format!("serialize record: {err}")))?
      .unbind(),
  )
}

/// Map a selector outcome to `{"outcome": ..., "records": [...]}` — ambiguity is an answer
/// (the candidates to refine with), never an exception.
fn selected_to_py<T: serde::Serialize>(
  py: Python<'_>,
  selected: vorpal_index::records::Selected<T>,
) -> PyResult<Py<PyAny>> {
  use pyo3::types::PyDict;
  let dict = PyDict::new(py);
  match selected {
    vorpal_index::records::Selected::NoMatch => {
      dict.set_item("outcome", "no-match")?;
      dict.set_item("records", pyo3::types::PyList::empty(py))?;
    }
    vorpal_index::records::Selected::Ambiguous(candidates) => {
      dict.set_item("outcome", "ambiguous")?;
      dict.set_item("records", record_to_py(py, &candidates)?)?;
    }
    vorpal_index::records::Selected::Hits(hits) => {
      dict.set_item("outcome", "hits")?;
      dict.set_item("records", record_to_py(py, &hits)?)?;
    }
  }
  Ok(dict.into())
}

fn target_of(
  name: &str,
  path: Option<String>,
  kind: Option<String>,
  id: Option<u64>,
  all: bool,
) -> vorpal_index::GraphTarget {
  vorpal_index::GraphTarget {
    name: name.to_string(),
    id,
    // Durable ids arrive through the name as `eid:<hex>` (parsed by the selector).
    external_id: None,
    path_suffix: path,
    kind,
    merge_all: all,
    show_ids: true,
  }
}

/// A pinned index session (IMPROVEMENTS #8): `Index.open(dir)` resolves the live generation
/// **once**, and every query on the object answers from exactly that generation — a rebuild
/// landing mid-session can never split ids or spans across index states. Results are native
/// Python objects sharing the vorpal-index record schema (the same fields MCP's
/// `structuredContent` serializes), so ids/eids/grades/spans need no prose parsing.
///
/// Contract notes:
/// - **Staleness**: the session reads the pinned generation's immutable artifacts; a newer
///   index appearing on disk is invisible until you `open()` again. Deleted generations
///   (garbage-collected while a session holds them) keep serving from the open mmaps on
///   Unix semantics.
/// - **Threads/processes**: the object is read-only after open and safe to share across
///   Python threads; separate processes open their own sessions.
/// - **Format compatibility**: `open` fails loudly on unreadable/foreign artifacts —
///   there is no silent cross-version reinterpretation.
/// - **Iteration/pagination**: methods return complete typed lists (in-process, no wire
///   cap); slice or iterate natively. The MCP surface is where cursor pagination lives.
#[pyclass]
pub struct Index {
  kg: vorpal_kg::Kg,
  generation_dir: std::path::PathBuf,
  generation: String,
}

#[pymethods]
impl Index {
  /// Open `index_dir`, pinning its CURRENT generation for the session's lifetime.
  #[staticmethod]
  pub fn open(index_dir: &str) -> PyResult<Self> {
    let root = std::path::Path::new(index_dir);
    let generation_dir = vorpal_kg::resolve_index_dir(root);
    let kg = vorpal_kg::Kg::load(&generation_dir).map_err(|e| to_py_err(Box::new(e)))?;
    let generation = generation_dir
      .file_name()
      .and_then(|name| name.to_str())
      .unwrap_or("")
      .to_string();
    Ok(Self {
      kg,
      generation_dir,
      generation,
    })
  }

  /// The pinned generation's content id ("" for a legacy flat index).
  #[getter]
  pub fn generation(&self) -> &str {
    &self.generation
  }

  /// One node's typed record, or None.
  pub fn node(&self, py: Python<'_>, id: u64) -> PyResult<Py<PyAny>> {
    match vorpal_index::records::node_record(&self.kg, vorpal_kg::NodeId::new(id)) {
      Some(record) => record_to_py(py, &record),
      None => Ok(py.None()),
    }
  }

  /// Typed candidate listing for a selector (the `node` verb): every match is the answer.
  #[pyo3(signature = (name, path=None, kind=None, id=None, all=false))]
  pub fn nodes(
    &self,
    py: Python<'_>,
    name: &str,
    path: Option<String>,
    kind: Option<String>,
    id: Option<u64>,
    all: bool,
  ) -> PyResult<Py<PyAny>> {
    let records =
      vorpal_index::records::listing_records(&self.kg, &target_of(name, path, kind, id, all))
        .map_err(PyRuntimeError::new_err)?;
    record_to_py(py, &records)
  }

  /// Typed edge query (`callers`/`references`/`importers`/`implementors`/`typeusers`):
  /// `{"outcome": "hits"|"ambiguous"|"no-match", "records": [...]}`, each hit carrying its
  /// resolution grade.
  #[pyo3(signature = (verb, name, path=None, kind=None, id=None, all=false))]
  #[allow(clippy::too_many_arguments)]
  pub fn related(
    &self,
    py: Python<'_>,
    verb: &str,
    name: &str,
    path: Option<String>,
    kind: Option<String>,
    id: Option<u64>,
    all: bool,
  ) -> PyResult<Py<PyAny>> {
    let selected = vorpal_index::records::related_records(
      &self.kg,
      verb,
      &target_of(name, path, kind, id, all),
    )
    .map_err(PyRuntimeError::new_err)?;
    selected_to_py(py, selected)
  }

  /// Typed relation-restricted traversal: BFS steps with depth, parent (`via`), relation,
  /// and grade — the same contract as the CLI/MCP `reachable`.
  #[pyo3(signature = (name, direction, relations=None, max_depth=None, min_grade=None, path=None, kind=None, id=None, all=false))]
  #[allow(clippy::too_many_arguments)]
  pub fn reachable(
    &self,
    py: Python<'_>,
    name: &str,
    direction: &str,
    relations: Option<Vec<String>>,
    max_depth: Option<u32>,
    min_grade: Option<&str>,
    path: Option<String>,
    kind: Option<String>,
    id: Option<u64>,
    all: bool,
  ) -> PyResult<Py<PyAny>> {
    let dir = match direction {
      "in" => vorpal_kg::Direction::In,
      "out" => vorpal_kg::Direction::Out,
      other => {
        return Err(PyRuntimeError::new_err(format!(
          "direction must be \"in\" or \"out\", got '{other}'"
        )));
      }
    };
    let relations = match relations {
      None => vec![vorpal_kg::EdgeType::CALLS],
      Some(names) => {
        let mut out = Vec::with_capacity(names.len());
        for name in &names {
          out.push(
            vorpal_kg::EdgeType::from_name(name)
              .ok_or_else(|| PyRuntimeError::new_err(format!("unknown relation '{name}'")))?,
          );
        }
        if out.is_empty() {
          vec![vorpal_kg::EdgeType::CALLS]
        } else {
          out
        }
      }
    };
    let min_confidence =
      vorpal_index::min_confidence_for_grade(min_grade).map_err(to_py_err)?;
    let selected = vorpal_index::records::reach_records(
      &self.kg,
      &target_of(name, path, kind, id, all),
      dir,
      &relations,
      max_depth.filter(|&d| d > 0),
      min_confidence,
    )
    .map_err(PyRuntimeError::new_err)?;
    selected_to_py(py, selected)
  }

  /// Typed evidence (`why`): edge form (`to_id`) or absence form (`name`).
  #[pyo3(signature = (from_id, to_id=None, name=None))]
  pub fn why(
    &self,
    py: Python<'_>,
    from_id: u64,
    to_id: Option<u64>,
    name: Option<&str>,
  ) -> PyResult<Py<PyAny>> {
    if to_id.is_none() && name.is_none() {
      return Err(PyRuntimeError::new_err(
        "pass to_id (edge evidence) or name (absence evidence)",
      ));
    }
    let records = vorpal_index::records::evidence_records(&self.kg, from_id, to_id, name);
    record_to_py(py, &records)
  }

  /// Typed hybrid search over the pinned generation: hits with score and per-channel
  /// ranking provenance. Structured filters (IMPROVEMENTS #9) apply to every channel
  /// before ranking, so `k` results means `k` matching results.
  #[pyo3(signature = (query, k=10, path=None, prefix=None, kind=None, lang=None, exported=false, exclude_tests=false))]
  #[allow(clippy::too_many_arguments)]
  pub fn search(
    &self,
    py: Python<'_>,
    query: &str,
    k: usize,
    path: Option<String>,
    prefix: Option<String>,
    kind: Option<String>,
    lang: Option<String>,
    exported: bool,
    exclude_tests: bool,
  ) -> PyResult<Py<PyAny>> {
    let filter = vorpal_index::SearchFilter {
      path_prefix: prefix,
      path_suffix: path,
      kind,
      lang,
      exported_only: exported,
      exclude_tests,
    };
    // The pinned generation dir IS the index dir here (resolve is idempotent), so a rebuild
    // landing mid-session cannot swap the ranking's graph or ANN tier under us.
    let records =
      vorpal_index::search_records_filtered(&self.generation_dir, query, k, &filter)
        .map_err(to_py_err)?;
    record_to_py(py, &records)
  }
}

/// [`index_build`], returning the typed [`BuildReport`] instead of the rendered line.
#[pyfunction]
#[pyo3(signature = (src, out=None))]
pub fn index_build_report(py: Python<'_>, src: &str, out: Option<&str>) -> PyResult<BuildReport> {
  let src = src.to_string();
  let out = out.map(str::to_string);
  let result: Result<BuildReport, String> = py.detach(move || {
    let src = std::path::Path::new(&src);
    let out = out
      .map(std::path::PathBuf::from)
      .unwrap_or_else(|| src.join(".vorpal/index"));
    let report = vorpal_index::build_index(src, &out).map_err(|e| e.to_string())?;
    Ok(BuildReport {
      indexed: report.indexed as u64,
      skipped: report.skipped as u64,
      nodes: report.nodes as u64,
      resolved: report.resolved as u64,
      ambiguous: report.ambiguous as u64,
      external: report.external as u64,
      masked: report.masked as u64,
      reused: report.reused,
    })
  });
  result.map_err(PyRuntimeError::new_err)
}
