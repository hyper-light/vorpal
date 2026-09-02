//! Async twins of the repository API: every blocking operation as an `AsyncTask`
//! computing on libuv's thread pool, so an `indexBuild` of a multi-million-line tree
//! never freezes the event loop. Naming follows the parser precedent (`parse` /
//! `parseAsync`): each sync function in [`crate::repo`] gains an `Async`-suffixed twin
//! returning a `Promise`. The `Index` class stays synchronous by design — its queries
//! answer in well under a millisecond from a pinned, mmapped generation, so a Promise
//! would cost more than the call.

use napi::bindgen_prelude::*;
use napi::{Env, Task};
use napi_derive::napi;

use crate::repo::{BuildReport, GraphOptions, NodeInfo, TuneQueryInput};

/// JSON payload with the napi identity `serde_json::Value` itself lacks (`TypeName`),
/// so ranked/tune results ride the same generic task as every other return type.
pub struct Json(pub serde_json::Value);

impl TypeName for Json {
  fn type_name() -> &'static str {
    "object"
  }
  fn value_type() -> ValueType {
    ValueType::Object
  }
}

impl ToNapiValue for Json {
  unsafe fn to_napi_value(env: napi::sys::napi_env, val: Self) -> Result<napi::sys::napi_value> {
    unsafe { serde_json::Value::to_napi_value(env, val.0) }
  }
}

/// One blocking repo call, boxed for the uv pool. Each constructor captures owned
/// arguments; `compute` runs the same body the sync twin runs.
pub struct RepoTask<T: Send + 'static> {
  work: Option<Box<dyn FnOnce() -> Result<T> + Send + 'static>>,
}

impl<T: Send + 'static> RepoTask<T> {
  pub(crate) fn new(work: impl FnOnce() -> Result<T> + Send + 'static) -> Self {
    Self {
      work: Some(Box::new(work)),
    }
  }
}

impl<T: Send + ToNapiValue + TypeName + 'static> Task for RepoTask<T> {
  type Output = T;
  type JsValue = T;

  fn compute(&mut self) -> Result<Self::Output> {
    (self.work.take().expect("computed once"))()
  }

  fn resolve(&mut self, _env: Env, output: Self::Output) -> Result<Self::JsValue> {
    Ok(output)
  }
}

/// `indexBuild`, off the event loop. Resolves to the committed generation directory.
#[cfg_attr(test, allow(dead_code))] // napi registration glue is absent under cfg(test)
#[napi]
pub fn index_build_async(src: String, out: Option<String>) -> AsyncTask<RepoTask<String>> {
  AsyncTask::new(RepoTask::new(move || crate::repo::index_build(src, out)))
}

/// `indexBuildReport`, off the event loop.
#[cfg_attr(test, allow(dead_code))] // napi registration glue is absent under cfg(test)
#[napi]
pub fn index_build_report_async(
  src: String,
  out: Option<String>,
) -> AsyncTask<RepoTask<BuildReport>> {
  AsyncTask::new(RepoTask::new(move || {
    crate::repo::index_build_report(src, out)
  }))
}

/// `indexSearch`, off the event loop.
#[cfg_attr(test, allow(dead_code))] // napi registration glue is absent under cfg(test)
#[napi]
pub fn index_search_async(
  index_dir: String,
  query: String,
  k: Option<u32>,
  explain: Option<bool>,
) -> AsyncTask<RepoTask<String>> {
  AsyncTask::new(RepoTask::new(move || {
    crate::repo::index_search(index_dir, query, k, explain)
  }))
}

/// `indexSearchRanked`, off the event loop (encoder reranking is seconds-scale work).
#[cfg_attr(test, allow(dead_code))] // napi registration glue is absent under cfg(test)
#[napi]
pub fn index_search_ranked_async(
  index_dir: String,
  query: String,
  k: Option<u32>,
) -> AsyncTask<RepoTask<Json>> {
  AsyncTask::new(RepoTask::new(move || {
    crate::repo::index_search_ranked(index_dir, query, k).map(Json)
  }))
}

/// `indexGraph`, off the event loop.
#[cfg_attr(test, allow(dead_code))] // napi registration glue is absent under cfg(test)
#[napi]
pub fn index_graph_async(
  index_dir: String,
  verb: String,
  name: String,
  options: Option<GraphOptions>,
) -> AsyncTask<RepoTask<String>> {
  AsyncTask::new(RepoTask::new(move || {
    crate::repo::index_graph(index_dir, verb, name, options)
  }))
}

/// `indexNode`, off the event loop.
#[cfg_attr(test, allow(dead_code))] // napi registration glue is absent under cfg(test)
#[napi]
pub fn index_node_async(index_dir: String, id: i64) -> AsyncTask<RepoTask<NodeInfo>> {
  AsyncTask::new(RepoTask::new(move || crate::repo::index_node(index_dir, id)))
}

/// `indexTune`, off the event loop (it runs one search per labelled query).
#[cfg_attr(test, allow(dead_code))] // napi registration glue is absent under cfg(test)
#[napi]
pub fn index_tune_async(
  index_dir: String,
  queries: Vec<TuneQueryInput>,
  k: Option<u32>,
  apply: Option<bool>,
) -> AsyncTask<RepoTask<Json>> {
  AsyncTask::new(RepoTask::new(move || {
    crate::repo::index_tune(index_dir, queries, k, apply).map(Json)
  }))
}

/// `semanticInstall`, off the event loop — this one downloads hundreds of megabytes of
/// weights; blocking a server's loop on it was never acceptable.
#[cfg_attr(test, allow(dead_code))] // napi registration glue is absent under cfg(test)
#[napi]
pub fn semantic_install_async(
  variant: String,
  root: Option<String>,
) -> AsyncTask<RepoTask<String>> {
  AsyncTask::new(RepoTask::new(move || {
    crate::models::semantic_install(variant, root)
  }))
}

/// `semanticEnable`, off the event loop.
#[cfg_attr(test, allow(dead_code))] // napi registration glue is absent under cfg(test)
#[napi]
pub fn semantic_enable_async(
  variant: String,
  root: Option<String>,
) -> AsyncTask<RepoTask<String>> {
  AsyncTask::new(RepoTask::new(move || {
    crate::models::semantic_enable(variant, root)
  }))
}
