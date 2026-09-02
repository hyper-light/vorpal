//! `vorpal tune` — measure the optional ranking features on YOUR queries and
//! write this index's switches from the verdicts (semantic-tier per-corpus law:
//! each feature helps some codebases and hurts others, so the index's own
//! measurements decide).
//!
//! Queries file: one query per line; blank lines and `#` comments skipped. A
//! line may carry an expectation — `query => expected` — where `expected` is a
//! case-insensitive substring of the hit you wanted (its name or its path). With
//! expectations, tune scores each feature by reciprocal rank, prints the verdict
//! table, and WRITES the per-index switches (`--dry-run` reports without
//! writing). Without expectations, tune prints per-query before/after summaries
//! for eyeballing and writes nothing.
//!
//! The measurement/verdict/write core is [`vorpal_index::tune::tune_index`] —
//! ONE implementation shared with the SDK bindings; this file is parsing and
//! rendering.

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::Parser;

use crate::kg::{index_dir, missing_index_hint};
use vorpal_index::records::SearchHitRecord;
use vorpal_index::tune::{FeatureTally, TuneQuery, tune_index};

#[derive(Parser)]
pub struct TuneArg {
  /// Queries file: one per line, optionally `query => expected-substring`.
  #[clap(long, value_name = "FILE")]
  queries: PathBuf,
  /// Hits examined per query.
  #[clap(short, default_value_t = 10)]
  k: usize,
  /// Report the verdicts without writing any switch.
  #[clap(long)]
  dry_run: bool,
  /// Index directory (default: `./.vorpal/index`).
  #[clap(long)]
  index: Option<PathBuf>,
}

fn parse_queries(text: &str) -> Vec<TuneQuery> {
  text
    .lines()
    .map(str::trim)
    .filter(|line| !line.is_empty() && !line.starts_with('#'))
    .map(|line| match line.split_once("=>") {
      Some((query, expected)) => TuneQuery {
        query: query.trim().to_string(),
        expected: Some(expected.trim().to_string()),
      },
      None => TuneQuery {
        query: line.to_string(),
        expected: None,
      },
    })
    .collect()
}

fn top_line(hits: &[SearchHitRecord]) -> String {
  hits
    .first()
    .map(|hit| hit.node.name.clone())
    .unwrap_or_else(|| "(none)".to_string())
}

pub fn run_tune(arg: TuneArg) -> Result<ExitCode> {
  let dir = index_dir(arg.index);
  let text = std::fs::read_to_string(&arg.queries)
    .with_context(|| format!("reading {}", arg.queries.display()))?;
  let queries = parse_queries(&text);
  if queries.is_empty() {
    anyhow::bail!("{} holds no queries", arg.queries.display());
  }
  let k = arg.k.max(1);
  // Eyeball summaries for the unlabelled lines (presentation only — the scored
  // path lives in the shared core).
  let searcher = vorpal_index::open_searcher(&dir)
    .map_err(|e| anyhow::anyhow!("{e}"))
    .with_context(|| missing_index_hint(&dir))?;
  let filter = vorpal_index::SearchFilter::default();
  for entry in queries.iter().filter(|entry| entry.expected.is_none()) {
    let (base, reranked) = searcher
      .records_ranked(&entry.query, k, &filter)
      .map_err(|e| anyhow::anyhow!("query {:?}: {e}", entry.query))?;
    let (bm25_off, bm25_on) = searcher
      .records_bm25_pair(&entry.query, k, &filter)
      .map_err(|e| anyhow::anyhow!("query {:?}: {e}", entry.query))?;
    let line = format!(
      "{:?}: top fused = {}; reranked = {}; bm25 flips top = {}",
      entry.query,
      top_line(&base),
      reranked
        .as_deref()
        .map(top_line)
        .unwrap_or_else(|| "(no encoder)".to_string()),
      if top_line(&bm25_off) == top_line(&bm25_on) { "no" } else { "yes" },
    );
    println!("{line}");
  }

  let report = tune_index(&dir, &queries, k, !arg.dry_run)
    .map_err(|e| anyhow::anyhow!("{e}"))
    .with_context(|| missing_index_hint(&dir))?;
  if report.labelled == 0 {
    println!(
      "\n(no `query => expected` lines — eyeball summaries only; add expectations to \
       get verdicts and switch writes)"
    );
    return Ok(ExitCode::SUCCESS);
  }

  println!(
    "\nverdicts over {} labelled queries (reciprocal rank of the expected hit, top {k}):",
    report.labelled
  );
  let row = |name: &str, tally: &FeatureTally, note: &str| {
    let verdict = match tally.verdict {
      Some(true) => "ON (improves)",
      Some(false) => "OFF (regresses)",
      None => "no signal — unchanged",
    };
    let line = format!(
      "  {name:<18} mean RR {:.3} → {:.3}   {}W/{}L   → {verdict}{note}",
      tally.mean_off, tally.mean_on, tally.wins, tally.losses
    );
    println!("{line}");
  };
  if report.encoder_present {
    row("encoder reranker", &report.reranker, "");
  } else {
    let hint = match searcher.encoder_status() {
      Some(status) => format!(" ({status})"),
      None => " (no encoder enabled — `vorpal enable`)".to_string(),
    };
    println!("  encoder reranker   untested{hint}");
  }
  row("bm25 channel", &report.bm25, " (override holds until the index retrains)");
  if report.dense_present {
    row("dense channel", &report.dense, " (per-index override: dense.channel)");
  } else {
    println!("  dense channel      untested (no fresh dense sidecar for this index/encoder — warm with a dense budget)");
  }

  if arg.dry_run {
    println!("\n--dry-run: no switches written");
    return Ok(ExitCode::SUCCESS);
  }
  match &report.wrote_encoder {
    Some(sentinel) if sentinel == "off" => println!(
      "wrote {} = off → reranker OPTED OUT for this index (shadows any global enable; \
       delete the file to revert)",
      dir.join("encoder.dir").display()
    ),
    Some(_) => println!(
      "wrote {} → reranker PINNED ON for this index",
      dir.join("encoder.dir").display()
    ),
    None if report.encoder_present => println!("reranker: no signal — switch unchanged"),
    None => {}
  }
  match report.wrote_bm25 {
    Some(enabled) => println!(
      "bm25 channel override written: {} (holds until the index content retrains — \
       re-run tune after big changes)",
      if enabled { "ON" } else { "OFF" }
    ),
    None => println!("bm25: no signal — verdict unchanged"),
  }
  match report.wrote_dense {
    Some(enabled) => println!(
      "wrote {} = {} → dense channel {} for this index (shadows the warm gate's verdict; \
       delete the file to revert)",
      dir.join("dense.channel").display(),
      if enabled { "on" } else { "off" },
      if enabled { "PINNED ON" } else { "OPTED OUT" }
    ),
    None if report.dense_present => println!("dense channel: no signal — verdict unchanged"),
    None => {}
  }
  Ok(ExitCode::SUCCESS)
}
