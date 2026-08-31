//! The per-edge evidence sidecar (IMPROVEMENTS §5 / 07-29 §4): one fixed-width row per
//! reference *occurrence*, retaining what resolution knew — the source span of the
//! referencing token, the resolver branch, the packed confidence, the candidate count, the
//! retained tie-set alternatives, and the occurrences that produced **no** edge. This is
//! what lets every relation answer "why does this exist?", "why this target and not the
//! alternatives?", and "why is there no edge here at all?".
//!
//! **Flat layout v2** (`evidence.bin`): `magic + version + row_count u64 + alt_pool_len
//! u64`, then 36-byte little-endian rows `[from u32][to u32][name_hash u32][etype u16]
//! [reason u8][confidence u8][outcome u8][alt_count u8][pad u16][candidates u32]
//! [span_start u32][span_end u32][alt_off u32]`, followed by the alternatives pool
//! (`alt_pool_len` u32 node ids). `to == u32::MAX` marks a no-edge outcome. Rows sort by
//! `(from, to, etype, span_start, span_end)` + a total tiebreak — canonical, so the
//! artifact is a pure function of the occurrence set. Lookup is a binary search on the
//! `(from, to)` prefix.
//!
//! **Bucketed layout v3** (`evidence/<k>.bin` + `evidence/toc.bin`, P4.3 — written for
//! bucketed generations): rows live in the slab of their SOURCE's bucket, `from` stored as
//! the bucket-local ordinal and `to` (plus every alternatives entry) as the P4.0 identity
//! `(file_key u64, ordinal u32)` — position-independent under file adds/removes anywhere
//! else, and never silently truncated the way a packed word would be. (The
//! `(bucket, bucket-local)` prototype was measured first and REJECTED: a kernel one-file
//! function-append shifted bucket-mates' ordinals and cascaded through incoming references
//! into 251 of 257 slabs.) Slab rows sort by `(from_local, to_key, to_ordinal, …)` with
//! the same total tiebreak as the dense sort; lookups binary-search that stored order.
//! The writer hard-links every slab whose bytes match the prior TOC's digest. The dense-id
//! API is unchanged — conversion happens at the store boundary through the node-store
//! TOC's file table (`NodeIdMap`).
//!
//! Both layouts are optional and versioned: missing/foreign files answer "no evidence
//! recorded", never an error.

use std::fs;
use std::io::{self, Write};
use std::path::Path;

use vorpal_mem::{AccessPattern, CorpusProbe, Hotness, MappedStore, ResourcePolicy, StoreKind};

const MAGIC: &[u8; 4] = b"VEVD";
const VERSION: u32 = 2;
const ROW: usize = 36;
const HEADER: usize = 24;

/// Bucketed (v3) constants: slab files, slab header, TOC.
pub const EVIDENCE_DIR: &str = "evidence";
pub const EVIDENCE_TOC: &str = "evidence/toc.bin";
const SLAB_MAGIC: &[u8; 4] = b"VEVB";
const TOC_MAGIC: &[u8; 4] = b"VEVT";
const V3: u32 = 3;
/// Slab header: magic + version + bucket u32 + rows u64 + pool entries u64.
const SLAB_HEADER: usize = 28;
/// v3 row: `[from_local u32][to_key u64][to_ordinal u32][name_hash u32][etype u16]
/// [reason u8][confidence u8][outcome u8][alt_count u8][candidates u32][span_start u32]
/// [span_end u32][alt_off u32]` — 42 bytes. Destinations are `(file_key, ordinal)`: the
/// P4.0 identity, position-independent under file adds/removes ANYWHERE else (the
/// (bucket, bucket-local) prototype measured 6/257 slab carry on a kernel one-file edit —
/// bucket-mates' ordinal shifts cascade through incoming references globally).
const ROW_V3: usize = 42;
/// v3 pool entry: `(file_key u64, ordinal u32)`.
const POOL_V3: usize = 12;
/// TOC header: magic + version + bucket count u32 + total rows u64.
const TOC_HEADER: usize = 20;
/// One per-bucket TOC row: rows u64 + pool entries u64 + byte len u64 + digest u64.
const TOC_ROW: usize = 32;
/// The v3 no-edge destination sentinel (sorts last under the stored key order).
const NO_EDGE_V3: (u64, u32) = (u64::MAX, u32::MAX);

/// The `to` sentinel marking an occurrence that produced no edge.
pub const NO_EDGE: u32 = u32::MAX;

/// What an occurrence produced. Stored as a `u8` tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvidenceOutcome {
  /// A graph edge (`to` is a real node).
  Edge,
  /// No definition with the name exists in the tree (std/dependency target).
  External,
  /// Definitions exist, but none was safely attributable from this site.
  Masked,
}

impl EvidenceOutcome {
  pub fn tag(self) -> u8 {
    match self {
      Self::Edge => 0,
      Self::External => 1,
      Self::Masked => 2,
    }
  }

  pub fn from_tag(tag: u8) -> Self {
    match tag {
      1 => Self::External,
      2 => Self::Masked,
      _ => Self::Edge,
    }
  }

  pub fn label(self) -> &'static str {
    match self {
      Self::Edge => "edge",
      Self::External => "external",
      Self::Masked => "masked",
    }
  }
}

