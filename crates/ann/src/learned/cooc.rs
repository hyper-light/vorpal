//! Definition-window co-occurrence and PPMI (docs/wip/SEMANTIC_TIER.md §2b, TACL 2015
//! Q15-1016 knobs): symmetric window within each document, context-distribution
//! smoothing α = 0.75 on the context marginals (the "one always-safe knob", ≈ +3 pts),
//! and NO PMI shift (shifting costs SVD −14 pts). Small windows favor count models —
//! the window is a cited parameter of the method, recorded in model provenance.
//!
//! Determinism: documents are consumed in caller order, pair counts live in a dense
//! id-keyed accumulation (BTreeMap — iteration order is the sorted pair order, so every
//! downstream reduction is fixed-order by construction). No floats until PPMI; counts
//! are exact u64.

use std::collections::BTreeMap;
use std::collections::HashMap;

/// The context-distribution smoothing exponent — TACL 2015's α = 0.75, the empirically
/// always-safe setting (cited constant, not a tunable).
pub const CDS_ALPHA: f64 = 0.75;

/// An interned vocabulary: token string → dense id, in FIRST-SEEN order (caller feeds
/// documents deterministically, so ids are deterministic).
#[derive(Default)]
pub struct Vocab {
  ids: HashMap<String, u32>,
  terms: Vec<String>,
}

impl Vocab {
  pub fn intern(&mut self, term: &str) -> u32 {
    if let Some(&id) = self.ids.get(term) {
      return id;
    }
    let id = self.terms.len() as u32;
    self.ids.insert(term.to_string(), id);
    self.terms.push(term.to_string());
    id
  }

  pub fn get(&self, term: &str) -> Option<u32> {
    self.ids.get(term).copied()
  }

  pub fn term(&self, id: u32) -> Option<&str> {
    self.terms.get(id as usize).map(String::as_str)
  }

  pub fn len(&self) -> usize {
    self.terms.len()
  }

  pub fn is_empty(&self) -> bool {
    self.terms.is_empty()
  }

  pub fn terms(&self) -> &[String] {
    &self.terms
  }
}

/// Symmetric-window co-occurrence counts over interned token ids. Pairs are stored
/// once, canonically ordered (min, max), each event counted for BOTH directions'
/// marginals — the standard symmetric formulation.
#[derive(Default)]
pub struct CoocCounts {
  /// (min id, max id) → joint count. BTreeMap: sorted iteration = fixed reduction order.
  pairs: BTreeMap<(u32, u32), u64>,
  /// Per-id total occurrences as a window CENTER (== as a context, by symmetry).
  marginals: Vec<u64>,
  /// Total pair events (each unordered pair event once).
  total: u64,
}

impl CoocCounts {
  /// Count all symmetric-window pairs of one document. `window` is the max distance
  /// (a center pairs with up to `window` neighbors on each side, within the document).
  /// Self-pairs of the same POSITION never occur; the same TERM at two positions does.
  pub fn add_document(&mut self, ids: &[u32], window: usize) {
    for (center_pos, &center) in ids.iter().enumerate() {
      if self.marginals.len() <= center as usize {
        self.marginals.resize(center as usize + 1, 0);
      }
      let end = (center_pos + window + 1).min(ids.len());
      for &context in ids.get(center_pos + 1..end).unwrap_or(&[]) {
        let key = (center.min(context), center.max(context));
        *self.pairs.entry(key).or_insert(0) += 1;
        if self.marginals.len() <= context as usize {
          self.marginals.resize(context as usize + 1, 0);
        }
        // One unordered event feeds both terms' marginals and the grand total once.
        self.marginals[center as usize] += 1;
        self.marginals[context as usize] += 1;
        self.total += 1;
      }
    }
  }

  /// Bulk construction from pre-collected canonical pair events (each event already
  /// `(min, max)`): sort + run-length — the streaming path for corpora where per-pair
  /// BTreeMap insertion would thrash (event volume ≫ distinct pairs). Identical
  /// semantics to repeated [`CoocCounts::add_document`] windows producing the same
  /// events, which the tests pin.
  pub fn from_events(mut events: Vec<(u32, u32)>) -> CoocCounts {
    use rayon::prelude::*;
    events.par_sort_unstable();
    let mut counts = CoocCounts::default();
    let mut index = 0usize;
    while index < events.len() {
      let key = events[index];
      let mut run = 0u64;
      while index < events.len() && events[index] == key {
        run += 1;
        index += 1;
      }
      counts.pairs.insert(key, run);
      let max_id = key.1 as usize;
      if counts.marginals.len() <= max_id {
        counts.marginals.resize(max_id + 1, 0);
      }
      counts.marginals[key.0 as usize] += run;
      counts.marginals[key.1 as usize] += run;
      counts.total += run;
    }
    counts
  }

