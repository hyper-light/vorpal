//! The persisted lexical tier (IMPROVEMENTS #9): a token → node-id posting index, so the
//! name channel stops scanning and tokenizing every node per query.
//!
//! Same architecture as the ANN tier: **generation-stamped, warm-built, fallback-correct**.
//! The file lives beside `ann.bin` in the generation, its header carries the node-segment
//! stamp, and a query uses it only when the stamp matches the loaded graph — anything else
//! (missing, torn, foreign, stale) routes the query to the exhaustive scan, which is always
//! correct, while a background warm heals the tier.
//!
//! Recall contract: the scan's three name tiers (exact string, token-equal, token-subset)
//! all require the candidate's name tokens to be a **superset** of the query tokens, so the
//! intersection of the query tokens' posting lists contains every scan hit; the searcher
//! then classifies only those candidates. Queries with no tokens fall back to the scan.
//!
//! Layout (`postings.bin`), all little-endian, deterministic (tokens sorted bytewise, ids
//! ascending). v2 (semantic-tier Stage 4) adds what BM25 needs and nothing else — a
//! saturating u8 term frequency per posting and a dense doc-length section — because
//! length normalization is the component that discriminates 2–6-token names, and
//! recomputing it would re-instate the string scan this tier exists to remove:
//!   [VPST][version u32 = 2][stamp u64][node_count u64][doc_count u64][avgdl f64][token_count u64]
//!   doc lengths: node_count × u8 (name token count pre-dedup, saturating; 0 = no tokens)
//!   token_count × { token_len u16, token bytes, postings_offset u64, postings_count u64 }
//!   postings pool: (node id u32, tf u8) — 5 bytes per posting, ids ascending per token
//!
//! Node ids are u32 here (dense per-generation locators; the 32-bit ceiling is IMPROVEMENTS
//! #13's explicit boundary, same as the evidence sidecar).

use std::collections::HashMap;
use std::fs;
use std::io::Write as _;
use std::path::Path;

use vorpal_ann::tokenize;
use vorpal_kg::{Kg, NodeId};

const MAGIC: &[u8; 4] = b"VPST";
const VERSION: u32 = 2;

/// Okapi BM25 parameters — Robertson's standard values, identical to Lucene's
/// defaults; CITED constants recorded in the format doc, not tuned here.
const BM25_K1: f32 = 1.2;
const BM25_B: f32 = 0.75;

/// Lucene's nonnegative IDF: ln(1 + (N − df + 0.5)/(df + 0.5)) — never negative, so
/// a common token can dilute but not penalize (the short-query supremacy invariant).
fn bm25_idf(doc_count: u64, df: u64) -> f32 {
  (1.0 + (doc_count as f64 - df as f64 + 0.5) / (df as f64 + 0.5)).ln() as f32
}

/// The BM25 term component: tf·(k1+1) / (tf + k1·(1 − b + b·dl/avgdl)).
fn bm25_term(tf: f32, dl: f32, avgdl: f32) -> f32 {
  tf * (BM25_K1 + 1.0) / (tf + BM25_K1 * (1.0 - BM25_B + BM25_B * dl / avgdl))
}

