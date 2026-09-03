//! `cargo xtask searcheval` — the graded retrieval-quality harness (semantic-tier plan,
//! Stage 0). Where `cargo xtask eval` measures agent-task efficiency over THIS repo,
//! searcheval measures RANKING quality on an **arbitrary index** against a hand-graded
//! labels file: per-class NDCG@10 / MRR / recall@5, a per-query rank table, a double-run
//! determinism gate, and (`--overlap`) the tier-vs-exact top-10 agreement figure the ANN
//! owner tracks (docs/wip/BENCHMARKS.md records 66/80 at kernel scale).
//!
//! Labels are data, not code: `xtask/labels/*.json` ship graded sets for this repo,
//! cpython, and the Linux kernel (each with a `<set>.evidence.md` sidecar citing the
//! source line that proves every grade-3 answer). Every labelled name is
//! EXISTENCE-CHECKED against the index before anything is scored — a renamed symbol fails
//! the run loudly instead of silently scoring as a retrieval miss. A label's optional
//! `path` disambiguates duplicate names: a plain suffix (`ends_with`) or, with a leading
//! `/`, a tree-root-anchored path (needs `--root <tree>`; see [`path_matches`]).
//!
//! Metrics are the standard IR definitions, no house variants:
//! * NDCG@10 — graded gains (2^grade − 1), log2(rank+2) discount, ideal DCG from the
//!   query's own grade multiset.
//! * MRR — reciprocal rank of the first grade ≥ 2 hit.
//! * recall@5 — fraction of a query's grade ≥ 2 labels surfaced in the top 5.
//!
//! The index is measured AS IT IS: absent/stale warm tiers are reported (and the numbers
//! then describe the exact fallback), never built behind the caller's back — warm first
//! with `vorpal-index __warm-ann <dir>` when the tier is the thing under test.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use vorpal_index::{SearchFilter, Searcher, records::SearchHitRecord};
use vorpal_kg::{Kg, NodeId};

#[derive(Deserialize)]
struct LabelsFile {
  /// Human note about the set (provenance, grading date); printed, never interpreted.
  #[serde(default)]
  description: String,
  queries: Vec<LabeledQuery>,
}

#[derive(Deserialize)]
struct LabeledQuery {
  class: String,
  query: String,
  relevant: Vec<Relevant>,
}

#[derive(Deserialize)]
struct Relevant {
  name: String,
  /// Optional path disambiguator for corpora where one name has many definitions
  /// (kernel static functions). A plain value is matched against the hit's `path` with
  /// `ends_with`; a value starting with `/` is ANCHORED at the indexed tree's root
  /// (tree-relative equality, `--root` required) — the only form that can single out
  /// `lib/rbtree.c` from its `tools/lib/rbtree.c` mirror.
  #[serde(default)]
  path: Option<String>,
  /// 1 = marginally relevant, 2 = relevant, 3 = the definitive answer.
  grade: u8,
}

#[derive(Default)]
struct ClassAgg {
  queries: usize,
  ndcg: f64,
  mrr: f64,
  recall: f64,
}

