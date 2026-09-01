//! The RESPAN compose (P4.5b): for edits where every non-span product field is byte-equal
//! (comment insertions, blank lines, formatting — the class BETWEEN the stamp-only cutoff
//! and a semantic edit), resolution outcomes are a span-free theorem: same names, forms,
//! qualifiers, argument shapes, and candidate universe ⇒ same targets, same confidences,
//! same everything except WHERE. So the next generation composes mechanically from the
//! prior one:
//!
//! - node slabs: only the edited files' `span_start`/`span_end`/`content_hash` column
//!   entries change (names, paths, signatures, eids, flags, kinds, scc are all proven
//!   stable — and VERIFIED against fresh pipeline-derived rows, never assumed);
//! - evidence slabs: the edited files' rows re-span through an exact
//!   `(old span, name hash) → new span` map and the slab re-encodes through the SAME
//!   builder the full save uses;
//! - dataflow rows re-span the same way; edge slabs, the usage family, `names.idx`, and
//!   `graph.bin` hard-link UNTOUCHED (dense ids and edges are unchanged — only the node
//!   fold in `graph.stamp` refreshes);
//! - every other bucket of every family hard-links.
//!
//! The composed generation must be byte-identical to what the full pipeline would commit
//! for the same tree — the convergence gate in `tests/pack_v2.rs` holds it there. Any
//! verification failure inside this module aborts the compose (the caller falls back to
//! the full pipeline); it never commits a guess.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::Path;

use vorpal_segment::{Segment, SegmentBuilder};

use crate::kg::{NODES_DIR, NODES_TOC, NodeIdMap};

/// One edited file's respan plan, derived by the caller from the prior and fresh products.
pub struct FileRespan {
  pub file_key: u64,
  /// Fresh `(span_start, span_end, content_hash)` per layout ordinal — EVERY row of the
  /// file, file node first, derived by running the fresh product through the pipeline's
  /// own ingest (`Ingestor::ingest_product`) so the values are pipeline-exact.
  pub rows: Vec<(u32, u32, u64)>,
  /// `(old span_start, old span_end, referenced-name hash32) → new span` for the file's
  /// evidence and dataflow rows. Built positionally from the (validated-equal) ref
  /// sequences of the prior and fresh products.
  pub ref_spans: HashMap<(u32, u32, u32), (u32, u32)>,
  /// `(old span_start, old span_end) → new span` — the dataflow rows carry no name hash;
  /// the caller guarantees this projection is injective (it aborts on collision).
  pub call_spans: HashMap<(u32, u32), (u32, u32)>,
}

