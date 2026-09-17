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
  /// Filter the answer's rows (`Index.related`): rows outside the scope are dropped and
  /// counted in `outsideScope`. `indexGraph` renders the unscoped text answer and refuses it.
  pub scope: Option<ScopeOptions>,
}

/// Path and row filters, the MCP `scope` object: `within` / `except` are path prefixes
/// relative to the source root (the tree a default-layout `<src>/.vorpal/index` names) or
/// absolute; `@file`, `@dir`, `@package` bind to the symbol a `related` / `reachable` call
/// is about; `classes` keeps `source`, `test`, `vendored`, `generated` files; `kind`,
/// `lang`, `exported` filter rows; `changedSince` keeps files changed since a git ref
/// (`worktree` = uncommitted edits). A path that names nothing is an error.
#[napi(object)]
#[derive(Default, Clone)]
pub struct ScopeOptions {
  pub within: Option<Vec<String>>,
  pub except: Option<Vec<String>>,
  pub classes: Option<Vec<String>>,
  pub kind: Option<String>,
  pub lang: Option<String>,
  pub exported: Option<bool>,
  pub changed_since: Option<String>,
}

fn scope_spec(options: &ScopeOptions) -> vorpal_index::ScopeSpec {
  vorpal_index::ScopeSpec {
    within: options.within.clone().unwrap_or_default(),
    except: options.except.clone().unwrap_or_default(),
    classes: options.classes.clone().unwrap_or_default(),
    kind: options.kind.clone(),
    lang: options.lang.clone(),
    exported: options.exported,
    changed_since: options.changed_since.clone(),
  }
}