  pub fn total_events(&self) -> u64 {
    self.total
  }

  pub fn marginal(&self, id: u32) -> u64 {
    self.marginals.get(id as usize).copied().unwrap_or(0)
  }

  /// Sorted iteration over (min, max) → count.
  pub fn pairs(&self) -> impl Iterator<Item = (&(u32, u32), &u64)> {
    self.pairs.iter()
  }

  pub fn pair_count(&self) -> usize {
    self.pairs.len()
  }
}

/// Smoothed context mass: Σ count(x)^α over every id with a nonzero marginal.
pub(crate) fn smoothed_mass(marginals: &[u64]) -> Result<f64, String> {
  let mass: f64 = marginals
    .iter()
    .filter(|&&count| count > 0)
    .map(|&count| (count as f64).powf(CDS_ALPHA))
    .sum();
  if !(mass.is_finite() && mass > 0.0) {
    return Err(format!(
      "PPMI smoothing mass degenerate ({mass}) over {} marginals",
      marginals.len()
    ));
  }
  Ok(mass)
}

/// The symmetric cds-smoothed PMI of one pair — may be ≤ 0 (PPMI keeps only > 0).
/// ONE source of the formula for the in-memory and streaming paths, so the two can
/// never drift: `PMI = ½(ln(P(a,b)/(P(a)·P_α(b))) + ln(P(a,b)/(P(b)·P_α(a))))`.
pub(crate) fn symmetric_pmi(
  a: u32,
  b: u32,
  joint: u64,
  marginal_a: u64,
  marginal_b: u64,
  total: f64,
  mass: f64,
) -> Result<f64, String> {
  let joint_p = joint as f64 / total;
  let p_a = marginal_a as f64 / total;
  let p_b = marginal_b as f64 / total;
  let p_a_smooth = (marginal_a as f64).powf(CDS_ALPHA) / mass;
  let p_b_smooth = (marginal_b as f64).powf(CDS_ALPHA) / mass;
  if p_a <= 0.0 || p_b <= 0.0 || p_a_smooth <= 0.0 || p_b_smooth <= 0.0 {
    return Err(format!(
      "PPMI marginal degenerate for pair ({a}, {b}): joint {joint} without marginals"
    ));
  }
  let pmi_ab = (joint_p / (p_a * p_b_smooth)).ln();
  let pmi_ba = (joint_p / (p_b * p_a_smooth)).ln();
  let pmi = 0.5 * (pmi_ab + pmi_ba);
  if !pmi.is_finite() {
    return Err(format!("non-finite PMI for pair ({a}, {b})"));
  }
  Ok(pmi)
}

/// Streaming PPMI over ascending aggregated pairs — the external-count path's view of
/// the exact same formula as [`ppmi`]. Pull-based (an `Iterator`) so downstream stages
/// zip, size, and fill without materializing the triples.
pub struct PpmiStream<'a, I> {
  pairs: I,
  marginals: &'a [u64],
  total: f64,
  mass: f64,
}

/// Build a [`PpmiStream`]. Errors if the count set is empty — a model cannot be
/// trained from nothing, and the caller must state the lexical fallback instead.
pub fn ppmi_stream<'a, I>(
  pairs: I,
  marginals: &'a [u64],
  total_events: u64,
) -> Result<PpmiStream<'a, I>, String>
where
  I: Iterator<Item = Result<(u32, u32, u64), String>>,
{
  if total_events == 0 {
    return Err("PPMI over zero co-occurrence events (corpus too small to train)".to_string());
  }
  Ok(PpmiStream {
    pairs,
    marginals,
    total: total_events as f64,
    mass: smoothed_mass(marginals)?,
  })
}

