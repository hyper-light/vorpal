//! ANN acceptance: exactness of the flat tier, recall of the accelerated tiers (always with
//! exact rerank), bit-identical deterministic builds, and persistence round-trips.

use std::path::PathBuf;

use vorpal_ann::{AnnConfig, AnnIndex, Embedder, LexicalEmbedder};

/// Local deterministic xorshift for fixtures.
struct Rng(u64);
impl Rng {
  fn next(&mut self) -> u64 {
    let mut x = self.0;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    self.0 = x;
    x.wrapping_mul(0x2545_F491_4F6C_DD1D)
  }
  fn unit(&mut self) -> f32 {
    (self.next() % 10_000) as f32 / 10_000.0 - 0.5
  }
}

type Rows = Vec<(u64, Vec<f32>)>;
type Queries = Vec<Vec<f32>>;

/// Clustered fixture: `clusters` centers with `per` noisy points each; queries near centers.
fn clustered(dim: usize, clusters: usize, per: usize, seed: u64) -> (Rows, Queries) {
  let mut rng = Rng(seed);
  let mut rows = Vec::new();
  let mut queries = Vec::new();
  for c in 0..clusters {
    let center: Vec<f32> = (0..dim).map(|_| rng.unit() * 4.0).collect();
    for p in 0..per {
      let noisy: Vec<f32> = center.iter().map(|x| x + rng.unit() * 0.3).collect();
      rows.push(((c * per + p) as u64, noisy));
    }
    queries.push(center.iter().map(|x| x + rng.unit() * 0.2).collect());
  }
  (rows, queries)
}

fn brute_force(rows: &[(u64, Vec<f32>)], query: &[f32], k: usize) -> Vec<u64> {
  fn norm(v: &[f32]) -> Vec<f32> {
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    v.iter().map(|x| if n > 0.0 { x / n } else { *x }).collect()
  }
  let q = norm(query);
  let mut all: Vec<(u64, f32)> = rows
    .iter()
    .map(|(id, v)| {
      let v = norm(v);
      let d: f32 = v.iter().zip(&q).map(|(a, b)| (a - b) * (a - b)).sum();
      (*id, d)
    })
    .collect();
  all.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
  all.truncate(k);
  all.into_iter().map(|(id, _)| id).collect()
}

fn recall_at_10(index: &AnnIndex, rows: &[(u64, Vec<f32>)], queries: &[Vec<f32>]) -> f32 {
  let mut hits = 0usize;
  let mut total = 0usize;
  for q in queries {
    let truth = brute_force(rows, q, 10);
    let got: Vec<u64> = index.search(q, 10).into_iter().map(|(id, _)| id).collect();
    hits += truth.iter().filter(|t| got.contains(t)).count();
    total += truth.len();
  }
  hits as f32 / total as f32
}

#[test]
fn flat_exact_matches_brute_force_exactly() {
  let (rows, queries) = clustered(24, 20, 30, 7);
  let index = AnnIndex::build(24, rows.clone(), None);
  assert_eq!(index.config(), AnnConfig::FlatExact);
  for q in &queries {
    let truth = brute_force(&rows, q, 5);
    let got: Vec<u64> = index.search(q, 5).into_iter().map(|(id, _)| id).collect();
    assert_eq!(got, truth);
  }
}

#[test]
fn quantized_tier_has_high_recall_on_dense_vectors() {
  // The quantized tier is opt-in (for dense/neural embeddings, where sign codes discriminate).
  let (rows, queries) = clustered(64, 80, 80, 11);
  let index = AnnIndex::build(64, rows.clone(), Some(AnnConfig::FlatQuantized));
  let recall = recall_at_10(&index, &rows, &queries);
  assert!(recall >= 0.9, "quantized recall@10 = {recall}");
}

#[test]
fn vamana_tier_has_high_recall() {
  let (rows, queries) = clustered(32, 40, 20, 13); // 800 rows, tier forced
  let index = AnnIndex::build(32, rows.clone(), Some(AnnConfig::Vamana));
  let recall = recall_at_10(&index, &rows, &queries);
  assert!(recall >= 0.9, "vamana recall@10 = {recall}");
}

#[test]
fn builds_are_bit_identical() {
  let (rows, _) = clustered(16, 10, 20, 17);
  let dir = std::env::temp_dir().join(format!("vorpal-ann-det-{}", std::process::id()));
  let _ = std::fs::create_dir_all(&dir);
  let a_path: PathBuf = dir.join("a.bin");
  let b_path: PathBuf = dir.join("b.bin");
  AnnIndex::build(16, rows.clone(), Some(AnnConfig::Vamana))
    .save(&a_path)
    .unwrap();
  AnnIndex::build(16, rows, Some(AnnConfig::Vamana))
    .save(&b_path)
    .unwrap();
  assert_eq!(
    std::fs::read(&a_path).unwrap(),
    std::fs::read(&b_path).unwrap(),
    "same input must build the same index, byte for byte"
  );
  let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn persistence_round_trips_search_results() {
  let (rows, queries) = clustered(32, 15, 25, 19);
  let index = AnnIndex::build(32, rows, None);
  let dir = std::env::temp_dir().join(format!("vorpal-ann-rt-{}", std::process::id()));
  let _ = std::fs::create_dir_all(&dir);
  let path = dir.join("ann.bin");
  index.save(&path).unwrap();
  let loaded = AnnIndex::load(&path).unwrap();
  for q in &queries {
    assert_eq!(index.search(q, 10), loaded.search(q, 10));
  }
  let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn adaptive_config_scales_with_n() {
  assert_eq!(AnnConfig::for_n(0), AnnConfig::FlatExact);
  assert_eq!(AnnConfig::for_n(65_536), AnnConfig::FlatExact);
  assert_eq!(AnnConfig::for_n(65_537), AnnConfig::Vamana);
}

#[test]
fn empty_and_lexical_end_to_end() {
  let index = AnnIndex::build(16, Vec::new(), None);
  assert!(index.search(&[1.0; 16], 5).is_empty());

  // Lexical embedder → index → search: identifier queries find their definitions.
  let embedder = LexicalEmbedder::default();
  let names = [
    "resolve_import_path table file",
    "hamming distance quantizer",
    "greedy beam search vamana",
  ];
  let rows: Vec<(u64, Vec<f32>)> = names
    .iter()
    .enumerate()
    .map(|(i, n)| (i as u64, embedder.embed(n)))
    .collect();
  let index = AnnIndex::build(embedder.dim(), rows, None);
  let hits = index.search(&embedder.embed("import path resolution"), 1);
  assert_eq!(hits[0].0, 0, "{hits:?}");
}
