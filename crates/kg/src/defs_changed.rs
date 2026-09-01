//! The DEFS-CHANGED compose (P4.5c-3; multi-file sessions since S2-b): artifact surgery
//! for edits whose DEFINITION SETS moved — adds, removes, renames, signature changes.
//! The shift law makes it tractable: bucket-major order is one sequence, so the edited
//! files' row deltas accumulate — `translate(x) = x + Σ_{i: x ≥ old_end_i} delta_i` —
//! bucket-base-relative locals cancel for every bucket past the last edited one in it,
//! and every durable coordinate is identity-coded `(file_key, ordinal)`, stable unless
//! it targets an edited-file ordinal that moved — whose referrers are exactly the
//! usage-dirty files the session already re-resolved. A defs-STABLE member of a mixed
//! session is simply a block with delta 0 and every ordinal unmoved. What remains is
//! mechanical: splice each edited bucket's node columns and heap around the scratch
//! seals' bytes (file heap runs are back-to-back by the writer's gather order —
//! asserted, not assumed), swap the session files' evidence/edge rows, translate the
//! global dense-id artifacts (dataflow, names.idx, the graph cache), and re-splice the
//! TOCs. Any surprise is an error: the caller falls back to the full pipeline, never
//! commits a guess.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::Path;

use vorpal_segment::{Segment, SegmentBuilder};

use crate::EdgeType;
use crate::defs_stable::{
  EDGE_TOC_HEADER, EDGE_TOC_ROW, EVIDENCE_TOC_HEADER, EVIDENCE_TOC_ROW, EdgeClass, SlabRow,
  edge_class, encode_edge_slab, localize, read_edge_slab,
};
use crate::kg::{NODES_DIR, NodeIdMap};

/// One session file's fresh outcomes, in the SUCCESSOR dense space.
pub struct ChangedFilePlan {
  pub file_key: u64,
  pub evidence: Vec<crate::EvidenceRow>,
  pub edges: Vec<(u32, u32, EdgeType)>,
  pub request_edges: Vec<(u32, u32, EdgeType)>,
  pub flows: Vec<crate::DataflowRow>,
}

/// The full defs-changed surgery plan. `files` holds EVERY session file's fresh
/// outcomes (edited and usage-dirty alike, any order); `edited` names the files whose
/// definition sets moved, each with its per-OLD-ordinal unmoved flags.
pub struct DefsChangedPlan {
  pub files: Vec<ChangedFilePlan>,
  /// Per edited file: `(file_key, unmoved)`. `unmoved[ord]` says whether that
  /// definition's row identity AND ordinal survived unchanged. An unmoved ordinal's
  /// successor dense id is `new_start + ordinal` (identical to its prior id when no
  /// earlier block's delta reaches it) — which is exactly why its referrers are not
  /// usage-dirty: identity-coded `(file_key, ordinal)` slab rows are invariant, and the
  /// dense translation is total. A moved/removed ordinal must never be targeted by a
  /// carried row (premise violation → error).
  pub edited: Vec<(u64, Vec<bool>)>,
  /// The repaired GLOBAL pair set (`a < b`, sorted) in the successor space.
  pub fresh_pairs: Vec<(u64, u64, u8)>,
  /// Endpoints of every added/removed/relabeled pair, successor space.
  pub changed_srcs: Vec<u32>,
  /// The sigs family's complete new row set, successor space.
  pub sig_rows: Vec<crate::SigFamilyRow>,
}

/// One edited file's shift block, resolved against the prior universe.
struct ShiftBlock<'a> {
  file_key: u64,
  old_start: u64,
  old_end: u64,
  /// `old_start + Σ delta(earlier blocks)`.
  new_start: u64,
  new_rows: u32,
  bucket: usize,
  /// See [`DefsChangedPlan::edited`].
  unmoved: &'a [bool],
  /// The file's scratch single-file seal.
  fresh: &'a crate::Kg,
}

/// The multi-block shift law: ascending disjoint blocks + prefix deltas.
struct Shift<'a> {
  blocks: Vec<ShiftBlock<'a>>,
  /// `prefix[i]` = Σ delta of blocks `[0, i)`; len = blocks.len() + 1.
  prefix: Vec<i64>,
}

impl Shift<'_> {
  fn translate(&self, prior_dense: u64) -> Option<u64> {
    let i = self.blocks.partition_point(|b| b.old_end <= prior_dense);
    if i < self.blocks.len() && self.blocks[i].old_start <= prior_dense {
      // Inside an edited block: an UNMOVED ordinal keeps its coordinate — successor id
      // `new_start + ordinal`; anything else has no successor coordinate.
      let block = &self.blocks[i];
      let ordinal = (prior_dense - block.old_start) as usize;
      return block
        .unmoved
        .get(ordinal)
        .copied()
        .unwrap_or(false)
        .then_some(block.new_start + (prior_dense - block.old_start));
    }
    Some((prior_dense as i64 + self.prefix[i]) as u64)
  }

  fn block_containing_old(&self, prior_dense: u64) -> Option<&ShiftBlock<'_>> {
    let i = self.blocks.partition_point(|b| b.old_end <= prior_dense);
    (i < self.blocks.len() && self.blocks[i].old_start <= prior_dense)
      .then(|| &self.blocks[i])
  }
}

