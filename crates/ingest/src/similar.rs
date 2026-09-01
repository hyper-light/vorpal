//! Link-time near-clone pairing over the persisted sketches (see `signature.rs`): LSH banding
//! proposes candidate pairs, the sketch estimate verifies them, and each definition keeps at
//! most [`MAX_PARTNERS`] partners — the symmetric `similar_to` edge family.
//!
//! Every ceiling is counted and stated on the report; nothing is dropped silently.

use rayon::prelude::*;

use crate::signature::{estimate, BINS};

/// Sketch bytes per band; 16 bands of 4 bytes — a pair at 0.7 similarity is proposed with
/// probability 1 − (1 − 0.7⁴)¹⁶ ≈ 0.99.
const BAND_ROWS: usize = 4;
const BANDS: usize = BINS / BAND_ROWS;
/// Accept a pair at or above this estimated Jaccard similarity.
pub const MIN_SIMILARITY: f64 = 0.7;
/// Shingle-count ratio below which a pair cannot reach the threshold (a loose bound — the
/// counts include repeats).
const MIN_SIZE_RATIO: f64 = 0.5;
/// Bucket members beyond which a bucket pairs each member with its lowest-id member (a star)
/// instead of all pairs — bounds the quadratic blow-up of a large clone family.
const ALL_PAIRS_BUCKET: usize = 64;
/// Partners each definition keeps (highest similarity first, then lowest id). A pair
/// survives when EITHER side keeps it, so a large clone family's representative — the one
/// node every member pairs with under the star rule — links to every member.
pub const MAX_PARTNERS: usize = 8;
/// Candidate pairs considered per build.
const MAX_CANDIDATES: usize = 8_000_000;

/// One signed definition, as replayed from its product.
#[derive(Clone, PartialEq, Eq)]
pub struct SigRow {
  pub node: u64,
  pub shingles: u32,
  pub sketch: [u8; BINS],
}

/// What the pass did, for the index report.
#[derive(Debug, Default, Clone)]
pub struct SimilarReport {
  /// Symmetric pairs sealed (each pair counted once).
  pub edges: u64,
  /// Definitions that carried a sketch.
  pub signed: u64,
  /// Buckets pairs were starred in instead of enumerated (clone families larger than the
  /// all-pairs bound).
  pub starred_buckets: u64,
  /// Why the pass produced nothing, or what it truncated — stated, never a silent zero.
  pub note: Option<String>,
}

/// Similar pairs `(a, b, confidence)` with `a < b`, sorted; confidence = similarity × 100.
// The sigs family (P4.5c) persists exactly these sketches; the widths must never drift.
const _: () = assert!(BINS == vorpal_kg::SIG_SKETCH_LEN);

pub(crate) fn similar_pairs(
  mut rows: Vec<SigRow>,
) -> (Vec<(u64, u64, u8)>, SimilarReport, Vec<SigRow>) {
  let mut report = SimilarReport {
    signed: rows.len() as u64,
    ..SimilarReport::default()
  };
  if rows.len() < 2 {
    report.note = Some(match rows.len() {
      0 => "no definition reached the signing floor (32 tokens)".to_string(),
      _ => "one signed definition — nothing to pair".to_string(),
    });
    return (Vec::new(), report, rows);
  }
  let started = std::time::Instant::now();
  rows.par_sort_unstable_by_key(|r| r.node);
  rows.dedup_by_key(|r| r.node);
  // Band keys: (band, 4 sketch bytes) → row index. Sorted, so equal keys are adjacent.
  let mut keyed: Vec<(u64, u32)> = Vec::with_capacity(rows.len() * BANDS);
  for (i, row) in rows.iter().enumerate() {
    for band in 0..BANDS {
      let bytes = &row.sketch[band * BAND_ROWS..(band + 1) * BAND_ROWS];
      let key = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
      keyed.push(((band as u64) << 32 | key as u64, i as u32));
    }
  }
  keyed.par_sort_unstable();
  let mut candidates: Vec<(u32, u32)> = Vec::new();
  let mut truncated = false;
  let mut start = 0;
  'buckets: while start < keyed.len() {
    let key = keyed[start].0;
    let mut end = start + 1;
    while end < keyed.len() && keyed[end].0 == key {
      end += 1;
    }
    let bucket = &keyed[start..end];
    if bucket.len() >= 2 {
      if bucket.len() <= ALL_PAIRS_BUCKET {
        for (p, &(_, a)) in bucket.iter().enumerate() {
          for &(_, b) in &bucket[p + 1..] {
            if candidates.len() >= MAX_CANDIDATES {
              truncated = true;
              break 'buckets;
            }
            candidates.push((a.min(b), a.max(b)));
          }
        }
      } else {
        report.starred_buckets += 1;
        let hub = bucket[0].1;
        for &(_, b) in &bucket[1..] {
          if candidates.len() >= MAX_CANDIDATES {
            truncated = true;
            break 'buckets;
          }
          candidates.push((hub.min(b), hub.max(b)));
        }
      }
    }
    start = end;
  }
  drop(keyed);
  candidates.par_sort_unstable();
  candidates.dedup();
  if vorpal_kg::phase_trace_enabled() {
    vorpal_kg::phase_stamp(&format!(
      "similar: {} rows -> {} candidates in {:?}",
      rows.len(),
      candidates.len(),
      started.elapsed()
    ));
  }
  // Verify: size prefilter, then the sketch estimate.
  let accepted: Vec<(u32, u32, u8)> = candidates
    .par_iter()
    .filter_map(|&(a, b)| {
      let (ra, rb) = (&rows[a as usize], &rows[b as usize]);
      let (small, large) = (ra.shingles.min(rb.shingles), ra.shingles.max(rb.shingles));
      if large == 0 || (small as f64) / (large as f64) < MIN_SIZE_RATIO {
        return None;
      }
      let similarity = estimate(&ra.sketch, &rb.sketch);
      (similarity >= MIN_SIMILARITY).then(|| (a, b, (similarity * 100.0).round() as u8))
    })
    .collect();
  drop(candidates);
  // Partner cap: a pair survives if it is within either endpoint's best MAX_PARTNERS.
  let mut directed: Vec<(u32, u8, u32)> = Vec::with_capacity(accepted.len() * 2);
  for &(a, b, c) in &accepted {
    directed.push((a, 255 - c, b));
    directed.push((b, 255 - c, a));
  }
  directed.par_sort_unstable();
  let mut kept: Vec<(u32, u32)> = Vec::new();
  let mut i = 0;
  while i < directed.len() {
    let node = directed[i].0;
    let mut j = i;
    while j < directed.len() && directed[j].0 == node && j - i < MAX_PARTNERS {
      let other = directed[j].2;
      kept.push((node.min(other), node.max(other)));
      j += 1;
    }
    while j < directed.len() && directed[j].0 == node {
      j += 1;
    }
    i = j;
  }
  kept.par_sort_unstable();
  kept.dedup();
  let confidence: std::collections::HashMap<(u32, u32), u8> =
    accepted.iter().map(|&(a, b, c)| ((a, b), c)).collect();
  let pairs: Vec<(u64, u64, u8)> = kept
    .iter()
    .filter_map(|&(a, b)| {
      let c = *confidence.get(&(a, b))?;
      Some((rows[a as usize].node, rows[b as usize].node, c))
    })
    .collect();
  report.edges = pairs.len() as u64;
  if vorpal_kg::phase_trace_enabled() {
    vorpal_kg::phase_stamp(&format!(
      "similar: {} accepted -> {} pairs kept in {:?}",
      accepted.len(),
      pairs.len(),
      started.elapsed()
    ));
  }
  if truncated {
    report.note = Some(format!(
      "candidate ceiling reached ({MAX_CANDIDATES} pairs) — similarity search truncated; \
       later buckets were not examined"
    ));
  } else if pairs.is_empty() {
    report.note = Some("no pair of signed definitions reached 0.7 similarity".to_string());
  }
  (pairs, report, rows)
}