/// Resolve a call's scope against the index's source root, binding `@…` entries to
/// `anchor` (the symbol's own path). An empty scope is no scope.
fn resolve_scope(
  options: Option<&ScopeOptions>,
  source_root: Option<&std::path::Path>,
  anchor: Option<&str>,
) -> Result<Option<vorpal_index::PathScope>> {
  let Some(options) = options else {
    return Ok(None);
  };
  let spec = scope_spec(options);
  if spec.is_empty() {
    return Ok(None);
  }
  let scope = vorpal_index::PathScope::from_spec(&spec, source_root, anchor).map_err(Error::from_reason)?;
  if !scope.deferred().is_empty() {
    return Err(Error::from_reason(format!(
      "scope entries {} bind to a symbol: use them on related or reachable, not search",
      scope.deferred().join(", ")
    )));
  }
  Ok((!scope.is_empty()).then_some(scope))
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

/// Graph query with the shared symbol-selector contract.
#[napi]
pub fn index_graph(
  index_dir: String,
  verb: String,
  name: String,
  options: Option<GraphOptions>,
) -> Result<String> {
  let options = options.unwrap_or_default();
  if options.scope.as_ref().is_some_and(|scope| !scope_spec(scope).is_empty()) {
    return Err(Error::from_reason(
      "scope is honoured by Index.related; indexGraph renders the unscoped text answer",
    ));
  }
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
  /// Filter the reached rows; the walk itself is unchanged (a node reached through a file
  /// outside the scope is still found, with its `via`). Dropped rows are counted in
  /// `outsideScope`.
  pub scope: Option<ScopeOptions>,
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
  /// Shared, immutable, mmap-backed graph — `Arc` so async methods can run their reads
  /// on the uv pool while the JS object stays alive (the daemon shares `Kg` the same way).
  kg: std::sync::Arc<vorpal_kg::Kg>,
  generation_dir: std::path::PathBuf,
  generation: String,
  /// The tree a default-layout index dir names; relative scope entries resolve against it.
  source_root: Option<std::path::PathBuf>,
}

#[napi]
impl Index {
  /// Open `index_dir`, pinning its CURRENT generation for the session's lifetime.
  #[napi(factory)]
  pub fn open(index_dir: String) -> Result<Index> {
    let root = std::path::Path::new(&index_dir);
    let generation_dir = vorpal_kg::resolve_index_dir(root);
    let kg = std::sync::Arc::new(vorpal_kg::Kg::load(&generation_dir).map_err(|e| to_napi_err(Box::new(e)))?);
    let generation = generation_dir
      .file_name()
      .and_then(|name| name.to_str())
      .unwrap_or("")
      .to_string();
    Ok(Index {
      kg,
      generation_dir,
      generation,
      source_root: vorpal_index::default_layout_root(root),
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
    node_core(&self.kg, id)
  }

  /// Typed candidate listing for a selector: every match is the answer.
  #[napi]
  pub fn nodes(&self, name: String, options: Option<GraphOptions>) -> Result<serde_json::Value> {
    nodes_core(&self.kg, name, options)
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
    related_core(&self.kg, self.source_root.as_deref(), verb, name, options)
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
    reachable_core(&self.kg, self.source_root.as_deref(), name, direction, options)
  }

  /// Typed evidence (`why`): edge form (`toId`) or absence form (`name`).
  #[napi]
  pub fn why(
    &self,
    from_id: i64,
    to_id: Option<i64>,
    name: Option<String>,
  ) -> Result<serde_json::Value> {
    why_core(&self.kg, from_id, to_id, name)
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
    search_core(&self.generation_dir, self.source_root.as_deref(), query, k, options)
  }

  /// `node`, off the event loop.
  #[napi]
  pub fn node_async(&self, id: i64) -> AsyncTask<crate::repo_async::RepoTask<crate::repo_async::Json>> {
    let kg = self.kg.clone();
    AsyncTask::new(crate::repo_async::RepoTask::new(move || {
      node_core(&kg, id).map(crate::repo_async::Json)
    }))
  }

  /// `nodes`, off the event loop.
  #[napi]
  pub fn nodes_async(
    &self,
    name: String,
    options: Option<GraphOptions>,
  ) -> AsyncTask<crate::repo_async::RepoTask<crate::repo_async::Json>> {
    let kg = self.kg.clone();
    AsyncTask::new(crate::repo_async::RepoTask::new(move || {
      nodes_core(&kg, name, options).map(crate::repo_async::Json)
    }))
  }

  /// `related`, off the event loop.
  #[napi]
  pub fn related_async(
    &self,
    verb: String,
    name: String,
    options: Option<GraphOptions>,
  ) -> AsyncTask<crate::repo_async::RepoTask<crate::repo_async::Json>> {
    let kg = self.kg.clone();
    let root = self.source_root.clone();
    AsyncTask::new(crate::repo_async::RepoTask::new(move || {
      related_core(&kg, root.as_deref(), verb, name, options).map(crate::repo_async::Json)
    }))
  }

  /// `reachable`, off the event loop — deep traversals on big graphs are the class
  /// method most worth taking off the loop.
  #[napi]
  pub fn reachable_async(
    &self,
    name: String,
    direction: String,
    options: Option<ReachOptions>,
  ) -> AsyncTask<crate::repo_async::RepoTask<crate::repo_async::Json>> {
    let kg = self.kg.clone();
    let root = self.source_root.clone();
    AsyncTask::new(crate::repo_async::RepoTask::new(move || {
      reachable_core(&kg, root.as_deref(), name, direction, options).map(crate::repo_async::Json)
    }))
  }

  /// `why`, off the event loop.
  #[napi]
  pub fn why_async(
    &self,
    from_id: i64,
    to_id: Option<i64>,
    name: Option<String>,
  ) -> AsyncTask<crate::repo_async::RepoTask<crate::repo_async::Json>> {
    let kg = self.kg.clone();
    AsyncTask::new(crate::repo_async::RepoTask::new(move || {
      why_core(&kg, from_id, to_id, name).map(crate::repo_async::Json)
    }))
  }

  /// `search`, off the event loop.
  #[napi]
  pub fn search_async(
    &self,
    query: String,
    k: Option<u32>,
    options: Option<SearchOptions>,
  ) -> AsyncTask<crate::repo_async::RepoTask<crate::repo_async::Json>> {
    let generation_dir = self.generation_dir.clone();
    let root = self.source_root.clone();
    AsyncTask::new(crate::repo_async::RepoTask::new(move || {
      search_core(&generation_dir, root.as_deref(), query, k, options).map(crate::repo_async::Json)
    }))
  }
}


// ── Shared method cores: one body per operation, called by the sync method on the
// caller's thread and by its `*Async` twin on the uv pool. ──

pub(crate) fn node_core(kg: &vorpal_kg::Kg, id: i64) -> Result<serde_json::Value> {
    let raw = u64::try_from(id).map_err(|_| Error::from_reason("id must be non-negative"))?;
    match vorpal_index::records::node_record(kg, vorpal_kg::NodeId::new(raw)) {
      Some(record) => serde_json::to_value(record).map_err(|e| Error::from_reason(e.to_string())),
      None => Ok(serde_json::Value::Null),
    }
  }

pub(crate) fn nodes_core(kg: &vorpal_kg::Kg, name: String, options: Option<GraphOptions>) -> Result<serde_json::Value> {
    let options = options.unwrap_or_default();
    let records =
      vorpal_index::records::listing_records(kg, &selector_target(name, &options))
        .map_err(Error::from_reason)?;
    serde_json::to_value(records).map_err(|e| Error::from_reason(e.to_string()))
  }

pub(crate) fn related_core(
  kg: &vorpal_kg::Kg,
  source_root: Option<&std::path::Path>,
  verb: String,
  name: String,
  options: Option<GraphOptions>,
) -> Result<serde_json::Value> {
  let options = options.unwrap_or_default();
  let target = selector_target(name, &options);
  let selected = vorpal_index::records::related_records(kg, &verb, &target).map_err(Error::from_reason)?;
  let anchor = anchor_of(kg, &target);
  let scope = resolve_scope(options.scope.as_ref(), source_root, anchor.as_deref())?;
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

pub(crate) fn reachable_core(
  kg: &vorpal_kg::Kg,
  source_root: Option<&std::path::Path>,
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
      kg,
      &target,
      dir,
      &relations,
      options.max_depth.filter(|&d| d > 0),
      min_confidence,
    )
    .map_err(Error::from_reason)?;
    // A view over the reached rows: the walk is unchanged, rows outside are counted.
    let anchor = anchor_of(kg, &target);
    let scope = resolve_scope(options.scope.as_ref(), source_root, anchor.as_deref())?;
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

pub(crate) fn why_core(kg: &vorpal_kg::Kg, from_id: i64, to_id: Option<i64>, name: Option<String>) -> Result<serde_json::Value> {
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
      vorpal_index::records::evidence_records(kg, from, to, name.as_deref());
    serde_json::to_value(records).map_err(|e| Error::from_reason(e.to_string()))
  }

pub(crate) fn search_core(
  generation_dir: &std::path::Path,
  source_root: Option<&std::path::Path>,
  query: String,
  k: Option<u32>,
  options: Option<SearchOptions>,
) -> Result<serde_json::Value> {
    let options = options.unwrap_or_default();
    let within = resolve_scope(options.scope.as_ref(), source_root, None)?;
    let filter = vorpal_index::SearchFilter {
      path_prefix: options.prefix,
      path_suffix: options.path,
      kind: options.kind,
      lang: options.lang,
      exported_only: options.exported.unwrap_or(false),
      exclude_tests: options.exclude_tests.unwrap_or(false),
      within,
    };
    // The pinned generation dir IS the index dir here (resolve is idempotent), so a rebuild
    // landing mid-session cannot swap the ranking's graph or ANN tier under us.
    let records = vorpal_index::search_records_filtered(
      generation_dir,
      &query,
      k.unwrap_or(10) as usize,
      &filter,
    )
    .map_err(to_napi_err)?;
    serde_json::to_value(records).map_err(|e| Error::from_reason(e.to_string()))
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
  /// Exclude definitions in test files from results.
  pub exclude_tests: Option<bool>,
  /// Search inside a scope: candidates are generated inside it, so `k` results means `k`
  /// results in scope. `@…` entries need a symbol and are refused here.
  pub scope: Option<ScopeOptions>,
}

/// `vorpal search --ranked`'s core: ONE search, two orderings — the fused ranking
/// and, when an encoder serves this index (per-index `encoder.dir` or the global
/// enable), the reranked ordering derived from the SAME fusion. Returns
/// `{ fused, reranked | null, encoderStatus | null }`; `encoderStatus` states why
/// a configured encoder is inactive.
#[napi]
pub fn index_search_ranked(
  index_dir: String,
  query: String,
  k: Option<u32>,
) -> Result<serde_json::Value> {
  let searcher =
    vorpal_index::open_searcher(std::path::Path::new(&index_dir)).map_err(to_napi_err)?;
  let (fused, reranked) = searcher
    .records_ranked(
      &query,
      k.unwrap_or(10) as usize,
      &vorpal_index::SearchFilter::default(),
    )
    .map_err(to_napi_err)?;
  Ok(serde_json::json!({
    "fused": fused,
    "reranked": reranked,
    "encoderStatus": searcher.encoder_status(),
  }))
}

/// One tune query for `indexTune`.
#[napi(object)]
pub struct TuneQueryInput {
  pub query: String,
  /// Case-insensitive substring of the expected hit's name or path.
  pub expected: Option<String>,
}

/// The `vorpal tune` core: measure the optional ranking features on YOUR queries
/// and, with `apply: true`, write this index's switches from the verdicts. Only
/// entries with `expected` score (reciprocal rank; both comparisons paired from
/// one search each). Returns the tune report: per-feature tallies and verdicts,
/// plus `wroteEncoder`/`wroteBm25` for any switch written (the BM25 override
/// holds until the index content retrains).
#[napi]
pub fn index_tune(
  index_dir: String,
  queries: Vec<TuneQueryInput>,
  k: Option<u32>,
  apply: Option<bool>,
) -> Result<serde_json::Value> {
  let queries: Vec<vorpal_index::tune::TuneQuery> = queries
    .into_iter()
    .map(|input| vorpal_index::tune::TuneQuery {
      query: input.query,
      expected: input.expected,
    })
    .collect();
  let report = vorpal_index::tune::tune_index(
    std::path::Path::new(&index_dir),
    &queries,
    k.unwrap_or(10) as usize,
    apply.unwrap_or(false),
  )
  .map_err(Error::from_reason)?;
  serde_json::to_value(report).map_err(|e| Error::from_reason(e.to_string()))
}

#[cfg(test)]
mod scope_tests {
  use super::{GraphOptions, ReachOptions, ScopeOptions, SearchOptions, related_core, reachable_core, search_core};

  fn fixture(tag: &str) -> (std::path::PathBuf, vorpal_kg::Kg) {
    let base = std::env::temp_dir().join(format!("vorpal-node-scope-{tag}-{}", std::process::id()));
    let src = base.join("src");
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(src.join("sub")).unwrap();
    std::fs::write(src.join("b.rs"), "pub fn target() -> i32 {\n    0\n}\n").unwrap();
    std::fs::write(src.join("a.rs"), "use b::target;\n\npub fn caller() -> i32 {\n    target()\n}\n").unwrap();
    std::fs::write(
      src.join("sub").join("c.rs"),
      "use b::target;\n\npub fn caller2() -> i32 {\n    target()\n}\n",
    )
    .unwrap();
    let index = src.join(".vorpal").join("index");
    vorpal_index::build_index(&src, &index).expect("index");
    let kg = vorpal_kg::Kg::load(&vorpal_kg::resolve_index_dir(&index)).expect("kg");
    (index, kg)
  }

  fn within(entries: &[&str]) -> ScopeOptions {
    ScopeOptions {
      within: Some(entries.iter().map(|e| e.to_string()).collect()),
      ..Default::default()
    }
  }

  #[test]
  fn related_reachable_and_search_honour_the_scope() {
    let (index, kg) = fixture("cores");
    let root = vorpal_index::default_layout_root(&index);
    assert!(root.is_some(), "default layout names its tree");

    let value = related_core(
      &kg,
      root.as_deref(),
      "callers".into(),
      "target".into(),
      Some(GraphOptions { scope: Some(within(&["sub"])), ..Default::default() }),
    )
    .unwrap();
    assert_eq!(value["outcome"], "hits", "{value}");
    assert_eq!(value["records"].as_array().unwrap().len(), 1, "{value}");
    assert_eq!(value["outsideScope"], 1, "{value}");
    assert!(value["records"][0]["path"].as_str().unwrap().ends_with("sub/c.rs"), "{value}");
    assert_eq!(value["scope"]["within"], serde_json::json!(["sub"]), "{value}");

    // `@dir` binds to the symbol asked about: caller2's own directory is `sub`.
    let value = related_core(
      &kg,
      root.as_deref(),
      "callees".into(),
      "caller2".into(),
      Some(GraphOptions { scope: Some(within(&["@dir"])), ..Default::default() }),
    )
    .unwrap();
    assert_eq!(value["records"].as_array().unwrap().len(), 0, "{value}");
    assert_eq!(value["outsideScope"], 1, "{value}");

    let value = reachable_core(
      &kg,
      root.as_deref(),
      "target".into(),
      "in".into(),
      Some(ReachOptions { scope: Some(within(&["sub"])), ..Default::default() }),
    )
    .unwrap();
    assert_eq!(value["outcome"], "hits", "{value}");
    for row in value["records"].as_array().unwrap() {
      assert!(row["path"].as_str().unwrap().ends_with("sub/c.rs"), "{value}");
    }
    assert_eq!(value["outsideScope"], 1, "{value}");

    let generation = vorpal_kg::resolve_index_dir(&index);
    let value = search_core(
      &generation,
      root.as_deref(),
      "caller".into(),
      Some(5),
      Some(SearchOptions { scope: Some(within(&["sub"])), ..Default::default() }),
    )
    .unwrap();
    let hits = value.as_array().expect("search returns the hit list");
    assert!(!hits.is_empty(), "{value}");
    for hit in hits {
      assert!(hit["path"].as_str().unwrap().ends_with("sub/c.rs"), "{value}");
    }

    // A scope entry that names nothing is an error, never an empty answer; `@dir` on a
    // search has no symbol to bind to.
    let err = search_core(&generation, root.as_deref(), "caller".into(), Some(5), Some(SearchOptions { scope: Some(within(&["nope"])), ..Default::default() })).unwrap_err();
    assert!(err.reason.contains("nope"), "{err}");
    let err = search_core(&generation, root.as_deref(), "caller".into(), Some(5), Some(SearchOptions { scope: Some(within(&["@dir"])), ..Default::default() })).unwrap_err();
    assert!(err.reason.contains("bind to a symbol"), "{err}");
    let _ = std::fs::remove_dir_all(index.parent().unwrap().parent().unwrap().parent().unwrap());
  }
}
