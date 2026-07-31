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
//! ascending):
//!   [VPST][version u32][stamp u64][token_count u64]
//!   token_count × { token_len u16, token bytes, ids_offset u64, ids_len u64 }
//!   ids pool: u32 node ids
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
const VERSION: u32 = 1;

/// Build and atomically persist the posting index for `kg` into `dir`, stamped with
/// `base_stamp` (the node-segment hash of the graph the tokens came from).
pub fn build_postings(kg: &Kg, dir: &Path, base_stamp: u64) -> std::io::Result<()> {
  vorpal_kg::phase_stamp("postings: build start");
  // token → ascending node ids. Iterating ids ascending keeps each list sorted for free.
  let mut lists: HashMap<String, Vec<u32>> = HashMap::new();
  for id in 0..kg.node_count() as u64 {
    let Some(view) = kg.node(NodeId::new(id)) else {
      continue;
    };
    let mut tokens = tokenize(view.name);
    tokens.sort_unstable();
    tokens.dedup();
    for token in tokens {
      lists.entry(token).or_default().push(id as u32);
    }
  }
  let mut tokens: Vec<(String, Vec<u32>)> = lists.into_iter().collect();
  tokens.sort_unstable_by(|a, b| a.0.cmp(&b.0));

  let mut header = Vec::with_capacity(tokens.len() * 24);
  let mut pool: Vec<u8> = Vec::new();
  for (token, ids) in &tokens {
    let bytes = token.as_bytes();
    header.extend_from_slice(&(bytes.len() as u16).to_le_bytes());
    header.extend_from_slice(bytes);
    header.extend_from_slice(&((pool.len() / 4) as u64).to_le_bytes());
    header.extend_from_slice(&(ids.len() as u64).to_le_bytes());
    for &id in ids {
      pool.extend_from_slice(&id.to_le_bytes());
    }
  }

  let tmp = dir.join("postings.bin.tmp");
  let path = dir.join("postings.bin");
  {
    let mut out = std::io::BufWriter::new(fs::File::create(&tmp)?);
    out.write_all(MAGIC)?;
    out.write_all(&VERSION.to_le_bytes())?;
    out.write_all(&base_stamp.to_le_bytes())?;
    out.write_all(&(tokens.len() as u64).to_le_bytes())?;
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
  /// token → (offset into `pool` in u32 units, len).
  tokens: HashMap<String, (u64, u64)>,
  pool: Vec<u8>,
}

impl Postings {
  /// Load `dir`'s posting index, if present and structurally sound. `None` is always safe:
  /// callers fall back to the scan.
  pub fn load(dir: &Path) -> Option<Postings> {
    let bytes = fs::read(dir.join("postings.bin")).ok()?;
    if bytes.len() < 24 || &bytes[0..4] != MAGIC {
      return None;
    }
    if u32::from_le_bytes(bytes[4..8].try_into().ok()?) != VERSION {
      return None;
    }
    let stamp = u64::from_le_bytes(bytes[8..16].try_into().ok()?);
    let token_count = u64::from_le_bytes(bytes[16..24].try_into().ok()?) as usize;
    let mut tokens = HashMap::with_capacity(token_count);
    let mut at = 24usize;
    for _ in 0..token_count {
      let len = u16::from_le_bytes(bytes.get(at..at + 2)?.try_into().ok()?) as usize;
      at += 2;
      let token = std::str::from_utf8(bytes.get(at..at + len)?).ok()?.to_string();
      at += len;
      let offset = u64::from_le_bytes(bytes.get(at..at + 8)?.try_into().ok()?);
      at += 8;
      let ids_len = u64::from_le_bytes(bytes.get(at..at + 8)?.try_into().ok()?);
      at += 8;
      tokens.insert(token, (offset, ids_len));
    }
    let pool = bytes.get(at..)?.to_vec();
    Some(Postings {
      stamp,
      tokens,
      pool,
    })
  }

  /// The node-segment stamp this index was built from.
  pub fn stamp(&self) -> u64 {
    self.stamp
  }

  fn list(&self, token: &str) -> Option<&[u8]> {
    let &(offset, len) = self.tokens.get(token)?;
    let start = (offset as usize).checked_mul(4)?;
    let end = start.checked_add((len as usize).checked_mul(4)?)?;
    self.pool.get(start..end)
  }

  /// Node ids whose names contain **all** of `query_tokens` (ascending). Empty input yields
  /// `None` (no lexical evidence — caller must scan), as does any token with no postings
  /// (the intersection is provably empty, returned as an empty vec).
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
    let first = lists[0];
    let mut out: Vec<u32> = first
      .chunks_exact(4)
      .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
      .collect();
    for list in &lists[1..] {
      let ids: Vec<u32> = list
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
        .collect();
      out.retain(|id| ids.binary_search(id).is_ok());
      if out.is_empty() {
        break;
      }
    }
    Some(out)
  }
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

  #[test]
  fn intersection_matches_scan_semantics() {
    // Build directly over a synthetic name table via the public surface: a real Kg is
    // exercised by the search-equivalence integration test; here the codec + intersection
    // logic round-trips through bytes.
    let dir = std::env::temp_dir().join(format!("vorpal-postings-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();

    // Hand-encode a tiny index: "load" → [1,3], "kg" → [3].
    let mut header = Vec::new();
    let mut pool: Vec<u8> = Vec::new();
    for (token, ids) in [("kg", vec![3u32]), ("load", vec![1u32, 3u32])] {
      header.extend_from_slice(&(token.len() as u16).to_le_bytes());
      header.extend_from_slice(token.as_bytes());
      header.extend_from_slice(&((pool.len() / 4) as u64).to_le_bytes());
      header.extend_from_slice(&(ids.len() as u64).to_le_bytes());
      for id in ids {
        pool.extend_from_slice(&id.to_le_bytes());
      }
    }
    let mut bytes = Vec::new();
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&VERSION.to_le_bytes());
    bytes.extend_from_slice(&7u64.to_le_bytes());
    bytes.extend_from_slice(&2u64.to_le_bytes());
    bytes.extend_from_slice(&header);
    bytes.extend_from_slice(&pool);
    fs::write(dir.join("postings.bin"), &bytes).unwrap();

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
}
