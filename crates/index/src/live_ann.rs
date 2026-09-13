//! The daemon's live vector tier (ANN_FRONTIER.md Tier 3, T3b): an [`vorpal_ann::AnnOverlay`]
//! keyed by DURABLE node identity (eid low halves), so it survives the dense-id renumbering
//! every generation performs. Per edit, the daemon deletes the changed files' old eids and
//! inserts their new rows (~18.5 ms CPU for a 100+100 edit at kernel scale) instead of
//! paying the ~330 CPU-second tier rebuild; the full rebuild demotes to a compactor run
//! behind a tombstone-debt trigger.
//!
//! Serving stays exact where it always was: the tier only proposes the semantic CANDIDATE
//! pool (translated eid → current-generation node id); `Searcher::run_with_semantic_pool`
//! then applies the same filters, full-precision re-embedding rerank, and fusion as every
//! other tier. A stale or missing translation drops the candidate — never a wrong answer,
//! only a thinner pool.

use std::collections::HashMap;
use std::path::Path;

use vorpal_ann::AnnOverlay;
use vorpal_kg::{Kg, NodeId};

use crate::{
  AnnIndex, active_embedder, annfiles, embed_node_into, persisted_model_provenance,
};

/// Truncated durable id: the eid's low 64 bits. Collisions are ~n²/2⁶⁵ (≈1e-7 at kernel
/// scale) and cost at worst one symbol's vector until the next compaction — never a wrong
/// search answer (the rerank re-embeds against the current graph).
fn eid_lo_of(kg: &Kg, id: u64) -> Option<u64> {
  let (external_id, _) = kg.node_identity(NodeId::new(id))?;
  external_id.map(|eid| eid as u64)
}

/// Probe cadence: re-measure after this fraction of live rows has churned since the last
/// measurement — 1/100 gives five probe points across the 5% tombstone-debt compaction
/// window (monitoring resolution tied to the existing trigger, not a tuned constant).
const PROBE_CHURN_DENOMINATOR: usize = 100;
/// Degradation bar under the self-anchored baseline: ~3× the probe's own quantization
/// step (32 probes × k=10 → 1/320 per oracle entry), so a trip is beyond probe-set drift.
/// Recall at or below `baseline − PROBE_DEGRADATION` retires the tier to the compactor.
const PROBE_DEGRADATION: f64 = 0.01;

/// A run of base rows `[base_lo, base_hi)` (base-generation ids, gaps allowed) whose
/// current ids are `base_id + shift`, with `shift = cur_lo - base_lo`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct IdRun {
  base_lo: u64,
  base_hi: u64,
  cur_lo: u64,
}

impl IdRun {
  fn shift(&self) -> i128 {
    i128::from(self.cur_lo) - i128::from(self.base_lo)
  }
  fn cur_hi(&self) -> u64 {
    self.cur_lo + (self.base_hi - self.base_lo)
  }
}

/// Fold `(base id, current id or dead)` pairs in base-row order into runs.
fn build_runs(pairs: impl Iterator<Item = (u64, Option<u64>)>) -> Vec<IdRun> {
  let mut runs: Vec<IdRun> = Vec::new();
  let mut open: Option<IdRun> = None;
  for (base, cur) in pairs {
    match (cur, open.as_mut()) {
      (Some(cur), Some(run)) if i128::from(cur) - i128::from(base) == run.shift() && base >= run.base_hi => {
        run.base_hi = base + 1;
      }
      (Some(cur), _) => {
        if let Some(run) = open.take() {
          runs.push(run);
        }
        open = Some(IdRun {
          base_lo: base,
          base_hi: base + 1,
          cur_lo: cur,
        });
      }
      (None, _) => {
        if let Some(run) = open.take() {
          runs.push(run);
        }
      }
    }
  }
  if let Some(run) = open {
    runs.push(run);
  }
  runs
}

fn runs_by_cur(runs: &[IdRun]) -> Vec<u32> {
  let mut order: Vec<u32> = (0..runs.len() as u32).collect();
  order.sort_by_key(|&i| runs[i as usize].cur_lo);
  order
}

