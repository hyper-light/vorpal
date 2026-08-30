//! Incremental overlay over a built Vamana tier (docs/wip/ANN_FRONTIER.md Tier 3 — the
//! FreshDiskANN consensus design): the committed tier stays IMMUTABLE; edits accumulate as
//! tombstones, appended rows, and per-node adjacency patches, so a daemon updates the
//! vector tier per edit (~one beam + prune per vector, microseconds-to-milliseconds)
//! instead of paying the full rebuild per generation.
//!
//! The no-decay condition (FreshDiskANN Fig. 2/3): every repair path prunes with the SAME
//! α > 1 rule the build uses — α-RNG density is what keeps recall flat under churn, and
//! aggressive (α = 1) repair is the documented decay mechanism. Deletes tombstone
//! instantly; searches still route THROUGH tombstoned nodes (removing them from routing is
//! the other documented collapse) but never return them. The canonical rebuild remains the
//! reconciliation anchor: the overlay is scratch state a compactor supersedes.
//!
//! Determinism: no RNG anywhere; fixed iteration orders; ties broken by (distance, id).
//! Replaying the same op sequence on the same base reproduces the overlay exactly.

use crate::index::{
  AnnGraphStore, AnnIndex, BUILD_SEED, CALIBRATION_K, CALIBRATION_PROBES, CALIBRATION_SEARCH_L,
  VAMANA_ALPHA, VAMANA_L_BUILD, VAMANA_R,
};
use crate::qmatrix::{dot_i8, quantize_row};

/// One merged-row candidate: `(row, exact squared distance)`.
type Scored = (u32, f32);

pub struct AnnOverlay {
  base: AnnIndex,
  /// Stable id per base row (overrides `base.ids` — the daemon keys rows by durable eid).
  base_ids: Vec<u64>,
  padded: usize,
  base_n: usize,
  r: usize,
  l: usize,
  alpha: f32,
  /// Tombstones over base + appended rows (index space is contiguous: base then new).
  dead: Vec<bool>,
  dead_count: usize,
  /// Appended rows: quantized with the SAME per-row scheme as the base matrix.
  new_codes: Vec<i8>,
  new_scales: Vec<f32>,
  new_snorm: Vec<f32>,
  new_ids: Vec<u64>,
  /// Adjacency patches: a row here fully replaces the base row (or defines a new row's list).
  patched: std::collections::HashMap<u32, Vec<u32>>,
  /// Stable id → merged row, maintained across inserts and deletes.
  id_to_row: std::collections::HashMap<u64, u32>,
}

impl AnnOverlay {
  /// Adopt a built (or loaded) Vamana-tier index. Returns `None` for flat tiers — at flat
  /// scale the full rebuild is milliseconds and an overlay is pure overhead.
  pub fn adopt(base: AnnIndex) -> Option<Self> {
    let ids: Vec<u64> = (0..base.len()).map(|row| base.ids[row]).collect();
    Self::adopt_with_ids(base, ids)
  }

  /// [`AnnOverlay::adopt`] with the base rows re-keyed by caller-supplied stable ids —
  /// the daemon's form: the persisted tier's ids are GENERATION-LOCAL node ids, but an
  /// overlay that outlives edits must key rows by the durable identity (the node eid),
  /// which survives dense-id renumbering across generations. `ids[row]` replaces
  /// `base.ids[row]` everywhere: dedup, deletes, and returned pools.
  pub fn adopt_with_ids(base: AnnIndex, ids: Vec<u64>) -> Option<Self> {
    base.quant.as_ref()?;
    let base_n = base.len();
    debug_assert_eq!(ids.len(), base_n);
    let padded = base.quant.as_ref().expect("checked").padded();
    let mut id_to_row = std::collections::HashMap::new();
    id_to_row.reserve(base_n);
    for (row, &id) in ids.iter().enumerate() {
      id_to_row.insert(id, row as u32);
    }
    Some(Self {
      base_ids: ids,
      padded,
      base_n,
      r: VAMANA_R,
      l: VAMANA_L_BUILD,
      alpha: VAMANA_ALPHA,
      dead: vec![false; base_n],
      dead_count: 0,
      new_codes: Vec::new(),
      new_scales: Vec::new(),
      new_snorm: Vec::new(),
      new_ids: Vec::new(),
      patched: std::collections::HashMap::new(),
      id_to_row,
      base,
    })
  }