/// Compose the node store, evidence, dataflow, and the untouched families of `staging`
/// from `prior` under `plans`. The caller has already validated eligibility; this module
/// re-verifies everything it touches and errors (→ full pipeline) on any surprise.
pub fn respan_generation(
  staging: &Path,
  prior: &Path,
  plans: &[FileRespan],
) -> io::Result<()> {
  let map = NodeIdMap::from_dir(prior)
    .ok_or_else(|| io::Error::other("respan: prior generation has no bucketed node store"))?;
  let by_key: HashMap<u64, &FileRespan> = plans.iter().map(|p| (p.file_key, p)).collect();
  if by_key.len() != plans.len() {
    return Err(io::Error::other("respan: duplicate file in plan"));
  }
  // Which buckets contain planned files, and each planned file's dense range.
  let bases = map.bases().to_vec();
  let buckets = bases.len() - 1;
  let mut planned_in_bucket: Vec<Vec<(u64, u64, u32)>> = vec![Vec::new(); buckets];
  for &(key, start, rows) in map.files() {
    if by_key.contains_key(&key) {
      let bucket = bases.partition_point(|&b| b <= start) - 1;
      planned_in_bucket[bucket].push((key, start, rows));
    }
  }
  let planned_files: usize = planned_in_bucket.iter().map(Vec::len).sum();
  if planned_files != plans.len() {
    return Err(io::Error::other(
      "respan: a planned file is not in the prior generation",
    ));
  }

  // ---- node store (shared with the defs-stable compose) ----
  let plan_refs: Vec<(u64, &FileRespan)> = plans.iter().map(|p| (p.file_key, p)).collect();
  let node_fold = rebuild_node_buckets(staging, prior, &map, &bases, &plan_refs, None)?;

  // ---- evidence ----
  let store = crate::evidence::EvidenceStore::open(prior)
    .ok_or_else(|| io::Error::other("respan: prior evidence unreadable"))?;
  let evidence_dir = staging.join(crate::evidence::EVIDENCE_DIR);
  fs::create_dir_all(&evidence_dir)?;
  let prior_ev_toc = fs::read(prior.join(crate::evidence::EVIDENCE_TOC))
    .map_err(|_| io::Error::other("respan: prior evidence TOC unreadable"))?;
  let mut ev_toc = prior_ev_toc;
  for bucket in 0..buckets {
    let name = format!("{bucket:04}.bin");
    if planned_in_bucket[bucket].is_empty() {
      let (from, to) = (
        prior.join(crate::evidence::EVIDENCE_DIR).join(&name),
        evidence_dir.join(&name),
      );
      let _ = fs::remove_file(&to);
      if fs::hard_link(&from, &to).is_err() {
        fs::copy(&from, &to)?;
      }
      continue;
    }
    let mut rows = store.rows_of_bucket(bucket);
    for row in &mut rows {
      let Some((key, _)) = map.locate(row.from) else {
        return Err(io::Error::other("respan: evidence row outside the universe"));
      };
      let Some(plan) = by_key.get(&key) else {
        continue;
      };
      let new_span = plan
        .ref_spans
        .get(&(row.span_start, row.span_end, row.name_hash))
        .ok_or_else(|| {
          io::Error::other("respan: evidence row has no span mapping — not a respan edit")
        })?;
      row.span_start = new_span.0;
      row.span_end = new_span.1;
    }
    let built = crate::evidence::build_slab(bucket, bases[bucket], &rows, &map)?;
    let tmp = evidence_dir.join(format!("{name}.tmp"));
    fs::write(&tmp, &built.bytes)?;
    fs::rename(&tmp, evidence_dir.join(&name))?;
    // Evidence TOC row: rows/pool counts unchanged; len + digest replaced.
    let at = 20 + bucket * 32;
    ev_toc
      .get_mut(at + 16..at + 24)
      .ok_or_else(|| io::Error::other("respan: evidence TOC too short"))?
      .copy_from_slice(&(built.bytes.len() as u64).to_le_bytes());
    ev_toc
      .get_mut(at + 24..at + 32)
      .ok_or_else(|| io::Error::other("respan: evidence TOC too short"))?
      .copy_from_slice(&built.digest.to_le_bytes());
    if built.rows != u64::from_le_bytes(ev_toc[at..at + 8].try_into().unwrap_or([0; 8])) {
      return Err(io::Error::other("respan: evidence row count moved — not a respan edit"));
    }
  }
  let toc_tmp = evidence_dir.join("toc.bin.tmp");
  fs::write(&toc_tmp, &ev_toc)?;
  fs::rename(&toc_tmp, staging.join(crate::evidence::EVIDENCE_TOC))?;

  // ---- untouched families: edges, usage, sigs — link members + TOCs verbatim ----
  // (sigs carry: the compose eligibility ladder proves every sketch and shingle count
  // equal, and (file_key, ordinal) keys are span-free — the slabs are byte-identical.)
  for (family, toc_rel) in [
    (crate::edgestore::EDGES_DIR, crate::edgestore::EDGES_TOC),
    (crate::usagestore::USAGE_DIR, crate::usagestore::USAGE_TOC),
    (crate::sigstore::SIGS_DIR, crate::sigstore::SIGS_TOC),
  ] {
    fs::create_dir_all(staging.join(family))?;
    for entry in fs::read_dir(prior.join(family))?.flatten() {
      let Ok(name) = entry.file_name().into_string() else {
        continue;
      };
      let (from, to) = (prior.join(family).join(&name), staging.join(family).join(&name));
      let _ = fs::remove_file(&to);
      if fs::hard_link(&from, &to).is_err() {
        fs::copy(&from, &to)?;
      }
    }
    if !staging.join(toc_rel).is_file() {
      return Err(io::Error::other("respan: family TOC missing after link"));
    }
  }

  // ---- dataflow: respan the planned files' rows, canonical re-save ----
  let mut flows = crate::dataflow::load_dataflow(prior)
    .ok_or_else(|| io::Error::other("respan: prior dataflow unreadable"))?;
  for row in &mut flows {
    let Some((key, _)) = map.locate(row.from) else {
      return Err(io::Error::other("respan: dataflow row outside the universe"));
    };
    let Some(plan) = by_key.get(&key) else {
      continue;
    };
    let new_span = plan.call_spans.get(&(row.span.0, row.span.1)).ok_or_else(|| {
      io::Error::other("respan: dataflow row has no span mapping — not a respan edit")
    })?;
    row.span = *new_span;
  }
  crate::dataflow::save_dataflow(staging, flows)?;

  // ---- derived caches: names.idx links (names and dense ids unchanged); graph.bin links
  // (edges unchanged) with a refreshed stamp (the node fold moved with the spans) ----
  for cache in ["names.idx", "graph.bin"] {
    let (from, to) = (prior.join(cache), staging.join(cache));
    if from.exists() {
      let _ = fs::remove_file(&to);
      if fs::hard_link(&from, &to).is_err() {
        fs::copy(&from, &to)?;
      }
    }
  }
  if staging.join("graph.bin").exists() {
    if let Some(stamp) = crate::edgestore::expected_stamp(staging, node_fold) {
      crate::edgestore::write_stamp(staging, stamp)?;
    }
  }
  Ok(())
}