/// Build and atomically persist the posting index for `kg` into `dir`, stamped with
/// `base_stamp` (the node-segment hash of the graph the tokens came from).
pub fn build_postings(kg: &Kg, dir: &Path, base_stamp: u64) -> std::io::Result<()> {
  vorpal_kg::phase_stamp("postings: build start");
  let node_count = kg.node_count();
  // token → ascending (id, tf) postings. Iterating ids ascending keeps each list
  // sorted for free; tf is the PRE-dedup occurrence count (BM25's term frequency),
  // saturating u8 — names are short identifier surfaces.
  let mut lists: HashMap<String, Vec<(u32, u8)>> = HashMap::new();
  let mut lengths = vec![0u8; node_count];
  let mut doc_count = 0u64;
  let mut length_sum = 0u64;
  for id in 0..node_count as u64 {
    let Some(view) = kg.node(NodeId::new(id)) else {
      continue;
    };
    let mut tokens = tokenize(view.name);
    if tokens.is_empty() {
      continue;
    }
    doc_count += 1;
    let length = tokens.len().min(u8::MAX as usize) as u8;
    lengths[id as usize] = length;
    length_sum += length as u64;
    tokens.sort_unstable();
    let mut index = 0;
    while index < tokens.len() {
      let mut run = 1;
      while index + run < tokens.len() && tokens[index + run] == tokens[index] {
        run += 1;
      }
      let tf = run.min(u8::MAX as usize) as u8;
      // Safe take: `run` was computed before the slot is emptied, and the outer loop
      // never revisits it.
      lists
        .entry(std::mem::take(&mut tokens[index]))
        .or_default()
        .push((id as u32, tf));
      index += run;
    }
  }
  // avgdl over the participating docs, in one deterministic integer sum.
  let avgdl = if doc_count > 0 {
    length_sum as f64 / doc_count as f64
  } else {
    0.0
  };
  let mut tokens: Vec<(String, Vec<(u32, u8)>)> = lists.into_iter().collect();
  tokens.sort_unstable_by(|a, b| a.0.cmp(&b.0));

  let mut header = Vec::with_capacity(tokens.len() * 24);
  let mut pool: Vec<u8> = Vec::new();
  for (token, postings) in &tokens {
    let bytes = token.as_bytes();
    header.extend_from_slice(&(bytes.len() as u16).to_le_bytes());
    header.extend_from_slice(bytes);
    header.extend_from_slice(&((pool.len() / 5) as u64).to_le_bytes());
    header.extend_from_slice(&(postings.len() as u64).to_le_bytes());
    for &(id, tf) in postings {
      pool.extend_from_slice(&id.to_le_bytes());
      pool.push(tf);
    }
  }

  let tmp = dir.join("postings.bin.tmp");
  let path = dir.join("postings.bin");
  {
    let mut out = std::io::BufWriter::new(fs::File::create(&tmp)?);
    out.write_all(MAGIC)?;
    out.write_all(&VERSION.to_le_bytes())?;
    out.write_all(&base_stamp.to_le_bytes())?;
    out.write_all(&(node_count as u64).to_le_bytes())?;
    out.write_all(&doc_count.to_le_bytes())?;
    out.write_all(&avgdl.to_le_bytes())?;
    out.write_all(&(tokens.len() as u64).to_le_bytes())?;
    out.write_all(&lengths)?;
    out.write_all(&header)?;
    out.write_all(&pool)?;
    out.flush()?;
  }
  fs::rename(&tmp, &path)?;
  vorpal_kg::phase_stamp("postings: build done");
  Ok(())
}

/// A loaded posting index. Small wrapper over the decoded token table; the ids pool stays
/// one flat buffer.
pub struct Postings {
  stamp: u64,
  /// token → (offset into `pool` in 5-byte posting units, posting count).
  tokens: HashMap<String, (u64, u64)>,
  pool: Vec<u8>,
  /// Dense per-node name token count (0 = no tokens; saturating u8).
  lengths: Vec<u8>,
  /// Participating docs (nodes with ≥1 name token) — BM25's N.
  doc_count: u64,
  /// Mean doc length over participating docs, cast once from the builder's f64.
  avgdl: f32,
}

impl Postings {
  /// Load `dir`'s posting index, if present and structurally sound. `None` is always safe:
  /// callers fall back to the scan.
  pub fn load(dir: &Path) -> Option<Postings> {
    let bytes = fs::read(dir.join("postings.bin")).ok()?;
    if bytes.len() < 48 || &bytes[0..4] != MAGIC {
      return None;
    }
    if u32::from_le_bytes(bytes[4..8].try_into().ok()?) != VERSION {
      return None;
    }
    let stamp = u64::from_le_bytes(bytes[8..16].try_into().ok()?);
    let node_count = u64::from_le_bytes(bytes[16..24].try_into().ok()?) as usize;
    let doc_count = u64::from_le_bytes(bytes[24..32].try_into().ok()?);
    let avgdl = f64::from_le_bytes(bytes[32..40].try_into().ok()?) as f32;
    let token_count = u64::from_le_bytes(bytes[40..48].try_into().ok()?) as usize;
    // A token entry encodes to at least 18 bytes (u16 length + empty token + two
    // u64s): a count past bytes/18 is structurally impossible — refuse it instead of
    // letting `with_capacity` panic on hostile or foreign bytes (no-panics law).
    if token_count > bytes.len() / 18 {
      return None;
    }
    let lengths = bytes.get(48..48usize.checked_add(node_count)?)?.to_vec();
    let mut tokens = HashMap::with_capacity(token_count);
    let mut at = 48usize.checked_add(node_count)?;
    for _ in 0..token_count {
      let len = u16::from_le_bytes(bytes.get(at..at + 2)?.try_into().ok()?) as usize;
      at += 2;
      let token = std::str::from_utf8(bytes.get(at..at + len)?).ok()?.to_string();
      at += len;
      let offset = u64::from_le_bytes(bytes.get(at..at + 8)?.try_into().ok()?);
      at += 8;
      let postings_len = u64::from_le_bytes(bytes.get(at..at + 8)?.try_into().ok()?);
      at += 8;
      tokens.insert(token, (offset, postings_len));
    }
    let pool = bytes.get(at..)?.to_vec();
    Some(Postings {
      stamp,
      tokens,
      pool,
      lengths,
      doc_count,
      avgdl,
    })
  }

