//! The sigs family (P4.5c-1): every signed definition's near-clone sketch ON DISK — the
//! corpus table scoped similar-pairing needs. Without it, any semantic body edit forces
//! the full pipeline just to re-pair one file's sketches against the corpus (the daemon
//! keeps this table in RAM; this is its disk twin, for the CLI's scoped compose).
//!
//! Rows are the P4.0 identity coding — `[file_key u64][ordinal u32][shingles u32]
//! [sketch; 64]` (80 bytes) — slab-bucketed by the definition's FILE bucket and sorted by
//! `(file_key, ordinal)`, so slab bytes are position-independent: only files whose
//! sketches actually changed re-key their slab, and everything else hard-links across
//! generations by TOC digest, exactly like every other family. Written for bucketed
//! generations only (the flat lane stays byte-frozen).
//!
//! Slab (`sigs/<k>.bin`): `[VSGS][version][bucket u32][rows u64]` + rows.
//! TOC (`sigs/toc.bin`): `[VSGT][version][bucket count u32][total rows u64]` +
//! per-slab `{rows u64, byte len u64, digest u64}`.

use std::fs;
use std::io::{self, Write};
use std::path::Path;

use vorpal_mem::{AccessPattern, CorpusProbe, Hotness, MappedStore, ResourcePolicy, StoreKind};

use crate::kg::NodeIdMap;

pub const SIGS_DIR: &str = "sigs";
pub const SIGS_TOC: &str = "sigs/toc.bin";
/// Sketch width in bytes. `vorpal-ingest` statically asserts its `signature::BINS` equals
/// this — the two crates can never drift silently.
pub const SIG_SKETCH_LEN: usize = 64;
const SLAB_MAGIC: &[u8; 4] = b"VSGS";
const TOC_MAGIC: &[u8; 4] = b"VSGT";
/// v2 (2026-09-01): the duplicate-node survivor became a law — content-total order,
/// smallest (shingles, sketch) wins. v1 families hold whichever row the unstable sort's
/// arrangement left first, so they retire here: `open` answers None, the scoped composes
/// decline, and the next full build writes the canonical family.
const VERSION: u32 = 2;
/// Slab header: magic + version + bucket u32 + row count u64.
const SLAB_HEADER: usize = 20;
/// One row: file_key u64 + ordinal u32 + shingles u32 + sketch.
const ROW: usize = 16 + SIG_SKETCH_LEN;
/// TOC header: magic + version + bucket count u32 + total rows u64.
const TOC_HEADER: usize = 20;
/// One per-slab TOC row: rows u64 + byte len u64 + digest u64.
const TOC_ROW: usize = 24;

/// One signed definition, dense-id keyed (the pipeline's canonical space).
#[derive(Clone)]
pub struct SigFamilyRow {
  pub node: u32,
  pub shingles: u32,
  pub sketch: [u8; SIG_SKETCH_LEN],
}

/// Whether `name` (generation-relative) is a sigs-family member.
pub fn is_sigs_member(name: &str) -> bool {
  if name == SIGS_TOC {
    return true;
  }
  name
    .strip_prefix("sigs/")
    .and_then(|f| f.strip_suffix(".bin"))
    .is_some_and(|k| !k.is_empty() && k.len() <= 5 && k.bytes().all(|b| b.is_ascii_digit()))
}

struct TocRow {
  rows: u64,
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
        rows: u64::from_le_bytes(row[0..8].try_into().ok()?),
        len: u64::from_le_bytes(row[8..16].try_into().ok()?),
        digest: u64::from_le_bytes(row[16..24].try_into().ok()?),
      });
    }
    if rows.iter().map(|r| r.rows).sum::<u64>() != total {
      return None;
    }
    Some(Toc { rows })
  }
}

