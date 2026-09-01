//! The DEFS-STABLE compose (P4.5c-2): artifact surgery for a single-file semantic edit
//! whose definition set is unchanged — bodies, references, sketches, and request sites
//! moved; names, kinds, signatures, imports, params, and returns did not.
//!
//! The theorem (verified in the ingest layer and held to scratch by the scoped oracle):
//! defs-stability keeps every file's node count and order, hence every dense id, hence
//! every OTHER file's evidence and edge bytes — except where the GLOBAL derivations reach:
//! the near-clone pair set (an edit can create or dissolve pairs anywhere) and the CALLS
//! condensation (`scc_size` is a node column over the global call graph). Both are handed
//! in EXACTLY: the caller supplies the repaired pair set with the endpoints whose similar
//! segments change, and this module recomputes the scc column only when the edited file's
//! call set actually moved, patching whichever buckets the ripple reaches.
//!
//! Everything rebuilds through the same byte layouts the full savers write — slab headers,
//! row encodings, TOC splices — and unchanged members hard-link. Any surprise is an error:
//! the caller falls back to the full pipeline, never commits a guess.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::Path;

use crate::EdgeType;
use crate::kg::{NODES_DIR, NodeIdMap};
use crate::respan::FileRespan;

/// One defs-stable edit's full surgery plan, in dense-id space (the universe is shared
/// with the prior generation by the defs-stable premise).
pub struct DefsStablePlan {
  pub file_key: u64,
  /// Fresh `(span_start, span_end, content_hash)` per layout ordinal — pipeline-exact,
  /// from a scratch single-file seal, stable fields verified row-by-row by the caller.
  pub node_rows: Vec<(u32, u32, u64)>,
  /// The file's fresh evidence rows (edge and no-edge outcomes).
  pub evidence: Vec<crate::EvidenceRow>,
  /// The file's fresh resolution + DATA_FLOWS edge stream, per-source log order.
  pub edges: Vec<(u32, u32, EdgeType)>,
  /// The file's request/notify tail segment.
  pub request_edges: Vec<(u32, u32, EdgeType)>,
  /// The repaired GLOBAL pair set (`a < b`, sorted — the pipeline's emission order).
  pub fresh_pairs: Vec<(u64, u64, u8)>,
  /// Endpoints of every added/removed/relabeled pair — their similar segments rewrite.
  pub changed_srcs: Vec<u32>,
  /// The sigs family's complete new row set (the swapped ledger the repair paired).
  pub sig_rows: Vec<crate::SigFamilyRow>,
  /// The file's fresh dataflow rows.
  pub flows: Vec<crate::DataflowRow>,
}

/// Edge-row class within a source's slab segment: the log emits them in this fixed phase
/// order (ingest containment → pre-link co-change → resolution with DATA_FLOWS spliced →
/// near-clone pairs → request/notify matches), and `compact_src_major`'s counting scatter
/// preserves per-source order into the slabs. The classes partition the etype space, so a
/// segment splits by CLASS — no positional guessing — and any interleaving is proof of a
/// premise violation (→ error → full pipeline).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum EdgeClass {
  Containment,
  Cochange,
  Resolution,
  Similar,
  Request,
}

fn edge_class(etype: EdgeType) -> EdgeClass {
  match etype.base() {
    EdgeType::DEFINES | EdgeType::HAS_METHOD | EdgeType::HAS_FIELD => EdgeClass::Containment,
    EdgeType::CHANGES_WITH => EdgeClass::Cochange,
    EdgeType::SIMILAR_TO => EdgeClass::Similar,
    EdgeType::REQUESTS | EdgeType::NOTIFIES => EdgeClass::Request,
    _ => EdgeClass::Resolution,
  }
}

/// A parsed edge-slab row in identity coding.
#[derive(Clone, Copy)]
struct SlabRow {
  src_local: u32,
  dst_key: u64,
  dst_ord: u32,
  etype: u16,
}

const EDGE_SLAB_HEADER: usize = 20;
const EDGE_ROW: usize = 18;
const EDGE_TOC_HEADER: usize = 20;
const EDGE_TOC_ROW: usize = 24;
const EVIDENCE_TOC_HEADER: usize = 20;
const EVIDENCE_TOC_ROW: usize = 32;