/// One occurrence's retained evidence. `etype` is the base edge type (confidence carried
/// separately); `reason` is a `vorpal_resolve::ResolveReason` tag, kept raw here so the store
/// stays dependency-free — consumers label it. `alternatives` holds the retained tie-set
/// candidate ids the chosen target beat (empty for unique picks and no-edge rows).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvidenceRow {
  pub from: u32,
  /// Target node id, or [`NO_EDGE`] for external/masked occurrences.
  pub to: u32,
  /// Low 32 bits of xxh3 of the referenced name — the absence-query key.
  pub name_hash: u32,
  pub etype: u16,
  pub reason: u8,
  pub confidence: u8,
  pub outcome: EvidenceOutcome,
  pub candidates: u32,
  pub span_start: u32,
  pub span_end: u32,
  pub alternatives: Vec<u32>,
}

impl EvidenceRow {
  fn key(&self) -> (u32, u32, u16, u32, u32) {
    (self.from, self.to, self.etype, self.span_start, self.span_end)
  }
}

/// Which evidence layout a writer publishes (readers sniff). `Bucketed` carries the
/// generation's dense-id ⇄ `(file_key, ordinal)` map (`Kg::node_id_map`) and the prior
/// generation dir for the hard-link carry.
pub enum EvidenceLayout<'a> {
  Flat,
  Bucketed {
    nodes: &'a crate::kg::NodeIdMap,
    prior: Option<&'a Path>,
  },
}

/// Canonically sort rows: the total order that makes the artifact a pure function of the
/// occurrence set — deterministic under any sort algorithm and any thread count.
fn sort_canonical(rows: &mut [EvidenceRow]) {
  use rayon::prelude::*;
  rows.par_sort_unstable_by(|a, b| {
    a.key()
      .cmp(&b.key())
      .then_with(|| a.name_hash.cmp(&b.name_hash))
      .then_with(|| a.reason.cmp(&b.reason))
      .then_with(|| a.confidence.cmp(&b.confidence))
      .then_with(|| a.outcome.tag().cmp(&b.outcome.tag()))
      .then_with(|| a.candidates.cmp(&b.candidates))
      .then_with(|| a.alternatives.cmp(&b.alternatives))
  });
}

/// Persist rows as `evidence.bin` under `dir` (tmp + rename) — the flat (v2) layout.
pub fn save(dir: &Path, rows: Vec<EvidenceRow>) -> io::Result<()> {
  save_with(dir, rows, &EvidenceLayout::Flat)
}

/// Persist rows under an explicit layout. Rows arrive in arrival order and are sorted
/// canonically here, so the bytes are a pure function of the occurrence set either way.
pub fn save_with(dir: &Path, mut rows: Vec<EvidenceRow>, layout: &EvidenceLayout<'_>) -> io::Result<()> {
  crate::phase_stamp("evidence: sort start");
  sort_canonical(&mut rows);
  crate::phase_stamp("evidence: encode start");
  match layout {
    EvidenceLayout::Flat => save_flat(dir, &rows),
    EvidenceLayout::Bucketed { nodes, prior } => save_bucketed(dir, &rows, nodes, *prior),
  }
}

fn save_flat(dir: &Path, rows: &[EvidenceRow]) -> io::Result<()> {
  use rayon::prelude::*;
  // Fixed-width rows encode at exact offsets: chunk the row set, prefix-sum each chunk's
  // alternative count (the only cross-row dependency), then encode rows and pool in
  // parallel straight into their final positions. The serial form issued ~13 tiny
  // `write_all`s per row — ~88M calls at kernel scale, single-threaded.
  const CHUNK: usize = 64 * 1024;
  let chunk_alt_counts: Vec<u32> = rows
    .par_chunks(CHUNK)
    .map(|chunk| chunk.iter().map(|r| r.alternatives.len() as u32).sum())
    .collect();
  let mut chunk_alt_offsets = Vec::with_capacity(chunk_alt_counts.len());
  let mut running = 0u32;
  for &count in &chunk_alt_counts {
    chunk_alt_offsets.push(running);
    running += count;
  }
  let pool_len = running as usize;

  let mut rows_buf = vec![0u8; rows.len() * ROW];
  let mut pool_buf = vec![0u8; pool_len * 4];
  {
    // Pool slices per chunk are disjoint by construction (prefix offsets); split_at_mut
    // walks them off the front in order.
    let mut pool_rest: &mut [u8] = &mut pool_buf;
    let mut pool_slices = Vec::with_capacity(chunk_alt_counts.len());
    for &count in &chunk_alt_counts {
      let (head, tail) = pool_rest.split_at_mut(count as usize * 4);
      pool_slices.push(head);
      pool_rest = tail;
    }
    rows
      .par_chunks(CHUNK)
      .zip(rows_buf.par_chunks_mut(CHUNK * ROW))
      .zip(pool_slices)
      .zip(chunk_alt_offsets)
      .for_each(|(((chunk, out_rows), out_pool), mut alt_off)| {
        let mut pool_at = 0usize;
        for (row, out) in chunk.iter().zip(out_rows.chunks_exact_mut(ROW)) {
          out[0..4].copy_from_slice(&row.from.to_le_bytes());
          out[4..8].copy_from_slice(&row.to.to_le_bytes());
          out[8..12].copy_from_slice(&row.name_hash.to_le_bytes());
          out[12..14].copy_from_slice(&row.etype.to_le_bytes());
          out[14] = row.reason;
          out[15] = row.confidence;
          out[16] = row.outcome.tag();
          out[17] = row.alternatives.len().min(255) as u8;
          out[18] = 0;
          out[19] = 0;
          out[20..24].copy_from_slice(&row.candidates.to_le_bytes());
          out[24..28].copy_from_slice(&row.span_start.to_le_bytes());
          out[28..32].copy_from_slice(&row.span_end.to_le_bytes());
          out[32..36].copy_from_slice(&alt_off.to_le_bytes());
          for &alt in &row.alternatives {
            out_pool[pool_at..pool_at + 4].copy_from_slice(&alt.to_le_bytes());
            pool_at += 4;
          }
          alt_off += row.alternatives.len() as u32;
        }
      });
  }
  crate::phase_stamp("evidence: write start");

  let tmp = dir.join("evidence.bin.tmp");
  let mut out = fs::File::create(&tmp)?;
  let mut header = [0u8; 24];
  header[0..4].copy_from_slice(MAGIC);
  header[4..8].copy_from_slice(&VERSION.to_le_bytes());
  header[8..16].copy_from_slice(&(rows.len() as u64).to_le_bytes());
  header[16..24].copy_from_slice(&(pool_len as u64).to_le_bytes());
  out.write_all(&header)?;
  out.write_all(&rows_buf)?;
  out.write_all(&pool_buf)?;
  drop(out);
  crate::phase_stamp("evidence: save done");
  fs::rename(&tmp, dir.join("evidence.bin"))?;
  // One truth per directory: a format downgrade retires the bucketed layout.
  if dir.join(EVIDENCE_DIR).is_dir() {
    let _ = fs::remove_dir_all(dir.join(EVIDENCE_DIR));
  }
  Ok(())
}