  /// The node-segment stamp this index was built from.
  pub fn stamp(&self) -> u64 {
    self.stamp
  }

  fn list(&self, token: &str) -> Option<&[u8]> {
    let &(offset, len) = self.tokens.get(token)?;
    let start = (offset as usize).checked_mul(5)?;
    let end = start.checked_add((len as usize).checked_mul(5)?)?;
    self.pool.get(start..end)
  }

  /// Node ids whose names contain **all** of `query_tokens` (ascending). Empty input yields
  /// `None` (no lexical evidence — caller must scan), as does any token with no postings
  /// (the intersection is provably empty, returned as an empty vec). AND semantics —
  /// untouched by BM25, which unions.
  pub fn candidates(&self, query_tokens: &[String]) -> Option<Vec<u32>> {
    if query_tokens.is_empty() {
      return None;
    }
    // Intersect starting from the shortest list.
    let mut lists: Vec<&[u8]> = Vec::with_capacity(query_tokens.len());
    for token in query_tokens {
      match self.list(token) {
        Some(list) => lists.push(list),
        None => return Some(Vec::new()),
      }
    }
    lists.sort_by_key(|list| list.len());
    let decode = |list: &[u8]| -> Vec<u32> {
      list
        .chunks_exact(5)
        .map(|record| u32::from_le_bytes([record[0], record[1], record[2], record[3]]))
        .collect()
    };
    let mut out: Vec<u32> = decode(lists[0]);
    for list in &lists[1..] {
      let ids = decode(list);
      out.retain(|id| ids.binary_search(id).is_ok());
      if out.is_empty() {
        break;
      }
    }
    Some(out)
  }

  /// The BM25 channel: rank every ADMITTED node matching ANY query token (union —
  /// partial matches score; `candidates`' AND semantics stay the name channel's),
  /// truncated to `pool`. `admit` is the search filter — filtered rows never appear
  /// (every channel pre-filters, so support/eliminator semantics stay exact), while
  /// df/IDF remain COLLECTION statistics (the standard filtered-BM25 convention).
  /// Scores sum in SORTED-token order (deduped — set semantics, matching the name
  /// channel), so the bytes are a pure function of the index, query, and filter;
  /// rank order is (score desc, id asc) under `total_cmp`. `None` = no lexical
  /// evidence possible (no tokens, or a degenerate index) — the caller may compute
  /// the identical ranking exhaustively.
  pub fn bm25_ranked(
    &self,
    query_tokens: &[String],
    pool: usize,
    admit: impl Fn(u32) -> bool,
  ) -> Option<Vec<u64>> {
    if query_tokens.is_empty() || self.avgdl <= 0.0 || pool == 0 {
      return None;
    }
    let mut sorted: Vec<&String> = query_tokens.iter().collect();
    sorted.sort_unstable();
    sorted.dedup();
    // The match floor: multi-token queries admit only rows matching ≥2 DISTINCT
    // tokens — the smallest non-trivial partial match, a structural cut, not a tuned
    // depth. Measured reason (kernel scale): RRF is scale-free, so a rank list built
    // from 1-of-n literal-token matches at huge df hands its arbitrary top real
    // fusion mass and re-injects exactly the lexical pollution the learned tier
    // fixed (short-keyword NDCG 0.206 → 0.109 before this floor).
    let required = sorted.len().min(2);
    let mut scores: HashMap<u32, (f32, u8)> = HashMap::new();
    for token in sorted {
      let Some(list) = self.list(token) else {
        continue;
      };
      let df = (list.len() / 5) as u64;
      let idf = bm25_idf(self.doc_count, df);
      for record in list.chunks_exact(5) {
        let id = u32::from_le_bytes([record[0], record[1], record[2], record[3]]);
        let length = self.lengths.get(id as usize).copied().unwrap_or(0);
        if length == 0 || !admit(id) {
          continue;
        }
        let entry = scores.entry(id).or_insert((0.0, 0));
        entry.0 += idf * bm25_term(record[4] as f32, length as f32, self.avgdl);
        entry.1 = entry.1.saturating_add(1);
      }
    }
    let mut ranked: Vec<(u32, f32)> = scores
      .into_iter()
      .filter(|&(_, (_, matched))| matched as usize >= required)
      .map(|(id, (score, _))| (id, score))
      .collect();
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    ranked.truncate(pool);
    Some(ranked.into_iter().map(|(id, _)| id as u64).collect())
  }
}

