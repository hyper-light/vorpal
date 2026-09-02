//! Native async: blocking repository operations exposed to Python as real awaitables,
//! driven by a **Rust-owned** worker pool. No tokio, no `asyncio` thread pool, and no
//! `Arc` in our code.
//!
//! Shape of one `await vorpal.search(...)`:
//!   1. On the calling thread (holding the GIL, inside the caller's running loop) we ask
//!      `asyncio` for the running loop and make a `Future`, returned immediately.
//!   2. The blocking work is boxed and handed to a lazily-grown pool of `std::thread`
//!      workers. Each runs it **GIL-free** (pure Rust — `search_index`/`build_index`),
//!      so N concurrent awaits do their native work on N cores at once.
//!   3. When the work finishes the worker reacquires the GIL only to schedule the
//!      Future's resolution back on the loop thread via `loop.call_soon_threadsafe` —
//!      the sole thread-safe way to touch a Future from off-loop. A cancelled Future is
//!      left untouched.
//!
//! The pool is a `OnceLock` singleton (immutable after init — not a rebindable global),
//! sized from `VORPAL_ASYNC_WORKERS` or `8× cores`, and grows a worker only when every
//! existing worker is busy (bounded, so an idle process holds no threads).

use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

use std::sync::Mutex;

use crossbeam_channel::{Receiver, Sender, unbounded};
use pyo3::IntoPyObjectExt;
use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use pyo3::types::{PyCFunction, PyList, PyTuple};

use crate::repo::{BuildReport, NodeInfo};

type Job = Box<dyn FnOnce() + Send + 'static>;

/// A lazily-grown, bounded pool of OS-thread workers for GIL-free blocking calls.
struct Pool {
  tx: Sender<Job>,
  rx: Receiver<Job>,
  max: usize,
  /// Workers spawned so far (monotonic, bounded by `max`).
  spawned: AtomicUsize,
  /// Workers **currently blocked on `recv`** (genuinely idle). Incremented the instant a
  /// worker enters the wait and decremented the instant it leaves, so `idle == 0` is an
  /// accurate "every worker is busy" signal — unlike an active-count that lags behind
  /// `recv` and made a burst of submits pile onto one or two workers.
  idle: AtomicUsize,
}

static POOL: OnceLock<Pool> = OnceLock::new();

fn pool() -> &'static Pool {
  POOL.get_or_init(|| {
    let max = std::env::var("VORPAL_ASYNC_WORKERS")
      .ok()
      .and_then(|s| s.parse::<usize>().ok())
      .filter(|&n| n > 0)
      .unwrap_or_else(|| {
        std::thread::available_parallelism()
          .map(|n| n.get())
          .unwrap_or(4)
          * 8
      });
    let (tx, rx) = unbounded::<Job>();
    Pool {
      tx,
      rx,
      max,
      spawned: AtomicUsize::new(0),
      idle: AtomicUsize::new(0),
    }
  })
}