pub struct LiveAnnTier {
  overlay: AnnOverlay,
  /// How the base tier's own ids map to the served generation's ids: runs of base rows
  /// whose current id is the base id plus a constant. The remap is per file block and
  /// keeps order, so an unchanged tree is one run and every edited file starts a new one
  /// — a few dozen entries where a per-row column would be 8 bytes × every row. Rebuilt
  /// from the stable ids whenever ids are refreshed.
  runs: Vec<IdRun>,
  /// `runs` indices ordered by current id (runs cover disjoint current ranges).
  runs_by_cur: Vec<u32>,
  /// eid_lo → node id in the generation `refresh_ids` last saw — the pool translation.
  eid_to_id: HashMap<u64, u64>,
  /// First probe of this adopted tier — the self-anchored recall reference (same probe
  /// machinery, same quantized domain, so later probes are directly comparable).
  baseline_recall: Option<f64>,
  /// The PINNED probe rows: recall is re-measured on the same rows across the tier's
  /// life (dead ones replaced deterministically), because 32 correlated probes carry
  /// ±points of set-to-set sampling spread — redrawing per probe would swamp the bar.
  probe_rows: Vec<u32>,
  /// Rows churned (tombstoned + inserted) since the last probe.
  rows_since_probe: usize,
  /// Latched by a probe at or below the degradation bar; the daemon's compaction trigger
  /// reads it through [`LiveAnnTier::needs_compaction`].
  degraded: bool,
}

/// Whether a corpus of `node_count` nodes can EVER have an adoptable live tier: the
/// builder quantizes only above the [`vorpal_ann::AnnConfig::for_n`] floor, and it embeds
/// a strict subset of nodes (`semantic_row_ids` filters Imports), so
/// `node_count <= floor  ⇒  rows <= floor  ⇒  the committed tier is flat  ⇒  adoption
/// declines`. The daemon consults this BEFORE spawning any adoption work — the common
/// small-repo case costs one integer compare instead of a thread + a tier load whose
/// verdict is predetermined. Above the floor the artifact itself stays the authority
/// (Import subtraction can still land the row count under the floor; the adopt's
/// quantized-graph check catches that band).
pub fn quantized_tier_possible(node_count: usize) -> bool {
  vorpal_ann::AnnConfig::for_n(node_count) == vorpal_ann::AnnConfig::Vamana
}

/// Why an adoption produced no tier — and whether a classic warm could change the
/// verdict. `curable: false` declines are properties of the GENERATION itself (a flat
/// base tier for this corpus size class): rebuilding the same artifacts can never cure
/// them, so the daemon's latch must survive warms until the next commit changes the
/// generation. Every decline is phase-stamped where it is decided — a silent decline is
/// what let the adopt→warm→re-adopt spin hide inside "environmental" test noise.
#[derive(Clone, Copy, Debug)]
pub struct AdoptDecline {
  pub curable: bool,
}