pub fn run(index: &Path, labels_path: &Path, overlap: bool, root: Option<&Path>) -> Result<()> {
  let raw = std::fs::read_to_string(labels_path)
    .with_context(|| format!("reading {}", labels_path.display()))?;
  let labels: LabelsFile =
    serde_json::from_str(&raw).with_context(|| format!("parsing {}", labels_path.display()))?;
  validate(&labels)?;
  // The index stores paths under the CANONICAL root spelling (the build canonicalizes its
  // src), so an anchored label must be resolved against the same spelling.
  let root: Option<String> = match root {
    Some(r) => Some(
      r.canonicalize()
        .with_context(|| format!("canonicalizing --root {}", r.display()))?
        .to_string_lossy()
        .into_owned(),
    ),
    None => None,
  };
  if root.is_none() && labels.queries.iter().flat_map(|q| &q.relevant).any(is_anchored) {
    bail!("labels use an anchored path (starts with '/'); pass --root <indexed tree>");
  }

  let searcher =
    Searcher::open(index).map_err(|e| anyhow::anyhow!("opening {}: {e}", index.display()))?;
  let (ann_fresh, postings_fresh) = searcher.tiers();
  println!(
    "index {} — ann tier {}, postings tier {}",
    index.display(),
    if ann_fresh { "fresh" } else { "ABSENT/STALE (semantic channel = exact fallback)" },
    if postings_fresh { "fresh" } else { "ABSENT/STALE (name channel = full scan)" },
  );
  match searcher.dense_status() {
    Some((rows, on)) => println!(
      "dense sidecar: {rows} rows, channel {}",
      if on { "ON" } else { "OFF (override)" }
    ),
    None => println!("dense sidecar: none for this encoder/generation"),
  }
  if !labels.description.is_empty() {
    println!("labels: {}", labels.description);
  }
  if !ann_fresh || !postings_fresh {
    println!("(warm first with `vorpal-index __warm-ann <dir>` to measure the tiers)");
  }

  check_labels_exist(index, &labels)?;

  let mut per_class: BTreeMap<&str, ClassAgg> = BTreeMap::new();
  let mut total_us: u128 = 0;
  let mut max_us: u128 = 0;
  println!("\n-- per query (rank of best top-grade label, 0-based; None = not in top-25) --");
  for q in &labels.queries {
    let started = Instant::now();
    let hits = search(&searcher, &q.query, 25)?;
    let us = started.elapsed().as_micros();
    total_us += us;
    max_us = max_us.max(us);

    // Determinism gate: the repo contract says the same query answers identically.
    let again = search(&searcher, &q.query, 25)?;
    let names = |hs: &[SearchHitRecord]| hs.iter().map(|h| h.node.name.clone()).collect::<Vec<_>>();
    if names(&hits) != names(&again) {
      bail!("non-deterministic ranking for {:?} (double-run mismatch)", q.query);
    }

    let matches = match_labels(
      hits.iter().map(|h| (h.node.name.as_str(), h.node.path.as_str())),
      &q.relevant,
      root.as_deref(),
    );
    let ndcg = ndcg_at(&matches, &q.relevant, 10);
    let mrr = matches
      .iter()
      .filter(|(_, grade)| *grade >= 2)
      .map(|(rank, _)| *rank)
      .min()
      .map(|rank| 1.0 / (rank + 1) as f64)
      .unwrap_or(0.0);
    let relevant2 = q.relevant.iter().filter(|r| r.grade >= 2).count();
    let hit2_in5 = matches.iter().filter(|(rank, grade)| *rank < 5 && *grade >= 2).count();
    let recall = hit2_in5 as f64 / relevant2 as f64;

    let top_grade = q.relevant.iter().map(|r| r.grade).max().unwrap_or(0);
    let best_top = matches
      .iter()
      .filter(|(_, grade)| *grade == top_grade)
      .map(|(rank, _)| *rank)
      .min();
    println!(
      "rank[{}] {:?} -> {:?}  ndcg@10 {:.3}  mrr {:.3}  recall@5 {:.2}",
      q.class, q.query, best_top, ndcg, mrr, recall
    );
    // `VORPAL_SEARCHEVAL_CHANNELS=1`: per-channel provenance of every labelled hit
    // in the top-25 — which channel surfaced it and at what rank (the
    // candidate-generation question the dense channel exists to answer).
    if std::env::var_os("VORPAL_SEARCHEVAL_CHANNELS").is_some() {
      for (rank, grade) in &matches {
        let hit = &hits[*rank];
        let provenance: Vec<String> = hit
          .channels
          .iter()
          .map(|c| format!("{}#{}", c.channel, c.rank))
          .collect();
        println!(
          "    label {:?} (grade {grade}) fused#{}  channels [{}]",
          hit.node.name,
          rank + 1,
          provenance.join(", ")
        );
      }
    }

    let agg = per_class.entry(q.class.as_str()).or_default();
    agg.queries += 1;
    agg.ndcg += ndcg;
    agg.mrr += mrr;
    agg.recall += recall;
  }

  println!("\n| class | queries | NDCG@10 | MRR | recall@5 |");
  println!("|---|---:|---:|---:|---:|");
  let mut overall = ClassAgg::default();
  for (class, agg) in &per_class {
    let n = agg.queries as f64;
    println!(
      "| {class} | {} | {:.3} | {:.3} | {:.3} |",
      agg.queries,
      agg.ndcg / n,
      agg.mrr / n,
      agg.recall / n
    );
    overall.queries += agg.queries;
    overall.ndcg += agg.ndcg;
    overall.mrr += agg.mrr;
    overall.recall += agg.recall;
  }
  let n = overall.queries as f64;
  println!(
    "| **all** | **{}** | **{:.3}** | **{:.3}** | **{:.3}** |",
    overall.queries,
    overall.ndcg / n,
    overall.mrr / n,
    overall.recall / n
  );
  println!(
    "\nquery latency: mean {} µs, max {} µs (k=25, {} path)",
    total_us / labels.queries.len() as u128,
    max_us,
    if ann_fresh { "tier" } else { "exact-fallback" },
  );

  if overlap {
    run_overlap(index, &searcher, ann_fresh, &labels)?;
  }
  Ok(())
}

