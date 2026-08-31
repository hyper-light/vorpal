//! The bucketed edge store (P4.3): per-SOURCE-bucket slabs are the graph's truth under
//! `VORPAL_FORMAT=next`; the dense CSR/CSC (`graph.bin`) demotes to a stamped DERIVED
//! CACHE (the ANN-tier precedent) — same bytes, same zero-copy mmap when fresh, rebuilt
//! from the slabs when stale or missing, excluded from the generation's content identity.
//!
//! Slab (`edges/<k>.bin`): `[VEDG][version][bucket u32][edges u64]`, then 18-byte rows
//! `[src_local u32][dst_key u64][dst_ordinal u32][etype u16]` in src-major emission order
//! — each source's out-edges in sealed-CSR order, sources ascending. Under the CSC law
//! (`Graph::compact_src_major`) that enumeration rebuilds BOTH directions bit-identically.
//! Destinations are the P4.0 identity `(file_key u64, ordinal u32)`: position-independent
//! under file adds/removes anywhere else — the `(bucket, bucket-local)` coding was
//! prototyped first and rejected by measurement (ordinal-shift cascade through incoming
//! references; see the evidence module). The TOC (`edges/toc.bin`) carries per-slab
//! digests — the writer hard-links every slab whose bytes match the prior generation's.
//!
//! The cache stamp (`graph.stamp`): `[VGST][version][node_stamp u64][edges_stamp u64]`
//! where `edges_stamp` folds the TOC's per-slab digests in order. `graph.bin` is served
//! only when both match; otherwise the loader rebuilds from slabs and re-caches
//! best-effort (lazy sidecar writes into a committed generation are the established ANN
//! posture).

use std::fs;
use std::io::{self, Write};
use std::path::Path;

use vorpal_graph::Graph;

pub const EDGES_DIR: &str = "edges";
pub const EDGES_TOC: &str = "edges/toc.bin";
const SLAB_MAGIC: &[u8; 4] = b"VEDG";
const TOC_MAGIC: &[u8; 4] = b"VEDT";
const STAMP_MAGIC: &[u8; 4] = b"VGST";
const VERSION: u32 = 1;
/// Slab header: magic + version + bucket u32 + edge count u64.
const SLAB_HEADER: usize = 20;
/// One row: src_local u32 + dst_key u64 + dst_ordinal u32 + etype u16.
const ROW: usize = 18;
/// TOC header: magic + version + bucket count u32 + total edges u64.
const TOC_HEADER: usize = 20;
/// One per-slab TOC row: edges u64 + byte len u64 + digest u64.
const TOC_ROW: usize = 24;

/// Whether `name` (generation-relative) is a bucketed edge-store member.
pub fn is_edges_member(name: &str) -> bool {
  if name == EDGES_TOC {
    return true;
  }
  name
    .strip_prefix("edges/")
    .and_then(|f| f.strip_suffix(".bin"))
    .is_some_and(|k| !k.is_empty() && k.len() <= 5 && k.bytes().all(|b| b.is_ascii_digit()))
}

struct TocRow {
  edges: u64,
  len: u64,
  digest: u64,
}

struct Toc {
  rows: Vec<TocRow>,
}

impl Toc {
  fn load(path: &Path) -> Option<Toc> {
    let bytes = fs::read(path).ok()?;
    if bytes.len() < TOC_HEADER || &bytes[0..4] != TOC_MAGIC {
      return None;
    }
    if u32::from_le_bytes(bytes[4..8].try_into().ok()?) != VERSION {
      return None;
    }
    let buckets = u32::from_le_bytes(bytes[8..12].try_into().ok()?) as usize;
    let total = u64::from_le_bytes(bytes[12..20].try_into().ok()?);
    if bytes.len() < TOC_HEADER + buckets * TOC_ROW {
      return None;
    }
    let mut rows = Vec::with_capacity(buckets);
    for k in 0..buckets {
      let at = TOC_HEADER + k * TOC_ROW;
      let row = &bytes[at..at + TOC_ROW];
      rows.push(TocRow {
        edges: u64::from_le_bytes(row[0..8].try_into().ok()?),
        len: u64::from_le_bytes(row[8..16].try_into().ok()?),
        digest: u64::from_le_bytes(row[16..24].try_into().ok()?),
      });
    }
    if rows.iter().map(|r| r.edges).sum::<u64>() != total {
      return None;
    }
    Some(Toc { rows })
  }