/// Channel parity when the posting tier is missing or stale: the IDENTICAL BM25
/// ranking computed in one pass over the graph — same df/avgdl/N derivation (integer
/// sums), same f32 arithmetic in the same sorted-token order — so results never
/// depend on which tier answered. Parallel over node chunks; per-node scores are
/// independent, and the final (score desc, id asc) sort makes the output a pure
/// function of the graph and the query at any thread count.
pub fn bm25_exhaustive(
  kg: &Kg,
  query_tokens: &[String],
  pool: usize,
  admit: impl Fn(u32) -> bool + Sync,
) -> Vec<u64> {
  use rayon::prelude::*;
  if query_tokens.is_empty() || pool == 0 {
    return Vec::new();
  }
  let mut sorted: Vec<&String> = query_tokens.iter().collect();
  sorted.sort_unstable();
  sorted.dedup();
  let node_count = kg.node_count();
  // Pass 1: per-token document frequencies + doc count + length sum (integer sums,
  // fixed 4096-node chunks folded in chunk order).
  const CHUNK: usize = 4096;
  let chunk_count = node_count.div_ceil(CHUNK).max(1);
  let partials: Vec<(Vec<u64>, u64, u64)> = (0..chunk_count)
    .into_par_iter()
    .map(|chunk| {
      let mut df = vec![0u64; sorted.len()];
      let mut docs = 0u64;
      let mut length_sum = 0u64;
      for id in (chunk * CHUNK)..((chunk + 1) * CHUNK).min(node_count) {
        let Some(view) = kg.node(NodeId::new(id as u64)) else {
          continue;
        };
        let tokens = tokenize(view.name);
        if tokens.is_empty() {
          continue;
        }
        docs += 1;
        length_sum += tokens.len().min(u8::MAX as usize) as u64;
        for (slot, token) in sorted.iter().enumerate() {
          if tokens.iter().any(|t| t == *token) {
            df[slot] += 1;
          }
        }
      }
      (df, docs, length_sum)
    })
    .collect();
  let mut df = vec![0u64; sorted.len()];
  let mut doc_count = 0u64;
  let mut length_sum = 0u64;
  for (chunk_df, docs, lengths) in &partials {
    for (total, part) in df.iter_mut().zip(chunk_df) {
      *total += part;
    }
    doc_count += docs;
    length_sum += lengths;
  }
  if doc_count == 0 {
    return Vec::new();
  }
  let avgdl = (length_sum as f64 / doc_count as f64) as f32;
  let idf: Vec<f32> = df.iter().map(|&d| bm25_idf(doc_count, d)).collect();
  // Pass 2: score each node — the same fn calls in the same sorted-token order as
  // the posting path, so scores agree bit for bit.
  let scored: Vec<(u32, f32)> = (0..node_count as u64)
    .into_par_iter()
    .filter_map(|id| {
      if !admit(id as u32) {
        return None;
      }
      let view = kg.node(NodeId::new(id))?;
      let tokens = tokenize(view.name);
      if tokens.is_empty() {
        return None;
      }
      let length = tokens.len().min(u8::MAX as usize) as u8;
      let mut score = 0.0f32;
      let mut matched = 0usize;
      for (slot, token) in sorted.iter().enumerate() {
        let tf = tokens.iter().filter(|t| *t == *token).count();
        if tf > 0 {
          matched += 1;
          let tf = tf.min(u8::MAX as usize) as u8;
          score += idf[slot] * bm25_term(tf as f32, length as f32, avgdl);
        }
      }
      // The same ≥2-distinct-tokens match floor as the posting path (parity law).
      (matched >= sorted.len().min(2)).then_some((id as u32, score))
    })
    .collect();
  let mut ranked = scored;
  ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
  ranked.truncate(pool);
  ranked.into_iter().map(|(id, _)| id as u64).collect()
}