impl Pool {
  fn submit(&'static self, job: Job) {
    self
      .tx
      .send(job)
      .expect("vorpal async pool channel closed");
    // Spawn a worker unless one is already idle to grab this job — and keep growing (up to
    // `max`) while nothing is idle, so a burst of N submits ends up on ~min(N, max) workers
    // running in parallel rather than serialized behind one.
    while self.idle.load(Ordering::Acquire) == 0 {
      let spawned = self.spawned.load(Ordering::Acquire);
      if spawned >= self.max {
        return; // at ceiling; jobs wait for a free worker
      }
      if self
        .spawned
        .compare_exchange(spawned, spawned + 1, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
      {
        self.spawn_worker(spawned);
        return;
      }
    }
  }

  fn spawn_worker(&'static self, index: usize) {
    let rx = self.rx.clone();
    let idle: &'static AtomicUsize = &self.idle;
    std::thread::Builder::new()
      .name(format!("vorpal-async-{index}"))
      .spawn(move || {
        loop {
          idle.fetch_add(1, Ordering::AcqRel);
          let job = rx.recv();
          idle.fetch_sub(1, Ordering::AcqRel);
          match job {
            Ok(job) => job(),
            Err(_) => break, // channel closed (never, in practice — the pool is 'static)
          }
        }
      })
      .expect("spawn vorpal async worker");
  }
}

/// The guarded Future-completion callable, created once and reused. It runs on the loop
/// thread (scheduled via `call_soon_threadsafe`), so touching the Future is safe; a
/// cancelled Future is skipped (setting one would raise `InvalidStateError`).
fn resolver(py: Python<'_>) -> PyResult<Py<PyAny>> {
  static RESOLVER: OnceLock<Py<PyAny>> = OnceLock::new();
  if let Some(r) = RESOLVER.get() {
    return Ok(r.clone_ref(py));
  }
  let closure = PyCFunction::new_closure(
    py,
    Some(c"_vorpal_resolve"),
    None,
    |args: &Bound<'_, PyTuple>, _kwargs: Option<&Bound<'_, pyo3::types::PyDict>>| -> PyResult<()> {
      let fut = args.get_item(0)?;
      let value = args.get_item(1)?;
      let is_exc = args.get_item(2)?.extract::<bool>()?;
      if fut.call_method0("cancelled")?.extract::<bool>()? {
        return Ok(());
      }
      if is_exc {
        fut.call_method1("set_exception", (value,))?;
      } else {
        fut.call_method1("set_result", (value,))?;
      }
      Ok(())
    },
  )?;
  let stored = closure.into_any().unbind();
  // If another thread raced us, keep the winner; both are equivalent.
  let _ = RESOLVER.set(stored.clone_ref(py));
  Ok(RESOLVER.get().unwrap().clone_ref(py))
}

/// Turn a blocking `work` closure into a Python awaitable resolved by the Rust pool.
pub(crate) fn dispatch<T, F>(py: Python<'_>, work: F) -> PyResult<Py<PyAny>>
where
  T: for<'py> IntoPyObject<'py> + Send + 'static,
  F: FnOnce() -> Result<T, String> + Send + 'static,
{
  let asyncio = py.import("asyncio")?;
  let event_loop = asyncio.call_method0("get_running_loop")?;
  let future = event_loop.call_method0("create_future")?;
  let fut_return = future.clone().unbind();
  let fut_worker = future.unbind();
  let loop_worker = event_loop.unbind();

  pool().submit(Box::new(move || {
    let outcome = work(); // GIL-free: the actual index/search work
    Python::attach(|py| {
      let Ok(resolver) = resolver(py) else {
        return;
      };
      let loop_ = loop_worker.bind(py);
      let (payload, is_exc): (Py<PyAny>, bool) = match outcome {
        Ok(value) => match value.into_py_any(py) {
          Ok(obj) => (obj, false),
          Err(e) => (e.into_value(py).into_any(), true),
        },
        Err(msg) => (PyRuntimeError::new_err(msg).into_value(py).into_any(), true),
      };
      // Best-effort: if the loop is already closed (interpreter shutdown), drop it.
      let _ = loop_.call_method1(
        "call_soon_threadsafe",
        (resolver, fut_worker.bind(py), payload, is_exc),
      );
    });
  }));
  Ok(fut_return)
}

// ── Awaitable repository API ─────────────────────────────────────────────────
// Thin wrappers: own their arguments (so the closure is `Send + 'static`) and hand the
// blocking call to the pool. The synchronous `index_*` twins live in `repo`.

/// `await vorpal.build(src, out=None)` — returns the one-line report.
#[pyfunction]
#[pyo3(signature = (src, out=None))]
pub fn build(py: Python<'_>, src: String, out: Option<String>) -> PyResult<Py<PyAny>> {
  dispatch(py, move || {
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
  })
}

/// `await vorpal.build_report(src, out=None)` — returns the typed [`BuildReport`].
#[pyfunction]
#[pyo3(signature = (src, out=None))]
pub fn build_report(py: Python<'_>, src: String, out: Option<String>) -> PyResult<Py<PyAny>> {
  dispatch(py, move || {
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
  })
}

/// `await vorpal.search(index_dir, query, k=10, explain=False)`.
#[pyfunction]
#[pyo3(signature = (index_dir, query, k=10, explain=false))]
pub fn search(
  py: Python<'_>,
  index_dir: String,
  query: String,
  k: usize,
  explain: bool,
) -> PyResult<Py<PyAny>> {
  dispatch(py, move || {
    let dir = std::path::Path::new(&index_dir);
    if explain {
      vorpal_index::search_index_explained(dir, &query, k).map_err(|e| e.to_string())
    } else {
      vorpal_index::search_index(dir, &query, k).map_err(|e| e.to_string())
    }
  })
}

/// Scatter-gather state for a batched await: N pool jobs fill their own slot, and the last
/// to finish resolves the single Future. `Arc` is the legitimate case here — a genuinely
/// shared, `'static` result buffer written by many worker threads (borrows can't express it).
struct Batch {
  slots: Vec<Mutex<Option<Result<String, String>>>>,
  remaining: std::sync::atomic::AtomicUsize,
  loop_: Py<PyAny>,
  fut: Py<PyAny>,
}

/// `await vorpal.search_many(index_dir, [q1, q2, …], k=10)` — run many searches over one index
/// concurrently and get back a list of results in query order. One Future and one loop-thread
/// resolution amortize the event-loop coordination that caps per-call awaits, so the worker
/// pool saturates every core. All jobs share the one cached [`Searcher`] mapping.
#[pyfunction]
#[pyo3(signature = (index_dir, queries, k=10))]
pub fn search_many(
  py: Python<'_>,
  index_dir: String,
  queries: Vec<String>,
  k: usize,
) -> PyResult<Py<PyAny>> {
  use std::sync::atomic::Ordering;
  let asyncio = py.import("asyncio")?;
  let event_loop = asyncio.call_method0("get_running_loop")?;
  let future = event_loop.call_method0("create_future")?;
  let fut_return = future.clone().unbind();
  let n = queries.len();
  if n == 0 {
    future.call_method1("set_result", (PyList::empty(py),))?;
    return Ok(fut_return);
  }
  // Open the index once and share the mapping across every job — one cache lookup for the
  // whole batch, then lock-free reads. (Opening per job would re-lock the searcher cache N
  // times, which measurably capped core utilization on large batches.)
  let searcher = match vorpal_index::open_searcher(std::path::Path::new(&index_dir)) {
    Ok(s) => s,
    Err(e) => {
      future.call_method1(
        "set_exception",
        (PyRuntimeError::new_err(e.to_string()),),
      )?;
      return Ok(fut_return);
    }
  };
  let batch = std::sync::Arc::new(Batch {
    slots: (0..n).map(|_| Mutex::new(None)).collect(),
    remaining: std::sync::atomic::AtomicUsize::new(n),
    loop_: event_loop.unbind(),
    fut: future.unbind(),
  });
  for (i, query) in queries.into_iter().enumerate() {
    let batch = batch.clone();
    let searcher = searcher.clone();
    pool().submit(Box::new(move || {
      let result = searcher
        .search_rendered(&query, k, false)
        .map_err(|e| e.to_string());
      *batch.slots[i].lock().unwrap() = Some(result);
      // The AcqRel on `remaining` publishes every slot write to the last finisher.
      if batch.remaining.fetch_sub(1, Ordering::AcqRel) != 1 {
        return;
      }
      Python::attach(|py| {
        let Ok(resolver) = resolver(py) else {
          return;
        };
        // Gather in query order; the first error fails the whole batch.
        let mut values: Vec<String> = Vec::with_capacity(batch.slots.len());
        let mut error: Option<String> = None;
        for slot in &batch.slots {
          match slot.lock().unwrap().take() {
            Some(Ok(v)) => values.push(v),
            Some(Err(e)) => {
              error = Some(e);
              break;
            }
            None => error = Some("batch slot never filled".into()),
          }
        }
        let (payload, is_exc): (Py<PyAny>, bool) = match error {
          Some(msg) => (PyRuntimeError::new_err(msg).into_value(py).into_any(), true),
          None => match values.into_py_any(py) {
            Ok(list) => (list, false),
            Err(e) => (e.into_value(py).into_any(), true),
          },
        };
        let loop_ = batch.loop_.bind(py);
        let _ = loop_.call_method1(
          "call_soon_threadsafe",
          (resolver, batch.fut.bind(py), payload, is_exc),
        );
      });
    }));
  }
  Ok(fut_return)
}

/// `await vorpal.node(index_dir, id)` — returns the typed [`NodeInfo`].
#[pyfunction]
pub fn node(py: Python<'_>, index_dir: String, id: u64) -> PyResult<Py<PyAny>> {
  dispatch(py, move || {
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
  })
}

/// `await vorpal.graph(index_dir, verb, name, …)`.
#[pyfunction]
#[pyo3(signature = (index_dir, verb, name, path=None, kind=None, id=None, all=false, ids=false))]
#[allow(clippy::too_many_arguments)]
pub fn graph(
  py: Python<'_>,
  index_dir: String,
  verb: String,
  name: String,
  path: Option<String>,
  kind: Option<String>,
  id: Option<u64>,
  all: bool,
  ids: bool,
) -> PyResult<Py<PyAny>> {
  dispatch(py, move || {
    let target = vorpal_index::GraphTarget {
      name,
      id,
      external_id: None,
      path_suffix: path,
      kind,
      merge_all: all,
      show_ids: ids,
    };
    vorpal_index::graph_query_selected(std::path::Path::new(&index_dir), &verb, &target)
      .map_err(|e| e.to_string())
  })
}

/// A serializable payload carried through [`dispatch`]: resolved on the worker under
/// `Python::attach` by pythonizing straight into the Future — the async twin of
/// `repo::record_to_py`.
pub(crate) struct Pythonized<T: serde::Serialize>(pub(crate) T);

impl<'py, T: serde::Serialize> pyo3::IntoPyObject<'py> for Pythonized<T> {
  type Target = pyo3::PyAny;
  type Output = pyo3::Bound<'py, pyo3::PyAny>;
  type Error = PyErr;

  fn into_pyobject(self, py: Python<'py>) -> Result<Self::Output, Self::Error> {
    pythonize::pythonize(py, &self.0)
      .map_err(|err| PyRuntimeError::new_err(format!("serialize record: {err}")))
  }
}

/// `await vorpal.search_ranked(index_dir, query, k=10)` — one search, both orderings
/// (`{"fused": [...], "reranked": [...] | None, "encoderStatus": str | None}`).
#[pyfunction]
#[pyo3(signature = (index_dir, query, k=10))]
pub fn search_ranked(
  py: Python<'_>,
  index_dir: String,
  query: String,
  k: usize,
) -> PyResult<Py<PyAny>> {
  dispatch(py, move || {
    crate::repo::ranked_value(&index_dir, &query, k).map(Pythonized)
  })
}

/// `await vorpal.tune(index_dir, queries, k=10, apply=False)` — the `vorpal tune` core
/// as an awaitable; `queries` is a list of `(query, expected_substring_or_None)` pairs.
#[pyfunction]
#[pyo3(signature = (index_dir, queries, k=10, apply=false))]
pub fn tune(
  py: Python<'_>,
  index_dir: String,
  queries: Vec<(String, Option<String>)>,
  k: usize,
  apply: bool,
) -> PyResult<Py<PyAny>> {
  dispatch(py, move || {
    let queries: Vec<vorpal_index::tune::TuneQuery> = queries
      .into_iter()
      .map(|(query, expected)| vorpal_index::tune::TuneQuery { query, expected })
      .collect();
    vorpal_index::tune::tune_index(std::path::Path::new(&index_dir), &queries, k, apply)
      .map(Pythonized)
  })
}

/// `await vorpal.install(variant, root=None)` — download + install the encoder weights
/// (hundreds of megabytes) without blocking the loop; returns the model directory.
#[pyfunction]
#[pyo3(signature = (variant, root=None))]
pub fn install(py: Python<'_>, variant: String, root: Option<String>) -> PyResult<Py<PyAny>> {
  dispatch(py, move || crate::models::install_path(&variant, root))
}

/// `await vorpal.enable(variant, root=None)` — install AND enable globally; returns the
/// model directory.
#[pyfunction]
#[pyo3(signature = (variant, root=None))]
pub fn enable(py: Python<'_>, variant: String, root: Option<String>) -> PyResult<Py<PyAny>> {
  dispatch(py, move || crate::models::enable_path(&variant, root))
}