impl LiveAnnTier {
  /// Adopt the committed generation's tier, re-keyed by eids — **stale-tolerant**: on an
  /// actively edited tree the classic warm can never land a tier that is still fresh by
  /// adoption time (the bootstrap race the first daemon validation exposed), so adoption
  /// reconciles WHATEVER tier exists through the per-file identity map (`ann.files`):
  /// unchanged files' rows remap positionally to current ids; changed/vanished files' rows
  /// tombstone; every current node the base never embedded inserts through the overlay's
  /// own insert path. The result is EXACT for the served graph regardless of how far
  /// behind the persisted tier is (bounded by the overlay ceiling — past that, `None` and
  /// the classic warm rebuilds densely).
  ///
  /// Declines (typed, every one stamped): no/foreign-model tier, unreadable tier, or churn
  /// beyond the reconciliation ceiling — curable, the classic warm may retry once; a flat
  /// base tier — incurable for this generation, the caller latches until the next commit.
  pub fn adopt(generation_dir: &Path, kg: &Kg) -> Result<Self, AdoptDecline> {
    // Live-tier rows embed LEXICALLY (this tier predates the learned engine; a
    // learned-selected index serves live pools through the same rerank, which
    // re-embeds at full precision under the SELECTED model — recorded seam).
    let embedder = crate::ActiveEmbedder::Lexical(active_embedder());
    let dim = embedder.dim();
    // Model-provenance gate: reconciliation can bridge GENERATION drift, never MODEL
    // drift. Curable: a warm with the ACTIVE embedder rewrites provenance.
    if persisted_model_provenance(generation_dir).as_ref() != Some(&embedder.provenance()) {
      vorpal_kg::phase_stamp("live-ann: adopt declined (model provenance missing/foreign)");
      return Err(AdoptDecline { curable: true });
    }
    // Curable: the warm builds exactly these artifacts.
    let Some(view) = annfiles::OverlayView::assemble(generation_dir, kg, dim) else {
      vorpal_kg::phase_stamp("live-ann: adopt declined (no reconcilable tier artifacts)");
      return Err(AdoptDecline { curable: true });
    };
    let ann = match AnnIndex::load(&generation_dir.join("ann.bin")) {
      Ok(ann) => ann,
      Err(err) => {
        // Curable: an unreadable/corrupt tier file is what the warm rewrites.
        vorpal_kg::phase_stamp(&format!("live-ann: adopt declined (ann.bin unreadable: {err})"));
        return Err(AdoptDecline { curable: true });
      }
    };
    // INCURABLE: a flat base tier is a property of the generation's size class — the
    // warm rebuilds the same flat tier forever. Latching this (instead of re-warming)
    // is what broke the adopt→fail→warm→re-adopt spin every small-repo daemon ran.
    if !ann.has_quantized_graph() {
      vorpal_kg::phase_stamp("live-ann: adopt declined (flat base tier — below the quantized-graph floor)");
      return Err(AdoptDecline { curable: false });
    }
    let mut eids = Vec::with_capacity(ann.len());
    let mut runs = build_runs((0..ann.len()).map(|row| {
      let old_id = ann.row_id(row);
      (old_id, view.remap(old_id))
    }));
    let mut dead_sentinels: Vec<u64> = Vec::new();
    for row in 0..ann.len() {
      let old_id = ann.row_id(row);
      let new_id = view.remap(old_id);
      match new_id.and_then(|new_id| eid_lo_of(kg, new_id)) {
        Some(eid) => {
          eids.push(eid);
        }
        None => {
          // Dead base row (changed/deleted file, or a pre-eid node): key it with a unique
          // sentinel and tombstone it right after adoption — it keeps routing, never
          // returns. Sentinels descend from u64::MAX, far outside blake3-derived eids.
          let sentinel = u64::MAX - dead_sentinels.len() as u64;
          eids.push(sentinel);
          dead_sentinels.push(sentinel);
        }
      }
    }
    let Some(overlay) = AnnOverlay::adopt_with_ids(ann, eids) else {
      // Post-quant-check this is unexpected; stamped and curable so ONE warm-mediated
      // retry happens — the server's attempts cap bounds anything persistent.
      vorpal_kg::phase_stamp("live-ann: adopt declined (overlay refused the base tier)");
      return Err(AdoptDecline { curable: true });
    };
    runs.shrink_to_fit();
    let by_cur = runs_by_cur(&runs);
    let mut tier = Self {
      overlay,
      runs,
      runs_by_cur: by_cur,
      eid_to_id: HashMap::new(),
      baseline_recall: None,
      probe_rows: Vec::new(),
      rows_since_probe: 0,
      degraded: false,
    };
    for sentinel in dead_sentinels {
      tier.overlay.delete(sentinel);
    }
    tier.refresh_ids(kg);
    // Rows the base never embedded (changed + new files since the tier was built): insert
    // through the same per-edit path, so reconciliation and steady-state are one code path.
    let mut row_buf = vec![0.0f32; dim];
    let embed_root = crate::embedding_root(kg);
    for &id in &view.overlay_ids {
      let Some(eid) = eid_lo_of(kg, id) else { continue };
      embed_node_into(kg, &embedder, id, &mut row_buf, &embed_root);
      tier.overlay.insert(eid, &row_buf);
    }
    vorpal_kg::phase_stamp(&format!(
      "live-ann: adopted {} live rows ({} base rows tombstoned, {} inserted)",
      tier.overlay.live_len(),
      tier.overlay.dead_len(),
      view.overlay_ids.len(),
    ));
    Ok(tier)
  }