  pub fn live_len(&self) -> usize {
    self.base_n + self.new_ids.len() - self.dead_count
  }

  pub fn dead_len(&self) -> usize {
    self.dead_count
  }

  /// Tombstoned fraction of all rows ever present — the caller's compaction trigger.
  pub fn dead_fraction(&self) -> f64 {
    let total = self.base_n + self.new_ids.len();
    if total == 0 {
      return 0.0;
    }
    self.dead_count as f64 / total as f64
  }

  #[inline]
  fn total_rows(&self) -> usize {
    self.base_n + self.new_ids.len()
  }

  #[inline]
  fn codes_of(&self, row: u32) -> &[i8] {
    let quant = self.base.quant.as_ref().expect("vamana tier");
    if (row as usize) < self.base_n {
      quant.row_codes(row)
    } else {
      let at = (row as usize - self.base_n) * self.padded;
      &self.new_codes[at..at + self.padded]
    }
  }

  #[inline]
  fn scale_of(&self, row: u32) -> f32 {
    let quant = self.base.quant.as_ref().expect("vamana tier");
    if (row as usize) < self.base_n {
      quant.scales[row as usize]
    } else {
      self.new_scales[row as usize - self.base_n]
    }
  }

  #[inline]
  fn snorm_of(&self, row: u32) -> f32 {
    let quant = self.base.quant.as_ref().expect("vamana tier");
    if (row as usize) < self.base_n {
      quant.snorm[row as usize]
    } else {
      self.new_snorm[row as usize - self.base_n]
    }
  }

  #[inline]
  fn id_of(&self, row: u32) -> u64 {
    if (row as usize) < self.base_n {
      self.base_ids[row as usize]
    } else {
      self.new_ids[row as usize - self.base_n]
    }
  }

  /// Exact squared distance between two merged rows — the same formula shape the base
  /// matrix uses, over whichever store each row lives in.
  #[inline]
  fn dist_pair(&self, a: u32, b: u32) -> f32 {
    let dot = dot_i8(self.codes_of(a), self.codes_of(b));
    self.snorm_of(a) + self.snorm_of(b) - 2.0 * self.scale_of(a) * self.scale_of(b) * dot as f32
  }

  /// Exact squared distance between a merged row and a quantized query.
  #[inline]
  fn dist_to_query(&self, row: u32, q_codes: &[i8], q_scale: f32, q_snorm: f32) -> f32 {
    let dot = dot_i8(self.codes_of(row), q_codes);
    self.snorm_of(row) + q_snorm - 2.0 * self.scale_of(row) * q_scale * dot as f32
  }

  fn neighbors_of(&self, row: u32) -> &[u32] {
    if let Some(list) = self.patched.get(&row) {
      return list;
    }
    if (row as usize) < self.base_n {
      // Same row semantics as Adjacency::row, matched directly so the borrow outlives the
      // call (the Adjacency wrapper is a by-value temporary).
      return match &self.base.graph {
        AnnGraphStore::Csr { offsets, targets } => {
          &targets[offsets[row as usize] as usize..offsets[row as usize + 1] as usize]
        }
        AnnGraphStore::Flat { flat, lens, cap } => {
          let at = row as usize * cap;
          &flat[at..at + lens[row as usize] as usize]
        }
      };
    }
    &[]
  }