  /// The fold over per-slab digests — the edge half of the cache stamp.
  fn edges_stamp(&self) -> u64 {
    let mut hasher = xxhash_rust::xxh3::Xxh3::new();
    for row in &self.rows {
      hasher.update(&row.digest.to_le_bytes());
    }
    hasher.digest()
  }
}

/// Persist the graph's truth as per-source-bucket slabs + TOC, hard-linking every slab
/// whose bytes match the prior generation's digests.
pub(crate) fn save(
  dir: &Path,
  graph: &Graph,
  nodes: &crate::kg::NodeIdMap,
  prior: Option<&Path>,
) -> io::Result<()> {
  use rayon::prelude::*;
  let bases = nodes.bases();
  if bases.len() < 2 {
    return Err(io::Error::other("bucketed edge store requires node bases"));
  }
  let buckets = bases.len() - 1;
  let edges_dir = dir.join(EDGES_DIR);
  fs::create_dir_all(&edges_dir)?;
  let prior_toc = prior.and_then(|p| Toc::load(&p.join(EDGES_TOC)));
  let prior_ok = prior_toc.as_ref().is_some_and(|toc| toc.rows.len() == buckets);

  struct Built {
    edges: u64,
    bytes: Vec<u8>,
    digest: u64,
  }
  let built: io::Result<Vec<Built>> = (0..buckets)
    .into_par_iter()
    .map(|bucket| {
      let (lo, hi) = (bases[bucket], bases[bucket + 1]);
      let mut count = 0usize;
      for src in lo..hi {
        count += graph.out_degree(src as u32);
      }
      let mut bytes = Vec::with_capacity(SLAB_HEADER + count * ROW);
      bytes.extend_from_slice(SLAB_MAGIC);
      bytes.extend_from_slice(&VERSION.to_le_bytes());
      bytes.extend_from_slice(&(bucket as u32).to_le_bytes());
      bytes.extend_from_slice(&(count as u64).to_le_bytes());
      for src in lo..hi {
        let src_local = (src - lo) as u32;
        let u = src as u32;
        for (&dst, &etype) in graph.out_targets(u).iter().zip(graph.out_edge_types(u)) {
          let (dst_key, dst_ord) = nodes
            .locate_bulk(dst)
            .ok_or_else(|| io::Error::other("edge endpoint outside the node universe"))?;
          bytes.extend_from_slice(&src_local.to_le_bytes());
          bytes.extend_from_slice(&dst_key.to_le_bytes());
          bytes.extend_from_slice(&dst_ord.to_le_bytes());
          bytes.extend_from_slice(&etype.to_le_bytes());
        }
      }
      let digest = xxhash_rust::xxh3::xxh3_64(&bytes);
      Ok(Built {
        edges: count as u64,
        bytes,
        digest,
      })
    })
    .collect();
  let built = built?;

  for (bucket, slab) in built.iter().enumerate() {
    let name = format!("{bucket:04}.bin");
    let carried = prior_ok
      && prior_toc.as_ref().is_some_and(|toc| {
        let row = &toc.rows[bucket];
        row.edges == slab.edges && row.len == slab.bytes.len() as u64 && row.digest == slab.digest
      });
    if carried {
      let from = prior
        .map(|p| p.join(EDGES_DIR).join(&name))
        .expect("carried implies a prior");
      let to = edges_dir.join(&name);
      if from == to {
        continue; // legacy same-directory publish: already in place
      }
      let _ = fs::remove_file(&to);
      if fs::hard_link(&from, &to).is_ok() {
        continue;
      }
      // Link refused: fall through to the write — same bytes, full cost.
    }
    let tmp = edges_dir.join(format!("{name}.tmp"));
    fs::write(&tmp, &slab.bytes)?;
    fs::rename(&tmp, edges_dir.join(&name))?;
  }
  // TOC last — the family's commit record.
  let total: u64 = built.iter().map(|s| s.edges).sum();
  let toc_tmp = edges_dir.join("toc.bin.tmp");
  let mut out = fs::File::create(&toc_tmp)?;
  out.write_all(TOC_MAGIC)?;
  out.write_all(&VERSION.to_le_bytes())?;
  out.write_all(&(buckets as u32).to_le_bytes())?;
  out.write_all(&total.to_le_bytes())?;
  for slab in &built {
    out.write_all(&slab.edges.to_le_bytes())?;
    out.write_all(&(slab.bytes.len() as u64).to_le_bytes())?;
    out.write_all(&slab.digest.to_le_bytes())?;
  }
  drop(out);
  fs::rename(&toc_tmp, dir.join(EDGES_TOC))?;
  // Stale members beyond this publish's bucket count.
  if let Ok(dirents) = fs::read_dir(&edges_dir) {
    for entry in dirents.flatten() {
      if let Ok(name) = entry.file_name().into_string() {
        let stale = name
          .strip_suffix(".bin")
          .and_then(|k| k.parse::<u32>().ok())
          .is_some_and(|k| k as usize >= buckets);
        if stale || name.ends_with(".tmp") {
          let _ = fs::remove_file(entry.path());
        }
      }
    }
  }
  Ok(())
}