/// Compose the successor generation for a defs-changed session (one or more edited
/// files, plus their usage-dirty referrers). `fresh` pairs each edited file's key with
/// its scratch single-file seal — the seal's rows, strings, and heap bytes ARE the
/// scratch build's for that block.
pub fn compose_defs_changed(
  staging: &Path,
  prior: &Path,
  prior_kg: &crate::Kg,
  fresh: &[(u64, &crate::Kg)],
  plan: &DefsChangedPlan,
) -> io::Result<()> {
  let map = NodeIdMap::from_dir(prior)
    .ok_or_else(|| io::Error::other("defs-changed: prior has no bucketed node store"))?;
  let bases = map.bases().to_vec();
  let buckets = bases.len() - 1;
  if plan.edited.is_empty() || plan.files.is_empty() {
    return Err(io::Error::other("defs-changed: empty plan"));
  }
  if fresh.len() != plan.edited.len() {
    return Err(io::Error::other("defs-changed: seal set disagrees with the edited set"));
  }

  // ---- the shift law's blocks, ascending, with cumulative deltas ----
  let mut blocks: Vec<ShiftBlock<'_>> = Vec::with_capacity(plan.edited.len());
  for (key, unmoved) in &plan.edited {
    let &(_, old_start, old_rows) = map
      .files()
      .iter()
      .find(|&&(k, _, _)| k == *key)
      .ok_or_else(|| io::Error::other("defs-changed: edited file outside the prior universe"))?;
    if unmoved.len() != old_rows as usize {
      return Err(io::Error::other("defs-changed: unmoved set disagrees with the prior block"));
    }
    let &(_, seal) = fresh
      .iter()
      .find(|&&(k, _)| k == *key)
      .ok_or_else(|| io::Error::other("defs-changed: edited file has no seal"))?;
    let new_rows = u32::try_from(seal.node_count())
      .map_err(|_| io::Error::other("defs-changed: fresh seal beyond the row space"))?;
    blocks.push(ShiftBlock {
      file_key: *key,
      old_start,
      old_end: old_start + u64::from(old_rows),
      new_start: 0, // filled below in old_start order
      new_rows,
      bucket: bases.partition_point(|&b| b <= old_start) - 1,
      unmoved,
      fresh: seal,
    });
  }
  blocks.sort_by_key(|b| b.old_start);
  let mut prefix: Vec<i64> = Vec::with_capacity(blocks.len() + 1);
  prefix.push(0);
  for block in &mut blocks {
    let cum = *prefix.last().expect("non-empty prefix");
    block.new_start = (block.old_start as i64 + cum) as u64;
    prefix
      .push(cum + i64::from(block.new_rows) - (block.old_end - block.old_start) as i64);
  }
  let shift = Shift { blocks, prefix };
  let edited_buckets: HashSet<usize> = shift.blocks.iter().map(|b| b.bucket).collect();

  // ---- the successor identity: bases, file table, map ----
  // A bucket boundary shifts by the cumulative delta of every block strictly below it
  // (files never straddle buckets, so a block is entirely below or entirely at/above).
  let mut new_bases = bases.clone();
  for base in &mut new_bases {
    let i = shift.blocks.partition_point(|b| b.old_end <= *base);
    *base = (*base as i64 + shift.prefix[i]) as u64;
  }
  let mut new_files: Vec<(u64, u64, u32)> = Vec::with_capacity(map.files().len());
  for &(key, start, rows) in map.files() {
    if let Some(block) = shift.blocks.iter().find(|b| b.file_key == key) {
      new_files.push((key, block.new_start, block.new_rows));
    } else {
      let translated = shift
        .translate(start)
        .ok_or_else(|| io::Error::other("defs-changed: file table overlaps an edited block"))?;
      new_files.push((key, translated, rows));
    }
  }
  let new_map = NodeIdMap::from_parts(new_bases.clone(), new_files.clone());
  let session_keys: HashSet<u64> = plan.files.iter().map(|f| f.file_key).collect();
  let session_ranges: Vec<(u64, u64)> = plan
    .files
    .iter()
    .map(|f| {
      let &(_, start, rows) = new_files
        .iter()
        .find(|&&(key, _, _)| key == f.file_key)
        .expect("session file present in the successor table");
      (start, start + u64::from(rows))
    })
    .collect();
  let in_session = |dense: u64| session_ranges.iter().any(|&(lo, hi)| (lo..hi).contains(&dense));

  crate::phase_stamp("defs-changed surgery: identity ready");
  // ---- scc over the successor call graph (the definition set moved; recompute always) ----
  let scc_new: Vec<u32> = {
    let node_count = new_bases[buckets] as usize;
    let mut log = vorpal_graph::EdgeLog::default();
    for u in 0..prior_kg.node_count() as u64 {
      let Some(new_src) = shift.translate(u) else { continue };
      if in_session(new_src) {
        continue; // replaced by the plans below
      }
      for (&dst, &etype) in prior_kg
        .graph_out_targets(u as u32)
        .iter()
        .zip(prior_kg.graph_out_edge_types(u as u32))
      {
        if EdgeType(etype).base() != EdgeType::CALLS {
          continue;
        }
        let new_dst = shift.translate(u64::from(dst)).ok_or_else(|| {
          io::Error::other("defs-changed: a non-dirty call targets the edited block")
        })?;
        log.push(new_src as u32, new_dst as u32, EdgeType(etype));
      }
    }
    for file in &plan.files {
      for &(from, to, etype) in &file.edges {
        if etype.base() == EdgeType::CALLS {
          log.push(from, to, etype);
        }
      }
    }
    crate::scc::scc_sizes(node_count, &log)
  };

  crate::phase_stamp("defs-changed surgery: scc done");
  // ---- node store: the edited bucket splices around the fresh seal; every other bucket
  // links unless its scc column moved ----
  let nodes_dir = staging.join(NODES_DIR);
  fs::create_dir_all(&nodes_dir)?;
  let mut node_toc = fs::read(prior.join(crate::NODES_TOC))
    .map_err(|_| io::Error::other("defs-changed: prior node TOC unreadable"))?;
  for bucket in 0..buckets {
    let vseg_name = format!("{bucket:04}.vseg");
    let heap_name = format!("{bucket:04}.heap");
    let (new_lo, new_hi) = (new_bases[bucket] as usize, new_bases[bucket + 1] as usize);
    if !edited_buckets.contains(&bucket) {
      // Content is positionally identical (locals cancel); only the scc column can move.
      let prior_bytes = fs::read(prior.join(NODES_DIR).join(&vseg_name))?;
      let segment = Segment::open_owned(prior_bytes)
        .map_err(|err| io::Error::other(format!("defs-changed: prior node slab: {err}")))?;
      let col = |name: &str| -> io::Result<usize> {
        segment
          .column_index(name)
          .ok_or_else(|| io::Error::other(format!("defs-changed: slab missing column {name}")))
      };
      let scc_col = segment
        .column_at(col("scc_size")?)
        .and_then(|c| c.as_slice::<u32>().map(<[u32]>::to_vec))
        .ok_or_else(|| io::Error::other("defs-changed: scc column not sliceable"))?;
      if scc_col.as_slice() == &scc_new[new_lo..new_hi] {
        for name in [&vseg_name, &heap_name] {
          let (from, to) = (prior.join(NODES_DIR).join(name), nodes_dir.join(name));
          let _ = fs::remove_file(&to);
          if fs::hard_link(&from, &to).is_err() {
            fs::copy(&from, &to)?;
          }
        }
        continue;
      }
      // scc ripple: rebuild the vseg with the new column; the heap links (strings did
      // not move for this bucket).
      let (bytes, digest) =
        rebuild_vseg_with(&segment, None, &scc_new[new_lo..new_hi])?;
      let tmp = nodes_dir.join(format!("{vseg_name}.tmp"));
      fs::write(&tmp, &bytes)?;
      fs::rename(&tmp, nodes_dir.join(&vseg_name))?;
      let (from, to) = (prior.join(NODES_DIR).join(&heap_name), nodes_dir.join(&heap_name));
      let _ = fs::remove_file(&to);
      if fs::hard_link(&from, &to).is_err() {
        fs::copy(&from, &to)?;
      }
      let at = 20 + bucket * 36 + 4;
      node_toc
        .get_mut(at..at + 8)
        .ok_or_else(|| io::Error::other("defs-changed: node TOC too short"))?
        .copy_from_slice(&(bytes.len() as u64).to_le_bytes());
      node_toc
        .get_mut(at + 8..at + 16)
        .ok_or_else(|| io::Error::other("defs-changed: node TOC too short"))?
        .copy_from_slice(&digest.to_le_bytes());
      continue;
    }

    // An edited bucket: columns and heap splice around the fresh seals' bytes — one
    // pass over ALL of this bucket's edited blocks, ascending.
    let prior_bytes = fs::read(prior.join(NODES_DIR).join(&vseg_name))?;
    let segment = Segment::open_owned(prior_bytes)
      .map_err(|err| io::Error::other(format!("defs-changed: prior node slab: {err}")))?;
    let prior_heap = fs::read(prior.join(NODES_DIR).join(&heap_name))?;
    let bucket_blocks: Vec<&ShiftBlock<'_>> =
      shift.blocks.iter().filter(|b| b.bucket == bucket).collect();
    let raw_rows: Vec<crate::kg::RawNodeRows> = bucket_blocks
      .iter()
      .map(|b| b.fresh.raw_node_rows())
      .collect::<io::Result<_>>()?;
    let splices: Vec<(usize, usize, &crate::kg::RawNodeRows)> = bucket_blocks
      .iter()
      .zip(&raw_rows)
      .map(|(b, rows)| {
        (
          (b.old_start - bases[bucket]) as usize,
          (b.old_end - bases[bucket]) as usize,
          rows,
        )
      })
      .collect();
    let (bytes, heap, digest, heap_digest) = splice_edited_bucket(
      &segment,
      &prior_heap,
      &splices,
      &scc_new[new_lo..new_hi],
    )?;
    let tmp = nodes_dir.join(format!("{vseg_name}.tmp"));
    fs::write(&tmp, &bytes)?;
    fs::rename(&tmp, nodes_dir.join(&vseg_name))?;
    let tmp = nodes_dir.join(format!("{heap_name}.tmp"));
    fs::write(&tmp, &heap)?;
    fs::rename(&tmp, nodes_dir.join(&heap_name))?;
    let at = 20 + bucket * 36;
    node_toc
      .get_mut(at..at + 4)
      .ok_or_else(|| io::Error::other("defs-changed: node TOC too short"))?
      .copy_from_slice(&((new_hi - new_lo) as u32).to_le_bytes());
    node_toc[at + 4..at + 12].copy_from_slice(&(bytes.len() as u64).to_le_bytes());
    node_toc[at + 12..at + 20].copy_from_slice(&digest.to_le_bytes());
    node_toc[at + 20..at + 28].copy_from_slice(&(heap.len() as u64).to_le_bytes());
    node_toc[at + 28..at + 36].copy_from_slice(&heap_digest.to_le_bytes());
  }
  // Header total + the file table rewrite (starts shifted, the edited row count fresh).
  let new_total = new_bases[buckets];
  node_toc
    .get_mut(12..20)
    .ok_or_else(|| io::Error::other("defs-changed: node TOC header too short"))?
    .copy_from_slice(&new_total.to_le_bytes());
  // Layout: [header 20][buckets x 36][file_count u64][files x 20].
  let count_at = 20 + buckets * 36;
  let table_at = count_at + 8;
  let expect_len = table_at + new_files.len() * 20;
  if node_toc.len() != expect_len {
    return Err(io::Error::other("defs-changed: node TOC file table shape moved"));
  }
  node_toc[count_at..count_at + 8]
    .copy_from_slice(&(new_files.len() as u64).to_le_bytes());
  for (i, &(key, start, rows)) in new_files.iter().enumerate() {
    let at = table_at + i * 20;
    node_toc[at..at + 8].copy_from_slice(&key.to_le_bytes());
    node_toc[at + 8..at + 16].copy_from_slice(&start.to_le_bytes());
    node_toc[at + 16..at + 20].copy_from_slice(&rows.to_le_bytes());
  }
  let toc_tmp = nodes_dir.join("toc.bin.tmp");
  fs::write(&toc_tmp, &node_toc)?;
  fs::rename(&toc_tmp, staging.join(crate::NODES_TOC))?;

  crate::phase_stamp("defs-changed surgery: nodes done");
  // ---- evidence: the session files' buckets swap their rows (translated); others link ----
  let store = crate::evidence::EvidenceStore::open(prior)
    .ok_or_else(|| io::Error::other("defs-changed: prior evidence unreadable"))?;
  let session_buckets: HashSet<usize> = plan
    .files
    .iter()
    .map(|f| {
      let &(_, start, _) = new_files
        .iter()
        .find(|&&(key, _, _)| key == f.file_key)
        .expect("session file in table");
      new_bases.partition_point(|&b| b <= start) - 1
    })
    .collect();
  let evidence_dir = staging.join(crate::EVIDENCE_DIR);
  fs::create_dir_all(&evidence_dir)?;
  let mut ev_toc = fs::read(prior.join(crate::EVIDENCE_TOC))
    .map_err(|_| io::Error::other("defs-changed: prior evidence TOC unreadable"))?;
  let mut dropped_by_file: HashMap<u64, Vec<crate::EvidenceRow>> = HashMap::new();
  for bucket in 0..buckets {
    let name = format!("{bucket:04}.bin");
    if !session_buckets.contains(&bucket) {
      let (from, to) =
        (prior.join(crate::EVIDENCE_DIR).join(&name), evidence_dir.join(&name));
      let _ = fs::remove_file(&to);
      if fs::hard_link(&from, &to).is_err() {
        fs::copy(&from, &to)?;
      }
      continue;
    }
    let mut rows: Vec<crate::EvidenceRow> = Vec::new();
    for row in store.rows_of_bucket(bucket) {
      let Some((key, _)) = map.locate(row.from) else {
        return Err(io::Error::other("defs-changed: evidence row outside the universe"));
      };
      if session_keys.contains(&key) {
        dropped_by_file.entry(key).or_default().push(row);
        continue;
      }
      let mut row = row;
      row.from = translate_u32(&shift, row.from)?;
      if row.to != crate::NO_EDGE {
        row.to = translate_u32(&shift, row.to)?;
      }
      for alt in &mut row.alternatives {
        *alt = translate_u32(&shift, *alt)?;
      }
      rows.push(row);
    }
    for file in &plan.files {
      let &(_, start, _) = new_files
        .iter()
        .find(|&&(key, _, _)| key == file.file_key)
        .expect("session file in table");
      let file_bucket_new = new_bases.partition_point(|&b| b <= start) - 1;
      if file_bucket_new == bucket {
        rows.extend(file.evidence.iter().cloned());
      }
    }
    let built = crate::evidence::build_slab(bucket, new_bases[bucket], &rows, &new_map)?;
    let tmp = evidence_dir.join(format!("{name}.tmp"));
    fs::write(&tmp, &built.bytes)?;
    fs::rename(&tmp, evidence_dir.join(&name))?;
    let at = EVIDENCE_TOC_HEADER + bucket * EVIDENCE_TOC_ROW;
    let prior_rows_here = u64::from_le_bytes(
      ev_toc
        .get(at..at + 8)
        .ok_or_else(|| io::Error::other("defs-changed: evidence TOC too short"))?
        .try_into()
        .map_err(|_| io::Error::other("defs-changed: evidence TOC"))?,
    );
    let prior_total = u64::from_le_bytes(
      ev_toc[12..20].try_into().map_err(|_| io::Error::other("defs-changed: evidence TOC"))?,
    );
    ev_toc[12..20].copy_from_slice(&(prior_total - prior_rows_here + built.rows).to_le_bytes());
    ev_toc[at..at + 8].copy_from_slice(&built.rows.to_le_bytes());
    ev_toc[at + 8..at + 16].copy_from_slice(&built.pool.to_le_bytes());
    ev_toc[at + 16..at + 24].copy_from_slice(&(built.bytes.len() as u64).to_le_bytes());
    ev_toc[at + 24..at + 32].copy_from_slice(&built.digest.to_le_bytes());
  }
  let toc_tmp = evidence_dir.join("toc.bin.tmp");
  fs::write(&toc_tmp, &ev_toc)?;
  fs::rename(&toc_tmp, staging.join(crate::EVIDENCE_TOC))?;

  crate::phase_stamp("defs-changed surgery: evidence done");
  // ---- edges: session buckets + changed-similar buckets rebuild; the edited bucket's
  // later files shift their locals by the delta; every other bucket's locals cancel ----
  let mut rewrite_srcs: HashMap<usize, HashSet<u32>> = HashMap::new();
  for (file, &(lo, hi)) in plan.files.iter().zip(&session_ranges) {
    let _ = file;
    let bucket = new_bases.partition_point(|&b| b <= lo) - 1;
    for src in lo..hi {
      rewrite_srcs.entry(bucket).or_default().insert(src as u32);
    }
  }
  for &src in &plan.changed_srcs {
    let bucket = new_bases.partition_point(|&b| b <= u64::from(src)) - 1;
    rewrite_srcs.entry(bucket).or_default().insert(src);
  }
  let mut similar_of: HashMap<u32, Vec<SlabRow>> = HashMap::new();
  {
    let need: HashSet<u32> = rewrite_srcs.values().flatten().copied().collect();
    for &(a, b, confidence) in &plan.fresh_pairs {
      let label = EdgeType::SIMILAR_TO.with_confidence(confidence);
      for (s, d) in [(a, b), (b, a)] {
        let s32 = s as u32;
        if need.contains(&s32) {
          let src_bucket = new_bases.partition_point(|&base| base <= s) - 1;
          similar_of.entry(s32).or_default().push(localize(
            &new_map,
            new_bases[src_bucket],
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
    .map_err(|_| io::Error::other("defs-changed: prior edge TOC unreadable"))?;
  let mut rebuilt_rows: HashMap<usize, Vec<SlabRow>> = HashMap::new();
  for bucket in 0..buckets {
    let name = format!("{bucket:04}.bin");
    let needs_rebuild = rewrite_srcs.contains_key(&bucket) || edited_buckets.contains(&bucket);
    if !needs_rebuild {
      let (from, to) = (prior.join(crate::EDGES_DIR).join(&name), edges_dir.join(&name));
      let _ = fs::remove_file(&to);
      if fs::hard_link(&from, &to).is_err() {
        fs::copy(&from, &to)?;
      }
      continue;
    }
    let prior_rows = read_edge_slab(&prior.join(crate::EDGES_DIR).join(&name), bucket)?;
    let bucket_base_new = new_bases[bucket];
    let bucket_base_old = bases[bucket];
    let empty = HashSet::new();
    let bucket_rewrites = rewrite_srcs.get(&bucket).unwrap_or(&empty);
    let mut out: Vec<SlabRow> = Vec::with_capacity(prior_rows.len());
    let mut i = 0usize;
    let old_len = (bases[bucket + 1] - bucket_base_old) as u32;
    // Walk prior sources in order; emit successor sources in order. Sources map
    // monotonically (the edited file stays in place), so one forward pass suffices.
    let mut emitted: Vec<(u32, Vec<SlabRow>)> = Vec::new();
    for src_local_old in 0..old_len {
      let run_start = i;
      while i < prior_rows.len() && prior_rows[i].src_local == src_local_old {
        i += 1;
      }
      let run = &prior_rows[run_start..i];
      let src_dense_old = bucket_base_old + u64::from(src_local_old);
      if shift.block_containing_old(src_dense_old).is_some() {
        continue; // an edited file's old SOURCES are replaced wholesale by the fresh loop
      }
      let Some(src_dense_new) = shift.translate(src_dense_old) else {
        return Err(io::Error::other("defs-changed: untranslatable carried source"));
      };
      let src_local_new = (src_dense_new - bucket_base_new) as u32;
      if !bucket_rewrites.contains(&(src_dense_new as u32)) {
        let mut moved = Vec::with_capacity(run.len());
        for row in run {
          moved.push(SlabRow {
            src_local: src_local_new,
            ..*row
          });
        }
        emitted.push((src_local_new, moved));
        continue;
      }
      // A dirty (non-edited) source: containment + co-change carry; resolution, similar,
      // and requests replace.
      let mut last = EdgeClass::Containment;
      for row in run {
        let class = edge_class(EdgeType(row.etype));
        if class < last {
          return Err(io::Error::other(
            "defs-changed: prior edge run interleaves log phases — premise violated",
          ));
        }
        last = class;
      }
      let keep = |class: EdgeClass| {
        run.iter().filter(move |row| edge_class(EdgeType(row.etype)) == class)
      };
      let mut segment: Vec<SlabRow> = Vec::new();
      for row in keep(EdgeClass::Containment).chain(keep(EdgeClass::Cochange)) {
        segment.push(SlabRow {
          src_local: src_local_new,
          ..*row
        });
      }
      let owner = plan
        .files
        .iter()
        .zip(&session_ranges)
        .find(|(_, range)| (range.0..range.1).contains(&src_dense_new));
      if let Some((file, _)) = owner {
        for &(from, to, etype) in file.edges.iter().filter(|(f, _, _)| u64::from(*f) == src_dense_new)
        {
          segment.push(localize(&new_map, bucket_base_new, from, to, etype)?);
        }
      } else {
        for row in keep(EdgeClass::Resolution) {
          segment.push(SlabRow {
            src_local: src_local_new,
            ..*row
          });
        }
      }
      if let Some(similar) = similar_of.get(&(src_dense_new as u32)) {
        segment.extend_from_slice(similar);
      }
      if let Some((file, _)) = owner {
        for &(from, to, etype) in
          file.request_edges.iter().filter(|(f, _, _)| u64::from(*f) == src_dense_new)
        {
          segment.push(localize(&new_map, bucket_base_new, from, to, etype)?);
        }
      } else {
        for row in keep(EdgeClass::Request) {
          segment.push(SlabRow {
            src_local: src_local_new,
            ..*row
          });
        }
      }
      emitted.push((src_local_new, segment));
    }
    if i != prior_rows.len() {
      return Err(io::Error::other("defs-changed: prior edge slab rows out of source order"));
    }
    // The edited files' fresh sources (each only in its own bucket).
    for block in shift.blocks.iter().filter(|b| b.bucket == bucket) {
      let file = plan
        .files
        .iter()
        .find(|f| f.file_key == block.file_key)
        .ok_or_else(|| io::Error::other("defs-changed: edited file missing from the plan"))?;
      let edited_lo = block.new_start;
      let edited_hi = block.new_start + u64::from(block.new_rows);
      for src_dense in edited_lo..edited_hi {
        let src_local_new = (src_dense - bucket_base_new) as u32;
        let mut segment: Vec<SlabRow> = Vec::new();
        // Containment for the fresh block comes from the SCRATCH SEAL's own edges.
        let fresh_ord = src_dense - edited_lo;
        for (&dst, &etype) in block
          .fresh
          .graph_out_targets(fresh_ord as u32)
          .iter()
          .zip(block.fresh.graph_out_edge_types(fresh_ord as u32))
        {
          if edge_class(EdgeType(etype)) == EdgeClass::Containment {
            segment.push(localize(
              &new_map,
              bucket_base_new,
              src_local_new + bucket_base_new as u32,
              (edited_lo + u64::from(dst)) as u32,
              EdgeType(etype),
            )?);
          }
        }
        for &(from, to, etype) in
          file.edges.iter().filter(|(f, _, _)| u64::from(*f) == src_dense)
        {
          segment.push(localize(&new_map, bucket_base_new, from, to, etype)?);
        }
        if let Some(similar) = similar_of.get(&(src_dense as u32)) {
          segment.extend_from_slice(similar);
        }
        for &(from, to, etype) in
          file.request_edges.iter().filter(|(f, _, _)| u64::from(*f) == src_dense)
        {
          segment.push(localize(&new_map, bucket_base_new, from, to, etype)?);
        }
        emitted.push((src_local_new, segment));
      }
    }
    emitted.sort_by_key(|(local, _)| *local);
    for (_, segment) in emitted {
      out.extend(segment);
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
        .ok_or_else(|| io::Error::other("defs-changed: edge TOC too short"))?
        .try_into()
        .map_err(|_| io::Error::other("defs-changed: edge TOC"))?,
    );
    let prior_total = u64::from_le_bytes(
      edge_toc[12..20].try_into().map_err(|_| io::Error::other("defs-changed: edge TOC"))?,
    );
    edge_toc[12..20]
      .copy_from_slice(&(prior_total - prior_edges_here + out.len() as u64).to_le_bytes());
    edge_toc[at..at + 8].copy_from_slice(&(out.len() as u64).to_le_bytes());
    edge_toc[at + 8..at + 16].copy_from_slice(&(bytes.len() as u64).to_le_bytes());
    edge_toc[at + 16..at + 24].copy_from_slice(&digest.to_le_bytes());
  }
  let toc_tmp = edges_dir.join("toc.bin.tmp");
  fs::write(&toc_tmp, &edge_toc)?;
  fs::rename(&toc_tmp, staging.join(crate::EDGES_TOC))?;

  crate::phase_stamp("defs-changed surgery: edges done");
  // ---- the successor graph cache ----
  {
    let node_count = new_bases[buckets];
    let mut srcs: Vec<u32> = Vec::new();
    let mut dsts: Vec<u32> = Vec::new();
    let mut etypes: Vec<u16> = Vec::new();
    let starts: HashMap<u64, u64> =
      new_files.iter().map(|&(key, start, _)| (key, start)).collect();
    for bucket in 0..buckets {
      if let Some(rows) = rebuilt_rows.get(&bucket) {
        let bucket_base = new_bases[bucket];
        for row in rows {
          let dst = starts
            .get(&row.dst_key)
            .map(|&start| (start + u64::from(row.dst_ord)) as u32)
            .ok_or_else(|| io::Error::other("defs-changed: edge dst outside the universe"))?;
          srcs.push((bucket_base + u64::from(row.src_local)) as u32);
          dsts.push(dst);
          etypes.push(row.etype);
        }
        continue;
      }
      for u_old in bases[bucket]..bases[bucket + 1] {
        let Some(u_new) = shift.translate(u_old) else { continue };
        for (&dst, &etype) in prior_kg
          .graph_out_targets(u_old as u32)
          .iter()
          .zip(prior_kg.graph_out_edge_types(u_old as u32))
        {
          let new_dst = shift.translate(u64::from(dst)).ok_or_else(|| {
            io::Error::other("defs-changed: a non-dirty edge targets the edited block")
          })?;
          srcs.push(u_new as u32);
          dsts.push(new_dst as u32);
          etypes.push(etype);
        }
      }
    }
    let graph = vorpal_graph::Graph::from_parts(node_count as u32, &srcs, &dsts, &etypes);
    // The node fold over the staged vsegs, in bucket order — the stamp's node half.
    let mut fold = xxhash_rust::xxh3::Xxh3::new();
    for bucket in 0..buckets {
      fold.update(&fs::read(nodes_dir.join(format!("{bucket:04}.vseg")))?);
    }
    if let Some(stamp) = crate::edgestore::expected_stamp(staging, fold.digest()) {
      let _ = crate::edgestore::write_cache(staging, &graph, stamp);
    }
  }

  crate::phase_stamp("defs-changed surgery: graph cache done");
  // ---- usage: every session file's postings delta, bucket-scoped ----
  let removed: HashSet<(u32, u64)> = dropped_by_file
    .iter()
    .flat_map(|(&key, rows)| rows.iter().map(move |row| (row.name_hash, key)))
    .collect();
  let mut added: Vec<(u32, u64)> = plan
    .files
    .iter()
    .flat_map(|file| file.evidence.iter().map(|row| (row.name_hash, file.file_key)))
    .collect();
  added.sort_unstable();
  added.dedup();
  crate::usagestore::apply_delta(staging, prior, buckets as u32, &removed, &added)?;

  crate::phase_stamp("defs-changed surgery: usage done");
  // ---- sigs: the repaired ledger over the successor map ----
  crate::sigstore::save_sigs(staging, &plan.sig_rows, &new_map, Some(prior))?;

  crate::phase_stamp("defs-changed surgery: sigs done");
  // ---- dataflow: drop session rows, translate the rest, extend, canonical save ----
  let mut flows = crate::dataflow::load_dataflow(prior)
    .ok_or_else(|| io::Error::other("defs-changed: prior dataflow unreadable"))?;
  let mut kept: Vec<crate::DataflowRow> = Vec::with_capacity(flows.len());
  for mut row in flows.drain(..) {
    let Some((key, _)) = map.locate(row.from) else {
      return Err(io::Error::other("defs-changed: dataflow row outside the universe"));
    };
    if session_keys.contains(&key) {
      continue;
    }
    row.from = translate_u32(&shift, row.from)?;
    row.to = translate_u32(&shift, row.to)?;
    kept.push(row);
  }
  for file in &plan.files {
    kept.extend(file.flows.iter().cloned());
  }
  crate::dataflow::save_dataflow(staging, kept)?;

  crate::phase_stamp("defs-changed surgery: dataflow done");
  // ---- names.idx: names CHANGED — regenerate by translation + the fresh blocks' names ----
  {
    let fresh_total: usize = shift.blocks.iter().map(|b| b.fresh.node_count()).sum();
    let mut pairs: Vec<(u64, u64)> = Vec::new();
    if let Some((hashes, ids)) = prior_kg.names_pairs() {
      pairs.reserve(hashes.len() + fresh_total);
      for (&hash, &id) in hashes.iter().zip(ids) {
        if shift.block_containing_old(id).is_some() {
          continue; // each fresh block re-contributes every surviving name below
        }
        if let Some(new_id) = shift.translate(id) {
          pairs.push((hash, new_id));
        }
      }
    } else {
      return Err(io::Error::other("defs-changed: prior generation has no name index"));
    }
    for block in &shift.blocks {
      for ord in 0..block.fresh.node_count() as u64 {
        if let Some(name) = block.fresh.node_name(crate::NodeId::new(ord)) {
          pairs.push((
            xxhash_rust::xxh3::xxh3_64(name.as_bytes()),
            block.new_start + ord,
          ));
        }
      }
    }
    pairs.sort_unstable();
    crate::kg::write_names_index_pairs(staging, &pairs)?;
  }
  Ok(())
}

fn translate_u32(shift: &Shift, dense: u32) -> io::Result<u32> {
  shift
    .translate(u64::from(dense))
    .map(|id| id as u32)
    .ok_or_else(|| io::Error::other("defs-changed: a carried row targets the edited block"))
}

/// Rebuild a vseg from a prior segment with an optional heap-offset shift and a fresh scc
/// column — the non-edited-bucket scc-ripple lane.
fn rebuild_vseg_with(
  segment: &Segment,
  _shift: Option<()>,
  scc: &[u32],
) -> io::Result<(Vec<u8>, u64)> {
  let col = |name: &str| -> io::Result<usize> {
    segment
      .column_index(name)
      .ok_or_else(|| io::Error::other(format!("defs-changed: slab missing column {name}")))
  };
  let u8s = |idx: usize| -> io::Result<Vec<u8>> {
    segment
      .column_at(idx)
      .and_then(|c| c.as_slice::<u8>().map(<[u8]>::to_vec))
      .ok_or_else(|| io::Error::other("defs-changed: column not sliceable"))
  };
  let u32s = |idx: usize| -> io::Result<Vec<u32>> {
    segment
      .column_at(idx)
      .and_then(|c| c.as_slice::<u32>().map(<[u32]>::to_vec))
      .ok_or_else(|| io::Error::other("defs-changed: column not sliceable"))
  };
  let u64s = |idx: usize| -> io::Result<Vec<u64>> {
    segment
      .column_at(idx)
      .and_then(|c| c.as_slice::<u64>().map(<[u64]>::to_vec))
      .ok_or_else(|| io::Error::other("defs-changed: column not sliceable"))
  };
  let kind = u8s(col("kind")?)?;
  let name_off = u32s(col("name_off")?)?;
  let name_len = u32s(col("name_len")?)?;
  let path_off = u32s(col("path_off")?)?;
  let path_len = u32s(col("path_len")?)?;
  let sig_off = u32s(col("sig_off")?)?;
  let sig_len = u32s(col("sig_len")?)?;
  let content_hash = u64s(col("content_hash")?)?;
  let eid_lo = u64s(col("eid_lo")?)?;
  let eid_hi = u64s(col("eid_hi")?)?;
  let flags = u8s(col("flags")?)?;
  let span_start = u32s(col("span_start")?)?;
  let span_end = u32s(col("span_end")?)?;
  let mut builder = SegmentBuilder::new(0);
  let build_err = |_| io::Error::other("defs-changed: node slab rebuild failed");
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
  builder.add_u32("scc_size", scc).map_err(build_err)?;
  let bytes = builder.build().map_err(build_err)?;
  let digest = xxhash_rust::xxh3::xxh3_64(&bytes);
  Ok((bytes, digest))
}

/// An edited bucket: prior columns/heap spliced around the fresh seals' rows and heap
/// bytes — k splices in one region walk (multi-file since S2-b). File heap runs are
/// back-to-back by the writer's gather order — ASSERTED here (region extents must tile
/// monotonically), errored otherwise.
#[allow(clippy::type_complexity)]
fn splice_edited_bucket(
  segment: &Segment,
  prior_heap: &[u8],
  splices: &[(usize, usize, &crate::kg::RawNodeRows)],
  scc: &[u32],
) -> io::Result<(Vec<u8>, Vec<u8>, u64, u64)> {
  let col = |name: &str| -> io::Result<usize> {
    segment
      .column_index(name)
      .ok_or_else(|| io::Error::other(format!("defs-changed: slab missing column {name}")))
  };
  let u8s = |idx: usize| -> io::Result<Vec<u8>> {
    segment
      .column_at(idx)
      .and_then(|c| c.as_slice::<u8>().map(<[u8]>::to_vec))
      .ok_or_else(|| io::Error::other("defs-changed: column not sliceable"))
  };
  let u32s = |idx: usize| -> io::Result<Vec<u32>> {
    segment
      .column_at(idx)
      .and_then(|c| c.as_slice::<u32>().map(<[u32]>::to_vec))
      .ok_or_else(|| io::Error::other("defs-changed: column not sliceable"))
  };
  let u64s = |idx: usize| -> io::Result<Vec<u64>> {
    segment
      .column_at(idx)
      .and_then(|c| c.as_slice::<u64>().map(<[u64]>::to_vec))
      .ok_or_else(|| io::Error::other("defs-changed: column not sliceable"))
  };
  let kind = u8s(col("kind")?)?;
  let name_off = u32s(col("name_off")?)?;
  let name_len = u32s(col("name_len")?)?;
  let path_off = u32s(col("path_off")?)?;
  let path_len = u32s(col("path_len")?)?;
  let sig_off = u32s(col("sig_off")?)?;
  let sig_len = u32s(col("sig_len")?)?;
  let content_hash = u64s(col("content_hash")?)?;
  let eid_lo = u64s(col("eid_lo")?)?;
  let eid_hi = u64s(col("eid_hi")?)?;
  let flags = u8s(col("flags")?)?;
  let span_start = u32s(col("span_start")?)?;
  let span_end = u32s(col("span_end")?)?;
  let rows_old = kind.len();
  if splices.is_empty() {
    return Err(io::Error::other("defs-changed: edited bucket with no splice"));
  }
  for window in splices.windows(2) {
    if window[0].1 > window[1].0 {
      return Err(io::Error::other("defs-changed: splice blocks overlap or misorder"));
    }
  }
  if splices.last().map(|&(_, hi, _)| hi).unwrap_or(0) > rows_old {
    return Err(io::Error::other("defs-changed: edited block beyond the bucket"));
  }

  // Heap extent of a prior row range, from len>0 references.
  let extent = |range: std::ops::Range<usize>| -> Option<(usize, usize)> {
    let (mut lo, mut hi) = (usize::MAX, 0usize);
    for i in range {
      for (off, len) in [
        (name_off[i], name_len[i]),
        (path_off[i], path_len[i]),
        (sig_off[i], sig_len[i]),
      ] {
        if len > 0 {
          lo = lo.min(off as usize);
          hi = hi.max(off as usize + len as usize);
        }
      }
    }
    (lo != usize::MAX).then_some((lo, hi))
  };

  // Region walk: alternate GAP (carried prior rows) and SPLICE (a fresh seal). Heap
  // bytes concatenate region by region; each row's offsets rebase to its region's
  // output start. The writer packs file runs back-to-back, so region extents must tile
  // monotonically — asserted, errored otherwise.
  let rows_new = rows_old - splices.iter().map(|&(lo, hi, _)| hi - lo).sum::<usize>()
    + splices.iter().map(|&(_, _, f)| f.kind.len()).sum::<usize>();
  if scc.len() != rows_new {
    return Err(io::Error::other("defs-changed: scc slice disagrees with the bucket"));
  }
  let mut heap: Vec<u8> = Vec::with_capacity(
    prior_heap.len() + splices.iter().map(|&(_, _, f)| f.heap.len()).sum::<usize>(),
  );
  let mut kind_new = Vec::with_capacity(rows_new);
  let mut name_off_new = Vec::with_capacity(rows_new);
  let mut name_len_new = Vec::with_capacity(rows_new);
  let mut path_off_new = Vec::with_capacity(rows_new);
  let mut path_len_new = Vec::with_capacity(rows_new);
  let mut sig_off_new = Vec::with_capacity(rows_new);
  let mut sig_len_new = Vec::with_capacity(rows_new);
  let mut content_hash_new = Vec::with_capacity(rows_new);
  let mut eid_lo_new = Vec::with_capacity(rows_new);
  let mut eid_hi_new = Vec::with_capacity(rows_new);
  let mut flags_new = Vec::with_capacity(rows_new);
  let mut span_start_new = Vec::with_capacity(rows_new);
  let mut span_end_new = Vec::with_capacity(rows_new);
  let mut prior_heap_cursor = 0usize; // monotone tiling checkpoint
  let emit_gap = |lo: usize, hi: usize, cursor: &mut usize, heap: &mut Vec<u8>,
                  name_off_new: &mut Vec<u32>, name_len_new: &mut Vec<u32>,
                  path_off_new: &mut Vec<u32>, path_len_new: &mut Vec<u32>,
                  sig_off_new: &mut Vec<u32>, sig_len_new: &mut Vec<u32>,
                  kind_new: &mut Vec<u8>, content_hash_new: &mut Vec<u64>,
                  eid_lo_new: &mut Vec<u64>, eid_hi_new: &mut Vec<u64>,
                  flags_new: &mut Vec<u8>, span_start_new: &mut Vec<u32>,
                  span_end_new: &mut Vec<u32>|
   -> io::Result<()> {
    if lo == hi {
      return Ok(());
    }
    let region_out = heap.len() as i64;
    let region_shift = if let Some((ext_lo, ext_hi)) = extent(lo..hi) {
      if ext_lo < *cursor || ext_hi > prior_heap.len() {
        return Err(io::Error::other(
          "defs-changed: heap runs are not block-adjacent — premise violated",
        ));
      }
      heap.extend_from_slice(&prior_heap[ext_lo..ext_hi]);
      *cursor = ext_hi;
      region_out - ext_lo as i64
    } else {
      0
    };
    for i in lo..hi {
      kind_new.push(kind[i]);
      let rebase = |off: u32, len: u32| -> u32 {
        if len > 0 { (off as i64 + region_shift) as u32 } else { 0 }
      };
      name_off_new.push(rebase(name_off[i], name_len[i]));
      name_len_new.push(name_len[i]);
      path_off_new.push(rebase(path_off[i], path_len[i]));
      path_len_new.push(path_len[i]);
      sig_off_new.push(rebase(sig_off[i], sig_len[i]));
      sig_len_new.push(sig_len[i]);
      content_hash_new.push(content_hash[i]);
      eid_lo_new.push(eid_lo[i]);
      eid_hi_new.push(eid_hi[i]);
      flags_new.push(flags[i]);
      span_start_new.push(span_start[i]);
      span_end_new.push(span_end[i]);
    }
    Ok(())
  };
  let mut row_cursor = 0usize;
  for &(f_lo, f_hi, fresh) in splices {
    emit_gap(
      row_cursor, f_lo, &mut prior_heap_cursor, &mut heap, &mut name_off_new,
      &mut name_len_new, &mut path_off_new, &mut path_len_new, &mut sig_off_new,
      &mut sig_len_new, &mut kind_new, &mut content_hash_new, &mut eid_lo_new,
      &mut eid_hi_new, &mut flags_new, &mut span_start_new, &mut span_end_new,
    )?;
    // Skip the replaced block's heap bytes in the tiling cursor.
    if let Some((ext_lo, ext_hi)) = extent(f_lo..f_hi) {
      if ext_lo < prior_heap_cursor || ext_hi > prior_heap.len() {
        return Err(io::Error::other(
          "defs-changed: heap runs are not block-adjacent — premise violated",
        ));
      }
      prior_heap_cursor = ext_hi;
    }
    let splice_out = heap.len() as u32;
    heap.extend_from_slice(&fresh.heap);
    for i in 0..fresh.kind.len() {
      kind_new.push(fresh.kind[i]);
      let rebase =
        |off: u32, len: u32| -> u32 { if len > 0 { off + splice_out } else { 0 } };
      name_off_new.push(rebase(fresh.name_off[i], fresh.name_len[i]));
      name_len_new.push(fresh.name_len[i]);
      path_off_new.push(rebase(fresh.path_off[i], fresh.path_len[i]));
      path_len_new.push(fresh.path_len[i]);
      sig_off_new.push(rebase(fresh.sig_off[i], fresh.sig_len[i]));
      sig_len_new.push(fresh.sig_len[i]);
      content_hash_new.push(fresh.content_hash[i]);
      eid_lo_new.push(fresh.eid_lo[i]);
      eid_hi_new.push(fresh.eid_hi[i]);
      flags_new.push(fresh.flags[i]);
      span_start_new.push(fresh.span_start[i]);
      span_end_new.push(fresh.span_end[i]);
    }
    row_cursor = f_hi;
  }
  emit_gap(
    row_cursor, rows_old, &mut prior_heap_cursor, &mut heap, &mut name_off_new,
    &mut name_len_new, &mut path_off_new, &mut path_len_new, &mut sig_off_new,
    &mut sig_len_new, &mut kind_new, &mut content_hash_new, &mut eid_lo_new,
    &mut eid_hi_new, &mut flags_new, &mut span_start_new, &mut span_end_new,
  )?;
  let mut builder = SegmentBuilder::new(0);
  let build_err = |_| io::Error::other("defs-changed: node slab rebuild failed");
  builder.add_u8("kind", &kind_new).map_err(build_err)?;
  builder.add_u32("name_off", &name_off_new).map_err(build_err)?;
  builder.add_u32("name_len", &name_len_new).map_err(build_err)?;
  builder.add_u32("path_off", &path_off_new).map_err(build_err)?;
  builder.add_u32("path_len", &path_len_new).map_err(build_err)?;
  builder.add_u32("sig_off", &sig_off_new).map_err(build_err)?;
  builder.add_u32("sig_len", &sig_len_new).map_err(build_err)?;
  builder.add_u64("content_hash", &content_hash_new).map_err(build_err)?;
  builder.add_u64("eid_lo", &eid_lo_new).map_err(build_err)?;
  builder.add_u64("eid_hi", &eid_hi_new).map_err(build_err)?;
  builder.add_u8("flags", &flags_new).map_err(build_err)?;
  builder.add_u32("span_start", &span_start_new).map_err(build_err)?;
  builder.add_u32("span_end", &span_end_new).map_err(build_err)?;
  builder.add_u32("scc_size", scc).map_err(build_err)?;
  let bytes = builder.build().map_err(build_err)?;
  let digest = xxhash_rust::xxh3::xxh3_64(&bytes);
  let heap_digest = xxhash_rust::xxh3::xxh3_64(&heap);
  Ok((bytes, heap, digest, heap_digest))
}
