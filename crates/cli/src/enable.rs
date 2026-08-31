//! `vorpal enable <option>` / `vorpal disable <option>` — install/enable and
//! disable the advanced semantic embedder (semantic-tier Stage 6). Enable is the
//! ONLY path that downloads model weights, always explicit, always
//! checksum-pinned; disable removes the global enable for exactly the named
//! variant (weights stay installed, per-index `encoderDir` selections are
//! untouched and always win).

use std::process::ExitCode;

use anyhow::Result;
use clap::Parser;

use vorpal_index::models::{DisableOutcome, ModelVariant};

#[derive(Parser)]
pub struct EnableArg {
  /// What to enable: `semantic-f32` (pinned upstream weights, 547 MB) or
  /// `semantic-f16` (locally converted, ~274 MB on disk).
  pub option: String,
}

#[derive(Parser)]
pub struct DisableArg {
  /// What to disable: `semantic-f32` or `semantic-f16` — the variant currently
  /// enabled globally.
  pub option: String,
}

fn parse_variant(option: &str) -> Result<ModelVariant> {
  ModelVariant::parse(option).ok_or_else(|| {
    anyhow::anyhow!("unknown option '{option}': expected semantic-f32 or semantic-f16")
  })
}

pub fn run_enable(arg: EnableArg) -> Result<ExitCode> {
  let variant = parse_variant(&arg.option)?;
  let mut progress = |line: &str| println!("{line}");
  let model_dir = vorpal_index::models::install(variant, None, &mut progress)
    .map_err(|e| anyhow::anyhow!("install failed: {e}"))?;
  let selection = vorpal_index::models::enable_global(&model_dir)
    .map_err(|e| anyhow::anyhow!("enabling: {e}"))?;
  println!(
    "advanced embedder ENABLED globally\n  weights: {}\n  enable file: {}\n\
     per-index `encoderDir` in vorpalconfig.yml overrides this; `vorpal disable {}` reverts.",
    model_dir.display(),
    selection.display(),
    arg.option,
  );
  Ok(ExitCode::SUCCESS)
}

pub fn run_disable(arg: DisableArg) -> Result<ExitCode> {
  let variant = parse_variant(&arg.option)?;
  match vorpal_index::models::disable_variant(variant)
    .map_err(|e| anyhow::anyhow!("disabling: {e}"))?
  {
    DisableOutcome::Disabled(model_dir) => {
      println!(
        "advanced embedder DISABLED globally\n  weights kept: {}\n\
         re-enable with `vorpal enable {}`; per-index `encoderDir` selections are untouched.",
        model_dir.display(),
        arg.option,
      );
      Ok(ExitCode::SUCCESS)
    }
    DisableOutcome::NotEnabled => {
      println!("nothing is enabled globally");
      Ok(ExitCode::SUCCESS)
    }
    DisableOutcome::EnabledElsewhere(current) => {
      anyhow::bail!(
        "the global enable points at {}, not {} — nothing disabled \
         (disable the matching variant instead)",
        current.display(),
        arg.option,
      );
    }
  }
}