  /// Rebuild the eid → node-id translation for a newly served generation. O(n); the daemon
  /// runs it off the serve path (searches before it completes just use the classic tiers).
  pub fn refresh_ids(&mut self, kg: &Kg) {
    let mut map = HashMap::with_capacity(kg.node_count());
    for id in 0..kg.node_count() as u64 {
      if let Some(eid) = eid_lo_of(kg, id) {
        map.insert(eid, id);
      }
    }
    self.eid_to_id = map;
    // Ids may have moved: re-derive the runs from the stable ids.
    let base = self.overlay.base_len() as u32;
    let base_ids = self.overlay.base_ids();
    let map = &self.eid_to_id;
    let overlay = &self.overlay;
    let runs = build_runs((0..base).map(|row| {
      (base_ids[row as usize], map.get(&overlay.stable_id_of(row)).copied())
    }));
    self.runs_by_cur = runs_by_cur(&runs);
    self.runs = runs;
  }

  /// The current id of a base row, by its run (`None` for a dead or unmapped row).
  fn current_id_of_base(&self, base_id: u64) -> Option<u64> {
    let at = self.runs.partition_point(|run| run.base_lo <= base_id);
    let run = self.runs.get(at.checked_sub(1)?)?;
    (base_id < run.base_hi).then(|| (i128::from(base_id) + run.shift()) as u64)
  }

  /// Exact code-space top-`take` inside `ranges` (current dense ids), nearest first —
  /// the scoped counterpart of [`LiveAnnTier::search_ids`]: no beam, no overfetch, cost
  /// proportional to the rows the scope covers.
  pub fn scan_scoped(&self, query_vec: &[f32], take: usize, ranges: &crate::scope::ScopeRanges) -> Vec<u64> {
    let base = self.overlay.base_len() as u32;
    let total = self.overlay.total_rows() as u32;
    let base_ids = self.overlay.base_ids();
    // Where the scope's rows sit: each current-id range meets a few runs; each meeting
    // is a base-id range, located on the ascending base ids by binary search. Appended
    // rows (few, arbitrary ids) are scanned with a membership test.
    let mut row_ranges: Vec<std::ops::Range<u32>> = Vec::new();
    for r in ranges.ranges() {
      let first = self
        .runs_by_cur
        .partition_point(|&i| self.runs[i as usize].cur_hi() <= r.start);
      for &i in &self.runs_by_cur[first..] {
        let run = &self.runs[i as usize];
        if run.cur_lo >= r.end {
          break;
        }
        let lo = r.start.max(run.cur_lo);
        let hi = r.end.min(run.cur_hi());
        if lo >= hi {
          continue;
        }
        let base_lo = (i128::from(lo) - run.shift()) as u64;
        let base_hi = (i128::from(hi) - run.shift()) as u64;
        let row_lo = base_ids.partition_point(|&id| id < base_lo) as u32;
        let row_hi = base_ids.partition_point(|&id| id < base_hi) as u32;
        if row_hi > row_lo {
          row_ranges.push(row_lo..row_hi);
        }
      }
    }
    row_ranges.push(base..total);
    let map = &self.eid_to_id;
    let overlay = &self.overlay;
    let current_of = |row: u32| -> Option<u64> {
      if row < base {
        self.current_id_of_base(base_ids[row as usize])
      } else {
        map.get(&overlay.stable_id_of(row)).copied()
      }
    };
    overlay
      .scan_filtered_in(query_vec, take, &row_ranges, |row| {
        // Base rows located above are inside the ranges by construction.
        row < base || current_of(row).is_some_and(|id| ranges.contains(id))
      })
      .into_iter()
      .filter_map(|(row, _)| current_of(row))
      .collect()
  }

  /// Live rows in the tier (base minus tombstones plus appended).
  pub fn rows(&self) -> usize {
    self.overlay.live_len()
  }

