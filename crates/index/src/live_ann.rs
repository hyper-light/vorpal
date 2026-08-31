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

use vorpal_ann::{AnnOverlay, Embedder as _};
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

pub struct LiveAnnTier {
  overlay: AnnOverlay,
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
    let embedder = active_embedder();
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
    let mut dead_sentinels: Vec<u64> = Vec::new();
    for row in 0..ann.len() {
      let old_id = ann.row_id(row);
      match view.remap(old_id).and_then(|new_id| eid_lo_of(kg, new_id)) {
        Some(eid) => eids.push(eid),
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
    let mut tier = Self {
      overlay,
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
  }

  /// Apply one edit's churn: tombstone the removed eids, then (re)insert every added eid by
  /// embedding its node from the CURRENT graph — the same recipe (`embed_node_into`) the
  /// full build uses, so live rows and rebuilt rows are byte-equal vectors.
  pub fn apply_edit(&mut self, kg: &Kg, removed_eids: &[u64], added_eids: &[u64]) {
    for &eid in removed_eids {
      self.overlay.delete(eid);
    }
    let embedder = active_embedder();
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