impl<I> Iterator for PpmiStream<'_, I>
where
  I: Iterator<Item = Result<(u32, u32, u64), String>>,
{
  type Item = Result<(u32, u32, f64), String>;

  fn next(&mut self) -> Option<Self::Item> {
    loop {
      let (a, b, joint) = match self.pairs.next()? {
        Ok(record) => record,
        Err(error) => return Some(Err(error)),
      };
      let marginal = |id: u32| self.marginals.get(id as usize).copied().unwrap_or(0);
      match symmetric_pmi(a, b, joint, marginal(a), marginal(b), self.total, self.mass) {
        Ok(pmi) if pmi > 0.0 => return Some(Ok((a, b, pmi))),
        Ok(_) => continue,
        Err(error) => return Some(Err(error)),
      }
    }
  }
}

/// Positive PMI with context-distribution smoothing (TACL 2015, cds α = 0.75; no
/// shift): for the symmetric matrix M, `PPMI(w, c) = max(0, log( P(w,c) / (P(w) ·
/// P_α(c)) ))` where `P_α(c) = count(c)^α / Σ_x count(x)^α`. Symmetrized by evaluating
/// both (w, c) and (c, w) smoothing roles and averaging — the matrix stays symmetric,
/// which the SVD step's symmetric eigenvalue weighting requires. Returns the sparse
/// upper triangle in the counts' sorted order. Implemented over [`PpmiStream`] — one
/// formula source for both paths.
pub fn ppmi(counts: &CoocCounts) -> Result<Vec<(u32, u32, f64)>, String> {
  ppmi_stream(
    counts.pairs().map(|(&(a, b), &count)| Ok((a, b, count))),
    &counts.marginals,
    counts.total_events(),
  )?
  .collect()
}

#[cfg(test)]
mod tests {
  use super::*;

  fn intern_doc(vocab: &mut Vocab, words: &[&str]) -> Vec<u32> {
    words.iter().map(|w| vocab.intern(w)).collect()
  }

  #[test]
  fn window_pairs_and_marginals_hand_counted() {
    // Document: a b a, window 1 → events: (a,b) at 0-1, (b,a) at 1-2 — both stored as
    // the canonical (a,b) pair → joint(a,b) = 2, total = 2.
    // Marginals: each event credits both sides once → a: 2, b: 2.
    let mut vocab = Vocab::default();
    let ids = intern_doc(&mut vocab, &["a", "b", "a"]);
    let mut counts = CoocCounts::default();
    counts.add_document(&ids, 1);
    assert_eq!(counts.total_events(), 2);
    let pairs: Vec<_> = counts.pairs().map(|(&k, &v)| (k, v)).collect();
    assert_eq!(pairs, vec![((0, 1), 2)]);
    assert_eq!(counts.marginal(0), 2);
    assert_eq!(counts.marginal(1), 2);
  }

  #[test]
  fn window_two_reaches_two_positions() {
    // a b c, window 2 → (a,b), (a,c), (b,c): 3 events.
    let mut vocab = Vocab::default();
    let ids = intern_doc(&mut vocab, &["a", "b", "c"]);
    let mut counts = CoocCounts::default();
    counts.add_document(&ids, 2);
    assert_eq!(counts.total_events(), 3);
    assert_eq!(counts.pair_count(), 3);
  }

  #[test]
  fn documents_never_leak_windows_into_each_other() {
    let mut vocab = Vocab::default();
    let doc1 = intern_doc(&mut vocab, &["a", "b"]);
    let doc2 = intern_doc(&mut vocab, &["c", "d"]);
    let mut counts = CoocCounts::default();
    counts.add_document(&doc1, 4);
    counts.add_document(&doc2, 4);
    // No (b, c) pair across the boundary.
    assert_eq!(counts.pair_count(), 2);
    assert!(counts.pairs().all(|(&(x, y), _)| (x, y) == (0, 1) || (x, y) == (2, 3)));
  }

  #[test]
  fn ppmi_matches_a_hand_computed_value_exactly() {
    // Corpus: the single document [a, b], window 1.
    //   total = 1; joint(a,b) = 1; marginal(a) = marginal(b) = 1.
    //   P(a,b) = 1; P(a) = P(b) = 1.
    //   smoothed mass = 1^α + 1^α = 2; P_α(a) = P_α(b) = 1/2.
    //   PMI(a,b) = ln( 1 / (1 · 1/2) ) = ln 2 — both directions identical.
    let mut vocab = Vocab::default();
    let ids = intern_doc(&mut vocab, &["a", "b"]);
    let mut counts = CoocCounts::default();
    counts.add_document(&ids, 1);
    let rows = ppmi(&counts).unwrap();
    assert_eq!(rows.len(), 1);
    let (a, b, value) = rows[0];
    assert_eq!((a, b), (0, 1));
    assert!((value - 2f64.ln()).abs() < 1e-12, "got {value}, want ln 2");
  }