  /// Beam search over the merged view (the base algorithm's semantics: sorted (dist, id)
  /// beam, frontier cursor, batch admission). Tombstoned rows ROUTE — they stay expandable
  /// waypoints — and are filtered only from returned pools.
  fn beam(&self, q_codes: &[i8], q_scale: f32, q_snorm: f32, l: usize) -> Vec<Scored> {
    let entry = self.base.medoid;
    if self.total_rows() == 0 {
      return Vec::new();
    }
    let mut seen: std::collections::HashSet<u32> =
      std::collections::HashSet::with_capacity(l * 8);
    let mut beam: Vec<(f32, u32, bool)> = Vec::with_capacity(l + 1);
    seen.insert(entry);
    beam.push((self.dist_to_query(entry, q_codes, q_scale, q_snorm), entry, false));
    let mut visited: Vec<Scored> = Vec::with_capacity(l * 3);
    let mut cand: Vec<(f32, u32)> = Vec::with_capacity(64);
    let mut frontier = 0usize;
    while frontier < beam.len() {
      if beam[frontier].2 {
        frontier += 1;
        continue;
      }
      beam[frontier].2 = true;
      let (dist_next, next, _) = beam[frontier];
      frontier += 1;
      visited.push((next, dist_next));
      cand.clear();
      for &nb in self.neighbors_of(next) {
        if seen.insert(nb) {
          cand.push((self.dist_to_query(nb, q_codes, q_scale, q_snorm), nb));
        }
      }
      if cand.is_empty() {
        continue;
      }
      cand.sort_unstable_by(|a, b| (a.0, a.1).partial_cmp(&(b.0, b.1)).unwrap());
      let old = std::mem::replace(&mut beam, Vec::with_capacity(l + 1));
      let (mut oi, mut ci) = (0usize, 0usize);
      let mut first_new: Option<usize> = None;
      while beam.len() < l && (oi < old.len() || ci < cand.len()) {
        let take_new = match (old.get(oi), cand.get(ci)) {
          (Some(&(od, ov, _)), Some(&(cd, cv))) => (cd, cv) < (od, ov),
          (None, Some(_)) => true,
          (Some(_), None) => false,
          (None, None) => unreachable!(),
        };
        if take_new {
          if first_new.is_none() {
            first_new = Some(beam.len());
          }
          let (cd, cv) = cand[ci];
          beam.push((cd, cv, false));
          ci += 1;
        } else {
          beam.push(old[oi]);
          oi += 1;
        }
      }
      if let Some(at) = first_new
        && at < frontier
      {
        frontier = at;
      }
    }
    visited
  }

