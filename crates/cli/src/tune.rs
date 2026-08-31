//! `vorpal tune` — measure the optional ranking features on YOUR queries and
//! write this index's switches from the verdicts (semantic-tier per-corpus law:
//! each feature helps some codebases and hurts others, so the index's own
//! measurements decide).
//!
//! Queries file: one query per line; blank lines and `#` comments skipped. A
//! line may carry an expectation — `query => expected` — where `expected` is a
//! case-insensitive substring of the hit you wanted (its name or its path). With
//! expectations, tune scores each feature by reciprocal rank over your queries,
//! prints the verdict table, and WRITES the per-index switches (`--dry-run`
//! reports without writing): the encoder reranker via `encoder.dir` (a model
//! path to pin it on, the `off` sentinel to shadow a global enable) and the
//! BM25 channel via a manual record override (which holds until the index's
//! content changes and retrains — re-run tune after big changes). Without
//! expectations, tune prints per-query before/after summaries for eyeballing
//! and writes nothing.
//!
//! Both comparisons come from ONE search per query per feature — never serial
//! duplicate runs: the reranked ordering derives from the same fusion
//! (`records_ranked`), and the BM25 pair from the same channel pass
//! (`records_bm25_pair`).

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::Parser;

use crate::kg::{index_dir, missing_index_hint};
use vorpal_index::records::SearchHitRecord;

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

struct TuneQuery {
  query: String,
  expected: Option<String>,
}

fn parse_queries(text: &str) -> Vec<TuneQuery> {
  text
    .lines()
    .map(str::trim)
    .filter(|line| !line.is_empty() && !line.starts_with('#'))
    .map(|line| match line.split_once("=>") {
      Some((query, expected)) => TuneQuery {
        query: query.trim().to_string(),
        expected: Some(expected.trim().to_lowercase()),
      },
      None => TuneQuery {
        query: line.to_string(),
        expected: None,
      },
    })
    .collect()
}

/// Reciprocal rank of the first hit whose name or path contains `expected`.
fn reciprocal_rank(hits: &[SearchHitRecord], expected: &str) -> f64 {
  hits
    .iter()
    .position(|hit| {
      hit.node.name.to_lowercase().contains(expected)
        || hit.node.path.to_lowercase().contains(expected)
    })
    .map_or(0.0, |position| 1.0 / (position as f64 + 1.0))
}

/// Paired per-feature tally over the expectation queries.
#[derive(Default)]
struct Tally {
  mean_off: f64,
  mean_on: f64,
  wins: usize,
  losses: usize,
  queries: usize,
}

impl Tally {
  fn measure(&mut self, off: f64, on: f64) {
    self.mean_off += off;
    self.mean_on += on;
    if on > off {
      self.wins += 1;
    } else if on < off {
      self.losses += 1;
    }
    self.queries += 1;
  }

  fn finish(&mut self) {
    if self.queries > 0 {
      self.mean_off /= self.queries as f64;
      self.mean_on /= self.queries as f64;
    }
  }

  /// The verdict rule, printed with the table: ON iff the mean strictly improves
  /// and wins are not outnumbered.
  fn verdict_on(&self) -> Option<bool> {
    if self.queries == 0 || (self.mean_on == self.mean_off && self.wins == self.losses) {
      return None; // no signal — leave the switch alone
    }
    Some(self.mean_on > self.mean_off && self.wins >= self.losses)
  }

  fn evidence(&self, feature: &str) -> String {
    format!(
      "manual: vorpal tune ({feature}: {} queries, mean RR {:.3}→{:.3}, {}W/{}L)",
      self.queries, self.mean_off, self.mean_on, self.wins, self.losses
    )
  }
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
  let searcher = vorpal_index::open_searcher(&dir)
    .map_err(|e| anyhow::anyhow!("{e}"))
    .with_context(|| missing_index_hint(&dir))?;
  let filter = vorpal_index::SearchFilter::default();
  let k = arg.k.max(1);
  let encoder_active = searcher.encoder_status().is_none();
  // encoder_status None = active OR unconfigured; distinguish via a probe rerank.
  let labelled = queries.iter().filter(|q| q.expected.is_some()).count();

