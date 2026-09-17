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
  /// Shared, immutable, mmap-backed graph — `Arc` so `*_async` methods can run their
  /// reads GIL-free on the worker pool while the Python object stays alive.
  kg: std::sync::Arc<vorpal_kg::Kg>,
  generation_dir: std::path::PathBuf,
  generation: String,
  /// The tree a default-layout index dir names; relative scope entries resolve against it.
  source_root: Option<std::path::PathBuf>,
}

#[pymethods]
impl Index {
  /// Open `index_dir`, pinning its CURRENT generation for the session's lifetime.
  #[staticmethod]
  pub fn open(index_dir: &str) -> PyResult<Self> {
    let root = std::path::Path::new(index_dir);
    let generation_dir = vorpal_kg::resolve_index_dir(root);
    let kg = std::sync::Arc::new(vorpal_kg::Kg::load(&generation_dir).map_err(|e| to_py_err(Box::new(e)))?);
    let generation = generation_dir
      .file_name()
      .and_then(|name| name.to_str())
      .unwrap_or("")
      .to_string();
    Ok(Self {
      kg,
      generation_dir,
      generation,
      source_root: vorpal_index::default_layout_root(root),
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
  /// `within` / `exclude` / `classes` / `changed_since` filter the answer's rows (the MCP
  /// `scope`): rows outside are dropped and counted in `outsideScope`. Paths are relative
  /// to the source root or absolute; `@file`, `@dir`, `@package` bind to the symbol asked
  /// about; `classes` keeps `source`, `test`, `vendored`, `generated` files;
  /// `changed_since` keeps files changed since a git ref (`"worktree"` = uncommitted edits).
  #[pyo3(signature = (verb, name, path=None, kind=None, id=None, all=false, within=None, exclude=None, classes=None, changed_since=None))]
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
    within: Option<Vec<String>>,
    exclude: Option<Vec<String>>,
    classes: Option<Vec<String>>,
    changed_since: Option<String>,
  ) -> PyResult<Py<PyAny>> {
    let value = related_value(
      &self.kg,
      self.source_root.as_deref(),
      verb,
      name,
      path,
      kind,
      id,
      all,
      ScopeArgs { within, exclude, classes, changed_since },
    )
    .map_err(PyRuntimeError::new_err)?;
    record_to_py(py, &value)
  }

  /// Typed relation-restricted traversal: BFS steps with depth, parent (`via`), relation,
  /// and grade — the same contract as the CLI/MCP `reachable`.
  /// Same scope keywords as `related`; the walk itself is unchanged (a node reached through
  /// a file outside the scope is still found, with its `via`).
  #[pyo3(signature = (name, direction, relations=None, max_depth=None, min_grade=None, path=None, kind=None, id=None, all=false, within=None, exclude=None, classes=None, changed_since=None))]
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
    within: Option<Vec<String>>,
    exclude: Option<Vec<String>>,
    classes: Option<Vec<String>>,
    changed_since: Option<String>,
  ) -> PyResult<Py<PyAny>> {
    let value = reachable_value(
      &self.kg,
      self.source_root.as_deref(),
      name,
      direction,
      relations,
      max_depth,
      min_grade,
      path,
      kind,
      id,
      all,
      ScopeArgs { within, exclude, classes, changed_since },
    )
    .map_err(PyRuntimeError::new_err)?;
    record_to_py(py, &value)
  }

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
  /// `within` / `exclude` / `classes` / `changed_since` search inside a scope: candidates
  /// are generated inside it, so `k` results means `k` results in scope. `@…` entries need
  /// a symbol and are refused here.
  #[pyo3(signature = (query, k=10, path=None, prefix=None, kind=None, lang=None, exported=false, exclude_tests=false, within=None, exclude=None, classes=None, changed_since=None))]
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
    within: Option<Vec<String>>,
    exclude: Option<Vec<String>>,
    classes: Option<Vec<String>>,
    changed_since: Option<String>,
  ) -> PyResult<Py<PyAny>> {
    // The pinned generation dir IS the index dir here (resolve is idempotent), so a rebuild
    // landing mid-session cannot swap the ranking's graph or ANN tier under us.
    let value = search_value(
      &self.generation_dir,
      self.source_root.as_deref(),
      query,
      k,
      path,
      prefix,
      kind,
      lang,
      exported,
      exclude_tests,
      ScopeArgs { within, exclude, classes, changed_since },
    )
    .map_err(PyRuntimeError::new_err)?;
    record_to_py(py, &value)
  }