fn read_edge_slab(path: &Path, bucket: usize) -> io::Result<Vec<SlabRow>> {
  let bytes = fs::read(path)?;
  if bytes.len() < EDGE_SLAB_HEADER
    || &bytes[0..4] != b"VEDG"
    || u32::from_le_bytes(bytes[8..12].try_into().map_err(|_| io::Error::other("edge header"))?)
      != bucket as u32
  {
    return Err(io::Error::other("defs-stable: prior edge slab header mismatch"));
  }
  let count =
    u64::from_le_bytes(bytes[12..20].try_into().map_err(|_| io::Error::other("edge header"))?)
      as usize;
  if bytes.len() != EDGE_SLAB_HEADER + count * EDGE_ROW {
    return Err(io::Error::other("defs-stable: prior edge slab length mismatch"));
  }
  let mut rows = Vec::with_capacity(count);
  for i in 0..count {
    let at = EDGE_SLAB_HEADER + i * EDGE_ROW;
    let row = &bytes[at..at + EDGE_ROW];
    rows.push(SlabRow {
      src_local: u32::from_le_bytes(row[0..4].try_into().map_err(|_| io::Error::other("row"))?),
      dst_key: u64::from_le_bytes(row[4..12].try_into().map_err(|_| io::Error::other("row"))?),
      dst_ord: u32::from_le_bytes(row[12..16].try_into().map_err(|_| io::Error::other("row"))?),
      etype: u16::from_le_bytes(row[16..18].try_into().map_err(|_| io::Error::other("row"))?),
    });
  }
  Ok(rows)
}

fn encode_edge_slab(bucket: usize, rows: &[SlabRow]) -> (Vec<u8>, u64) {
  let mut bytes = Vec::with_capacity(EDGE_SLAB_HEADER + rows.len() * EDGE_ROW);
  bytes.extend_from_slice(b"VEDG");
  bytes.extend_from_slice(&1u32.to_le_bytes());
  bytes.extend_from_slice(&(bucket as u32).to_le_bytes());
  bytes.extend_from_slice(&(rows.len() as u64).to_le_bytes());
  for row in rows {
    bytes.extend_from_slice(&row.src_local.to_le_bytes());
    bytes.extend_from_slice(&row.dst_key.to_le_bytes());
    bytes.extend_from_slice(&row.dst_ord.to_le_bytes());
    bytes.extend_from_slice(&row.etype.to_le_bytes());
  }
  let digest = xxhash_rust::xxh3::xxh3_64(&bytes);
  (bytes, digest)
}

/// Localize a dense-id edge into identity coding.
fn localize(
  map: &NodeIdMap,
  src_base: u64,
  from: u32,
  to: u32,
  etype: EdgeType,
) -> io::Result<SlabRow> {
  let (dst_key, dst_ord) = map
    .locate_bulk(to)
    .ok_or_else(|| io::Error::other("defs-stable: edge endpoint outside the universe"))?;
  Ok(SlabRow {
    src_local: u32::try_from(u64::from(from) - src_base)
      .map_err(|_| io::Error::other("defs-stable: source beyond its bucket"))?,
    dst_key,
    dst_ord,
    etype: etype.0,
  })
}