  /// The build's α-domination prune (lazy first-occluder form), over merged-row distances.
  fn prune(&self, mut candidates: Vec<Scored>) -> Vec<u32> {
    candidates.sort_by(|a, b| {
      (a.1, a.0)
        .partial_cmp(&(b.1, b.0))
        .unwrap_or(std::cmp::Ordering::Equal)
    });
    candidates.dedup_by_key(|c| c.0);
    candidates.truncate(self.l);
    let mut result: Vec<u32> = Vec::with_capacity(self.r.min(candidates.len()));
    'candidates: for &(v, dist_p) in &candidates {
      for &s_id in &result {
        if v == s_id {
          continue 'candidates;
        }
        if self.alpha * self.dist_pair(s_id, v) <= dist_p {
          continue 'candidates;
        }
      }
      result.push(v);
      if result.len() >= self.r {
        break;
      }
    }
    result
  }

  /// Insert (or replace) `id` with `vector`. A live row under the same id is tombstoned
  /// first, so upsert and insert are one operation. Cost: one beam + one prune + ≤ r
  /// back-edge merges — the build's per-point work, once.
  pub fn insert(&mut self, id: u64, vector: &[f32]) {
    if let Some(&row) = self.id_to_row.get(&id)
      && !self.dead[row as usize]
    {
      self.dead[row as usize] = true;
      self.dead_count += 1;
    }
    // Quantize with the row scheme (NOT the query scheme): the row joins the store.
    let dim = self.base.dim;
    let mut codes = vec![0i8; self.padded];
    let (scale, snorm) = quantize_row(&vector[..dim.min(vector.len())], &mut codes[..dim]);
    let q_codes = codes.clone();

    let visited = self.beam(&q_codes, scale, snorm, self.l);
    let new_row = self.total_rows() as u32;
    let candidates: Vec<Scored> = visited.into_iter().filter(|&(v, _)| v != new_row).collect();
    let out = self.prune(candidates);

    self.new_codes.extend_from_slice(&codes);
    self.new_scales.push(scale);
    self.new_snorm.push(snorm);
    self.new_ids.push(id);
    self.dead.push(false);
    self.id_to_row.insert(id, new_row);

    // Back-edges through the same α-prune (the no-decay condition): each chosen neighbor
    // merges the new row into its list and re-prunes only on overflow.
    for &j in &out {
      let mut list: Vec<u32> = self.neighbors_of(j).to_vec();
      if list.contains(&new_row) {
        continue;
      }
      list.push(new_row);
      if list.len() > self.r {
        let scored: Vec<Scored> = list.iter().map(|&v| (v, self.dist_pair(j, v))).collect();
        list = self.prune(scored);
      }
      self.patched.insert(j, list);
    }
    self.patched.insert(new_row, out);
  }

  /// Tombstone `id`. O(1); the row keeps routing until a compaction retires it. Unknown
  /// ids are a no-op (delete-of-absent and double-delete are both idempotent).
  pub fn delete(&mut self, id: u64) {
    if let Some(&row) = self.id_to_row.get(&id)
      && !self.dead[row as usize]
    {
      self.dead[row as usize] = true;
      self.dead_count += 1;
    }
  }

  /// Draw a fresh probe set for [`AnnOverlay::pool_recall_probe_with`], or top up an
  /// existing one: alive `prior` rows are KEPT (recall probes must re-measure the same
  /// rows over a tier's life — 32 correlated probes carry ±points of set-to-set sampling
  /// spread, so redrawing per probe would swamp the degradation bar), dead ones are
  /// replaced from a deterministic seeded stream. Same overlay state + same `prior` →
  /// same result. Rows are opaque cookies: hold them and pass them back.
  pub fn refresh_probe_rows(&self, prior: &[u32]) -> Vec<u32> {
    let total = self.total_rows();
    let probe_count = CALIBRATION_PROBES.min(self.live_len());
    let mut probes: Vec<u32> = prior
      .iter()
      .copied()
      .filter(|&row| (row as usize) < total && !self.dead[row as usize])
      .collect();
    probes.truncate(probe_count);
    let mut chosen: std::collections::HashSet<u32> = probes.iter().copied().collect();
    // The build calibration's selection recipe, distinct salt (independent stream);
    // bounded attempts keep degenerate states (nearly-all-dead) from spinning.
    let mut rng = crate::Rng::new(BUILD_SEED ^ 0x5EED_CA11_0B5E_7B02);
    let mut attempts = 0usize;
    while probes.len() < probe_count && attempts < total * 4 {
      attempts += 1;
      let candidate = rng.below(total) as u32;
      if !self.dead[candidate as usize] && chosen.insert(candidate) {
        probes.push(candidate);
      }
    }
    probes
  }

  /// Measured pool recall of the CURRENT overlay against a pinned probe set — the build
  /// calibration's exact recipe (exact quantized-domain oracle over the live set, then
  /// visited-set membership at the production beam shape), so the number is directly
  /// comparable to the build's `pool_recall` and to earlier probes of the SAME rows.
  /// Deterministic for a given overlay state and probe set. `None` when nothing is
  /// measurable (no live probes, or a degenerate live set).
  ///
  /// Cost: probes × live-rows exact distances (rayon-parallel over probes) + probes
  /// beams — background-thread work, never the serve path.
  pub fn pool_recall_probe_with(&self, probes: &[u32]) -> Option<f64> {
    let total = self.total_rows();
    if self.live_len() < 2 {
      return None;
    }
    use rayon::prelude::*;
    let (hits, want) = probes
      .par_iter()
      .filter(|&&probe| (probe as usize) < total && !self.dead[probe as usize])
      .map(|&probe| {
        // Exact quantized top-K over the live set (probe excluded, ties by row — the
        // build oracle's ordering), then the production-shaped beam.
        let mut best: Vec<(f32, u32)> = Vec::with_capacity(CALIBRATION_K + 1);
        for row in 0..total as u32 {
          if row == probe || self.dead[row as usize] {
            continue;
          }
          let key = (self.dist_pair(probe, row), row);
          if best.len() < CALIBRATION_K {
            let at = best.partition_point(|entry| *entry < key);
            best.insert(at, key);
          } else if key < *best.last().expect("k > 0") {
            let at = best.partition_point(|entry| *entry < key);
            best.insert(at, key);
            best.pop();
          }
        }
        let visited = self.beam(
          self.codes_of(probe),
          self.scale_of(probe),
          self.snorm_of(probe),
          CALIBRATION_SEARCH_L.min(total),
        );
        let pool: std::collections::HashSet<u32> = visited.iter().map(|&(v, _)| v).collect();
        let hit = best.iter().filter(|(_, row)| pool.contains(row)).count();
        (hit, best.len())
      })
      .reduce(|| (0, 0), |a, b| (a.0 + b.0, a.1 + b.1));
    if want == 0 {
      return None;
    }
    Some(hits as f64 / want as f64)
  }

  /// [`AnnOverlay::pool_recall_probe_with`] over a freshly drawn set — the one-shot form
  /// for tools and tests; long-lived tiers pin the set and refresh it instead.
  pub fn pool_recall_probe(&self) -> Option<f64> {
    self.pool_recall_probe_with(&self.refresh_probe_rows(&[]))
  }

  /// The live visited pool for `query`, exact distances, sorted (distance, id-row) — the
  /// caller reranks/fuses exactly as with the base tier. Tombstoned rows never appear.
  pub fn search_pool(&self, query: &[f32], l: usize) -> Vec<(u64, f32)> {
    let dim = self.base.dim;
    let mut codes = vec![0i8; self.padded];
    let (scale, snorm) = quantize_row(&query[..dim.min(query.len())], &mut codes[..dim]);
    let mut pool = self.beam(&codes, scale, snorm, l.max(1));
    pool.sort_unstable_by(|a, b| (a.1, a.0).partial_cmp(&(b.1, b.0)).unwrap());
    pool
      .into_iter()
      .filter(|&(row, _)| !self.dead[row as usize])
      .map(|(row, dist)| (self.id_of(row), dist))
      .collect()
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::index::AnnConfig;

  fn splitmix(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
  }

  fn vector_for(id: u64, dim: usize) -> Vec<f32> {
    let mut state = id.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xA5A5_5A5A;
    let mut v: Vec<f32> = (0..dim)
      .map(|_| (splitmix(&mut state) as f64 / u64::MAX as f64) as f32 - 0.5)
      .collect();
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    for x in &mut v {
      *x /= norm.max(1e-9);
    }
    v
  }

  /// Exact top-k of the LIVE set by true f32 distance (the oracle).
  fn brute_topk(live: &[(u64, Vec<f32>)], query: &[f32], k: usize) -> Vec<u64> {
    let mut scored: Vec<(f32, u64)> = live
      .iter()
      .map(|(id, v)| {
        let d: f32 = v.iter().zip(query).map(|(a, b)| (a - b) * (a - b)).sum();
        (d, *id)
      })
      .collect();
    scored.sort_by(|a, b| a.partial_cmp(b).unwrap());
    scored.into_iter().take(k).map(|(_, id)| id).collect()
  }

  fn pool_recall(overlay: &AnnOverlay, live: &[(u64, Vec<f32>)], probes: &[Vec<f32>]) -> f64 {
    let mut hit = 0usize;
    let mut want = 0usize;
    for probe in probes {
      let truth = brute_topk(live, probe, 10);
      let pool: std::collections::HashSet<u64> =
        overlay.search_pool(probe, 64).into_iter().map(|(id, _)| id).collect();
      want += truth.len();
      hit += truth.iter().filter(|id| pool.contains(id)).count();
    }
    hit as f64 / want as f64
  }

  const DIM: usize = 64;

  fn build_base(n: u64) -> (AnnIndex, Vec<(u64, Vec<f32>)>) {
    let rows: Vec<(u64, Vec<f32>)> = (0..n).map(|id| (id, vector_for(id, DIM))).collect();
    let index = AnnIndex::build(DIM, rows.clone(), Some(AnnConfig::Vamana));
    (index, rows)
  }

  #[test]
  fn churn_preserves_pool_recall_and_hides_tombstones() {
    let (base, rows) = build_base(3000);
    let mut overlay = AnnOverlay::adopt(base).expect("vamana tier");
    let mut live: Vec<(u64, Vec<f32>)> = rows;
    let probes: Vec<Vec<f32>> = (0..40u64).map(|i| vector_for(1_000_000 + i, DIM)).collect();
    let start = pool_recall(&overlay, &live, &probes);
    assert!(start > 0.85, "adopted-base recall sanity: {start}");

    // 10 churn cycles of 5%: delete 150, insert 150 fresh.
    let mut next_id = 3000u64;
    for cycle in 0..10u64 {
      for k in 0..150u64 {
        let victim = live[((cycle * 977 + k * 131) as usize * 7919) % live.len()].0;
        overlay.delete(victim);
        live.retain(|(id, _)| *id != victim);
      }
      for _ in 0..150 {
        let v = vector_for(next_id, DIM);
        overlay.insert(next_id, &v);
        live.push((next_id, v));
        next_id += 1;
      }
    }
    let end = pool_recall(&overlay, &live, &probes);
    assert!(
      end >= start - 0.05,
      "recall decayed under churn: {start} -> {end}"
    );

    // Tombstones never surface; fresh inserts are retrievable as their own nearest row.
    let dead_id = live[0].0;
    let dead_vec = live[0].1.clone();
    overlay.delete(dead_id);
    assert!(
      overlay.search_pool(&dead_vec, 64).iter().all(|(id, _)| *id != dead_id),
      "tombstoned id surfaced in a pool"
    );
    let fresh = vector_for(9_999_999, DIM);
    overlay.insert(9_999_999, &fresh);
    let top = overlay.search_pool(&fresh, 64);
    assert_eq!(top.first().map(|(id, _)| *id), Some(9_999_999));
  }

  #[test]
  fn replay_is_deterministic() {
    let run = || {
      let (base, _) = build_base(2000);
      let mut overlay = AnnOverlay::adopt(base).expect("vamana tier");
      for i in 0..200u64 {
        overlay.insert(10_000 + i, &vector_for(10_000 + i, DIM));
        if i % 3 == 0 {
          overlay.delete(i * 7 % 2000);
        }
      }
      let probe = vector_for(777_777, DIM);
      overlay.search_pool(&probe, 64)
    };
    assert_eq!(run(), run(), "same ops on same base must reproduce exactly");
  }

  #[test]
  fn recall_probe_is_deterministic_and_survives_churn() {
    let (base, _) = build_base(3000);
    let mut overlay = AnnOverlay::adopt(base).expect("vamana tier");
    let baseline = overlay.pool_recall_probe().expect("measurable");
    assert_eq!(
      overlay.pool_recall_probe(),
      Some(baseline),
      "same state must probe to the same value"
    );
    assert!(baseline > 0.85, "adopted-base probe sanity: {baseline}");

    // The churn pattern of `churn_preserves_pool_recall_and_hides_tombstones`, judged by
    // the runtime probe instead of the f32 test oracle — the two metrics must agree that
    // recall holds (same 0.05 test-stability bound).
    let mut live: Vec<u64> = (0..3000).collect();
    let mut next_id = 3000u64;
    for cycle in 0..10u64 {
      for k in 0..150u64 {
        let victim = live[((cycle * 977 + k * 131) as usize * 7919) % live.len()];
        overlay.delete(victim);
        live.retain(|id| *id != victim);
      }
      for _ in 0..150 {
        overlay.insert(next_id, &vector_for(next_id, DIM));
        live.push(next_id);
        next_id += 1;
      }
    }
    let end = overlay.pool_recall_probe().expect("still measurable");
    assert!(
      end >= baseline - 0.05,
      "probe says recall decayed under churn: {baseline} -> {end}"
    );
  }

  #[test]
  fn probe_set_pins_alive_rows_and_replaces_dead_deterministically() {
    let (base, _) = build_base(3000);
    let mut overlay = AnnOverlay::adopt(base).expect("vamana tier");
    let pinned = overlay.refresh_probe_rows(&[]);
    assert_eq!(pinned.len(), 32);
    assert_eq!(pinned, overlay.refresh_probe_rows(&pinned), "no churn -> identical set");

    // Kill two probe rows: refresh must keep every survivor (order too) and top up with
    // deterministic replacements.
    overlay.delete(pinned[3] as u64); // base rows: id == row for this fixture
    overlay.delete(pinned[17] as u64);
    let refreshed = overlay.refresh_probe_rows(&pinned);
    assert_eq!(refreshed.len(), 32);
    let survivors: Vec<u32> =
      pinned.iter().copied().filter(|r| *r != pinned[3] && *r != pinned[17]).collect();
    assert_eq!(&refreshed[..30], survivors.as_slice(), "alive rows keep their order");
    assert!(!refreshed.contains(&pinned[3]) && !refreshed.contains(&pinned[17]));
    assert_eq!(
      refreshed,
      overlay.refresh_probe_rows(&pinned),
      "replacement draw is deterministic"
    );
    // Pinned measurement stays available and sane after the replacement.
    let measured = overlay.pool_recall_probe_with(&refreshed).expect("measurable");
    assert!(measured > 0.85, "pinned-set probe sanity: {measured}");
  }

  #[test]
  fn upsert_replaces_and_flat_tier_is_refused() {
    let (base, _) = build_base(2000);
    let mut overlay = AnnOverlay::adopt(base).expect("vamana tier");
    let moved = vector_for(555_555, DIM);
    overlay.insert(42, &moved);
    let top = overlay.search_pool(&moved, 64);
    assert_eq!(top.first().map(|(id, _)| *id), Some(42), "upsert serves the new vector");
    let flat = AnnIndex::build(DIM, vec![(1, vector_for(1, DIM))], Some(AnnConfig::FlatExact));
    assert!(AnnOverlay::adopt(flat).is_none(), "flat tiers rebuild instead");
  }
}