  // ── Async twins: the same reads, GIL-free on the worker pool, resolved on the
  // caller's running asyncio loop. `await index.node_async(...)`. ──

  /// `node`, as an awaitable.
  pub fn node_async(&self, py: Python<'_>, id: u64) -> PyResult<Py<PyAny>> {
    let kg = self.kg.clone();
    crate::async_bridge::dispatch(py, move || node_value(&kg, id).map(crate::async_bridge::Pythonized))
  }

  /// `nodes`, as an awaitable.
  #[pyo3(signature = (name, path=None, kind=None, id=None, all=false))]
  pub fn nodes_async(
    &self,
    py: Python<'_>,
    name: String,
    path: Option<String>,
    kind: Option<String>,
    id: Option<u64>,
    all: bool,
  ) -> PyResult<Py<PyAny>> {
    let kg = self.kg.clone();
    crate::async_bridge::dispatch(py, move || {
      nodes_value(&kg, &name, path, kind, id, all).map(crate::async_bridge::Pythonized)
    })
  }

  /// `related`, as an awaitable.
  #[pyo3(signature = (verb, name, path=None, kind=None, id=None, all=false, within=None, exclude=None, classes=None, changed_since=None))]
  #[allow(clippy::too_many_arguments)]
  pub fn related_async(
    &self,
    py: Python<'_>,
    verb: String,
    name: String,
    path: Option<String>,
    kind: Option<String>,
    id: Option<u64>,
    all: bool,
    within: Option<Vec<String>>,
    exclude: Option<Vec<String>>,
    classes: Option<Vec<String>>,
    changed_since: Option<String>,
  ) -> PyResult<Py<PyAny>> {
    let kg = self.kg.clone();
    let root = self.source_root.clone();
    crate::async_bridge::dispatch(py, move || {
      related_value(
        &kg,
        root.as_deref(),
        &verb,
        &name,
        path,
        kind,
        id,
        all,
        ScopeArgs { within, exclude, classes, changed_since },
      )
      .map(crate::async_bridge::Pythonized)
    })
  }

  /// `reachable`, as an awaitable — the traversal most worth taking off the loop.
  #[pyo3(signature = (name, direction, relations=None, max_depth=None, min_grade=None, path=None, kind=None, id=None, all=false, within=None, exclude=None, classes=None, changed_since=None))]
  #[allow(clippy::too_many_arguments)]
  pub fn reachable_async(
    &self,
    py: Python<'_>,
    name: String,
    direction: String,
    relations: Option<Vec<String>>,
    max_depth: Option<u32>,
    min_grade: Option<String>,
    path: Option<String>,
    kind: Option<String>,
    id: Option<u64>,
    all: bool,
    within: Option<Vec<String>>,
    exclude: Option<Vec<String>>,
    classes: Option<Vec<String>>,
    changed_since: Option<String>,
  ) -> PyResult<Py<PyAny>> {
    let kg = self.kg.clone();
    let root = self.source_root.clone();
    crate::async_bridge::dispatch(py, move || {
      reachable_value(
        &kg,
        root.as_deref(),
        &name,
        &direction,
        relations,
        max_depth,
        min_grade.as_deref(),
        path,
        kind,
        id,
        all,
        ScopeArgs { within, exclude, classes, changed_since },
      )
      .map(crate::async_bridge::Pythonized)
    })
  }

  /// `why`, as an awaitable.
  #[pyo3(signature = (from_id, to_id=None, name=None))]
  pub fn why_async(
    &self,
    py: Python<'_>,
    from_id: u64,
    to_id: Option<u64>,
    name: Option<String>,
  ) -> PyResult<Py<PyAny>> {
    let kg = self.kg.clone();
    crate::async_bridge::dispatch(py, move || {
      why_value(&kg, from_id, to_id, name.as_deref()).map(crate::async_bridge::Pythonized)
    })
  }