  #[test]
  fn ppmi_ranks_tight_association_above_a_frequent_context() {
    // Hand-checked corpus: 3×[a,b], 2×[a,c], 2×[b,c], 4×[c,d], window 1.
    //   total = 11; joints: ab 3, ac 2, bc 2, cd 4; marginals a 5, b 5, c 8, d 4.
    //   Smoothed mass = 2·5^.75 + 8^.75 + 4^.75 ≈ 14.273.
    //   PPMI(a,b) ≈ 0.9405 (both smoothing directions equal);
    //   PPMI(a,c) ≈ ½(0.1823 + 0.0647) ≈ 0.1235 — positive but far weaker: the cds
    //   denominator grows sublinearly for the frequent context c, the exact correction
    //   TACL 2015 measures.
    let mut vocab = Vocab::default();
    let mut counts = CoocCounts::default();
    for _ in 0..3 {
      let ids = intern_doc(&mut vocab, &["a", "b"]);
      counts.add_document(&ids, 1);
    }
    for _ in 0..2 {
      let ids = intern_doc(&mut vocab, &["a", "c"]);
      counts.add_document(&ids, 1);
      let ids = intern_doc(&mut vocab, &["b", "c"]);
      counts.add_document(&ids, 1);
    }
    for _ in 0..4 {
      let ids = intern_doc(&mut vocab, &["c", "d"]);
      counts.add_document(&ids, 1);
    }
    let rows = ppmi(&counts).unwrap();
    let value = |x: u32, y: u32| {
      rows
        .iter()
        .find(|&&(a, b, _)| (a, b) == (x.min(y), x.max(y)))
        .map(|&(_, _, v)| v)
    };
    let ab = value(0, 1).expect("(a,b) must be positive");
    let ac = value(0, 2).expect("(a,c) must be positive");
    assert!((ab - 0.9405).abs() < 5e-4, "hand-computed PPMI(a,b): got {ab}");
    assert!((ac - 0.1235).abs() < 5e-4, "hand-computed PPMI(a,c): got {ac}");
    assert!(ab > ac);
  }

  #[test]
  fn from_events_matches_add_document() {
    let mut vocab = Vocab::default();
    let ids = intern_doc(&mut vocab, &["a", "b", "a", "c"]);
    let mut incremental = CoocCounts::default();
    incremental.add_document(&ids, 2);

    // The same windows as explicit canonical events.
    let mut events = Vec::new();
    for (i, &center) in ids.iter().enumerate() {
      for &context in ids.get(i + 1..(i + 3).min(ids.len())).unwrap_or(&[]) {
        events.push((center.min(context), center.max(context)));
      }
    }
    let bulk = CoocCounts::from_events(events);
    assert_eq!(bulk.total_events(), incremental.total_events());
    assert_eq!(
      bulk.pairs().collect::<Vec<_>>(),
      incremental.pairs().collect::<Vec<_>>()
    );
    for id in 0..vocab.len() as u32 {
      assert_eq!(bulk.marginal(id), incremental.marginal(id));
    }
  }

  #[test]
  fn ppmi_of_nothing_is_a_typed_error() {
    let counts = CoocCounts::default();
    assert!(ppmi(&counts).is_err());
  }

  #[test]
  fn ppmi_is_deterministic_and_sorted() {
    let mut vocab = Vocab::default();
    let mut counts = CoocCounts::default();
    for doc in [&["x", "y", "z"][..], &["y", "z", "w"][..], &["z", "w", "x"][..]] {
      let ids = intern_doc(&mut vocab, doc);
      counts.add_document(&ids, 2);
    }
    let first = ppmi(&counts).unwrap();
    let second = ppmi(&counts).unwrap();
    assert_eq!(first.len(), second.len());
    for (a, b) in first.iter().zip(&second) {
      assert_eq!(a.0, b.0);
      assert_eq!(a.1, b.1);
      assert_eq!(a.2.to_bits(), b.2.to_bits());
    }
    assert!(first.windows(2).all(|w| w[0].0 < w[1].0 || (w[0].0 == w[1].0 && w[0].1 < w[1].1)));
  }
}