/// Rebuild the dense graph from the slabs: src-major concatenation → `compact_src_major`'s
/// exact input, so the result is bit-identical to the sealed graph. Loud on any
/// inconsistency — a generation directory is atomic, so a half-present edge store means a
/// mixed generation.
pub(crate) fn load_graph(dir: &Path, nodes: &crate::kg::NodeIdMap) -> io::Result<Graph> {
  let toc = Toc::load(&dir.join(EDGES_TOC))
    .ok_or_else(|| io::Error::other("bucketed edge store: unreadable TOC"))?;
  let bases = nodes.bases();
  if bases.len() != toc.rows.len() + 1 {
    return Err(io::Error::other(
      "bucketed edge store: bucket count disagrees with the node store",
    ));
  }
  let node_count = bases[bases.len() - 1];
  // Transient key → dense-start map: the rebuild densifies millions of endpoints, and a
  // binary search each is measurable wall on the cache-miss path.
  let starts: std::collections::HashMap<u64, (u64, u32)> = nodes
    .files()
    .iter()
    .map(|&(key, start, rows)| (key, (start, rows)))
    .collect();
  let total: usize = toc.rows.iter().map(|r| r.edges as usize).sum();
  let mut srcs = Vec::with_capacity(total);
  let mut dsts = Vec::with_capacity(total);
  let mut etypes = Vec::with_capacity(total);
  for (k, row) in toc.rows.iter().enumerate() {
    let path = dir.join(EDGES_DIR).join(format!("{k:04}.bin"));
    let bytes = fs::read(&path)?;
    if bytes.len() != row.len as usize
      || bytes.len() < SLAB_HEADER
      || &bytes[0..4] != SLAB_MAGIC
      || u32::from_le_bytes(bytes[4..8].try_into().map_err(|_| io::Error::other("slab header"))?)
        != VERSION
      || u32::from_le_bytes(bytes[8..12].try_into().map_err(|_| io::Error::other("slab header"))?)
        != k as u32
    {
      return Err(io::Error::other(
        "bucketed edge store: slab disagrees with TOC (mixed generation)",
      ));
    }
    let count =
      u64::from_le_bytes(bytes[12..20].try_into().map_err(|_| io::Error::other("slab header"))?);
    if count != row.edges || bytes.len() != SLAB_HEADER + count as usize * ROW {
      return Err(io::Error::other("bucketed edge store: slab length mismatch"));
    }
    let src_base = bases[k];
    for i in 0..count as usize {
      let at = SLAB_HEADER + i * ROW;
      let row_bytes = &bytes[at..at + ROW];
      let src_local =
        u32::from_le_bytes(row_bytes[0..4].try_into().map_err(|_| io::Error::other("row"))?);
      let dst_key =
        u64::from_le_bytes(row_bytes[4..12].try_into().map_err(|_| io::Error::other("row"))?);
      let dst_ord =
        u32::from_le_bytes(row_bytes[12..16].try_into().map_err(|_| io::Error::other("row"))?);
      let etype =
        u16::from_le_bytes(row_bytes[16..18].try_into().map_err(|_| io::Error::other("row"))?);
      let dst = starts
        .get(&dst_key)
        .filter(|&&(_, rows)| dst_ord < rows)
        .map(|&(start, _)| (start + u64::from(dst_ord)) as u32)
        .ok_or_else(|| io::Error::other("edge destination outside the node universe"))?;
      if u64::from(src_local) + src_base >= node_count || u64::from(dst) >= node_count {
        return Err(io::Error::other("edge endpoint beyond the node universe"));
      }
      srcs.push((src_base + u64::from(src_local)) as u32);
      dsts.push(dst);
      etypes.push(etype);
    }
  }
  Ok(Graph::from_parts(node_count as u32, &srcs, &dsts, &etypes))
}