/// Dense id → stored `(file_key, ordinal)` coordinates.
fn locate(nodes: &crate::kg::NodeIdMap, id: u32) -> io::Result<(u64, u32)> {
  if id == NO_EDGE {
    return Ok(NO_EDGE_V3);
  }
  nodes
    .locate_bulk(id)
    .ok_or_else(|| io::Error::other("evidence id outside the node universe"))
}

fn save_bucketed(
  dir: &Path,
  rows: &[EvidenceRow],
  nodes: &crate::kg::NodeIdMap,
  prior: Option<&Path>,
) -> io::Result<()> {
  use rayon::prelude::*;
  let bases = nodes.bases();
  if bases.len() < 2 {
    return Err(io::Error::other("bucketed evidence requires node bases"));
  }
  let buckets = bases.len() - 1;
  let evidence_dir = dir.join(EVIDENCE_DIR);
  fs::create_dir_all(&evidence_dir)?;
  // Rows are canonically sorted by dense (from, …), and dense ids are bucket-major, so
  // the slab partition is a contiguous split — find each bucket's row range.
  let mut starts = Vec::with_capacity(buckets + 1);
  let mut cursor = 0usize;
  for bucket in 0..buckets {
    if cursor < rows.len() && u64::from(rows[cursor].from) < bases[bucket] {
      return Err(io::Error::other("evidence row below its bucket base"));
    }
    starts.push(cursor);
    while cursor < rows.len() && u64::from(rows[cursor].from) < bases[bucket + 1] {
      cursor += 1;
    }
  }
  starts.push(cursor);
  if cursor != rows.len() {
    return Err(io::Error::other("evidence row beyond the node id space"));
  }

  let prior_toc = prior.and_then(|p| EvidenceToc::load(&p.join(EVIDENCE_TOC)));
  let prior_ok = prior_toc.as_ref().is_some_and(|toc| toc.rows.len() == buckets);

  struct BuiltSlab {
    rows: u64,
    pool: u64,
    bytes: Vec<u8>,
    digest: u64,
  }
  let built: io::Result<Vec<BuiltSlab>> = (0..buckets)
    .into_par_iter()
    .map(|bucket| {
      let slab_rows = &rows[starts[bucket]..starts[bucket + 1]];
      let base = bases[bucket];
      // Slab order is the STORED key order — (from_local, to_key, to_ordinal, …) with the
      // same total tiebreak as the dense sort — so the binary search laws hold over the
      // bytes as written. A pure function of the row set (dense ids break the final ties
      // deterministically within one generation).
      let mut ordered: Vec<(u32, (u64, u32), &EvidenceRow)> = slab_rows
        .iter()
        .map(|row| {
          let from_local = (u64::from(row.from) - base) as u32;
          locate(nodes, row.to).map(|to| (from_local, to, row))
        })
        .collect::<io::Result<Vec<_>>>()?;
      ordered.sort_by(|a, b| {
        (a.0, a.1, a.2.etype, a.2.span_start, a.2.span_end)
          .cmp(&(b.0, b.1, b.2.etype, b.2.span_start, b.2.span_end))
          .then_with(|| a.2.name_hash.cmp(&b.2.name_hash))
          .then_with(|| a.2.reason.cmp(&b.2.reason))
          .then_with(|| a.2.confidence.cmp(&b.2.confidence))
          .then_with(|| a.2.outcome.tag().cmp(&b.2.outcome.tag()))
          .then_with(|| a.2.candidates.cmp(&b.2.candidates))
          .then_with(|| a.2.alternatives.cmp(&b.2.alternatives))
      });
      let pool_entries: usize = ordered.iter().map(|(_, _, r)| r.alternatives.len()).sum();
      let mut bytes =
        Vec::with_capacity(SLAB_HEADER + ordered.len() * ROW_V3 + pool_entries * POOL_V3);
      bytes.extend_from_slice(SLAB_MAGIC);
      bytes.extend_from_slice(&V3.to_le_bytes());
      bytes.extend_from_slice(&(bucket as u32).to_le_bytes());
      bytes.extend_from_slice(&(ordered.len() as u64).to_le_bytes());
      bytes.extend_from_slice(&(pool_entries as u64).to_le_bytes());
      let mut alt_off = 0u32;
      for (from_local, (to_key, to_ord), row) in &ordered {
        bytes.extend_from_slice(&from_local.to_le_bytes());
        bytes.extend_from_slice(&to_key.to_le_bytes());
        bytes.extend_from_slice(&to_ord.to_le_bytes());
        bytes.extend_from_slice(&row.name_hash.to_le_bytes());
        bytes.extend_from_slice(&row.etype.to_le_bytes());
        bytes.push(row.reason);
        bytes.push(row.confidence);
        bytes.push(row.outcome.tag());
        bytes.push(row.alternatives.len().min(255) as u8);
        bytes.extend_from_slice(&row.candidates.to_le_bytes());
        bytes.extend_from_slice(&row.span_start.to_le_bytes());
        bytes.extend_from_slice(&row.span_end.to_le_bytes());
        bytes.extend_from_slice(&alt_off.to_le_bytes());
        alt_off += row.alternatives.len() as u32;
      }
      for (_, _, row) in &ordered {
        for &alt in &row.alternatives {
          let (key, ord) = locate(nodes, alt)?;
          bytes.extend_from_slice(&key.to_le_bytes());
          bytes.extend_from_slice(&ord.to_le_bytes());
        }
      }
      let digest = xxhash_rust::xxh3::xxh3_64(&bytes);
      Ok(BuiltSlab {
        rows: ordered.len() as u64,
        pool: pool_entries as u64,
        bytes,
        digest,
      })
    })
    .collect();
  let built = built?;
  crate::phase_stamp("evidence: write start");

  for (bucket, slab) in built.iter().enumerate() {
    let name = format!("{bucket:04}.bin");
    let carried = prior_ok
      && prior_toc.as_ref().is_some_and(|toc| {
        let row = &toc.rows[bucket];
        row.rows == slab.rows
          && row.pool == slab.pool
          && row.len == slab.bytes.len() as u64
          && row.digest == slab.digest
      });
    if carried {
      let from = prior
        .map(|p| p.join(EVIDENCE_DIR).join(&name))
        .expect("carried implies a prior");
      let to = evidence_dir.join(&name);
      if from == to {
        continue; // legacy same-directory publish: already in place
      }
      let _ = fs::remove_file(&to);
      if fs::hard_link(&from, &to).is_ok() {
        continue;
      }
      // Link refused (cross-device, permissions): fall through — same bytes, full cost.
    }
    let tmp = evidence_dir.join(format!("{name}.tmp"));
    fs::write(&tmp, &slab.bytes)?;
    fs::rename(&tmp, evidence_dir.join(&name))?;
  }
  // TOC last — the family's commit record.
  let toc_tmp = evidence_dir.join("toc.bin.tmp");
  let mut out = fs::File::create(&toc_tmp)?;
  out.write_all(TOC_MAGIC)?;
  out.write_all(&V3.to_le_bytes())?;
  out.write_all(&(buckets as u32).to_le_bytes())?;
  out.write_all(&(rows.len() as u64).to_le_bytes())?;
  for slab in &built {
    out.write_all(&slab.rows.to_le_bytes())?;
    out.write_all(&slab.pool.to_le_bytes())?;
    out.write_all(&(slab.bytes.len() as u64).to_le_bytes())?;
    out.write_all(&slab.digest.to_le_bytes())?;
  }
  drop(out);
  fs::rename(&toc_tmp, dir.join(EVIDENCE_TOC))?;
  crate::phase_stamp("evidence: save done");
  // One truth per directory: retire the flat file and stale members.
  let _ = fs::remove_file(dir.join("evidence.bin"));
  if let Ok(dirents) = fs::read_dir(&evidence_dir) {
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

/// Whether `name` (generation-relative) is a bucketed-evidence member.
pub fn is_evidence_member(name: &str) -> bool {
  if name == EVIDENCE_TOC {
    return true;
  }
  name
    .strip_prefix("evidence/")
    .and_then(|f| f.strip_suffix(".bin"))
    .is_some_and(|k| !k.is_empty() && k.len() <= 5 && k.bytes().all(|b| b.is_ascii_digit()))
}

struct EvidenceTocRow {
  rows: u64,
  pool: u64,
  len: u64,
  digest: u64,
}

struct EvidenceToc {
  rows: Vec<EvidenceTocRow>,
}

impl EvidenceToc {
  fn load(path: &Path) -> Option<EvidenceToc> {
    let bytes = fs::read(path).ok()?;
    if bytes.len() < TOC_HEADER || &bytes[0..4] != TOC_MAGIC {
      return None;
    }
    if u32::from_le_bytes(bytes[4..8].try_into().ok()?) != V3 {
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
      rows.push(EvidenceTocRow {
        rows: u64::from_le_bytes(row[0..8].try_into().ok()?),
        pool: u64::from_le_bytes(row[8..16].try_into().ok()?),
        len: u64::from_le_bytes(row[16..24].try_into().ok()?),
        digest: u64::from_le_bytes(row[24..32].try_into().ok()?),
      });
    }
    if rows.iter().map(|r| r.rows).sum::<u64>() != total {
      return None;
    }
    Some(EvidenceToc { rows })
  }
}

/// One mapped v3 slab.
struct Slab {
  store: MappedStore,
  rows: usize,
  pool_at: usize,
  pool_len: usize,
}

enum Backing {
  Flat(MappedStore),
  Bucketed {
    /// One per bucket (`None` = empty slab elided from reads).
    slabs: Vec<Option<Slab>>,
    /// The generation's dense-id ⇄ (file_key, ordinal) map.
    map: crate::kg::NodeIdMap,
  },
}

/// The mapped read side: rows stay on disk; lookups touch only the pages a binary search
/// and its matching run need. The API speaks DENSE ids under both layouts.
pub struct EvidenceStore {
  backing: Backing,
  count: usize,
}

impl EvidenceStore {
  /// Map the evidence under `dir`, if present and current-format — bucketed layout first,
  /// then flat. `None` is "no evidence recorded" (older/foreign generation, torn file) — a
  /// degraded answer, never an error.
  pub fn open(dir: &Path) -> Option<EvidenceStore> {
    if dir.join(EVIDENCE_TOC).is_file() {
      return Self::open_bucketed(dir);
    }
    Self::open_flat(dir)
  }

  fn open_flat(dir: &Path) -> Option<EvidenceStore> {
    let store = MappedStore::map_file(
      &dir.join("evidence.bin"),
      StoreKind::Canonical,
      AccessPattern::Random,
      Hotness::Cold,
      &ResourcePolicy::probe(CorpusProbe::new(0, 0)),
    )
    .ok()?;
    let bytes = store.as_bytes();
    if bytes.len() < HEADER || &bytes[0..4] != MAGIC {
      return None;
    }
    if u32::from_le_bytes(bytes[4..8].try_into().ok()?) != VERSION {
      return None;
    }
    let count = u64::from_le_bytes(bytes[8..16].try_into().ok()?) as usize;
    let pool_len = u64::from_le_bytes(bytes[16..24].try_into().ok()?) as usize;
    let pool_at = HEADER + count * ROW;
    if bytes.len() != pool_at + pool_len * 4 {
      return None; // torn write or foreign generation
    }
    Some(EvidenceStore {
      backing: Backing::Flat(store),
      count,
    })
  }

  fn open_bucketed(dir: &Path) -> Option<EvidenceStore> {
    let toc = EvidenceToc::load(&dir.join(EVIDENCE_TOC))?;
    // The node-store TOC supplies the id map (same generation dir — atomic).
    let map = crate::kg::NodeIdMap::from_dir(dir)?;
    if map.bases().len() != toc.rows.len() + 1 {
      return None; // families from different generations
    }
    let mut slabs = Vec::with_capacity(toc.rows.len());
    let mut count = 0usize;
    for (k, row) in toc.rows.iter().enumerate() {
      if row.rows == 0 && row.pool == 0 {
        // Verify the empty slab exists with the declared length, then elide it.
        let meta = fs::metadata(dir.join(EVIDENCE_DIR).join(format!("{k:04}.bin"))).ok()?;
        if meta.len() != row.len {
          return None;
        }
        slabs.push(None);
        continue;
      }
      let path = dir.join(EVIDENCE_DIR).join(format!("{k:04}.bin"));
      let store = MappedStore::map_file(
        &path,
        StoreKind::Canonical,
        AccessPattern::Random,
        Hotness::Cold,
        &ResourcePolicy::probe(CorpusProbe::new(0, 0)),
      )
      .ok()?;
      let bytes = store.as_bytes();
      if bytes.len() != row.len as usize || bytes.len() < SLAB_HEADER {
        return None;
      }
      if &bytes[0..4] != SLAB_MAGIC
        || u32::from_le_bytes(bytes[4..8].try_into().ok()?) != V3
        || u32::from_le_bytes(bytes[8..12].try_into().ok()?) != k as u32
      {
        return None;
      }
      let rows = u64::from_le_bytes(bytes[12..20].try_into().ok()?) as usize;
      let pool_len = u64::from_le_bytes(bytes[20..28].try_into().ok()?) as usize;
      let pool_at = SLAB_HEADER + rows * ROW_V3;
      if rows as u64 != row.rows || bytes.len() != pool_at + pool_len * POOL_V3 {
        return None;
      }
      count += rows;
      slabs.push(Some(Slab {
        store,
        rows,
        pool_at,
        pool_len,
      }));
    }
    Some(EvidenceStore {
      backing: Backing::Bucketed { slabs, map },
      count,
    })
  }

  /// Visit every retained occurrence's referenced-name hash (all outcomes), decoding
  /// nothing else: one strided u32 read per row.
  pub fn for_each_name_hash(&self, mut f: impl FnMut(u32)) {
    match &self.backing {
      Backing::Flat(store) => {
        let bytes = store.as_bytes();
        let count = (bytes.len() - HEADER) / ROW.max(1);
        let count = count.min(self.count);
        for i in 0..count {
          let at = HEADER + i * ROW + 8;
          f(u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()));
        }
      }
      Backing::Bucketed { slabs, .. } => {
        for slab in slabs.iter().flatten() {
          let bytes = slab.store.as_bytes();
          for i in 0..slab.rows {
            let at = SLAB_HEADER + i * ROW_V3 + 16;
            f(u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()));
          }
        }
      }
    }
  }

  /// Every retained occurrence of edges `from → to`, across all edge types, in canonical
  /// order. Binary search on the sorted `(from, to)` prefix, then the contiguous run.
  pub fn edges_between(&self, from: u32, to: u32) -> Vec<EvidenceRow> {
    match &self.backing {
      Backing::Flat(_) => {
        let lo = self.partition_flat(|key| (key.0, key.1) < (from, to));
        let hi = self.partition_flat(|key| (key.0, key.1) <= (from, to));
        (lo..hi).map(|i| self.row_flat(i)).collect()
      }
      Backing::Bucketed { slabs, map } => {
        let Some((bucket, from_l)) = bucket_of(map.bases(), from) else {
          return Vec::new();
        };
        let want_to = match map.locate(to) {
          Some(pair) => pair,
          None if to == NO_EDGE => NO_EDGE_V3,
          None => return Vec::new(),
        };
        let Some(Some(slab)) = slabs.get(bucket) else {
          return Vec::new();
        };
        let lo = slab.partition(|key| (key.0, key.1, key.2) < (from_l, want_to.0, want_to.1));
        let hi = slab.partition(|key| (key.0, key.1, key.2) <= (from_l, want_to.0, want_to.1));
        (lo..hi).map(|i| slab.row(i, from, map)).collect()
      }
    }
  }

  /// Every retained occurrence originating at `from` — real edges first, then any no-edge
  /// outcomes (their `to` sentinel sorts last).
  pub fn edges_from(&self, from: u32) -> Vec<EvidenceRow> {
    match &self.backing {
      Backing::Flat(_) => {
        let lo = self.partition_flat(|key| key.0 < from);
        let hi = self.partition_flat(|key| key.0 <= from);
        (lo..hi).map(|i| self.row_flat(i)).collect()
      }
      Backing::Bucketed { slabs, map } => {
        let Some((bucket, from_l)) = bucket_of(map.bases(), from) else {
          return Vec::new();
        };
        let Some(Some(slab)) = slabs.get(bucket) else {
          return Vec::new();
        };
        let lo = slab.partition(|key| key.0 < from_l);
        let hi = slab.partition(|key| key.0 <= from_l);
        (lo..hi).map(|i| slab.row(i, from, map)).collect()
      }
    }
  }

  /// The no-edge occurrences at `from` whose referenced-name hash matches — "why is there
  /// no edge from here to anything named X?".
  pub fn absences_from(&self, from: u32, name_hash: u32) -> Vec<EvidenceRow> {
    self
      .edges_from(from)
      .into_iter()
      .filter(|r| r.to == NO_EDGE && r.name_hash == name_hash)
      .collect()
  }

  /// Every retained row, in canonical order — the complete occurrence population.
  pub fn rows(&self) -> Box<dyn Iterator<Item = EvidenceRow> + '_> {
    match &self.backing {
      Backing::Flat(_) => Box::new((0..self.count).map(|i| self.row_flat(i))),
      Backing::Bucketed { slabs, map } => Box::new(
        slabs
          .iter()
          .enumerate()
          .filter_map(|(bucket, slab)| slab.as_ref().map(|s| (bucket, s)))
          .flat_map(move |(bucket, slab)| {
            let base = map.bases()[bucket] as u32;
            (0..slab.rows).map(move |i| slab.row_with_base(i, base, map))
          }),
      ),
    }
  }

  pub fn len(&self) -> usize {
    self.count
  }

  pub fn is_empty(&self) -> bool {
    self.count == 0
  }

  fn flat_store(&self) -> &MappedStore {
    match &self.backing {
      Backing::Flat(store) => store,
      Backing::Bucketed { .. } => unreachable!("flat accessors are gated by the backing"),
    }
  }

  fn row_flat(&self, i: usize) -> EvidenceRow {
    let store = self.flat_store();
    let pool_at = HEADER + self.count * ROW;
    let at = HEADER + i * ROW;
    let b = &store.as_bytes()[at..at + ROW];
    let alt_count = b[17] as usize;
    let alt_off = u32::from_le_bytes(b[32..36].try_into().unwrap()) as usize;
    let pool_len = (store.as_bytes().len() - pool_at) / 4;
    let alternatives = (alt_off..(alt_off + alt_count).min(pool_len))
      .map(|slot| {
        let p = pool_at + slot * 4;
        u32::from_le_bytes(store.as_bytes()[p..p + 4].try_into().unwrap())
      })
      .collect();
    EvidenceRow {
      from: u32::from_le_bytes(b[0..4].try_into().unwrap()),
      to: u32::from_le_bytes(b[4..8].try_into().unwrap()),
      name_hash: u32::from_le_bytes(b[8..12].try_into().unwrap()),
      etype: u16::from_le_bytes(b[12..14].try_into().unwrap()),
      reason: b[14],
      confidence: b[15],
      outcome: EvidenceOutcome::from_tag(b[16]),
      candidates: u32::from_le_bytes(b[20..24].try_into().unwrap()),
      span_start: u32::from_le_bytes(b[24..28].try_into().unwrap()),
      span_end: u32::from_le_bytes(b[28..32].try_into().unwrap()),
      alternatives,
    }
  }

  /// `partition_point` over the mapped flat rows (key fields only — no pool reads).
  fn partition_flat(&self, pred: impl Fn(&(u32, u32)) -> bool) -> usize {
    let store = self.flat_store();
    let bytes = store.as_bytes();
    let (mut lo, mut hi) = (0usize, self.count);
    while lo < hi {
      let mid = (lo + hi) / 2;
      let at = HEADER + mid * ROW;
      let key = (
        u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()),
        u32::from_le_bytes(bytes[at + 4..at + 8].try_into().unwrap()),
      );
      if pred(&key) {
        lo = mid + 1;
      } else {
        hi = mid;
      }
    }
    lo
  }
}

