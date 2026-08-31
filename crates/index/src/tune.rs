//! Programmatic tune — the `vorpal tune` core, shared by the CLI and the SDK
//! bindings (one implementation, three surfaces): score the optional ranking
//! features over caller-supplied queries and, when asked, write this index's
//! switches from the verdicts.
//!
//! Measurement law (identical to the CLI): each comparison is paired from ONE
//! search — the reranker pair derives from a single fusion
//! ([`crate::Searcher::records_ranked`]), the BM25 pair from a single channel
//! pass ([`crate::Searcher::records_bm25_pair`], the warm gate's trick with the
//! active encoder reranking both sides). Scoring is reciprocal rank of the first
//! hit whose name or path contains the expectation (case-insensitive substring).
//! Verdict rule: ON iff the mean strictly improves AND wins are not outnumbered;
//! equal means with balanced wins = NO SIGNAL, and no signal never writes.
//!
//! Writes (`apply`): the reranker pins per-index via `encoder.dir` (the serving
//! model dir) or opts out via the `off` sentinel that shadows a global enable;
//! the BM25 verdict lands as a manual record override through the canonical
//! writer — it holds until the index content changes and retrains (callers
//! surface that).

use std::path::Path;

use serde::Serialize;

use crate::records::SearchHitRecord;
use crate::{SearchFilter, models, open_searcher};

/// One tune query: the search text and, optionally, a case-insensitive substring
/// of the hit the caller expected (matched against name or path).
pub struct TuneQuery {
  pub query: String,
  pub expected: Option<String>,
}

/// Paired per-feature tally over the labelled queries.
#[derive(Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct FeatureTally {
  pub queries: usize,
  pub mean_off: f64,
  pub mean_on: f64,
  pub wins: usize,
  pub losses: usize,
  /// `Some(true)` = ON improves, `Some(false)` = OFF (the feature regresses),
  /// `None` = no signal (switch left alone).
  pub verdict: Option<bool>,
}

impl FeatureTally {
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
    self.verdict = if self.queries == 0 || (self.mean_on == self.mean_off && self.wins == self.losses)
    {
      None
    } else {
      Some(self.mean_on > self.mean_off && self.wins >= self.losses)
    };
  }

  fn evidence(&self, feature: &str) -> String {
    format!(
      "manual: vorpal tune ({feature}: {} queries, mean RR {:.3}→{:.3}, {}W/{}L)",
      self.queries, self.mean_off, self.mean_on, self.wins, self.losses
    )
  }
}

/// What one tune run measured and (under `apply`) wrote.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TuneReport {
  /// Queries that carried an expectation (only these score).
  pub labelled: usize,
  /// Whether an encoder served the run (absent → the reranker went untested).
  pub encoder_present: bool,
  pub reranker: FeatureTally,
  pub bm25: FeatureTally,
  /// What `apply` wrote for the reranker: the pinned model dir, the literal
  /// `"off"` (per-index opt-out), or `None` (no signal, untested, or not applied).
  pub wrote_encoder: Option<String>,
  /// The BM25 override written, if any.
  pub wrote_bm25: Option<bool>,
}

/// Reciprocal rank of the first hit whose name or path contains `expected`
/// (already lowercased).
fn reciprocal_rank(hits: &[SearchHitRecord], expected: &str) -> f64 {
  hits
    .iter()
    .position(|hit| {
      hit.node.name.to_lowercase().contains(expected)
        || hit.node.path.to_lowercase().contains(expected)
    })
    .map_or(0.0, |position| 1.0 / (position as f64 + 1.0))
}

/// Run the tune measurements over `queries` (unlabelled entries are skipped —
/// they carry no score) and, when `apply` is set and a verdict exists, write the
/// index's switches. Returns the full report either way.
pub fn tune_index(
  index_root: &Path,
  queries: &[TuneQuery],
  k: usize,
  apply: bool,
) -> Result<TuneReport, String> {
  let searcher = open_searcher(index_root).map_err(|e| e.to_string())?;
  let filter = SearchFilter::default();
  let k = k.max(1);
  let mut reranker = FeatureTally::default();
  let mut bm25 = FeatureTally::default();
  let mut encoder_present = false;
  let mut labelled = 0usize;
  for entry in queries {
    let Some(expected) = &entry.expected else {
      continue;
    };
    let expected = expected.to_lowercase();
    labelled += 1;
    let (base, reranked) = searcher
      .records_ranked(&entry.query, k, &filter)
      .map_err(|e| format!("query {:?}: {e}", entry.query))?;
    let (bm25_off, bm25_on) = searcher
      .records_bm25_pair(&entry.query, k, &filter)
      .map_err(|e| format!("query {:?}: {e}", entry.query))?;
    if let Some(reranked) = &reranked {
      encoder_present = true;
      reranker.measure(reciprocal_rank(&base, &expected), reciprocal_rank(reranked, &expected));
    }
    bm25.measure(
      reciprocal_rank(&bm25_off, &expected),
      reciprocal_rank(&bm25_on, &expected),
    );
  }
  reranker.finish();
  bm25.finish();

  let mut wrote_encoder = None;
  let mut wrote_bm25 = None;
  if apply && labelled > 0 {
    if encoder_present {
      match reranker.verdict {
        Some(true) => {
          // Pin the model dir actually serving this handle: the per-index file
          // if it names one, else the global enable.
          let pinned = std::fs::read_to_string(index_root.join("encoder.dir"))
            .ok()
            .map(|text| text.trim().to_string())
            .filter(|text| !text.is_empty() && text != "off")
            .or_else(|| {
              let path = models::global_selection_path().ok()?;
              std::fs::read_to_string(path)
                .ok()
                .map(|text| text.trim().to_string())
            });
          if let Some(model_dir) = pinned {
            crate::write_encoder_selection(index_root, Path::new(&model_dir))
              .map_err(|e| format!("writing encoder.dir: {e}"))?;
            wrote_encoder = Some(model_dir);
          }
        }
        Some(false) => {
          crate::write_encoder_opt_out(index_root)
            .map_err(|e| format!("writing encoder.dir: {e}"))?;
          wrote_encoder = Some("off".to_string());
        }
        None => {}
      }
    }
    if let Some(enabled) = bm25.verdict {
      crate::set_bm25_override(index_root, enabled, &bm25.evidence("bm25"))?;
      wrote_bm25 = Some(enabled);
    }
  }
  Ok(TuneReport {
    labelled,
    encoder_present,
    reranker,
    bm25,
    wrote_encoder,
    wrote_bm25,
  })
}