  let mut rerank_tally = Tally::default();
  let mut bm25_tally = Tally::default();
  let mut encoder_present = false;

  for entry in &queries {
    let (base, reranked) = searcher
      .records_ranked(&entry.query, k, &filter)
      .map_err(|e| anyhow::anyhow!("query {:?}: {e}", entry.query))?;
    let (bm25_off, bm25_on) = searcher
      .records_bm25_pair(&entry.query, k, &filter)
      .map_err(|e| anyhow::anyhow!("query {:?}: {e}", entry.query))?;
    encoder_present |= reranked.is_some();
    match &entry.expected {
      Some(expected) => {
        if let Some(reranked) = &reranked {
          rerank_tally.measure(reciprocal_rank(&base, expected), reciprocal_rank(reranked, expected));
        }
        bm25_tally.measure(
          reciprocal_rank(&bm25_off, expected),
          reciprocal_rank(&bm25_on, expected),
        );
      }
      None => {
        let line = format!(
          "{:?}: top fused = {}; reranked = {}; bm25 flips top = {}",
          entry.query,
          top_line(&base),
          reranked.as_deref().map(top_line).unwrap_or_else(|| "(no encoder)".to_string()),
          if top_line(&bm25_off) == top_line(&bm25_on) { "no" } else { "yes" },
        );
        println!("{line}");
      }
    }
  }

  if labelled == 0 {
    println!(
      "\n(no `query => expected` lines — eyeball summaries only; add expectations to \
       get verdicts and switch writes)"
    );
    return Ok(ExitCode::SUCCESS);
  }
  rerank_tally.finish();
  bm25_tally.finish();

  println!("\nverdicts over {labelled} labelled queries (reciprocal rank of the expected hit, top {k}):");
  let row = |name: &str, tally: &Tally, note: &str| {
    let verdict = match tally.verdict_on() {
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
  if encoder_present {
    row("encoder reranker", &rerank_tally, "");
  } else {
    let hint = if encoder_active { " (no encoder enabled — `vorpal enable`)" } else { "" };
    println!("  encoder reranker   untested{hint}");
  }
  row("bm25 channel", &bm25_tally, " (override holds until the index retrains)");

  if arg.dry_run {
    println!("\n--dry-run: no switches written");
    return Ok(ExitCode::SUCCESS);
  }
  // Write the switches the verdicts license.
  if encoder_present {
    match rerank_tally.verdict_on() {
      Some(true) => {
        // Pin the reranker per-index with the model dir actually serving this
        // handle: the per-index file if it named one, else the global enable.
        let pinned = std::fs::read_to_string(dir.join("encoder.dir"))
          .ok()
          .map(|text| text.trim().to_string())
          .filter(|text| !text.is_empty() && text != "off")
          .or_else(|| {
            let path = vorpal_index::models::global_selection_path().ok()?;
            std::fs::read_to_string(path).ok().map(|text| text.trim().to_string())
          });
        match pinned {
          Some(model_dir) => {
            vorpal_index::write_encoder_selection(&dir, std::path::Path::new(&model_dir))
              .with_context(|| "writing encoder.dir")?;
            println!("wrote {} → reranker PINNED ON for this index", dir.join("encoder.dir").display());
          }
          None => println!("(reranker won but no enable to pin — unexpected; nothing written)"),
        }
      }
      Some(false) => {
        vorpal_index::write_encoder_opt_out(&dir).with_context(|| "writing encoder.dir")?;
        println!(
          "wrote {} = off → reranker OPTED OUT for this index (shadows any global enable; \
           delete the file to revert)",
          dir.join("encoder.dir").display()
        );
      }
      None => println!("reranker: no signal — switch unchanged"),
    }
  }
  match bm25_tally.verdict_on() {
    Some(enabled) => {
      vorpal_index::set_bm25_override(&dir, enabled, &bm25_tally.evidence("bm25"))
        .map_err(|e| anyhow::anyhow!("bm25 override: {e}"))?;
      println!(
        "bm25 channel override written: {} (holds until the index content retrains — \
         re-run tune after big changes)",
        if enabled { "ON" } else { "OFF" }
      );
    }
    None => println!("bm25: no signal — verdict unchanged"),
  }
  Ok(ExitCode::SUCCESS)
}
