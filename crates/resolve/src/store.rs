//! Retained reference store for the memory-primary daemon (SUBSECOND.md Phase 3).
//!
//! The same 34-byte records as [`crate::spill`], but **long-lived**: references append
//! per-file, each file owning a contiguous record range, so an edited file's old references
//! retire by range — never by content inspection — and its replacements append at the tail.
//! Each link feeds only the alive ranges.
//!
//! Lifetime-free by design: qualifier-carrying imports are retained *encoded* and decoded
//! against the caller's interner at link time, so a daemon can own a `RefStore` next to its
//! `Interner` without self-reference. Records store interned `NameId` bits and are therefore
//! process-private, exactly like the spill: create, feed, delete — never persist.

use std::fs::File;
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};

use rustc_hash::FxHashMap;

use crate::intern::Interner;
use crate::reference::{RefForm, RefKind, Reference};
use crate::spill::{RECORD, SPILL_CHUNK, decode_record, encode_record};

/// One file's footprint in the store: its contiguous record range and its qualified imports,
/// retained encoded (interner-free) for the link phase's import-binding pre-pass.
struct FileRefs {
  records: Range<u64>,
  qualified: Vec<u8>,
}

/// Long-lived, per-file-retirable reference store.
pub struct RefStore {
  path: PathBuf,
  out: BufWriter<File>,
  total: u64,
  /// Interned path bits → that file's alive footprint. At most one range per file: a
  /// re-append retires the old range wholesale first.
  files: FxHashMap<u32, FileRefs>,
  dead: u64,
}

impl RefStore {
  pub fn create(path: &Path) -> io::Result<Self> {
    Ok(Self {
      path: path.to_path_buf(),
      out: BufWriter::new(File::create(path)?),
      total: 0,
      files: FxHashMap::default(),
      dead: 0,
    })
  }

  /// Append one file's references as its (new) contiguous range. Any previous range for the
  /// same file is retired first, so "edit" and "first sight" are the same call. Every
  /// reference must belong to the named file (`from_path` bits equal `path_bits`).
  pub fn append_file<'i>(
    &mut self,
    path_bits: u32,
    references: impl IntoIterator<Item = &'i Reference<'i>>,
  ) -> io::Result<()> {
    self.retract_file(path_bits);
    let start = self.total;
    let mut qualified = Vec::new();
    let mut buf = [0u8; RECORD];
    for reference in references {
      debug_assert_eq!(
        reference.from_path.to_bits(),
        path_bits,
        "reference filed under the wrong path"
      );
      encode_record(reference, &mut buf);
      self.out.write_all(&buf)?;
      self.total += 1;
      if reference.kind == RefKind::Import && reference.form == RefForm::Static {
        qualified.extend_from_slice(&buf);
      }
    }
    if self.total > start {
      self.files.insert(
        path_bits,
        FileRefs {
          records: start..self.total,
          qualified,
        },
      );
    }
    Ok(())
  }

  /// Retire a file's references (no-op for a file the store has never seen — deletes and
  /// never-referencing files land here uniformly).
  pub fn retract_file(&mut self, path_bits: u32) {
    if let Some(old) = self.files.remove(&path_bits) {
      self.dead += old.records.end - old.records.start;
    }
  }

  /// Alive record count — the resolver's capacity hint.
  pub fn count(&self) -> u64 {
    self.files.values().map(|f| f.records.end - f.records.start).sum()
  }

  /// Retired fraction of everything ever written — the compaction trigger's input.
  pub fn dead_fraction(&self) -> f64 {
    if self.total == 0 {
      return 0.0;
    }
    self.dead as f64 / self.total as f64
  }

  /// The alive qualifier-carrying imports, decoded against `interner`, **in the caller's
  /// file order** — resolution downstream is order-sensitive in its *emissions* (edge-log
  /// and adjacency order), so the retained feed must follow the same canonical (path-sorted)
  /// file order a from-scratch build processes. Files absent from the store are skipped.
  pub fn qualified_imports<'i>(
    &self,
    interner: &'i Interner,
    order: impl IntoIterator<Item = u32>,
  ) -> Vec<Reference<'i>> {
    let mut out = Vec::new();
    for path_bits in order {
      let Some(file) = self.files.get(&path_bits) else {
        continue;
      };
      for record in file.qualified.chunks_exact(RECORD) {
        out.push(decode_record(
          interner,
          record.try_into().expect("record-sized blob"),
        ));
      }
    }
    out
  }

  /// Raw chunk reader over the ALIVE ranges only, **in the caller's file order** (see
  /// [`RefStore::qualified_imports`] for why order is the caller's). Chunks never span two
  /// ranges; each holds at most [`SPILL_CHUNK`] records. The writer is flushed first so the
  /// reader sees every appended record.
  pub fn raw_chunks(
    &mut self,
    order: impl IntoIterator<Item = u32>,
  ) -> io::Result<StoreRawChunks> {
    self.out.flush()?;
    let ranges: Vec<Range<u64>> = order
      .into_iter()
      .filter_map(|bits| self.files.get(&bits).map(|f| f.records.clone()))
      .collect();
    Ok(StoreRawChunks {
      file: File::open(&self.path)?,
      ranges,
      range_index: 0,
      cursor: 0,
      positioned: false,
    })
  }

  /// Decode one raw chunk against `interner` — the worker-side half of the feed.
  pub fn decode_chunk<'i>(&self, interner: &'i Interner, bytes: &[u8]) -> Vec<Reference<'i>> {
    debug_assert_eq!(bytes.len() % RECORD, 0);
    bytes
      .chunks_exact(RECORD)
      .map(|record| decode_record(interner, record.try_into().expect("record-sized chunk")))
      .collect()
  }

  /// Delete the backing file. Best-effort cleanup at daemon shutdown.
  pub fn remove(self) -> io::Result<()> {
    drop(self.out);
    std::fs::remove_file(&self.path)
  }
}

