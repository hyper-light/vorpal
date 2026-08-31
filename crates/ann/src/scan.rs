//! Fused exhaustive semantic scan — the fallback tier when no (fresh) index exists.
//!
//! For a one-shot query, the fastest *correct* algorithm over an unindexed corpus is not a
//! graph build: it is a single fused pass — embed a row into per-worker scratch, score it
//! against the query, keep a bounded top set, discard the vector. Nothing materializes (no
//! row matrix, no graph), recall is exact (a strict superset of any beam search), and at
//! kernel scale (~2.4M rows) the whole pass costs roughly the embedding time (~0.2s) versus
//! ~10s for a graph build. `AnnConfig::for_n` already declares this philosophy for small
//! corpora (flat-exact *is* the tier below 65k rows); this extends it to cold queries at any
//! scale.
//!
//! Determinism: per-row scores are independent of chunking, and `(distance, id)` is a total
//! order under `f32::total_cmp`, so the top-`take` set — and its order — is a pure function
//! of the rows and the query, at any thread count.

use rayon::prelude::*;

use crate::l2_sq;

/// Exhaustively score `ids` against `query` (a `dim`-length, normalized embedding — the
/// output of [`crate::Embedder::embed`]) and return the closest `take` as
/// `(id, squared L2)`, ascending. `fill(i, row)` writes row `i`'s embedding (the same
/// closure shape [`crate::AnnIndex::build_rows`] takes, so callers share the recipe).
pub fn exhaustive_semantic<F>(
  dim: usize,
  ids: &[u64],
  fill: F,
  query: &[f32],
  take: usize,
) -> Vec<(u64, f32)>
where
  F: Fn(usize, &mut [f32]) + Sync,
{
  if ids.is_empty() || take == 0 {
    return Vec::new();
  }
  debug_assert_eq!(query.len(), dim);
  let threads = rayon::current_num_threads().max(1);
  let chunk = ids.len().div_ceil(threads * 4).max(1);

  let mut merged: Vec<(f32, u64)> = ids
    .par_chunks(chunk)
    .enumerate()
    .map(|(chunk_index, chunk_ids)| {
      let mut row = vec![0.0f32; dim];
      // Bounded local top set: sorted ascending by (dist, id), capped at `take`.
      let mut top: Vec<(f32, u64)> = Vec::with_capacity(chunk_ids.len().min(take + 1));
      // Degenerate case — the cap cannot bind inside this chunk (`take` admits every
      // row), so the sorted-insert discipline below would only buy O(len²) memmove:
      // collect unordered and let the final merge sort order everything. Output is
      // identical (the merge sorts and truncates either way); this keeps full-population
      // fetches (e.g. the multi-phrase exhaustive rung) linear instead of quadratic.
      let unbounded = take >= chunk_ids.len();
      for (j, &id) in chunk_ids.iter().enumerate() {
        fill(chunk_index * chunk + j, &mut row);
        let dist = l2_sq(&row, query);
        if unbounded {
          top.push((dist, id));
          continue;
        }
        if top.len() == take {
          let &(worst, worst_id) = top.last().expect("take > 0");
          // Full, and this candidate is not strictly closer than the worst kept → skip.
          if worst.total_cmp(&dist).then(worst_id.cmp(&id)) != std::cmp::Ordering::Greater {
            continue;
          }
        }
        let at = top.partition_point(|&(d, v)| {
          d.total_cmp(&dist).then(v.cmp(&id)) == std::cmp::Ordering::Less
        });
        top.insert(at, (dist, id));
        top.truncate(take);
      }
      top
    })
    .reduce(Vec::new, |mut a, b| {
      a.extend(b);
      a
    });

  merged.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
  merged.truncate(take);
  merged.into_iter().map(|(dist, id)| (id, dist)).collect()
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::normalize;

  fn rows(n: usize, dim: usize) -> Vec<Vec<f32>> {
    (0..n)
      .map(|i| {
        let mut v = vec![0.0f32; dim];
        let mut state = (i as u64)
          .wrapping_mul(6364136223846793005)
          .wrapping_add(97);
        for _ in 0..6 {
          state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
          v[(state >> 33) as usize % dim] = ((state >> 16) & 0xFF) as f32 / 255.0 + 0.05;
        }
        normalize(&mut v);
        v
      })
      .collect()
  }

  #[test]
  fn matches_serial_reference_and_is_chunking_independent() {
    let dim = 32;
    let data = rows(5_000, dim);
    let ids: Vec<u64> = (0..data.len() as u64).map(|i| i * 3 + 7).collect();
    let mut query = vec![0.0f32; dim];
    query[3] = 1.0;

    let got = exhaustive_semantic(
      dim,
      &ids,
      |i, row| row.copy_from_slice(&data[i]),
      &query,
      25,
    );

    // Serial reference: score everything, sort by (dist, id), take 25.
    let mut reference: Vec<(f32, u64)> = data
      .iter()
      .zip(&ids)
      .map(|(row, &id)| (l2_sq(row, &query), id))
      .collect();
    reference.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
    reference.truncate(25);
    let reference: Vec<(u64, f32)> = reference.into_iter().map(|(d, i)| (i, d)).collect();

    assert_eq!(got, reference);
    // And again — bit-stable across runs.
    let again = exhaustive_semantic(
      dim,
      &ids,
      |i, row| row.copy_from_slice(&data[i]),
      &query,
      25,
    );
    assert_eq!(got, again);
  }

  #[test]
  fn full_population_take_matches_reference() {
    let dim = 16;
    let data = rows(3_000, dim);
    let ids: Vec<u64> = (0..data.len() as u64).collect();
    let mut query = vec![0.0f32; dim];
    query[1] = 1.0;
    // take ≥ n exercises the unbounded (collect-then-sort) chunk path.
    let got = exhaustive_semantic(
      dim,
      &ids,
      |i, row| row.copy_from_slice(&data[i]),
      &query,
      data.len() * 2,
    );
    let mut reference: Vec<(f32, u64)> = data
      .iter()
      .zip(&ids)
      .map(|(row, &id)| (l2_sq(row, &query), id))
      .collect();
    reference.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
    let reference: Vec<(u64, f32)> = reference.into_iter().map(|(d, i)| (i, d)).collect();
    assert_eq!(got, reference);
  }

  #[test]
  fn empty_inputs_yield_empty() {
    assert!(exhaustive_semantic(8, &[], |_, _| {}, &[0.0; 8], 10).is_empty());
    assert!(exhaustive_semantic(8, &[1, 2], |_, _| {}, &[0.0; 8], 0).is_empty());
  }
}
