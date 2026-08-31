//! Optional-model install/enable for the Node SDK (semantic-tier Stage 6
//! packaging): the same explicit, checksum-pinned path as `vorpal enable` —
//! weights are NEVER fetched implicitly and never ship inside the package.
//!
//! * `semanticInstall(variant, root?)` — download + verify (or reuse a verified
//!   install) and return the model directory; `root` adjusts placement (default
//!   `$VORPAL_MODELS_DIR` or `~/.vorpal/models`).
//! * `semanticEnable(variant, root?)` — install, then write the GLOBAL enable
//!   file every search consults when an index root has no selection of its own
//!   (per-index `encoderDir` always wins). Returns the model directory.
//! * `semanticDisable()` — remove the global enable (per-index selections are
//!   untouched); returns whether anything was enabled.
//!
//! `variant` is the CLI's grammar verbatim: `"semantic-f32"` or `"semantic-f16"`.
//! Runs on the calling thread (the download blocks) — wrap in a worker if the
//! event loop must stay free. Progress lines go to stderr.

use std::path::PathBuf;

use napi_derive::napi;

use vorpal_index::models::{ModelVariant, disable_global, enable_global, install};

fn parse_variant(variant: &str) -> napi::Result<ModelVariant> {
  ModelVariant::parse(variant).ok_or_else(|| {
    napi::Error::from_reason(format!(
      "unknown variant '{variant}': expected semantic-f32 or semantic-f16"
    ))
  })
}

fn install_impl(variant: &str, root: Option<String>) -> napi::Result<PathBuf> {
  let variant = parse_variant(variant)?;
  let root = root.map(PathBuf::from);
  let mut progress = |line: &str| eprintln!("{line}");
  install(variant, root.as_deref(), &mut progress).map_err(napi::Error::from_reason)
}

/// Download (or reuse) the pinned weights; returns the installed model directory.
#[napi]
pub fn semantic_install(variant: String, root: Option<String>) -> napi::Result<String> {
  Ok(install_impl(&variant, root)?.display().to_string())
}

/// Install AND enable globally; returns the installed model directory.
#[napi]
pub fn semantic_enable(variant: String, root: Option<String>) -> napi::Result<String> {
  let dir = install_impl(&variant, root)?;
  enable_global(&dir).map_err(napi::Error::from_reason)?;
  Ok(dir.display().to_string())
}

/// Remove the global enable; returns whether anything was enabled.
#[napi]
pub fn semantic_disable() -> napi::Result<bool> {
  disable_global().map_err(napi::Error::from_reason)
}