  /// Apply one edit's churn: tombstone the removed eids, then (re)insert every added eid by
  /// embedding its node from the CURRENT graph — the same recipe (`embed_node_into`) the
  /// full build uses, so live rows and rebuilt rows are byte-equal vectors.
  pub fn apply_edit(&mut self, kg: &Kg, removed_eids: &[u64], added_eids: &[u64]) {
    for &eid in removed_eids {
      self.overlay.delete(eid);
    }
    // Live-tier rows embed LEXICALLY (this tier predates the learned engine; a
    // learned-selected index serves live pools through the same rerank, which
    // re-embeds at full precision under the SELECTED model — recorded seam).
    let embedder = crate::ActiveEmbedder::Lexical(active_embedder());
    let mut row = vec![0.0f32; embedder.dim()];
    let embed_root = crate::embedding_root(kg);
    for &eid in added_eids {
      let Some(&id) = self.eid_to_id.get(&eid) else {
        continue; // not in the served graph (import node, vanished mid-burst) — skip
      };
      embed_node_into(kg, &embedder, id, &mut row, &embed_root);
      self.overlay.insert(eid, &row);
    }
    self.rows_since_probe += removed_eids.len() + added_eids.len();
  }

  /// Run the recall probe when due — after adoption (anchoring the baseline) and then per
  /// [`PROBE_CHURN_DENOMINATOR`] of live-row churn. Background-thread work (the daemon
  /// calls this from the same task that applied the churn); a probe at or below
  /// `baseline − PROBE_DEGRADATION` latches [`LiveAnnTier::needs_compaction`]. Stamps the
  /// measurement either way — the tier's quality is a number, not a hope.
  pub fn probe_if_due(&mut self) {
    let due = self.baseline_recall.is_none()
      || self.rows_since_probe * PROBE_CHURN_DENOMINATOR >= self.overlay.live_len().max(1);
    if self.degraded || !due {
      return;
    }
    let start = std::time::Instant::now();
    // Pinned probe set: keep alive rows, deterministically replace dead ones — the same
    // rows are re-measured across the tier's life so probes compare like with like.
    let refreshed = self.overlay.refresh_probe_rows(&self.probe_rows);
    let Some(measured) = self.overlay.pool_recall_probe_with(&refreshed) else {
      return; // too small to measure — flat-scale tiers never reach here in practice
    };
    self.probe_rows = refreshed;
    self.rows_since_probe = 0;
    let baseline = *self.baseline_recall.get_or_insert(measured);
    if measured <= baseline - PROBE_DEGRADATION {
      self.degraded = true;
    }
    vorpal_kg::phase_stamp(&format!(
      "live-ann: recall probe {measured:.4} (baseline {baseline:.4}, {} live, {:.2}% dead, {} ms){}",
      self.overlay.live_len(),
      self.overlay.dead_fraction() * 100.0,
      start.elapsed().as_millis(),
      if self.degraded { " — DEGRADED, retiring to compactor" } else { "" },
    ));
  }

  /// The daemon's compaction trigger: tombstone debt past the 5% ceiling, or measured
  /// recall through the degradation bar — either retires this tier to the classic warm.
  pub fn needs_compaction(&self) -> bool {
    self.degraded || self.overlay.dead_fraction() > 0.05
  }

  /// The semantic candidate pool for `query_vec`, translated to CURRENT-generation node
  /// ids. Unknown eids (deleted symbols, translation lag) drop out — thinner pool, never a
  /// wrong candidate; the caller's rerank re-embeds everything against the current graph.
  pub fn search_ids(&self, query_vec: &[f32], take: usize) -> Vec<u64> {
    self
      .overlay
      .search_pool(query_vec, take)
      .into_iter()
      .filter_map(|(eid, _)| self.eid_to_id.get(&eid).copied())
      .collect()
  }

  pub fn dead_fraction(&self) -> f64 {
    self.overlay.dead_fraction()
  }

  pub fn live_len(&self) -> usize {
    self.overlay.live_len()
  }
}

#[cfg(test)]
mod tests {
  /// The floor law the daemon's structural gate leans on: `semantic_row_ids` embeds a
  /// strict subset of nodes, so `node_count <= floor` PROVES the committed tier is flat.
  /// This pins the predicate to the builder's own config law — if `AnnConfig::for_n`
  /// ever moves, this fails instead of silently re-opening the adopt spin.
  #[test]
  fn quantized_floor_matches_the_builder_law() {
    assert!(!super::quantized_tier_possible(0));
    assert!(!super::quantized_tier_possible(65_536));
    assert!(super::quantized_tier_possible(65_537));
  }
}
