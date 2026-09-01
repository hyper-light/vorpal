//! The DEFS-STABLE compose (P4.5c-2, multi-file since S2): artifact surgery for a
//! session of semantic edits whose definition sets are unchanged — bodies, references,
//! sketches, and request sites moved; names, kinds, signatures, imports, params, and
//! returns did not, in ANY edited file.
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

/// One edited file's slice of a defs-stable session plan, in dense-id space (the
/// universe is shared with the prior generation by the defs-stable premise).
pub struct DefsStableFilePlan {
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
  /// The file's fresh dataflow rows.
  pub flows: Vec<crate::DataflowRow>,
}

/// A defs-stable session's full surgery plan: per-file slices plus the GLOBAL
/// derivations, which are session-wide by nature (pairing acts over the entire sketch
/// ledger; the sigs family is one ledger).
pub struct DefsStablePlan {
  /// The edited files' slices. Dense ids never move under this compose, so the slices
  /// are independent — order is irrelevant to the result (the savers canonicalize).
  pub files: Vec<DefsStableFilePlan>,
  /// The repaired GLOBAL pair set (`a < b`, sorted — the pipeline's emission order).
  pub fresh_pairs: Vec<(u64, u64, u8)>,
  /// Endpoints of every added/removed/relabeled pair — their similar segments rewrite.
  pub changed_srcs: Vec<u32>,
  /// The sigs family's complete new row set (the swapped ledger the repair paired).
  pub sig_rows: Vec<crate::SigFamilyRow>,
}

/// Edge-row class within a source's slab segment: the log emits them in this fixed phase
/// order (ingest containment → pre-link co-change → resolution with DATA_FLOWS spliced →
/// near-clone pairs → request/notify matches), and `compact_src_major`'s counting scatter
/// preserves per-source order into the slabs. The classes partition the etype space, so a
/// segment splits by CLASS — no positional guessing — and any interleaving is proof of a
/// premise violation (→ error → full pipeline).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum EdgeClass {
  Containment,
  Cochange,
  Resolution,
  Similar,
  Request,
}