/// Whether the persisted posting index matches `current_stamp`. Read-only; never builds.
pub fn postings_are_fresh(dir: &Path, current_stamp: u64) -> bool {
  // The header stamp is authoritative (the file is written atomically after the build).
  let Ok(bytes) = fs::read(dir.join("postings.bin")) else {
    return false;
  };
  bytes.len() >= 24
    && &bytes[0..4] == MAGIC
    && u32::from_le_bytes(bytes[4..8].try_into().unwrap()) == VERSION
    && u64::from_le_bytes(bytes[8..16].try_into().unwrap()) == current_stamp
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Hand-encode a tiny v2 index: nodes 1 and 3 have 2-token names; "load" → both
  /// (tf 1), "kg" → node 3 (tf 1). node_count 4, doc_count 2, avgdl 2.0.
  fn write_fixture(dir: &std::path::Path, stamp: u64) {
    let _ = fs::remove_dir_all(dir);
    fs::create_dir_all(dir).unwrap();
    let mut header = Vec::new();
    let mut pool: Vec<u8> = Vec::new();
    for (token, postings) in [("kg", vec![(3u32, 1u8)]), ("load", vec![(1u32, 1u8), (3u32, 1u8)])] {
      header.extend_from_slice(&(token.len() as u16).to_le_bytes());
      header.extend_from_slice(token.as_bytes());
      header.extend_from_slice(&((pool.len() / 5) as u64).to_le_bytes());
      header.extend_from_slice(&(postings.len() as u64).to_le_bytes());
      for (id, tf) in postings {
        pool.extend_from_slice(&id.to_le_bytes());
        pool.push(tf);
      }
    }
    let mut bytes = Vec::new();
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&VERSION.to_le_bytes());
    bytes.extend_from_slice(&stamp.to_le_bytes());
    bytes.extend_from_slice(&4u64.to_le_bytes()); // node_count
    bytes.extend_from_slice(&2u64.to_le_bytes()); // doc_count
    bytes.extend_from_slice(&2.0f64.to_le_bytes()); // avgdl
    bytes.extend_from_slice(&2u64.to_le_bytes()); // token_count
    bytes.extend_from_slice(&[0u8, 2, 0, 2]); // doc lengths
    bytes.extend_from_slice(&header);
    bytes.extend_from_slice(&pool);
    fs::write(dir.join("postings.bin"), &bytes).unwrap();
  }

  #[test]
  fn intersection_matches_scan_semantics() {
    let dir = std::env::temp_dir().join(format!("vorpal-postings-{}", std::process::id()));
    write_fixture(&dir, 7);
    let postings = Postings::load(&dir).expect("loads");
    assert_eq!(postings.stamp(), 7);
    assert!(postings_are_fresh(&dir, 7));
    assert!(!postings_are_fresh(&dir, 8));
    assert_eq!(
      postings.candidates(&["load".into()]).unwrap(),
      vec![1, 3]
    );
    assert_eq!(
      postings.candidates(&["load".into(), "kg".into()]).unwrap(),
      vec![3],
      "intersection"
    );
    assert_eq!(
      postings.candidates(&["missing".into()]).unwrap(),
      Vec::<u32>::new(),
      "an unknown token proves an empty intersection"
    );
    assert!(postings.candidates(&[]).is_none(), "no tokens → caller scans");
    let _ = fs::remove_dir_all(&dir);
  }

  #[test]
  fn bm25_matches_the_hand_computed_golden() {
    // Hand-computed on paper (doc_count 2, both docs dl = avgdl = 2, all tf = 1, so
    // every term component is exactly 1·(k1+1)/(1 + k1·(1−b+b)) = 2.2/2.2 = 1 and
    // scores are pure IDF sums):
    //   idf("load") = ln(1 + (2−2+0.5)/(2+0.5)) = ln(1.2)  = 0.18232156…
    //   idf("kg")   = ln(1 + (2−1+0.5)/(1+0.5)) = ln(2.0)  = 0.69314718…
    //   node 3 = ln(1.2) + ln(2.0) = 0.87546874…; node 1 = ln(1.2).
    let dir = std::env::temp_dir().join(format!("vorpal-postings-bm25-{}", std::process::id()));
    write_fixture(&dir, 7);
    let postings = Postings::load(&dir).expect("loads");
    let ranked = postings
      .bm25_ranked(&["load".into(), "kg".into()], 10, |_| true)
      .expect("tokens present");
    // The ≥2-distinct-tokens match floor drops node 1 (it matches only "load"):
    // single-token partials on multi-token queries are the measured pollution class.
    assert_eq!(ranked, vec![3], "only the 2-of-2 match survives the floor");
    // The filter pre-applies: rejecting node 3 leaves nothing above the floor —
    // filtered rows never appear in the channel (support/eliminator exactness).
    assert_eq!(
      postings
        .bm25_ranked(&["load".into(), "kg".into()], 10, |id| id != 3)
        .unwrap(),
      Vec::<u64>::new()
    );
    // The golden values pin the formula itself: idf(2,2) = ln(1.2), idf(2,1) = ln 2
    // (the latter against the named constant — the independent reference).
    assert!((bm25_idf(2, 2) as f64 - 0.182_321_556_8).abs() < 1e-6);
    assert!((bm25_idf(2, 1) as f64 - std::f64::consts::LN_2).abs() < 1e-6);
    assert_eq!(bm25_term(1.0, 2.0, 2.0), 1.0, "dl == avgdl ⇒ exactly 1");
    // Partial matches score (union, not intersection): "kg" alone ranks only node 3.
    assert_eq!(
      postings.bm25_ranked(&["kg".into()], 10, |_| true).unwrap(),
      vec![3]
    );
    // Pool truncation keeps the top of the same total order.
    assert_eq!(
      postings
        .bm25_ranked(&["load".into(), "kg".into()], 1, |_| true)
        .unwrap(),
      vec![3]
    );
    assert!(postings.bm25_ranked(&[], 10, |_| true).is_none());
    let _ = fs::remove_dir_all(&dir);
  }

  #[test]
  fn ranked_and_exhaustive_agree_at_any_thread_count() {
    // The channel-parity law: results never depend on which tier answered — the
    // persisted walk and the one-pass scan produce the IDENTICAL ranking, at any
    // rayon width (per-node scores are independent, statistics are fixed-order
    // integer sums, and both paths share every float in sorted-token order).
    let base = std::env::temp_dir().join(format!("vorpal-bm25-parity-{}", std::process::id()));
    let src = base.join("src");
    let out = base.join("index");
    let _ = fs::remove_dir_all(&base);
    fs::create_dir_all(&src).unwrap();
    fs::write(
      src.join("a.rs"),
      "pub fn socket_send() -> u32 { 1 }\npub fn socket_recv_buffer() -> u32 { 2 }\n\
       pub fn buffer_pool_alloc() -> u32 { 3 }\npub fn alloc() -> u32 { 4 }\n",
    )
    .unwrap();
    unsafe { std::env::set_var("VORPAL_NO_AUTOWARM", "1") };
    crate::build_index(&src, &out).unwrap();
    let dir = vorpal_kg::resolve_index_dir(&out);
    let kg = Kg::load(&dir).unwrap();
    build_postings(&kg, &dir, crate::stamp_of(&kg)).unwrap();
    let postings = Postings::load(&dir).expect("fresh postings");
    let cases: [&[&str]; 3] = [&["socket"], &["socket", "buffer"], &["alloc", "pool", "zzz"]];
    for tokens in cases {
      let tokens: Vec<String> = tokens.iter().map(|t| t.to_string()).collect();
      let ranked = postings.bm25_ranked(&tokens, 16, |_| true).unwrap();
      let wide = bm25_exhaustive(&kg, &tokens, 16, |_| true);
      assert_eq!(ranked, wide, "tier parity for {tokens:?}");
      let narrow = rayon::ThreadPoolBuilder::new()
        .num_threads(1)
        .build()
        .unwrap()
        .install(|| bm25_exhaustive(&kg, &tokens, 16, |_| true));
      assert_eq!(wide, narrow, "thread-count invariance for {tokens:?}");
    }
    let _ = fs::remove_dir_all(&base);
  }
}