/// Rebuild the node-store buckets for a set of per-file span/content patches — and, when
/// `scc_new` is given, for every bucket whose `scc_size` column moved (the defs-stable
/// compose's call-graph ripple; `None` for the respan compose, whose edges are untouched).
/// Unpatched buckets hard-link. Returns the vseg fold over ALL buckets in order (the node
/// half of the graph-cache stamp).
pub(crate) fn rebuild_node_buckets(
  staging: &Path,
  prior: &Path,
  map: &NodeIdMap,
  bases: &[u64],
  plans: &[(u64, &FileRespan)],
  scc_new: Option<&[u32]>,
) -> io::Result<u64> {
  let buckets = bases.len() - 1;
  let by_key: HashMap<u64, &FileRespan> = plans.iter().map(|&(key, plan)| (key, plan)).collect();
  if by_key.len() != plans.len() {
    return Err(io::Error::other("respan: duplicate file in plan"));
  }
  let mut planned_in_bucket: Vec<Vec<(u64, u64, u32)>> = vec![Vec::new(); buckets];
  for &(key, start, rows) in map.files() {
    if by_key.contains_key(&key) {
      let bucket = bases.partition_point(|&b| b <= start) - 1;
      planned_in_bucket[bucket].push((key, start, rows));
    }
  }
  let planned_files: usize = planned_in_bucket.iter().map(Vec::len).sum();
  if planned_files != plans.len() {
    return Err(io::Error::other(
      "respan: a planned file is not in the prior generation",
    ));
  }

  let nodes_dir = staging.join(NODES_DIR);
  fs::create_dir_all(&nodes_dir)?;
  let prior_toc_bytes = fs::read(prior.join(NODES_TOC))
    .map_err(|_| io::Error::other("respan: prior node TOC unreadable"))?;
  let mut new_vseg_meta: Vec<Option<(u64, u64)>> = vec![None; buckets]; // (len, digest)
  let mut vseg_fold = xxhash_rust::xxh3::Xxh3::new();
  for bucket in 0..buckets {
    let vseg_name = format!("{bucket:04}.vseg");
    let heap_name = format!("{bucket:04}.heap");
    let scc_bucket_differs = scc_new.is_some_and(|scc| {
      let (lo, hi) = (bases[bucket] as usize, bases[bucket + 1] as usize);
      // Cheap pre-read of just the scc column decides linking without a full rebuild.
      let prior_bytes = match fs::read(prior.join(NODES_DIR).join(&vseg_name)) {
        Ok(bytes) => bytes,
        Err(_) => return true, // unreadable: let the rebuild path surface the error
      };
      match Segment::open_owned(prior_bytes)
        .ok()
        .and_then(|seg| seg.column_index("scc_size").and_then(|i| seg.column_at(i).and_then(|c| c.as_slice::<u32>().map(<[u32]>::to_vec))))
      {
        Some(col) => col.as_slice() != &scc[lo..hi],
        None => true,
      }
    });
    if planned_in_bucket[bucket].is_empty() && !scc_bucket_differs {
      for name in [&vseg_name, &heap_name] {
        let (from, to) = (prior.join(NODES_DIR).join(name), nodes_dir.join(name));
        let _ = fs::remove_file(&to);
        if fs::hard_link(&from, &to).is_err() {
          fs::copy(&from, &to)?;
        }
      }
      let linked = fs::read(nodes_dir.join(&vseg_name))?;
      vseg_fold.update(&linked);
      continue;
    }
    // Rebuild this bucket's vseg: prior columns verbatim, with the planned files'
    // span/content-hash entries replaced. The heap is byte-identical (strings unchanged)
    // and hard-links.
    let prior_bytes = fs::read(prior.join(NODES_DIR).join(&vseg_name))?;
    let segment = Segment::open_owned(prior_bytes)
      .map_err(|err| io::Error::other(format!("respan: prior node slab: {err}")))?;
    let col = |name: &str| -> io::Result<usize> {
      segment
        .column_index(name)
        .ok_or_else(|| io::Error::other(format!("respan: prior slab missing column {name}")))
    };
    let slice_u32 = |idx: usize| -> io::Result<Vec<u32>> {
      segment
        .column_at(idx)
        .and_then(|c| c.as_slice::<u32>().map(<[u32]>::to_vec))
        .ok_or_else(|| io::Error::other("respan: column not sliceable"))
    };
    let slice_u64 = |idx: usize| -> io::Result<Vec<u64>> {
      segment
        .column_at(idx)
        .and_then(|c| c.as_slice::<u64>().map(<[u64]>::to_vec))
        .ok_or_else(|| io::Error::other("respan: column not sliceable"))
    };
    let slice_u8 = |idx: usize| -> io::Result<Vec<u8>> {
      segment
        .column_at(idx)
        .and_then(|c| c.as_slice::<u8>().map(<[u8]>::to_vec))
        .ok_or_else(|| io::Error::other("respan: column not sliceable"))
    };
    let kind = slice_u8(col("kind")?)?;
    let name_off = slice_u32(col("name_off")?)?;
    let name_len = slice_u32(col("name_len")?)?;
    let path_off = slice_u32(col("path_off")?)?;
    let path_len = slice_u32(col("path_len")?)?;
    let sig_off = slice_u32(col("sig_off")?)?;
    let sig_len = slice_u32(col("sig_len")?)?;
    let mut content_hash = slice_u64(col("content_hash")?)?;
    let eid_lo = slice_u64(col("eid_lo")?)?;
    let eid_hi = slice_u64(col("eid_hi")?)?;
    let flags = slice_u8(col("flags")?)?;
    let mut span_start = slice_u32(col("span_start")?)?;
    let mut span_end = slice_u32(col("span_end")?)?;
    let mut scc_size = slice_u32(col("scc_size")?)?;
    if let Some(scc) = scc_new {
      let (lo, hi) = (bases[bucket] as usize, bases[bucket + 1] as usize);
      scc_size.copy_from_slice(&scc[lo..hi]);
    }
    let bucket_base = bases[bucket];
    for &(key, start, rows) in &planned_in_bucket[bucket] {
      let plan = by_key[&key];
      if plan.rows.len() != rows as usize {
        return Err(io::Error::other(
          "respan: fresh row count differs from prior — not a respan edit",
        ));
      }
      let local = (start - bucket_base) as usize;
      for (i, &(new_start, new_end, new_hash)) in plan.rows.iter().enumerate() {
        span_start[local + i] = new_start;
        span_end[local + i] = new_end;
        content_hash[local + i] = new_hash;
      }
    }
    let mut builder = SegmentBuilder::new(0);
    let build_err = |_| io::Error::other("respan: node slab rebuild failed");
    builder.add_u8("kind", &kind).map_err(build_err)?;
    builder.add_u32("name_off", &name_off).map_err(build_err)?;
    builder.add_u32("name_len", &name_len).map_err(build_err)?;
    builder.add_u32("path_off", &path_off).map_err(build_err)?;
    builder.add_u32("path_len", &path_len).map_err(build_err)?;
    builder.add_u32("sig_off", &sig_off).map_err(build_err)?;
    builder.add_u32("sig_len", &sig_len).map_err(build_err)?;
    builder.add_u64("content_hash", &content_hash).map_err(build_err)?;
    builder.add_u64("eid_lo", &eid_lo).map_err(build_err)?;
    builder.add_u64("eid_hi", &eid_hi).map_err(build_err)?;
    builder.add_u8("flags", &flags).map_err(build_err)?;
    builder.add_u32("span_start", &span_start).map_err(build_err)?;
    builder.add_u32("span_end", &span_end).map_err(build_err)?;
    builder.add_u32("scc_size", &scc_size).map_err(build_err)?;
    let bytes = builder.build().map_err(build_err)?;
    let digest = xxhash_rust::xxh3::xxh3_64(&bytes);
    vseg_fold.update(&bytes);
    let tmp = nodes_dir.join(format!("{vseg_name}.tmp"));
    fs::write(&tmp, &bytes)?;
    fs::rename(&tmp, nodes_dir.join(&vseg_name))?;
    let (from, to) = (prior.join(NODES_DIR).join(&heap_name), nodes_dir.join(&heap_name));
    let _ = fs::remove_file(&to);
    if fs::hard_link(&from, &to).is_err() {
      fs::copy(&from, &to)?;
    }
    new_vseg_meta[bucket] = Some((bytes.len() as u64, digest));
  }
  // Node TOC: prior bytes with the rebuilt buckets' vseg len/digest spliced (rows, heap
  // columns, and the file table are unchanged by construction).
  let mut toc = prior_toc_bytes;
  for (bucket, meta) in new_vseg_meta.iter().enumerate() {
    if let Some((len, digest)) = meta {
      let at = 20 + bucket * 36 + 4; // header + rows-per-bucket stride + rows u32
      toc
        .get_mut(at..at + 8)
        .ok_or_else(|| io::Error::other("respan: node TOC too short"))?
        .copy_from_slice(&len.to_le_bytes());
      toc
        .get_mut(at + 8..at + 16)
        .ok_or_else(|| io::Error::other("respan: node TOC too short"))?
        .copy_from_slice(&digest.to_le_bytes());
    }
  }
  let toc_tmp = nodes_dir.join("toc.bin.tmp");
  fs::write(&toc_tmp, &toc)?;
  fs::rename(&toc_tmp, staging.join(NODES_TOC))?;
  let node_fold = vseg_fold.digest();
  Ok(node_fold)
}