/// Dense id → `(bucket index, bucket-local ordinal)` for the FROM side (slab routing).
fn bucket_of(bases: &[u64], id: u32) -> Option<(usize, u32)> {
  if id == NO_EDGE {
    return None;
  }
  let raw = u64::from(id);
  let bucket = bases.partition_point(|&base| base <= raw).checked_sub(1)?;
  if raw >= *bases.get(bucket + 1)? {
    return None;
  }
  Some((bucket, (raw - bases[bucket]) as u32))
}

impl Slab {
  /// `(from_local, to_key, to_ordinal)` at row `i`.
  fn key(&self, i: usize) -> (u32, u64, u32) {
    let at = SLAB_HEADER + i * ROW_V3;
    let b = &self.store.as_bytes()[at..at + 16];
    (
      u32::from_le_bytes(b[0..4].try_into().unwrap()),
      u64::from_le_bytes(b[4..12].try_into().unwrap()),
      u32::from_le_bytes(b[12..16].try_into().unwrap()),
    )
  }

  fn partition(&self, pred: impl Fn(&(u32, u64, u32)) -> bool) -> usize {
    let (mut lo, mut hi) = (0usize, self.rows);
    while lo < hi {
      let mid = (lo + hi) / 2;
      if pred(&self.key(mid)) {
        lo = mid + 1;
      } else {
        hi = mid;
      }
    }
    lo
  }