/// Iterator of raw record-byte chunks over a store's alive ranges.
pub struct StoreRawChunks {
  file: File,
  ranges: Vec<Range<u64>>,
  range_index: usize,
  cursor: u64,
  positioned: bool,
}

impl Iterator for StoreRawChunks {
  type Item = io::Result<Vec<u8>>;

  fn next(&mut self) -> Option<Self::Item> {
    loop {
      let range = self.ranges.get(self.range_index)?.clone();
      if !self.positioned {
        self.cursor = range.start;
        if let Err(err) = self.file.seek(SeekFrom::Start(range.start * RECORD as u64)) {
          return Some(Err(err));
        }
        self.positioned = true;
      }
      if self.cursor >= range.end {
        self.range_index += 1;
        self.positioned = false;
        continue;
      }
      let take = ((range.end - self.cursor) as usize).min(SPILL_CHUNK);
      let mut bytes = vec![0u8; take * RECORD];
      if let Err(err) = self.file.read_exact(&mut bytes) {
        return Some(Err(err));
      }
      self.cursor += take as u64;
      return Some(Ok(bytes));
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use vorpal_kg::NodeId;

  fn itn() -> &'static Interner {
    static INTERNER: std::sync::OnceLock<Interner> = std::sync::OnceLock::new();
    INTERNER.get_or_init(Interner::new)
  }

  fn make_ref(path: &str, name: &str, from: u64) -> Reference<'static> {
    Reference::new(itn(), NodeId::new(from), path, name, RefKind::Call).with_evidence(1, 2)
  }

  fn drain(store: &mut RefStore, order: &[u32]) -> Vec<Reference<'static>> {
    let mut out = Vec::new();
    let chunks: Vec<Vec<u8>> = store
      .raw_chunks(order.iter().copied())
      .unwrap()
      .collect::<io::Result<Vec<_>>>()
      .unwrap();
    for bytes in chunks {
      out.extend(store.decode_chunk(itn(), &bytes));
    }
    out
  }

  #[test]
  fn append_retract_append_feeds_only_alive_ranges() {
    let dir = std::env::temp_dir().join(format!("vorpal-refstore-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("refs.store");
    let mut store = RefStore::create(&path).unwrap();

    let a: Vec<Reference> = (0..5).map(|i| make_ref("a.py", "alpha", i)).collect();
    let b: Vec<Reference> = (0..70_000).map(|i| make_ref("b.py", "beta", i)).collect();
    let c: Vec<Reference> = (0..3).map(|i| make_ref("c.py", "gamma", i)).collect();
    let a_bits = a[0].from_path.to_bits();
    let b_bits = b[0].from_path.to_bits();
    store.append_file(a_bits, &a).unwrap();
    store.append_file(b_bits, &b).unwrap();
    store.append_file(c[0].from_path.to_bits(), &c).unwrap();
    assert_eq!(store.count(), 70_008);

    // Edit b: retract + re-append a replacement.
    let b2: Vec<Reference> = (0..4).map(|i| make_ref("b.py", "beta2", i + 100)).collect();
    store.append_file(b_bits, &b2).unwrap();
    assert_eq!(store.count(), 12);
    assert!(store.dead_fraction() > 0.99);

    // Canonical (path-sorted) order: a.py, b.py, c.py — the store follows it even though
    // b's alive range now sits at the file tail.
    let c_bits = c[0].from_path.to_bits();
    let order = [a_bits, b_bits, c_bits];
    let fed = drain(&mut store, &order);
    let mut expect = Vec::new();
    expect.extend(a.iter().copied());
    expect.extend(b2.iter().copied());
    expect.extend(c.iter().copied());
    assert_eq!(fed, expect);

    // Delete a entirely.
    store.retract_file(a_bits);
    let fed = drain(&mut store, &order);
    let mut expect = Vec::new();
    expect.extend(b2.iter().copied());
    expect.extend(c.iter().copied());
    assert_eq!(fed, expect);
    store.remove().unwrap();
    let _ = std::fs::remove_dir(&dir);
  }

  #[test]
  fn qualified_imports_track_retirement() {
    let dir = std::env::temp_dir().join(format!("vorpal-refstore-qi-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut store = RefStore::create(&dir.join("refs.store")).unwrap();

    let plain = make_ref("m.py", "thing", 1);
    let qualified = Reference::new(itn(), NodeId::new(2), "m.py", "pkg", RefKind::Import)
      .with_form(RefForm::Static)
      .with_qualifier(itn(), Some("pkg.sub".to_string()));
    let bits = plain.from_path.to_bits();
    let both = [plain, qualified];
    store.append_file(bits, both.iter()).unwrap();
    assert_eq!(store.qualified_imports(itn(), [bits]), vec![qualified]);

    let replacement = make_ref("m.py", "other", 3);
    store.append_file(bits, std::iter::once(&replacement)).unwrap();
    assert!(store.qualified_imports(itn(), [bits]).is_empty());
    store.remove().unwrap();
    let _ = std::fs::remove_dir(&dir);
  }
}
