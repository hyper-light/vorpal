//! `vorpal enable <option>` — install and globally enable the advanced semantic
//! embedder (semantic-tier Stage 6). The ONLY path that downloads model weights,
//! always explicit, always checksum-pinned; `vorpal enable off` removes the global
//! enable (per-index `encoderDir` selections are untouched and always win).

use std::process::ExitCode;

use anyhow::Result;
use clap::Parser;

#[derive(Parser)]
pub struct EnableArg {
  /// What to enable: `semantic-f32` (pinned upstream weights, 547 MB),
  /// `semantic-f16` (locally converted, ~274 MB on disk), or `off` (remove the
  /// global enable).
  pub option: String,
}

pub fn run_enable(arg: EnableArg) -> Result<ExitCode> {
  if arg.option == "off" {
    let removed = vorpal_index::models::disable_global()
      .map_err(|e| anyhow::anyhow!("disabling: {e}"))?;
    println!(
      "{}",
      if removed {
        "advanced embedder disabled globally (per-index encoderDir selections untouched)"
      } else {
        "nothing was enabled globally"
      }
    );
    return Ok(ExitCode::SUCCESS);
  }
  let Some(variant) = vorpal_index::models::ModelVariant::parse(&arg.option) else {
    anyhow::bail!(
      "unknown option '{}': expected semantic-f32, semantic-f16, or off",
      arg.option
    );
  };
  let mut progress = |line: &str| println!("{line}");
  let model_dir = vorpal_index::models::install(variant, None, &mut progress)
    .map_err(|e| anyhow::anyhow!("install failed: {e}"))?;
  let selection = vorpal_index::models::enable_global(&model_dir)
    .map_err(|e| anyhow::anyhow!("enabling: {e}"))?;
  println!(
    "advanced embedder ENABLED globally\n  weights: {}\n  enable file: {}\n\
     per-index `encoderDir` in vorpalconfig.yml overrides this; `vorpal enable off` reverts.",
    model_dir.display(),
    selection.display()
  );
  Ok(ExitCode::SUCCESS)
}