pub(crate) fn edge_class(etype: EdgeType) -> EdgeClass {
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
pub(crate) struct SlabRow {
  pub(crate) src_local: u32,
  pub(crate) dst_key: u64,
  pub(crate) dst_ord: u32,
  pub(crate) etype: u16,
}

const EDGE_SLAB_HEADER: usize = 20;
const EDGE_ROW: usize = 18;
pub(crate) const EDGE_TOC_HEADER: usize = 20;
pub(crate) const EDGE_TOC_ROW: usize = 24;
pub(crate) const EVIDENCE_TOC_HEADER: usize = 20;
pub(crate) const EVIDENCE_TOC_ROW: usize = 32;

pub(crate) fn read_edge_slab(path: &Path, bucket: usize) -> io::Result<Vec<SlabRow>> {
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

pub(crate) fn encode_edge_slab(bucket: usize, rows: &[SlabRow]) -> (Vec<u8>, u64) {
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
pub(crate) fn localize(
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

/// One edited file's dense-space coordinates, resolved once up front.
struct FileCtx {
  base: u64,
  rows: u32,
  bucket: usize,
}

/// Compose the bucketed generation for a defs-stable session (one or more edited
/// files). The caller has verified eligibility and stable fields per file; this module
/// re-verifies every structural premise it leans on and errors — never guesses — on any
/// surprise.
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
  if plan.files.is_empty() {
    return Err(io::Error::other("defs-stable: empty session"));
  }
  let mut ctxs: Vec<FileCtx> = Vec::with_capacity(plan.files.len());
  for file in &plan.files {
    let &(_, base, rows) = map
      .files()
      .iter()
      .find(|&&(key, _, _)| key == file.file_key)
      .ok_or_else(|| io::Error::other("defs-stable: file outside the prior universe"))?;
    if file.node_rows.len() != rows as usize {
      return Err(io::Error::other("defs-stable: node row count moved — not defs-stable"));
    }
    ctxs.push(FileCtx {
      base,
      rows,
      bucket: bases.partition_point(|&b| b <= base) - 1,
    });
  }
  // Ranges are disjoint (one manifest entry per key); sorted for the owner lookup.
  let mut ranges: Vec<(u64, u64, usize)> = ctxs
    .iter()
    .enumerate()
    .map(|(i, ctx)| (ctx.base, ctx.base + u64::from(ctx.rows), i))
    .collect();
  ranges.sort_unstable();
  let owner_of = |dense: u64| -> Option<usize> {
    let at = ranges.partition_point(|&(start, _, _)| start <= dense);
    (at > 0 && dense < ranges[at - 1].1).then(|| ranges[at - 1].2)
  };
  let in_any_range = |dense: u64| owner_of(dense).is_some();

  // ---- family carries, hoisted and PARALLEL (independent destination dir locks;
  // hard links on purpose — inode identity keeps chained builds warm, clonefile
  // measured-and-rejected). Rewritten members rename over their linked entries;
  // untouched members are done the moment this returns. ----
  crate::carry_families(
    prior,
    staging,
    &[
      (crate::kg::NODES_DIR, crate::kg::is_nodes_member as fn(&str) -> bool),
      (crate::evidence::EVIDENCE_DIR, crate::evidence::is_evidence_member),
      (crate::edgestore::EDGES_DIR, crate::edgestore::is_edges_member),
      (crate::usagestore::USAGE_DIR, crate::usagestore::is_usage_member),
      (crate::sigstore::SIGS_DIR, crate::sigstore::is_sigs_member),
    ],
  )?;

  // ---- scc: recompute ONLY when an edited file's call set moved ----
  // The condensation reads the (src, dst) CALLS pair set; parallel edges and every other
  // etype are invisible to it. Compare each file's prior and fresh call sets first — the
  // common body edit keeps them, and then every scc_size byte is provably stable.
  let mut slab_cache: HashMap<usize, Vec<SlabRow>> = HashMap::new();
  let mut read_bucket = |bucket: usize| -> io::Result<Vec<SlabRow>> {
    if let Some(rows) = slab_cache.get(&bucket) {
      return Ok(rows.clone());
    }
    let rows = read_edge_slab(
      &prior.join(crate::EDGES_DIR).join(format!("{bucket:04}.bin")),
      bucket,
    )?;
    slab_cache.insert(bucket, rows.clone());
    Ok(rows)
  };
  let starts: HashMap<u64, u64> =
    map.files().iter().map(|&(key, start, _)| (key, start)).collect();
  let densify = |row: &SlabRow| -> io::Result<u32> {
    starts
      .get(&row.dst_key)
      .map(|&start| (start + u64::from(row.dst_ord)) as u32)
      .ok_or_else(|| io::Error::other("defs-stable: edge destination outside the universe"))
  };
  let mut any_calls_moved = false;
  for (file, ctx) in plan.files.iter().zip(&ctxs) {
    let bucket_rows = read_bucket(ctx.bucket)?;
    let lo_local = (ctx.base - bases[ctx.bucket]) as u32;
    let hi_local = lo_local + ctx.rows;
    let mut prior_calls: HashSet<(u32, u32)> = HashSet::new();
    for row in &bucket_rows {
      if row.src_local >= lo_local
        && row.src_local < hi_local
        && EdgeType(row.etype).base() == EdgeType::CALLS
      {
        prior_calls.insert((row.src_local - lo_local, densify(row)?));
      }
    }
    let fresh_calls: HashSet<(u32, u32)> = file
      .edges
      .iter()
      .filter(|(_, _, etype)| etype.base() == EdgeType::CALLS)
      .map(|&(from, to, _)| ((u64::from(from) - ctx.base) as u32, to))
      .collect();
    if prior_calls != fresh_calls {
      any_calls_moved = true;
      break;
    }
  }
  let scc_new: Option<Vec<u32>> = if !any_calls_moved {
    None
  } else {
    // The ripple can reach any bucket: the global CALLS list is the prior GRAPH's calls
    // (zero slab decode — the CSR slices are the same truth) with every edited file's
    // replaced by its plan's, through the seal's own condensation.
    let node_count = bases[buckets] as usize;
    let mut log = vorpal_graph::EdgeLog::default();
    for u in 0..node_count as u32 {
      if in_any_range(u64::from(u)) {
        continue; // replaced by the plans' fresh calls below
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
    for file in &plan.files {
      for &(from, to, etype) in &file.edges {
        if etype.base() == EdgeType::CALLS {
          log.push(from, to, etype);
        }
      }
    }
    Some(crate::scc::scc_sizes(node_count, &log))
  };

  // ---- node store: span/content patches per file + scc patches wherever they land ----
  let respan_plans: Vec<FileRespan> = plan
    .files
    .iter()
    .map(|file| FileRespan {
      file_key: file.file_key,
      rows: file.node_rows.clone(),
      ref_spans: HashMap::new(),
      call_spans: HashMap::new(),
    })
    .collect();
  let respan_refs: Vec<(u64, &FileRespan)> =
    respan_plans.iter().map(|p| (p.file_key, p)).collect();
  let node_fold = crate::respan::rebuild_node_buckets(
    staging,
    prior,
    &map,
    &bases,
    &respan_refs,
    scc_new.as_deref(),
  )?;

  // ---- evidence: every edited bucket swaps its edited files' rows; the rest link ----
  let edited_buckets: HashSet<usize> = ctxs.iter().map(|ctx| ctx.bucket).collect();
  let store = crate::evidence::EvidenceStore::open(prior)
    .ok_or_else(|| io::Error::other("defs-stable: prior evidence unreadable"))?;
  let evidence_dir = staging.join(crate::EVIDENCE_DIR);
  let mut ev_toc = fs::read(prior.join(crate::EVIDENCE_TOC))
    .map_err(|_| io::Error::other("defs-stable: prior evidence TOC unreadable"))?;
  // Dropped rows keep their owning file for the usage delta ((name_hash, file_key)).
  let mut dropped_rows: Vec<(usize, crate::EvidenceRow)> = Vec::new();
  for (bucket, window) in bases.windows(2).enumerate() {
    let name = format!("{bucket:04}.bin");
    if !edited_buckets.contains(&bucket) {
      continue; // link-carried by the hoisted family batch
    }
    let mut rows = store.rows_of_bucket(bucket);
    rows.retain(|row| match owner_of(u64::from(row.from)) {
      Some(owner) => {
        dropped_rows.push((owner, row.clone()));
        false
      }
      None => true,
    });
    for (file, ctx) in plan.files.iter().zip(&ctxs) {
      if ctx.bucket == bucket {
        rows.extend(file.evidence.iter().cloned());
      }
    }
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

  // ---- edges: rebuild every edited bucket + every changed similar endpoint's bucket ----
  let mut segment_srcs: HashMap<usize, HashSet<u32>> = HashMap::new(); // bucket -> dense srcs to rewrite
  for ctx in &ctxs {
    for src in ctx.base..ctx.base + u64::from(ctx.rows) {
      segment_srcs.entry(ctx.bucket).or_default().insert(src as u32);
    }
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
  let mut edge_toc = fs::read(prior.join(crate::EDGES_TOC))
    .map_err(|_| io::Error::other("defs-stable: prior edge TOC unreadable"))?;
  let mut rebuilt_rows: HashMap<usize, Vec<SlabRow>> = HashMap::new();
  for bucket in 0..buckets {
    let name = format!("{bucket:04}.bin");
    let Some(rewrite_srcs) = segment_srcs.get(&bucket) else {
      continue; // link-carried by the hoisted family batch
    };
    let bucket_base = bases[bucket];
    let prior_rows = read_bucket(bucket)?;
    let mut out: Vec<SlabRow> = Vec::with_capacity(prior_rows.len());
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
      let owner = owner_of(u64::from(src_dense));
      if let Some(owner) = owner {
        let file = &plan.files[owner];
        for &(from, to, etype) in file.edges.iter().filter(|(from, _, _)| *from == src_dense)
        {
          debug_assert_eq!(from, src_dense);
          out.push(localize(&map, bucket_base, from, to, etype)?);
        }
      } else {
        out.extend(keep(EdgeClass::Resolution));
      }
      if let Some(similar) = similar_of.get(&src_dense) {
        out.extend_from_slice(similar);
      }
      if let Some(owner) = owner {
        let file = &plan.files[owner];
        for &(from, to, etype) in
          file.request_edges.iter().filter(|(from, _, _)| *from == src_dense)
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

  // ---- usage: the session's postings delta, bucket-scoped (untouched buckets link) ----
  let removed: HashSet<(u32, u64)> = dropped_rows
    .iter()
    .map(|&(owner, ref row)| (row.name_hash, plan.files[owner].file_key))
    .collect();
  let mut added: Vec<(u32, u64)> = plan
    .files
    .iter()
    .flat_map(|file| file.evidence.iter().map(|row| (row.name_hash, file.file_key)))
    .collect();
  added.sort_unstable();
  added.dedup();
  crate::usagestore::apply_delta(staging, prior, buckets as u32, &removed, &added)?;

  // ---- sigs: the repaired ledger, digest-carried per bucket ----
  crate::sigstore::save_sigs(staging, &plan.sig_rows, &map, Some(prior))?;

  // ---- dataflow: the canonical saver re-sorts; filter + extend is exact ----
  let mut flows = crate::dataflow::load_dataflow(prior)
    .ok_or_else(|| io::Error::other("defs-stable: prior dataflow unreadable"))?;
  flows.retain(|row| !in_any_range(u64::from(row.from)));
  for file in &plan.files {
    flows.extend(file.flows.iter().cloned());
  }
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
