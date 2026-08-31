//! Optional-model install/enable for the Python SDK (semantic-tier Stage 6
//! packaging): the same explicit, checksum-pinned path as `vorpal enable` —
//! weights are NEVER fetched implicitly and never ship inside the wheel.
//!
//! * `semantic_install(variant, root=None)` — download + verify (or reuse a
//!   verified install) and return the model directory; `root` adjusts placement
//!   (default `$VORPAL_MODELS_DIR` or `~/.vorpal/models`).
//! * `semantic_enable(variant, root=None)` — install, then write the GLOBAL
//!   enable file every search consults when an index root has no selection of
//!   its own (per-index `encoderDir` always wins). Returns the model directory.
//! * `semantic_disable()` — remove the global enable (per-index selections are
//!   untouched); returns whether anything was enabled.
//!
//! `variant` is the CLI's grammar verbatim: `"semantic-f32"` or `"semantic-f16"`.
//! Progress lines go to stderr.

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use std::path::PathBuf;

use vorpal_index::models::{ModelVariant, disable_global, enable_global, install};

fn parse_variant(variant: &str) -> PyResult<ModelVariant> {
  ModelVariant::parse(variant).ok_or_else(|| {
    PyRuntimeError::new_err(format!(
      "unknown variant '{variant}': expected semantic-f32 or semantic-f16"
    ))
  })
}

fn install_impl(variant: &str, root: Option<String>) -> PyResult<PathBuf> {
  let variant = parse_variant(variant)?;
  let root = root.map(PathBuf::from);
  let mut progress = |line: &str| eprintln!("{line}");
  install(variant, root.as_deref(), &mut progress).map_err(PyRuntimeError::new_err)
}

/// Download (or reuse) the pinned weights; returns the installed model directory.
#[pyfunction]
#[pyo3(signature = (variant, root=None))]
pub fn semantic_install(variant: &str, root: Option<String>) -> PyResult<String> {
  Ok(install_impl(variant, root)?.display().to_string())
}

/// Install AND enable globally; returns the installed model directory.
#[pyfunction]
#[pyo3(signature = (variant, root=None))]
pub fn semantic_enable(variant: &str, root: Option<String>) -> PyResult<String> {
  let dir = install_impl(variant, root)?;
  enable_global(&dir).map_err(PyRuntimeError::new_err)?;
  Ok(dir.display().to_string())
}

/// Remove the global enable; returns whether anything was enabled.
#[pyfunction]
pub fn semantic_disable() -> PyResult<bool> {
  disable_global().map_err(PyRuntimeError::new_err)
}
