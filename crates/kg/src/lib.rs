//! `vorpal-kg` — the L1→L3 bridge: turn extracted structure into a queryable knowledge graph.
//!
//! This crate wires the storage foundations together (§3.1→§3.3, §11):
//! - each entity is interned via [`vorpal_canonical`] → a dense `NodeId` (identity + dedup + skip),
//! - node attributes land in SoA columns sealed into a [`vorpal_segment`] `.vseg` (+ a string heap),
//! - containment relations are emitted as edges into a [`vorpal_graph`] graph.
//!
//! The input is [`vorpal_outline`] extraction (definitions/containment — the deterministic subset
//! available without cross-file resolution, i.e. the containment forest of §11.4). Calls/refs
//! edges arrive later behind the `Language`-trait resolver (§3.3); the assembly API is the same.
//!
//! [`KgWriter`] accumulates and [`KgWriter::seal`]s into a queryable [`Kg`].

mod dataflow;
mod evidence;
mod kg;
mod model;
mod writer;

pub use kg::{Kg, NodeView, SymbolSelector, resolve_index_dir};
pub use dataflow::{DataflowRow, DataflowStore, FlowView, save_dataflow};
pub use evidence::{EvidenceOutcome, EvidenceRow, EvidenceStore, NO_EDGE, save as save_evidence};
pub use model::SymbolKind;
pub use writer::{KgWriter, NodeDef, layout_entity_paths};

pub use vorpal_graph::{Direction, EdgeLog, EdgeType, ReachStep};
pub use vorpal_segment::NodeId;

/// Superlinearity telemetry (D7): log-spaced `(items, elapsed)` samples over one build phase,
/// fitted to `T ~ n^k` at the end. `k` meaningfully above 1 means per-item cost is GROWING
/// with progress — an accidental quadratic — and gets a WARN with the fitted exponent.
///
/// Documented blind spot (deliberately copied from the honest upstream framing): a per-item
/// cost proportional to TOTAL corpus size is flat *within* a run and invisible here; only
/// cross-corpus comparisons catch that class. Sampling is a single compare per tick on the
/// caller's thread — off the hot path by construction.
pub struct ScalingProbe {
  phase: &'static str,
  start: std::time::Instant,
  samples: Vec<(f64, f64)>,
  next_at: u64,
}

impl ScalingProbe {
  pub fn new(phase: &'static str) -> Self {
    Self {
      phase,
      start: std::time::Instant::now(),
      samples: Vec::with_capacity(32),
      next_at: 64,
    }
  }

  /// Record progress; samples are taken at geometrically spaced item counts (log-log spacing
  /// is exactly what the exponent fit wants).
  #[inline]
  pub fn tick(&mut self, done: u64) {
    if done < self.next_at {
      return;
    }
    let elapsed = self.start.elapsed().as_secs_f64();
    if elapsed > 0.0 {
      self.samples.push(((done as f64).ln(), elapsed.ln()));
    }
    self.next_at = (self.next_at + self.next_at / 2).max(done + 1);
  }

  /// Final sample + fit. WARNs when the fitted exponent crosses 1.35 on a phase big enough
  /// to mean it (≥4096 items, ≥500ms); always prints the exponent under `VORPAL_PHASE_TRACE`.
  pub fn finish(mut self, total: u64) {
    let elapsed = self.start.elapsed().as_secs_f64();
    if total > 0 && elapsed > 0.0 {
      self.samples.push(((total as f64).ln(), elapsed.ln()));
    }
    // Fit the [total/8, total] window only: the opening stretch of a phase is warm-up
    // (byte-budget fill, pool spin-up, cold caches) whose near-zero elapsed fakes a
    // super-quadratic curve. This is the classic 1/8-1/4-1/2-1 checkpoint window.
    let floor = ((total as f64) / 8.0).max(1.0).ln();
    let window: Vec<(f64, f64)> = self
      .samples
      .iter()
      .copied()
      .filter(|(x, _)| *x >= floor)
      .collect();
    let Some(k) = fit_exponent(&window) else {
      return;
    };
    if std::env::var_os("VORPAL_PHASE_TRACE").is_some() {
      eprintln!(
        "[scaling] phase={} k={k:.2} n={total} t={elapsed:.2}s ({} samples in window)",
        self.phase,
        window.len()
      );
    }
    if k >= 1.35 && window.len() >= 4 && elapsed >= 0.5 {
      eprintln!(
        "warning: scaling.superlinear phase={} k={k:.2} (n={total}, {elapsed:.1}s) — time is \
         growing faster than items within this run; a per-item cost proportional to corpus \
         size stays flat in-run and is NOT visible to this probe",
        self.phase
      );
    }
  }
}

/// Least-squares slope of `ln t` against `ln n`; `None` below 3 samples (no fit is honest,
/// a 2-point "fit" is noise).
fn fit_exponent(samples: &[(f64, f64)]) -> Option<f64> {
  if samples.len() < 3 {
    return None;
  }
  let n = samples.len() as f64;
  let mean_x = samples.iter().map(|(x, _)| x).sum::<f64>() / n;
  let mean_y = samples.iter().map(|(_, y)| y).sum::<f64>() / n;
  let mut num = 0.0;
  let mut den = 0.0;
  for (x, y) in samples {
    num += (x - mean_x) * (y - mean_y);
    den += (x - mean_x) * (x - mean_x);
  }
  (den > f64::EPSILON).then(|| num / den)
}

/// Phase stamp for RSS-timeline profiling, active only under `VORPAL_PHASE_TRACE`.
pub fn phase_stamp(label: &str) {
  if std::env::var_os("VORPAL_PHASE_TRACE").is_some() {
    #[cfg(feature = "alloc-stats")]
    let stats = {
      use tikv_jemalloc_ctl::{epoch, stats};
      epoch::advance().ok();
      format!(
        " [alloc={}MB active={}MB resident={}MB]",
        stats::allocated::read().unwrap_or(0) / 1048576,
        stats::active::read().unwrap_or(0) / 1048576,
        stats::resident::read().unwrap_or(0) / 1048576
      )
    };
    #[cfg(not(feature = "alloc-stats"))]
    let stats = "";
    eprintln!(
      "[phase {:.3}s] {label}{stats}",
      std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
    );
  }
}

#[cfg(test)]
mod scaling_tests {
  use super::fit_exponent;

  #[test]
  fn fit_recovers_known_exponents() {
    // t = n^k exactly → slope k.
    for k in [1.0_f64, 1.5, 2.0] {
      let samples: Vec<(f64, f64)> = (1..=6)
        .map(|i| {
          let n = (1000.0_f64) * (2.0_f64).powi(i);
          (n.ln(), (n.powf(k)).ln())
        })
        .collect();
      let fitted = fit_exponent(&samples).expect("enough samples");
      assert!((fitted - k).abs() < 1e-9, "k={k} fitted={fitted}");
    }
  }

  #[test]
  fn fit_refuses_underdetermined_input() {
    assert!(fit_exponent(&[]).is_none());
    assert!(fit_exponent(&[(1.0, 1.0), (2.0, 2.0)]).is_none());
    // Degenerate x-spread: no fit rather than a division blow-up.
    assert!(fit_exponent(&[(3.0, 1.0), (3.0, 2.0), (3.0, 3.0)]).is_none());
  }
}