  /// `search`, as an awaitable.
  #[pyo3(signature = (query, k=10, path=None, prefix=None, kind=None, lang=None, exported=false, exclude_tests=false, within=None, exclude=None, classes=None, changed_since=None))]
  #[allow(clippy::too_many_arguments)]
  pub fn search_async(
    &self,
    py: Python<'_>,
    query: String,
    k: usize,
    path: Option<String>,
    prefix: Option<String>,
    kind: Option<String>,
    lang: Option<String>,
    exported: bool,
    exclude_tests: bool,
    within: Option<Vec<String>>,
    exclude: Option<Vec<String>>,
    classes: Option<Vec<String>>,
    changed_since: Option<String>,
  ) -> PyResult<Py<PyAny>> {
    let generation_dir = self.generation_dir.clone();
    let root = self.source_root.clone();
    crate::async_bridge::dispatch(py, move || {
      search_value(
        &generation_dir,
        root.as_deref(),
        &query,
        k,
        path,
        prefix,
        kind,
        lang,
        exported,
        exclude_tests,
        ScopeArgs { within, exclude, classes, changed_since },
      )
      .map(crate::async_bridge::Pythonized)
    })
  }
}


// ── Shared Value-producing cores for the `*_async` Index methods: the same reads the
// sync methods perform, GIL-free, serialized once to `serde_json::Value` for the
// bridge's pythonizing resolver. ──

/// Map a selector outcome to `{"outcome": ..., "records": [...]}` — ambiguity is an answer
/// (the candidates to refine with), never an exception.
fn selected_to_value<T: serde::Serialize>(
  selected: vorpal_index::records::Selected<T>,
) -> Result<serde_json::Value, String> {
  let (outcome, records) = match selected {
    vorpal_index::records::Selected::NoMatch => ("no-match", serde_json::json!([])),
    vorpal_index::records::Selected::Ambiguous(candidates) => (
      "ambiguous",
      serde_json::to_value(candidates).map_err(|e| e.to_string())?,
    ),
    vorpal_index::records::Selected::Hits(hits) => (
      "hits",
      serde_json::to_value(hits).map_err(|e| e.to_string())?,
    ),
  };
  Ok(serde_json::json!({"outcome": outcome, "records": records}))
}

pub(crate) fn node_value(kg: &vorpal_kg::Kg, id: u64) -> Result<serde_json::Value, String> {
  match vorpal_index::records::node_record(kg, vorpal_kg::NodeId::new(id)) {
    Some(record) => serde_json::to_value(record).map_err(|e| e.to_string()),
    None => Ok(serde_json::Value::Null),
  }
}

pub(crate) fn nodes_value(
  kg: &vorpal_kg::Kg,
  name: &str,
  path: Option<String>,
  kind: Option<String>,
  id: Option<u64>,
  all: bool,
) -> Result<serde_json::Value, String> {
  let records = vorpal_index::records::listing_records(kg, &target_of(name, path, kind, id, all))?;
  serde_json::to_value(records).map_err(|e| e.to_string())
}

/// The scope keywords every query method takes (the MCP `scope` object, spelled for
/// Python: `exclude` because `except` is a keyword).
#[derive(Default)]
pub(crate) struct ScopeArgs {
  pub within: Option<Vec<String>>,
  pub exclude: Option<Vec<String>>,
  pub classes: Option<Vec<String>>,
  pub changed_since: Option<String>,
}

impl ScopeArgs {
  fn spec(&self) -> vorpal_index::ScopeSpec {
    vorpal_index::ScopeSpec {
      within: self.within.clone().unwrap_or_default(),
      except: self.exclude.clone().unwrap_or_default(),
      classes: self.classes.clone().unwrap_or_default(),
      changed_since: self.changed_since.clone(),
      ..Default::default()
    }
  }

  /// Resolve against the index's source root, binding `@…` entries to `anchor` (the
  /// symbol's own path). An empty scope is no scope; a deferred entry with no anchor is
  /// an error, never a silently unscoped answer.
  fn resolve(
    &self,
    source_root: Option<&std::path::Path>,
    anchor: Option<&str>,
  ) -> Result<Option<vorpal_index::PathScope>, String> {
    let spec = self.spec();
    if spec.is_empty() {
      return Ok(None);
    }
    let scope = vorpal_index::PathScope::from_spec(&spec, source_root, anchor)?;
    if !scope.deferred().is_empty() {
      return Err(format!(
        "scope entries {} bind to a symbol: use them on related or reachable, not search",
        scope.deferred().join(", ")
      ));
    }
    Ok((!scope.is_empty()).then_some(scope))
  }
}

/// The symbol's own path, for `@file` / `@dir` / `@package` and nearest-first ordering.
fn anchor_of(kg: &vorpal_kg::Kg, target: &vorpal_index::GraphTarget) -> Option<String> {
  vorpal_index::resolve_target(kg, target)
    .ok()
    .and_then(|ids| ids.first().copied())
    .and_then(|id| kg.node(id).map(|view| view.path.to_string()))
}