/// The cache stamp `graph.bin` is valid under: the node-store stamp and the edge-TOC fold.
pub(crate) fn cache_stamp_of(dir: &Path) -> Option<(u64, u64)> {
  let bytes = fs::read(dir.join("graph.stamp")).ok()?;
  if bytes.len() != 24 || &bytes[0..4] != STAMP_MAGIC {
    return None;
  }
  if u32::from_le_bytes(bytes[4..8].try_into().ok()?) != VERSION {
    return None;
  }
  Some((
    u64::from_le_bytes(bytes[8..16].try_into().ok()?),
    u64::from_le_bytes(bytes[16..24].try_into().ok()?),
  ))
}

/// The stamp the CURRENT slabs demand of a cache.
pub(crate) fn expected_stamp(dir: &Path, node_stamp: u64) -> Option<(u64, u64)> {
  Toc::load(&dir.join(EDGES_TOC)).map(|toc| (node_stamp, toc.edges_stamp()))
}

/// Write `graph.bin` + `graph.stamp` for `graph` under `dir` (tmp + rename each; stamp
/// last). Best-effort callers ignore the error — the cache is never load-bearing.
pub(crate) fn write_cache(dir: &Path, graph: &Graph, stamp: (u64, u64)) -> io::Result<()> {
  let tmp = dir.join("graph.bin.tmp");
  let mut out = std::io::BufWriter::with_capacity(1 << 20, fs::File::create(&tmp)?);
  graph.write_to(&mut out)?;
  out.flush()?;
  drop(out);
  fs::rename(&tmp, dir.join("graph.bin"))?;
  write_stamp(dir, stamp)
}

/// Write just `graph.stamp` (the respan compose links `graph.bin` verbatim — edges are
/// unchanged — and refreshes only the node half of the stamp).
pub(crate) fn write_stamp(dir: &Path, stamp: (u64, u64)) -> io::Result<()> {
  let stamp_tmp = dir.join("graph.stamp.tmp");
  let mut out = fs::File::create(&stamp_tmp)?;
  out.write_all(STAMP_MAGIC)?;
  out.write_all(&VERSION.to_le_bytes())?;
  out.write_all(&stamp.0.to_le_bytes())?;
  out.write_all(&stamp.1.to_le_bytes())?;
  drop(out);
  fs::rename(&stamp_tmp, dir.join("graph.stamp"))
}