#[cfg(test)]
mod tests {
  use super::*;

  fn row(node: u64, fill: u8, shingles: u32) -> SigRow {
    SigRow {
      node,
      shingles,
      sketch: [fill; BINS],
    }
  }

  #[test]
  fn pairs_equal_sketches_and_refuses_unrelated_or_mismatched_sizes() {
    let mut near = row(2, 1, 100);
    near.sketch[0] = 9; // 63/64 equal → ~0.98
    let rows = vec![
      row(1, 1, 100),
      near,
      row(3, 2, 100),   // unrelated fill
      row(4, 1, 10),    // equal sketch, but a tenth of the size: refused by the ratio
    ];
    let (pairs, report, _) = similar_pairs(rows);
    assert_eq!(pairs.len(), 1, "{pairs:?}");
    assert_eq!((pairs[0].0, pairs[0].1), (1, 2));
    assert!(pairs[0].2 >= 97, "{}", pairs[0].2);
    assert_eq!(report.signed, 4);
    assert_eq!(report.edges, 1);
    assert!(report.note.is_none());
  }

  #[test]
  fn large_families_star_and_partners_are_capped() {
    // 100 identical definitions: every bucket exceeds the all-pairs bound, so each member
    // pairs with the lowest id, and the hub keeps only MAX_PARTNERS partners.
    let rows: Vec<SigRow> = (0..100).map(|n| row(n, 5, 50)).collect();
    let (pairs, report, _) = similar_pairs(rows);
    assert!(report.starred_buckets > 0);
    assert!(pairs.iter().all(|&(a, _, _)| a == 0), "{pairs:?}");
    // Every member keeps its one pair to the representative (a pair survives when either
    // side keeps it), so the family stays discoverable from every member: 99 pairs, not
    // the 4,950 of an all-pairs enumeration.
    assert_eq!(pairs.len(), 99, "{pairs:?}");
    assert!(report.note.is_none());
    // A mid-sized family under the all-pairs bound is still capped per member.
    let rows: Vec<SigRow> = (0..40).map(|n| row(n, 5, 50)).collect();
    let (pairs, _, _) = similar_pairs(rows);
    let mut degree = vec![0usize; 40];
    for &(a, b, _) in &pairs {
      degree[a as usize] += 1;
      degree[b as usize] += 1;
    }
    // Each node keeps its MAX_PARTNERS lowest-id partners; the lowest ids are kept by
    // everyone, so degrees exceed the cap only at the family's head.
    assert!(degree.iter().all(|&d| d >= MAX_PARTNERS), "{degree:?}");
    assert!(pairs.len() < 40 * 39 / 2, "{}", pairs.len());
  }

  #[test]
  fn nothing_to_pair_is_stated() {
    let (_, report, _) = similar_pairs(vec![row(1, 1, 40)]);
    assert!(report.note.as_deref().unwrap().contains("one signed"));
    let (_, report, _) = similar_pairs(vec![row(1, 1, 40), row(2, 2, 40)]);
    assert!(report.note.as_deref().unwrap().contains("no pair"));
  }
}