/// Tier-vs-exact agreement: the same queries through the as-is searcher and through
/// [`Searcher::open_exact`] (every approximate tier refused), compared over the top 10 —
/// positional agreement (the BENCHMARKS 66/80 figure) and set agreement (misses that are
/// pure reordering vs candidates the beam never surfaced).
fn run_overlap(index: &Path, tier: &Searcher, ann_fresh: bool, labels: &LabelsFile) -> Result<()> {
  if !ann_fresh {
    println!("\noverlap: skipped — the ann tier is absent/stale, both paths are already exact");
    return Ok(());
  }
  let exact = Searcher::open_exact(index)
    .map_err(|e| anyhow::anyhow!("opening exact reference on {}: {e}", index.display()))?;
  println!("\n-- tier vs exact reference (top-10) --");
  let (mut pos_agree, mut set_agree, mut slots) = (0usize, 0usize, 0usize);
  let mut exact_us: u128 = 0;
  for q in &labels.queries {
    let t: Vec<String> =
      search(tier, &q.query, 10)?.into_iter().map(|h| h.node.name).collect();
    let started = Instant::now();
    let e: Vec<String> =
      search(&exact, &q.query, 10)?.into_iter().map(|h| h.node.name).collect();
    exact_us += started.elapsed().as_micros();
    let n = t.len().min(e.len());
    let pos = t.iter().zip(&e).filter(|(a, b)| a == b).count();
    let set = t.iter().filter(|name| e.contains(name)).count();
    pos_agree += pos;
    set_agree += set;
    slots += n;
    println!("overlap[{}] {:?}: {pos}/{n} positions, {set}/{n} set", q.class, q.query);
  }
  println!(
    "overlap total: {pos_agree}/{slots} positions, {set_agree}/{slots} set; exact path mean {} ms",
    exact_us / 1000 / labels.queries.len() as u128
  );
  Ok(())
}

fn search(searcher: &Searcher, query: &str, k: usize) -> Result<Vec<SearchHitRecord>> {
  searcher
    .records(query, k, &SearchFilter::default())
    .map_err(|e| anyhow::anyhow!("search {query:?}: {e}"))
}

fn is_anchored(r: &Relevant) -> bool {
  r.path.as_deref().is_some_and(|p| p.starts_with('/'))
}

/// The label-path rule: no path = any definition of the name; a plain path = `ends_with`
/// on the hit's (canonical, absolute) path; an anchored path (`/…`) = equality with the
/// hit's tree-relative path under `root`. An anchored label with no root never matches
/// (`run` refuses that configuration up front, so it is unreachable there).
fn path_matches(hit_path: &str, label_path: Option<&str>, root: Option<&str>) -> bool {
  match label_path {
    None => true,
    Some(anchored) if anchored.starts_with('/') => root.is_some_and(|root| {
      let rel = vorpal_kg::identity::tree_relative(hit_path, root);
      rel != hit_path && anchored[1..] == *rel
    }),
    Some(suffix) => hit_path.ends_with(suffix),
  }
}