/// Compose the bucketed generation for one defs-stable edit. The caller has verified
/// eligibility and stable fields; this module re-verifies every structural premise it
/// leans on and errors — never guesses — on any surprise.
pub fn compose_defs_stable(
  staging: &Path,
  prior: &Path,
  prior_kg: &crate::Kg,
  plan: &DefsStablePlan,
) -> io::Result<()> {
  let map = NodeIdMap::from_dir(prior)
    .ok_or_else(|| io::Error::other("defs-stable: prior has no bucketed node store"))?;
  let bases = map.bases().to_vec();
  let buckets = bases.len() - 1;
  let &(_, file_base, file_rows) = map
    .files()
    .iter()
    .find(|&&(key, _, _)| key == plan.file_key)
    .ok_or_else(|| io::Error::other("defs-stable: file outside the prior universe"))?;
  if plan.node_rows.len() != file_rows as usize {
    return Err(io::Error::other("defs-stable: node row count moved — not defs-stable"));
  }
  let file_range = file_base..file_base + u64::from(file_rows);
  let file_bucket = bases.partition_point(|&b| b <= file_base) - 1;

  // ---- scc: recompute ONLY when the edited file's call set moved ----
  // The condensation reads the (src, dst) CALLS pair set; parallel edges and every other
  // etype are invisible to it. Compare the file's prior and fresh call sets first — the
  // common body edit keeps them, and then every scc_size byte is provably stable.
  let prior_file_rows = read_edge_slab(
    &prior.join(crate::EDGES_DIR).join(format!("{file_bucket:04}.bin")),
    file_bucket,
  )?;
  let file_lo_local = (file_base - bases[file_bucket]) as u32;
  let file_hi_local = file_lo_local + file_rows;
  let starts: HashMap<u64, u64> =
    map.files().iter().map(|&(key, start, _)| (key, start)).collect();
  let densify = |row: &SlabRow| -> io::Result<u32> {
    starts
      .get(&row.dst_key)
      .map(|&start| (start + u64::from(row.dst_ord)) as u32)
      .ok_or_else(|| io::Error::other("defs-stable: edge destination outside the universe"))
  };
  let mut prior_calls: HashSet<(u32, u32)> = HashSet::new();
  for row in &prior_file_rows {
    if row.src_local >= file_lo_local
      && row.src_local < file_hi_local
      && EdgeType(row.etype).base() == EdgeType::CALLS
    {
      prior_calls.insert((row.src_local - file_lo_local, densify(row)?));
    }
  }
  let fresh_calls: HashSet<(u32, u32)> = plan
    .edges
    .iter()
    .filter(|(_, _, etype)| etype.base() == EdgeType::CALLS)
    .map(|&(from, to, _)| ((u64::from(from) - file_base) as u32, to))
    .collect();
  let scc_new: Option<Vec<u32>> = if prior_calls == fresh_calls {
    None
  } else {
    // The ripple can reach any bucket: the global CALLS list is the prior GRAPH's calls
    // (zero slab decode — the CSR slices are the same truth) with the edited file's
    // replaced by the plan's, through the seal's own condensation.
    let node_count = bases[buckets] as usize;
    let mut log = vorpal_graph::EdgeLog::default();
    for u in 0..node_count as u32 {
      if file_range.contains(&u64::from(u)) {
        continue; // replaced by the plan's fresh calls below
      }
      for (&dst, &etype) in prior_kg
        .graph_out_targets(u)
        .iter()
        .zip(prior_kg.graph_out_edge_types(u))
      {
        if EdgeType(etype).base() == EdgeType::CALLS {
          log.push(u, dst, EdgeType(etype));
        }
      }
    }
    for &(from, to, etype) in &plan.edges {
      if etype.base() == EdgeType::CALLS {
        log.push(from, to, etype);
      }
    }
    Some(crate::scc::scc_sizes(node_count, &log))
  };

  // ---- node store: span/content patches for the file + scc patches wherever they land ----
  let respan_plan = FileRespan {
    file_key: plan.file_key,
    rows: plan.node_rows.clone(),
    ref_spans: HashMap::new(),
    call_spans: HashMap::new(),
  };
  let node_fold = crate::respan::rebuild_node_buckets(
    staging,
    prior,
    &map,
    &bases,
    &[(plan.file_key, &respan_plan)],
    scc_new.as_deref(),
  )?;

  // ---- evidence: the file's bucket swaps its rows; every other bucket links ----
  let store = crate::evidence::EvidenceStore::open(prior)
    .ok_or_else(|| io::Error::other("defs-stable: prior evidence unreadable"))?;
  let evidence_dir = staging.join(crate::EVIDENCE_DIR);
  fs::create_dir_all(&evidence_dir)?;
  let mut ev_toc = fs::read(prior.join(crate::EVIDENCE_TOC))
    .map_err(|_| io::Error::other("defs-stable: prior evidence TOC unreadable"))?;
  let mut dropped_rows: Vec<crate::EvidenceRow> = Vec::new();
  for (bucket, window) in bases.windows(2).enumerate() {
    let name = format!("{bucket:04}.bin");
    if bucket != file_bucket {
      let (from, to) =
        (prior.join(crate::EVIDENCE_DIR).join(&name), evidence_dir.join(&name));
      let _ = fs::remove_file(&to);
      if fs::hard_link(&from, &to).is_err() {
        fs::copy(&from, &to)?;
      }
      continue;
    }
    let mut rows = store.rows_of_bucket(bucket);
    rows.retain(|row| {
      let in_file = file_range.contains(&u64::from(row.from));
      if in_file {
        dropped_rows.push(row.clone());
      }
      !in_file
    });
    rows.extend(plan.evidence.iter().cloned());
    let built = crate::evidence::build_slab(bucket, window[0], &rows, &map)?;
    let tmp = evidence_dir.join(format!("{name}.tmp"));
    fs::write(&tmp, &built.bytes)?;
    fs::rename(&tmp, evidence_dir.join(&name))?;
    // Row and pool counts CHANGE under this compose (unlike the respan) — splice the
    // whole TOC row and re-total the header.
    let at = EVIDENCE_TOC_HEADER + bucket * EVIDENCE_TOC_ROW;
    let prior_rows_here = u64::from_le_bytes(
      ev_toc
        .get(at..at + 8)
        .ok_or_else(|| io::Error::other("defs-stable: evidence TOC too short"))?
        .try_into()
        .map_err(|_| io::Error::other("defs-stable: evidence TOC"))?,
    );
    let total_at = 12..20;
    let prior_total = u64::from_le_bytes(
      ev_toc
        .get(total_at.clone())
        .ok_or_else(|| io::Error::other("defs-stable: evidence TOC header"))?
        .try_into()
        .map_err(|_| io::Error::other("defs-stable: evidence TOC header"))?,
    );
    let new_total = prior_total - prior_rows_here + built.rows;
    ev_toc[total_at].copy_from_slice(&new_total.to_le_bytes());
    ev_toc[at..at + 8].copy_from_slice(&built.rows.to_le_bytes());
    ev_toc[at + 8..at + 16].copy_from_slice(&built.pool.to_le_bytes());
    ev_toc[at + 16..at + 24].copy_from_slice(&(built.bytes.len() as u64).to_le_bytes());
    ev_toc[at + 24..at + 32].copy_from_slice(&built.digest.to_le_bytes());
  }
  let toc_tmp = evidence_dir.join("toc.bin.tmp");
  fs::write(&toc_tmp, &ev_toc)?;
  fs::rename(&toc_tmp, staging.join(crate::EVIDENCE_TOC))?;

  // ---- edges: rebuild the file's bucket + every changed similar endpoint's bucket ----
  let mut segment_srcs: HashMap<usize, HashSet<u32>> = HashMap::new(); // bucket -> dense srcs to rewrite
  for src in file_range.clone() {
    segment_srcs.entry(file_bucket).or_default().insert(src as u32);
  }
  for &src in &plan.changed_srcs {
    let bucket = bases.partition_point(|&b| b <= u64::from(src)) - 1;
    segment_srcs.entry(bucket).or_default().insert(src);
  }
  // Per-source fresh similar segments, in global pair-emission order (sorted pairs, a→b
  // then b→a — the drain's documented deterministic order).
  let mut similar_of: HashMap<u32, Vec<SlabRow>> = HashMap::new();
  {
    let need: HashSet<u32> = segment_srcs.values().flatten().copied().collect();
    for &(a, b, confidence) in &plan.fresh_pairs {
      let label = EdgeType::SIMILAR_TO.with_confidence(confidence);
      for (s, d) in [(a, b), (b, a)] {
        let s32 = s as u32;
        if need.contains(&s32) {
          let src_bucket = bases.partition_point(|&base| base <= s) - 1;
          similar_of.entry(s32).or_default().push(localize(
            &map,
            bases[src_bucket],
            s32,
            d as u32,
            label,
          )?);
        }
      }
    }
  }
  let edges_dir = staging.join(crate::EDGES_DIR);
  fs::create_dir_all(&edges_dir)?;
  let mut edge_toc = fs::read(prior.join(crate::EDGES_TOC))
    .map_err(|_| io::Error::other("defs-stable: prior edge TOC unreadable"))?;
  let mut rebuilt_rows: HashMap<usize, Vec<SlabRow>> = HashMap::new();
  for bucket in 0..buckets {
    let name = format!("{bucket:04}.bin");
    let Some(rewrite_srcs) = segment_srcs.get(&bucket) else {
      let (from, to) = (prior.join(crate::EDGES_DIR).join(&name), edges_dir.join(&name));
      let _ = fs::remove_file(&to);
      if fs::hard_link(&from, &to).is_err() {
        fs::copy(&from, &to)?;
      }
      continue;
    };
    let bucket_base = bases[bucket];
    let prior_rows = if bucket == file_bucket {
      prior_file_rows.clone()
    } else {
      read_edge_slab(&prior.join(crate::EDGES_DIR).join(&name), bucket)?
    };
    let mut out: Vec<SlabRow> = Vec::with_capacity(prior_rows.len() + plan.edges.len());
    let mut i = 0usize;
    let bucket_len = (bases[bucket + 1] - bucket_base) as u32;
    for src_local in 0..bucket_len {
      let run_start = i;
      while i < prior_rows.len() && prior_rows[i].src_local == src_local {
        i += 1;
      }
      let run = &prior_rows[run_start..i];
      // The class sequence within a run must be monotone — the log-phase premise.
      let mut last = EdgeClass::Containment;
      for row in run {
        let class = edge_class(EdgeType(row.etype));
        if class < last {
          return Err(io::Error::other(
            "defs-stable: prior edge run interleaves log phases — premise violated",
          ));
        }
        last = class;
      }
      let src_dense = (bucket_base + u64::from(src_local)) as u32;
      if !rewrite_srcs.contains(&src_dense) {
        out.extend_from_slice(run);
        continue;
      }
      let keep = |class: EdgeClass| run.iter().filter(move |row| edge_class(EdgeType(row.etype)) == class);
      out.extend(keep(EdgeClass::Containment));
      out.extend(keep(EdgeClass::Cochange));
      if file_range.contains(&u64::from(src_dense)) {
        for &(from, to, etype) in plan.edges.iter().filter(|(from, _, _)| *from == src_dense) {
          debug_assert_eq!(from, src_dense);
          out.push(localize(&map, bucket_base, from, to, etype)?);
        }
      } else {
        out.extend(keep(EdgeClass::Resolution));
      }
      if let Some(similar) = similar_of.get(&src_dense) {
        out.extend_from_slice(similar);
      }
      if file_range.contains(&u64::from(src_dense)) {
        for &(from, to, etype) in
          plan.request_edges.iter().filter(|(from, _, _)| *from == src_dense)
        {
          out.push(localize(&map, bucket_base, from, to, etype)?);
        }
      } else {
        out.extend(keep(EdgeClass::Request));
      }
    }
    if i != prior_rows.len() {
      return Err(io::Error::other("defs-stable: prior edge slab rows out of source order"));
    }
    let (bytes, digest) = encode_edge_slab(bucket, &out);
    rebuilt_rows.insert(bucket, out.clone());
    let tmp = edges_dir.join(format!("{name}.tmp"));
    fs::write(&tmp, &bytes)?;
    fs::rename(&tmp, edges_dir.join(&name))?;
    let at = EDGE_TOC_HEADER + bucket * EDGE_TOC_ROW;
    let prior_edges_here = u64::from_le_bytes(
      edge_toc
        .get(at..at + 8)
        .ok_or_else(|| io::Error::other("defs-stable: edge TOC too short"))?
        .try_into()
        .map_err(|_| io::Error::other("defs-stable: edge TOC"))?,
    );
    let prior_total = u64::from_le_bytes(
      edge_toc
        .get(12..20)
        .ok_or_else(|| io::Error::other("defs-stable: edge TOC header"))?
        .try_into()
        .map_err(|_| io::Error::other("defs-stable: edge TOC header"))?,
    );
    let new_total = prior_total - prior_edges_here + out.len() as u64;
    edge_toc[12..20].copy_from_slice(&new_total.to_le_bytes());
    edge_toc[at..at + 8].copy_from_slice(&(out.len() as u64).to_le_bytes());
    edge_toc[at + 8..at + 16].copy_from_slice(&(bytes.len() as u64).to_le_bytes());
    edge_toc[at + 16..at + 24].copy_from_slice(&digest.to_le_bytes());
  }
  let toc_tmp = edges_dir.join("toc.bin.tmp");
  fs::write(&toc_tmp, &edge_toc)?;
  fs::rename(&toc_tmp, staging.join(crate::EDGES_TOC))?;

  // ---- graph cache: the successor CSR/CSC from the prior graph + the delta ----
  // Without this, every build FOLLOWING a compose pays a full slab-decode rebuild
  // (measured ~1.4 s at kernel scale) — the lazy posture compounds on compose chains.
  // Unaffected sources stream from the prior CSR slices; affected buckets re-densify the
  // EXACT row sets the surgery just wrote, so the cache cannot drift from the slabs.
  {
    let node_count = bases[buckets];
    let mut srcs: Vec<u32> = Vec::new();
    let mut dsts: Vec<u32> = Vec::new();
    let mut etypes: Vec<u16> = Vec::new();
    for (bucket, window) in bases.windows(2).enumerate() {
      if let Some(rows) = rebuilt_rows.get(&bucket) {
        let bucket_base = window[0];
        for row in rows {
          srcs.push((bucket_base + u64::from(row.src_local)) as u32);
          dsts.push(densify(row)?);
          etypes.push(row.etype);
        }
        continue;
      }
      for u in window[0]..window[1] {
        let u = u as u32;
        let targets = prior_kg.graph_out_targets(u);
        srcs.extend(std::iter::repeat_n(u, targets.len()));
        dsts.extend_from_slice(targets);
        etypes.extend_from_slice(prior_kg.graph_out_edge_types(u));
      }
    }
    let graph = vorpal_graph::Graph::from_parts(node_count as u32, &srcs, &dsts, &etypes);
    if let Some(stamp) = crate::edgestore::expected_stamp(staging, node_fold) {
      // Best-effort like every cache write; the loader rebuilds from slabs if absent.
      let _ = crate::edgestore::write_cache(staging, &graph, stamp);
    }
  }

  // ---- usage: the file's referenced-name postings delta over the prior pair set ----
  let usage = crate::usagestore::UsageStore::open(prior)
    .ok_or_else(|| io::Error::other("defs-stable: prior usage unreadable"))?;
  let mut pairs: Vec<(u32, u64)> = usage.all_pairs();
  let old_file_pairs: HashSet<u32> = dropped_rows.iter().map(|row| row.name_hash).collect();
  pairs.retain(|&(hash, key)| !(key == plan.file_key && old_file_pairs.contains(&hash)));
  let mut new_file_pairs: Vec<(u32, u64)> = plan
    .evidence
    .iter()
    .map(|row| (row.name_hash, plan.file_key))
    .collect();
  new_file_pairs.sort_unstable();
  new_file_pairs.dedup();
  pairs.extend(new_file_pairs);
  crate::usagestore::save(staging, pairs, buckets as u32, Some(prior))?;

  // ---- sigs: the repaired ledger, digest-carried per bucket ----
  crate::sigstore::save_sigs(staging, &plan.sig_rows, &map, Some(prior))?;

  // ---- dataflow: the canonical saver re-sorts; filter + extend is exact ----
  let mut flows = crate::dataflow::load_dataflow(prior)
    .ok_or_else(|| io::Error::other("defs-stable: prior dataflow unreadable"))?;
  flows.retain(|row| !file_range.contains(&u64::from(row.from)));
  flows.extend(plan.flows.iter().cloned());
  crate::dataflow::save_dataflow(staging, flows)?;

  // ---- names.idx: names are defs-stable ⇒ byte-identical ⇒ link. graph.bin/graph.stamp:
  // edges CHANGED — the derived cache is deliberately omitted (no stale link, no stamp);
  // the loader rebuilds from the slabs lazily and re-caches, the established posture. ----
  let (from, to) = (prior.join("names.idx"), staging.join("names.idx"));
  if from.exists() {
    let _ = fs::remove_file(&to);
    if fs::hard_link(&from, &to).is_err() {
      fs::copy(&from, &to)?;
    }
  }
  Ok(())
}

// NODES_DIR is consumed by respan::rebuild_node_buckets; referenced here so the module's
// imports stay honest if that factoring ever moves.
const _: &str = NODES_DIR;