/// Persist the sketch table for a bucketed generation, hard-linking every slab whose bytes
/// match the prior generation's digests.
pub fn save_sigs(
  dir: &Path,
  rows: &[SigFamilyRow],
  nodes: &NodeIdMap,
  prior: Option<&Path>,
) -> io::Result<()> {
  use rayon::prelude::*;
  let bases = nodes.bases();
  if bases.len() < 2 {
    return Err(io::Error::other("sigs family requires node bases"));
  }
  let buckets = bases.len() - 1;
  let sigs_dir = dir.join(SIGS_DIR);
  fs::create_dir_all(&sigs_dir)?;
  // Identity-code and bucket every row, then sort per bucket by (file_key, ordinal) —
  // position-independent and a pure function of the row set.
  let mut coded: Vec<(usize, u64, u32, u32, [u8; SIG_SKETCH_LEN])> = Vec::with_capacity(rows.len());
  for row in rows {
    let (key, ordinal) = nodes
      .locate_bulk(row.node)
      .ok_or_else(|| io::Error::other("sig row outside the node universe"))?;
    let bucket = bases
      .partition_point(|&base| base <= u64::from(row.node))
      .checked_sub(1)
      .ok_or_else(|| io::Error::other("sig row below the id space"))?;
    coded.push((bucket, key, ordinal, row.shingles, row.sketch));
  }
  coded.par_sort_unstable_by(|a, b| (a.0, a.1, a.2, a.3).cmp(&(b.0, b.1, b.2, b.3)));
  let prior_toc = prior.and_then(|p| Toc::load(&p.join(SIGS_TOC)));
  let prior_ok = prior_toc.as_ref().is_some_and(|toc| toc.rows.len() == buckets);

  let mut starts = Vec::with_capacity(buckets + 1);
  let mut cursor = 0usize;
  for bucket in 0..buckets {
    starts.push(cursor);
    while cursor < coded.len() && coded[cursor].0 == bucket {
      cursor += 1;
    }
  }
  starts.push(cursor);

  struct Built {
    rows: u64,
    bytes: Vec<u8>,
    digest: u64,
  }
  let built: Vec<Built> = (0..buckets)
    .into_par_iter()
    .map(|bucket| {
      let slab = &coded[starts[bucket]..starts[bucket + 1]];
      let mut bytes = Vec::with_capacity(SLAB_HEADER + slab.len() * ROW);
      bytes.extend_from_slice(SLAB_MAGIC);
      bytes.extend_from_slice(&VERSION.to_le_bytes());
      bytes.extend_from_slice(&(bucket as u32).to_le_bytes());
      bytes.extend_from_slice(&(slab.len() as u64).to_le_bytes());
      for &(_, key, ordinal, shingles, sketch) in slab {
        bytes.extend_from_slice(&key.to_le_bytes());
        bytes.extend_from_slice(&ordinal.to_le_bytes());
        bytes.extend_from_slice(&shingles.to_le_bytes());
        bytes.extend_from_slice(&sketch);
      }
      let digest = xxhash_rust::xxh3::xxh3_64(&bytes);
      Built {
        rows: slab.len() as u64,
        bytes,
        digest,
      }
    })
    .collect();

  for (bucket, slab) in built.iter().enumerate() {
    let name = format!("{bucket:04}.bin");
    let carried = prior_ok
      && prior_toc.as_ref().is_some_and(|toc| {
        let row = &toc.rows[bucket];
        row.rows == slab.rows && row.len == slab.bytes.len() as u64 && row.digest == slab.digest
      });
    if carried {
      let from = prior
        .map(|p| p.join(SIGS_DIR).join(&name))
        .expect("carried implies a prior");
      let to = sigs_dir.join(&name);
      if from == to {
        continue; // legacy same-directory publish: already in place
      }
      let _ = fs::remove_file(&to);
      if fs::hard_link(&from, &to).is_ok() {
        continue;
      }
      // Link refused: fall through to the write — same bytes, full cost.
    }
    let tmp = sigs_dir.join(format!("{name}.tmp"));
    fs::write(&tmp, &slab.bytes)?;
    fs::rename(&tmp, sigs_dir.join(&name))?;
  }
  let total: u64 = built.iter().map(|s| s.rows).sum();
  let toc_tmp = sigs_dir.join("toc.bin.tmp");
  let mut out = fs::File::create(&toc_tmp)?;
  out.write_all(TOC_MAGIC)?;
  out.write_all(&VERSION.to_le_bytes())?;
  out.write_all(&(buckets as u32).to_le_bytes())?;
  out.write_all(&total.to_le_bytes())?;
  for slab in &built {
    out.write_all(&slab.rows.to_le_bytes())?;
    out.write_all(&(slab.bytes.len() as u64).to_le_bytes())?;
    out.write_all(&slab.digest.to_le_bytes())?;
  }
  drop(out);
  fs::rename(&toc_tmp, dir.join(SIGS_TOC))?;
  if let Ok(dirents) = fs::read_dir(&sigs_dir) {
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

/// The mapped read side: the whole table, densified through the generation's id map — the
/// scoped pairing's input.
pub struct SigStore {
  slabs: Vec<Option<(MappedStore, usize)>>,
}

impl SigStore {
  /// Map the family under `dir`. `None` = absent/foreign (callers escalate to the full
  /// pipeline — degraded, never wrong).
  pub fn open(dir: &Path) -> Option<SigStore> {
    let toc = Toc::load(&dir.join(SIGS_TOC))?;
    let mut slabs = Vec::with_capacity(toc.rows.len());
    for (k, row) in toc.rows.iter().enumerate() {
      let path = dir.join(SIGS_DIR).join(format!("{k:04}.bin"));
      let meta = fs::metadata(&path).ok()?;
      if meta.len() != row.len {
        return None; // mixed generation
      }
      if row.rows == 0 {
        slabs.push(None);
        continue;
      }
      let store = MappedStore::map_file(
        &path,
        StoreKind::Canonical,
        AccessPattern::Sequential,
        Hotness::Cold,
        &ResourcePolicy::probe(CorpusProbe::new(0, 0)),
      )
      .ok()?;
      let bytes = store.as_bytes();
      if bytes.len() < SLAB_HEADER
        || &bytes[0..4] != SLAB_MAGIC
        || u32::from_le_bytes(bytes[4..8].try_into().ok()?) != VERSION
        || u32::from_le_bytes(bytes[8..12].try_into().ok()?) != k as u32
      {
        return None;
      }
      let rows = u64::from_le_bytes(bytes[12..20].try_into().ok()?) as usize;
      if rows as u64 != row.rows || bytes.len() != SLAB_HEADER + rows * ROW {
        return None;
      }
      slabs.push(Some((store, rows)));
    }
    Some(SigStore { slabs })
  }

  /// Every signed definition, densified. `None` entries (a key the map cannot resolve —
  /// families from different generations) abort with `None`: a partial table would make
  /// scoped pairing silently wrong.
  pub fn rows(&self, nodes: &NodeIdMap) -> Option<Vec<SigFamilyRow>> {
    let mut out = Vec::new();
    for slab in self.slabs.iter().flatten() {
      let bytes = slab.0.as_bytes();
      for i in 0..slab.1 {
        let at = SLAB_HEADER + i * ROW;
        let b = &bytes[at..at + ROW];
        let key = u64::from_le_bytes(b[0..8].try_into().ok()?);
        let ordinal = u32::from_le_bytes(b[8..12].try_into().ok()?);
        let node = nodes.densify(key, ordinal)?;
        let mut sketch = [0u8; SIG_SKETCH_LEN];
        sketch.copy_from_slice(&b[16..16 + SIG_SKETCH_LEN]);
        out.push(SigFamilyRow {
          node,
          shingles: u32::from_le_bytes(b[12..16].try_into().ok()?),
          sketch,
        });
      }
    }
    Some(out)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn roundtrips_and_carries() {
    let base = std::env::temp_dir().join(format!("vorpal-sigs-{}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    let (a, b) = (base.join("a"), base.join("b"));
    fs::create_dir_all(&a).unwrap();
    fs::create_dir_all(&b).unwrap();
    // Bases [0, 4, 4, 12): bucket 1 empty (the same fixture shape evidence uses).
    let bases: Vec<u64> = vec![0, 4, 4, 12];
    crate::kg::write_node_bases_fixture(&a, &bases).unwrap();
    crate::kg::write_node_bases_fixture(&b, &bases).unwrap();
    let map = NodeIdMap::from_dir(&a).unwrap();
    let row = |node: u32, fill: u8| SigFamilyRow {
      node,
      shingles: 40 + u32::from(fill),
      sketch: [fill; SIG_SKETCH_LEN],
    };
    let rows = vec![row(2, 7), row(0, 1), row(9, 3)];
    save_sigs(&a, &rows, &map, None).unwrap();
    let store = SigStore::open(&a).unwrap();
    let mut got = store.rows(&map).unwrap();
    got.sort_by_key(|r| r.node);
    assert_eq!(got.len(), 3);
    assert_eq!((got[0].node, got[0].sketch[0]), (0, 1));
    assert_eq!((got[1].node, got[1].sketch[0]), (2, 7));
    assert_eq!((got[2].node, got[2].sketch[0]), (9, 3));
    // Carry: identical rows against a prior hard-link every slab.
    save_sigs(&b, &rows, &map, Some(&a)).unwrap();
    #[cfg(unix)]
    {
      use std::os::unix::fs::MetadataExt;
      for k in 0..3 {
        let name = format!("{k:04}.bin");
        assert_eq!(
          fs::metadata(a.join(SIGS_DIR).join(&name)).unwrap().ino(),
          fs::metadata(b.join(SIGS_DIR).join(&name)).unwrap().ino(),
          "unchanged sig slab {k} must hard-link"
        );
      }
    }
    let _ = fs::remove_dir_all(&base);
  }
}