/// Greedy rank-order label consumption: each hit takes the first unconsumed label it
/// matches (name equality + the path rule), each label scores at its best rank only —
/// duplicate definitions of a labelled name never double-count.
fn match_labels<'a>(
  hits: impl Iterator<Item = (&'a str, &'a str)>,
  relevant: &[Relevant],
  root: Option<&str>,
) -> Vec<(usize, u8)> {
  let mut consumed = vec![false; relevant.len()];
  let mut matches = Vec::new();
  for (rank, (name, path)) in hits.enumerate() {
    let found = relevant.iter().enumerate().position(|(i, r)| {
      !consumed[i] && r.name == name && path_matches(path, r.path.as_deref(), root)
    });
    if let Some(i) = found {
      consumed[i] = true;
      matches.push((rank, relevant[i].grade));
    }
  }
  matches
}

fn ndcg_at(matches: &[(usize, u8)], relevant: &[Relevant], k: usize) -> f64 {
  let gain = |grade: u8| ((1u32 << grade) - 1) as f64;
  let discount = |rank: usize| 1.0 / ((rank + 2) as f64).log2();
  let dcg: f64 = matches
    .iter()
    .filter(|(rank, _)| *rank < k)
    .map(|(rank, grade)| gain(*grade) * discount(*rank))
    .sum();
  let mut grades: Vec<u8> = relevant.iter().map(|r| r.grade).collect();
  grades.sort_unstable_by(|a, b| b.cmp(a));
  let idcg: f64 = grades
    .iter()
    .take(k)
    .enumerate()
    .map(|(rank, grade)| gain(*grade) * discount(rank))
    .sum();
  // validate() guarantees ≥1 label with grade ≥ 2, so idcg > 0. The `== 0.0` arm also
  // normalizes IEEE −0.0 (an empty match sum) so a total miss prints as 0.000.
  if dcg == 0.0 { 0.0 } else { dcg / idcg }
}

fn validate(labels: &LabelsFile) -> Result<()> {
  if labels.queries.is_empty() {
    bail!("labels file has no queries");
  }
  for q in &labels.queries {
    if q.class.is_empty() {
      bail!("query {:?} has an empty class", q.query);
    }
    if q.relevant.is_empty() {
      bail!("query {:?} has no relevant labels", q.query);
    }
    if q.relevant.iter().all(|r| r.grade < 2) {
      bail!("query {:?} has no grade ≥ 2 label — MRR/recall would be vacuous", q.query);
    }
    for r in &q.relevant {
      if !(1..=3).contains(&r.grade) {
        bail!("query {:?}: label {:?} grade {} outside 1..=3", q.query, r.name, r.grade);
      }
    }
    for (i, a) in q.relevant.iter().enumerate() {
      if q.relevant[..i].iter().any(|b| a.name == b.name && a.path == b.path) {
        bail!("query {:?}: duplicate label {:?}", q.query, a.name);
      }
    }
  }
  Ok(())
}