  fn densify(map: &crate::kg::NodeIdMap, key: u64, ordinal: u32) -> u32 {
    if (key, ordinal) == NO_EDGE_V3 {
      return NO_EDGE;
    }
    // A key the map cannot resolve means families from different generations — the open
    // verified counts, so this is unreachable in practice; NO_EDGE is the honest degraded
    // answer if it ever is.
    map.densify(key, ordinal).unwrap_or(NO_EDGE)
  }

  /// Decode row `i`; `from_dense` is the caller's already-dense from id.
  fn row(&self, i: usize, from_dense: u32, map: &crate::kg::NodeIdMap) -> EvidenceRow {
    self.decode(i, |_| from_dense, map)
  }

  /// Decode row `i` computing `from` off the slab's dense base (whole-store iteration).
  fn row_with_base(&self, i: usize, base: u32, map: &crate::kg::NodeIdMap) -> EvidenceRow {
    self.decode(i, |from_local| base + from_local, map)
  }

  fn decode(
    &self,
    i: usize,
    from: impl Fn(u32) -> u32,
    map: &crate::kg::NodeIdMap,
  ) -> EvidenceRow {
    let at = SLAB_HEADER + i * ROW_V3;
    let b = &self.store.as_bytes()[at..at + ROW_V3];
    let from_local = u32::from_le_bytes(b[0..4].try_into().unwrap());
    let to_key = u64::from_le_bytes(b[4..12].try_into().unwrap());
    let to_ord = u32::from_le_bytes(b[12..16].try_into().unwrap());
    let alt_count = b[25] as usize;
    let alt_off = u32::from_le_bytes(b[38..42].try_into().unwrap()) as usize;
    let alternatives = (alt_off..(alt_off + alt_count).min(self.pool_len))
      .map(|slot| {
        let p = self.pool_at + slot * POOL_V3;
        let bytes = self.store.as_bytes();
        let key = u64::from_le_bytes(bytes[p..p + 8].try_into().unwrap());
        let ordinal = u32::from_le_bytes(bytes[p + 8..p + 12].try_into().unwrap());
        Self::densify(map, key, ordinal)
      })
      .collect();
    EvidenceRow {
      from: from(from_local),
      to: Self::densify(map, to_key, to_ord),
      name_hash: u32::from_le_bytes(b[16..20].try_into().unwrap()),
      etype: u16::from_le_bytes(b[20..22].try_into().unwrap()),
      reason: b[22],
      confidence: b[23],
      outcome: EvidenceOutcome::from_tag(b[24]),
      candidates: u32::from_le_bytes(b[26..30].try_into().unwrap()),
      span_start: u32::from_le_bytes(b[30..34].try_into().unwrap()),
      span_end: u32::from_le_bytes(b[34..38].try_into().unwrap()),
      alternatives,
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn row(from: u32, to: u32, span: u32, alts: Vec<u32>) -> EvidenceRow {
    EvidenceRow {
      from,
      to,
      name_hash: 0xBEEF,
      etype: 1,
      reason: 6,
      confidence: 90,
      outcome: EvidenceOutcome::Edge,
      candidates: 1 + alts.len() as u32,
      span_start: span,
      span_end: span + 4,
      alternatives: alts,
    }
  }

  fn absent(from: u32, span: u32, external: bool) -> EvidenceRow {
    EvidenceRow {
      from,
      to: NO_EDGE,
      name_hash: 0xD00D,
      etype: 1,
      reason: 0,
      confidence: 0,
      outcome: if external {
        EvidenceOutcome::External
      } else {
        EvidenceOutcome::Masked
      },
      candidates: if external { 0 } else { 3 },
      span_start: span,
      span_end: span + 4,
      alternatives: Vec::new(),
    }
  }

  fn arrival() -> Vec<EvidenceRow> {
    vec![
      row(2, 7, 40, vec![9, 11]),
      absent(1, 90, true),
      row(1, 3, 10, vec![]),
      row(2, 7, 20, vec![5]),
      absent(2, 80, false),
    ]
  }

  #[test]
  fn roundtrips_sorts_and_looks_up() {
    let dir = std::env::temp_dir().join(format!("vorpal-evidence-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    // Arrival order scrambled; save canonicalizes rows AND the alternatives pool.
    save(&dir, arrival()).unwrap();
    let store = EvidenceStore::open(&dir).unwrap();
    assert_eq!(store.len(), 5);
    let hits = store.edges_between(2, 7);
    assert_eq!(hits.len(), 2);
    assert!(hits[0].span_start < hits[1].span_start, "canonical span order");
    assert_eq!(hits[0].alternatives, vec![5], "pool follows sorted rows");
    assert_eq!(hits[1].alternatives, vec![9, 11]);
    // Absence lookups by (from, name_hash); no-edge rows sort after real edges.
    assert_eq!(store.absences_from(1, 0xD00D).len(), 1);
    assert_eq!(store.absences_from(1, 0xD00D)[0].outcome, EvidenceOutcome::External);
    assert_eq!(store.absences_from(2, 0xD00D)[0].outcome, EvidenceOutcome::Masked);
    assert_eq!(store.absences_from(2, 0xBEEF), Vec::new());
    assert_eq!(store.edges_from(2).len(), 3, "edges then absences");

    // Determinism: same rows, any arrival order → identical bytes.
    let a = fs::read(dir.join("evidence.bin")).unwrap();
    save(
      &dir,
      vec![
        absent(2, 80, false),
        row(2, 7, 20, vec![5]),
        absent(1, 90, true),
        row(1, 3, 10, vec![]),
        row(2, 7, 40, vec![9, 11]),
      ],
    )
    .unwrap();
    assert_eq!(a, fs::read(dir.join("evidence.bin")).unwrap());

    // Torn file loads as None, not an error.
    let mut bytes = fs::read(dir.join("evidence.bin")).unwrap();
    bytes.truncate(bytes.len() - 3);
    fs::write(dir.join("evidence.bin"), &bytes).unwrap();
    assert!(EvidenceStore::open(&dir).is_none());
    let _ = fs::remove_dir_all(&dir);
  }

  /// The v3 oracle: a bucketed store must answer EXACTLY like the flat store over the same
  /// rows — every query, every field, dense ids throughout.
  #[test]
  fn bucketed_store_answers_equal_flat() {
    let base = std::env::temp_dir().join(format!("vorpal-evidence-v3-{}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    let (flat_dir, v3_dir) = (base.join("flat"), base.join("v3"));
    fs::create_dir_all(&flat_dir).unwrap();
    fs::create_dir_all(&v3_dir).unwrap();
    // Node space: three buckets with bases [0, 4, 4, 12) — bucket 1 empty.
    let bases: Vec<u64> = vec![0, 4, 4, 12];
    // A synthetic node-store TOC (with its file table) so the reader derives the id map.
    crate::kg::write_node_bases_fixture(&v3_dir, &bases).unwrap();
    let map = crate::kg::NodeIdMap::from_dir(&v3_dir).unwrap();
    save(&flat_dir, arrival()).unwrap();
    save_with(
      &v3_dir,
      arrival(),
      &EvidenceLayout::Bucketed {
        nodes: &map,
        prior: None,
      },
    )
    .unwrap();
    let flat = EvidenceStore::open(&flat_dir).unwrap();
    let v3 = EvidenceStore::open(&v3_dir).unwrap();
    assert_eq!(v3.len(), flat.len());
    let all_flat: Vec<EvidenceRow> = flat.rows().collect();
    let all_v3: Vec<EvidenceRow> = v3.rows().collect();
    assert_eq!(all_flat, all_v3, "row population diverges");
    for from in 0..12u32 {
      assert_eq!(flat.edges_from(from), v3.edges_from(from), "edges_from({from})");
      for to in [3u32, 7, NO_EDGE] {
        assert_eq!(
          flat.edges_between(from, to),
          v3.edges_between(from, to),
          "edges_between({from},{to})"
        );
      }
      for hash in [0xBEEFu32, 0xD00D] {
        assert_eq!(
          flat.absences_from(from, hash),
          v3.absences_from(from, hash),
          "absences_from({from},{hash:x})"
        );
      }
    }
    // Determinism + carry: re-save with a prior → every slab hard-links (same inodes).
    let v3b = base.join("v3b");
    fs::create_dir_all(&v3b).unwrap();
    crate::kg::write_node_bases_fixture(&v3b, &bases).unwrap();
    save_with(
      &v3b,
      arrival(),
      &EvidenceLayout::Bucketed {
        nodes: &map,
        prior: Some(&v3_dir),
      },
    )
    .unwrap();
    #[cfg(unix)]
    {
      use std::os::unix::fs::MetadataExt;
      for k in 0..3 {
        let name = format!("{k:04}.bin");
        assert_eq!(
          fs::metadata(v3_dir.join(EVIDENCE_DIR).join(&name)).unwrap().ino(),
          fs::metadata(v3b.join(EVIDENCE_DIR).join(&name)).unwrap().ino(),
          "unchanged slab {k} must hard-link"
        );
      }
    }
    let _ = fs::remove_dir_all(&base);
  }
}