fn stamp_scope(value: &mut serde_json::Value, scope: Option<&vorpal_index::PathScope>, outside: Option<usize>) {
  if let Some(scope) = scope {
    value["scope"] = serde_json::to_value(scope).unwrap_or(serde_json::Value::Null);
    if let Some(outside) = outside {
      value["outsideScope"] = serde_json::json!(outside);
    }
  }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn related_value(
  kg: &vorpal_kg::Kg,
  source_root: Option<&std::path::Path>,
  verb: &str,
  name: &str,
  path: Option<String>,
  kind: Option<String>,
  id: Option<u64>,
  all: bool,
  scope: ScopeArgs,
) -> Result<serde_json::Value, String> {
  let target = target_of(name, path, kind, id, all);
  let selected = vorpal_index::records::related_records(kg, verb, &target)?;
  let anchor = anchor_of(kg, &target);
  let scope = scope.resolve(source_root, anchor.as_deref())?;
  // The scope is a view over the rows, and the rows come nearest the symbol's file first,
  // as the daemon's `graph` answers do.
  let (selected, outside) = match (selected, &scope) {
    (vorpal_index::records::Selected::Hits(hits), Some(scope)) => {
      let before = hits.len();
      let mut kept: Vec<_> = hits
        .into_iter()
        .filter(|hit| vorpal_index::records::scope_admits_record(scope, &hit.node))
        .collect();
      if let Some(anchor) = anchor.as_deref() {
        vorpal_index::records::order_by_proximity(&mut kept, anchor);
      }
      let outside = before - kept.len();
      (vorpal_index::records::Selected::Hits(kept), Some(outside))
    }
    (other, _) => (other, None),
  };
  let mut value = selected_to_value(selected)?;
  stamp_scope(&mut value, scope.as_ref(), outside);
  Ok(value)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn reachable_value(
  kg: &vorpal_kg::Kg,
  source_root: Option<&std::path::Path>,
  name: &str,
  direction: &str,
  relations: Option<Vec<String>>,
  max_depth: Option<u32>,
  min_grade: Option<&str>,
  path: Option<String>,
  kind: Option<String>,
  id: Option<u64>,
  all: bool,
  scope: ScopeArgs,
) -> Result<serde_json::Value, String> {
  let dir = match direction {
    "in" => vorpal_kg::Direction::In,
    "out" => vorpal_kg::Direction::Out,
    other => return Err(format!("direction must be \"in\" or \"out\", got '{other}'")),
  };
  let relations = match relations {
    None => vec![vorpal_kg::EdgeType::CALLS],
    Some(names) => {
      let mut out = Vec::with_capacity(names.len());
      for name in &names {
        out.push(
          vorpal_kg::EdgeType::from_name(name)
            .ok_or_else(|| format!("unknown relation '{name}'"))?,
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
    vorpal_index::min_confidence_for_grade(min_grade).map_err(|e| e.to_string())?;
  let target = target_of(name, path, kind, id, all);
  let selected = vorpal_index::records::reach_records(
    kg,
    &target,
    dir,
    &relations,
    max_depth.filter(|&d| d > 0),
    min_confidence,
  )?;
  // A view over the reached rows: the walk is unchanged, rows outside are counted.
  let anchor = anchor_of(kg, &target);
  let scope = scope.resolve(source_root, anchor.as_deref())?;
  let (selected, outside) = match (selected, &scope) {
    (vorpal_index::records::Selected::Hits(rows), Some(scope)) => {
      let before = rows.len();
      let kept: Vec<_> = rows
        .into_iter()
        .filter(|row| vorpal_index::records::scope_admits_record(scope, &row.node))
        .collect();
      let outside = before - kept.len();
      (vorpal_index::records::Selected::Hits(kept), Some(outside))
    }
    (other, _) => (other, None),
  };
  let mut value = selected_to_value(selected)?;
  stamp_scope(&mut value, scope.as_ref(), outside);
  Ok(value)
}

pub(crate) fn why_value(
  kg: &vorpal_kg::Kg,
  from_id: u64,
  to_id: Option<u64>,
  name: Option<&str>,
) -> Result<serde_json::Value, String> {
  if to_id.is_none() && name.is_none() {
    return Err("pass to_id (edge evidence) or name (absence evidence)".to_string());
  }
  let records = vorpal_index::records::evidence_records(kg, from_id, to_id, name);
  serde_json::to_value(records).map_err(|e| e.to_string())
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn search_value(
  generation_dir: &std::path::Path,
  source_root: Option<&std::path::Path>,
  query: &str,
  k: usize,
  path: Option<String>,
  prefix: Option<String>,
  kind: Option<String>,
  lang: Option<String>,
  exported: bool,
  exclude_tests: bool,
  scope: ScopeArgs,
) -> Result<serde_json::Value, String> {
  let within = scope.resolve(source_root, None)?;
  let filter = vorpal_index::SearchFilter {
    path_prefix: prefix,
    path_suffix: path,
    kind,
    lang,
    exported_only: exported,
    exclude_tests,
    within,
  };
  let records = vorpal_index::search_records_filtered(generation_dir, query, k, &filter)
    .map_err(|e| e.to_string())?;
  serde_json::to_value(records).map_err(|e| e.to_string())
}

pub(crate) fn ranked_value(
  index_dir: &str,
  query: &str,
  k: usize,
) -> Result<serde_json::Value, String> {
  let searcher =
    vorpal_index::open_searcher(std::path::Path::new(index_dir)).map_err(|e| e.to_string())?;
  let (fused, reranked) = searcher
    .records_ranked(query, k, &vorpal_index::SearchFilter::default())
    .map_err(|e| e.to_string())?;
  Ok(serde_json::json!({
    "fused": serde_json::to_value(fused).map_err(|e| e.to_string())?,
    "reranked": serde_json::to_value(reranked).map_err(|e| e.to_string())?,
    "encoderStatus": searcher.encoder_status(),
  }))
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

/// `vorpal search --ranked`'s core: ONE search, two orderings — the fused ranking
/// and, when an encoder serves this index (per-index `encoder.dir` or the global
/// enable), the reranked ordering derived from the SAME fusion. Returns
/// `{"fused": [hits], "reranked": [hits] | None, "encoderStatus": str | None}`;
/// `encoderStatus` states why a configured encoder is inactive.
#[pyfunction]
#[pyo3(signature = (index_dir, query, k=10))]
pub fn index_search_ranked(
  py: Python<'_>,
  index_dir: &str,
  query: &str,
  k: usize,
) -> PyResult<Py<PyAny>> {
  #[derive(serde::Serialize)]
  #[serde(rename_all = "camelCase")]
  struct RankedAnswer {
    fused: Vec<vorpal_index::records::SearchHitRecord>,
    reranked: Option<Vec<vorpal_index::records::SearchHitRecord>>,
    encoder_status: Option<String>,
  }
  let index_dir = index_dir.to_string();
  let query = query.to_string();
  let result: Result<RankedAnswer, String> = py.detach(move || {
    let searcher =
      vorpal_index::open_searcher(std::path::Path::new(&index_dir)).map_err(|e| e.to_string())?;
    let (fused, reranked) = searcher
      .records_ranked(&query, k, &vorpal_index::SearchFilter::default())
      .map_err(|e| e.to_string())?;
    Ok(RankedAnswer {
      fused,
      reranked,
      encoder_status: searcher.encoder_status().map(str::to_string),
    })
  });
  record_to_py(py, &result.map_err(PyRuntimeError::new_err)?)
}

/// The `vorpal tune` core: measure the optional ranking features on YOUR queries
/// and, with `apply=True`, write this index's switches from the verdicts.
/// `queries` is a list of `(query, expected_substring_or_None)` pairs — only
/// labelled entries score (reciprocal rank of the expected hit; both comparisons
/// paired from one search each). Returns the tune report: per-feature tallies and
/// verdicts, plus `wroteEncoder`/`wroteBm25` describing any switch written (the
/// BM25 override holds until the index content retrains).
#[pyfunction]
#[pyo3(signature = (index_dir, queries, k=10, apply=false))]
pub fn index_tune(
  py: Python<'_>,
  index_dir: &str,
  queries: Vec<(String, Option<String>)>,
  k: usize,
  apply: bool,
) -> PyResult<Py<PyAny>> {
  let index_dir = index_dir.to_string();
  let result: Result<vorpal_index::tune::TuneReport, String> = py.detach(move || {
    let queries: Vec<vorpal_index::tune::TuneQuery> = queries
      .into_iter()
      .map(|(query, expected)| vorpal_index::tune::TuneQuery { query, expected })
      .collect();
    vorpal_index::tune::tune_index(std::path::Path::new(&index_dir), &queries, k, apply)
  });
  record_to_py(py, &result.map_err(PyRuntimeError::new_err)?)
}