/// One full node-segment pass proving every labelled NAME exists in the index — stale
/// labels (a refactor renamed the symbol) fail the run before any metric is computed.
/// Existence is name-level; `path` suffixes only disambiguate at match time.
fn check_labels_exist(index: &Path, labels: &LabelsFile) -> Result<()> {
  let kg = Kg::load(index).map_err(|e| anyhow::anyhow!("loading kg: {e}"))?;
  let mut needed: BTreeMap<&str, bool> = labels
    .queries
    .iter()
    .flat_map(|q| q.relevant.iter().map(|r| (r.name.as_str(), false)))
    .collect();
  let mut remaining = needed.len();
  for id in 0..kg.node_count() as u64 {
    if remaining == 0 {
      break;
    }
    if let Some(view) = kg.node(NodeId::new(id))
      && let Some(seen) = needed.get_mut(view.name)
      && !*seen
    {
      *seen = true;
      remaining -= 1;
    }
  }
  let missing: Vec<&str> =
    needed.iter().filter(|(_, seen)| !**seen).map(|(name, _)| *name).collect();
  if !missing.is_empty() {
    bail!(
      "labelled names not in this index (stale labels or wrong index): {}",
      missing.join(", ")
    );
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  fn label(name: &str, path: Option<&str>, grade: u8) -> Relevant {
    Relevant { name: name.into(), path: path.map(str::to_owned), grade }
  }

  #[test]
  fn plain_path_is_a_suffix_and_anchored_path_is_tree_relative_equality() {
    let root = "/src/linux";
    let lib = "/src/linux/lib/rbtree.c";
    let tools = "/src/linux/tools/lib/rbtree.c";
    // No path: every definition of the name qualifies.
    assert!(path_matches(lib, None, Some(root)));
    // Plain suffix: BOTH the tree file and its tools/ mirror end with it — the
    // ambiguity the anchored form exists to remove.
    assert!(path_matches(lib, Some("lib/rbtree.c"), Some(root)));
    assert!(path_matches(tools, Some("lib/rbtree.c"), Some(root)));
    assert!(path_matches(tools, Some("/lib/rbtree.c".trim_start_matches('/')), None));
    // Anchored: only the tree-relative equal path.
    assert!(path_matches(lib, Some("/lib/rbtree.c"), Some(root)));
    assert!(!path_matches(tools, Some("/lib/rbtree.c"), Some(root)));
    // Anchored without a root never matches (run() refuses this up front).
    assert!(!path_matches(lib, Some("/lib/rbtree.c"), None));
    // A hit outside the root is never an anchored match, even when its tail agrees.
    assert!(!path_matches("/elsewhere/lib/rbtree.c", Some("/lib/rbtree.c"), Some(root)));
  }

  #[test]
  fn match_labels_consumes_each_label_once_at_its_best_rank() {
    let relevant = vec![
      label("rb_erase", Some("/lib/rbtree.c"), 3),
      label("rb_erase_cached", None, 2),
    ];
    // tools/ mirror ranks first but is not the anchored definition; the real one is
    // rank 2; a second copy of the real one at rank 3 must not double-count.
    let hits = [
      ("rb_erase", "/src/linux/tools/lib/rbtree.c"),
      ("rb_first", "/src/linux/lib/rbtree.c"),
      ("rb_erase", "/src/linux/lib/rbtree.c"),
      ("rb_erase", "/src/linux/lib/rbtree.c"),
      ("rb_erase_cached", "/src/linux/include/linux/rbtree.h"),
    ];
    let matches = match_labels(hits.iter().copied(), &relevant, Some("/src/linux"));
    assert_eq!(matches, vec![(2, 3), (4, 2)]);
    // Without a root the anchored label can never match; the plain one still does.
    let matches = match_labels(hits.iter().copied(), &relevant, None);
    assert_eq!(matches, vec![(4, 2)]);
  }

  #[test]
  fn ndcg_is_one_for_the_ideal_ranking_and_zero_for_a_miss() {
    let relevant = vec![label("a", None, 3), label("b", None, 2), label("c", None, 1)];
    assert!((ndcg_at(&[(0, 3), (1, 2), (2, 1)], &relevant, 10) - 1.0).abs() < 1e-12);
    assert_eq!(ndcg_at(&[], &relevant, 10), 0.0);
    // Reversed order is strictly worse than ideal.
    assert!(ndcg_at(&[(0, 1), (1, 2), (2, 3)], &relevant, 10) < 1.0);
  }

  #[test]
  fn validate_rejects_vacuous_and_duplicate_labels() {
    let file = |relevant: Vec<Relevant>| LabelsFile {
      description: String::new(),
      queries: vec![LabeledQuery { class: "exact".into(), query: "q".into(), relevant }],
    };
    assert!(validate(&file(vec![label("a", None, 1)])).is_err());
    assert!(validate(&file(vec![label("a", None, 3), label("a", None, 2)])).is_err());
    assert!(validate(&file(vec![label("a", None, 3), label("a", Some("x.c"), 2)])).is_ok());
    assert!(validate(&file(vec![label("a", None, 4)])).is_err());
  }
}
