//! The products pack: every cached extraction product in **one appendable file**, mapped
//! zero-copy at replay.
//!
//! The loose-file cache paid one `open(2)` per product; at kernel scale that is 72k opens
//! per re-index, and macOS serializes enough of the open path that even 18 threads only
//! doubled throughput — the replay was syscall-bound, not byte-bound. The pack replaces it
//! for index runs: one mmap, per-entry slices, no opens. Loose files remain the write path
//! for *search-banked* products (concurrent scan processes must not contend on one file) and
//! are consolidated into the pack — then deleted — by the next index run.
//!
//! Layout (`products.pack`): magic + version, then length-prefixed records
//! `[path_len u32][path][body_len u32][body]` where `body` is the ordinary product codec.
//! The sidecar (`products.idx`) is `magic + version + covered_len u64 + count u64` plus
//! `[path_len u32][path][off u64][len u32]` per live entry — **an optimization, not a
//! source of truth**: a run killed after appending but before the sidecar lands loses no
//! work, because open() scans any records beyond `covered_len` (bounds-checked; a torn tail
//! record simply ends the scan) and products remain self-validating at decode time.
//!
//! **Determinism.** Records stream to the tail in arrival order *during* a run, but every
//! publish rewrites both files in **canonical order — live entries sorted by path** — so the
//! pack and sidecar are a pure function of the `(path, body)` set, independent of worker
//! completion order or incremental history. Two independent indexes of the same corpus
//! therefore produce byte-identical `products.pack`/`products.idx`. The rewrite is skipped
//! only when nothing changed (no appends, no dead bytes): the previous publish already left
//! the file canonical, so a no-change re-index stays a metadata check. Reads are
//! order-agnostic (they build a map), so the tail-scan recovery path is unaffected.

use std::collections::HashMap;
use std::fs;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use vorpal_mem::{AccessPattern, CorpusProbe, Hotness, MappedStore, ResourcePolicy, StoreKind};

pub const PACK_FILE: &str = "products.pack";
pub const PACK_INDEX: &str = "products.idx";
const PACK_MAGIC: &[u8; 4] = b"VPPK";
const IDX_MAGIC: &[u8; 4] = b"VPPI";
// v2: publishes are canonically ordered (entries sorted by path). v1 packs were written in
// arrival order; bumping the version retires them so the first index under this build rebuilds
// a canonical pack rather than inheriting a stale, unsorted layout.
const PACK_VERSION: u32 = 2;

/// A live entry: body offset + length within the pack.
type Entry = (u64, u32);

/// Read side: the mapped pack plus its live-entry index.
pub struct PackReader {
  store: Arc<MappedStore>,
  index: HashMap<Box<str>, Entry>,
}

impl PackReader {
  /// Open the pack under `dir`, if present and well-formed. Uses the sidecar when its
  /// covered length is consistent, then scans any appended tail; scans the whole pack when
  /// the sidecar is missing or invalid (the killed-run recovery path).
  pub fn open(dir: &Path) -> Option<PackReader> {
    let pack_path = dir.join(PACK_FILE);
    let store = Arc::new(
      MappedStore::map_file(
        &pack_path,
        StoreKind::VectorsFull,
        AccessPattern::Sequential,
        Hotness::Hot,
        &ResourcePolicy::probe(CorpusProbe::new(0, 0)),
      )
      .ok()?,
    );
    let bytes = store.as_bytes();
    if bytes.len() < 8 || &bytes[0..4] != PACK_MAGIC {
      return None;
    }
    if u32::from_le_bytes(bytes[4..8].try_into().ok()?) != PACK_VERSION {
      return None;
    }

    let mut index = HashMap::new();
    let mut scan_from = 8usize;
    if let Some((entries, covered)) = read_sidecar(&dir.join(PACK_INDEX), bytes.len()) {
      index = entries;
      scan_from = covered;
    }
    // Recovery / tail scan: pick up records the sidecar has not seen. A torn final record
    // fails a bounds check and ends the scan; whatever decoded cleanly is kept.
    let mut at = scan_from;
    while at + 8 <= bytes.len() {
      let path_len = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
      let Some(path_end) = (at + 4)
        .checked_add(path_len)
        .filter(|&e| e + 4 <= bytes.len())
      else {
        break;
      };
      let Ok(path) = std::str::from_utf8(&bytes[at + 4..path_end]) else {
        break;
      };
      let body_len = u32::from_le_bytes(bytes[path_end..path_end + 4].try_into().unwrap()) as usize;
      let body_at = path_end + 4;
      let Some(body_end) = body_at.checked_add(body_len).filter(|&e| e <= bytes.len()) else {
        break;
      };
      index.insert(path.into(), (body_at as u64, body_len as u32));
      at = body_end;
    }
    Some(PackReader { store, index })
  }

  /// The cached product bytes for `path`, if packed. Decode + stamp validation stay the
  /// caller's job — exactly as with a loose file's bytes.
  pub fn get(&self, path: &str) -> Option<&[u8]> {
    let &(off, len) = self.index.get(path)?;
    self
      .store
      .as_bytes()
      .get(off as usize..off as usize + len as usize)
  }

  fn entry(&self, path: &str) -> Option<Entry> {
    self.index.get(path).copied()
  }

  /// Every packed `(path, product bytes)` pair, in unspecified order — whole-bank sweeps
  /// (coverage overviews) sort their own results. Bytes are the raw cached product; decode
  /// and stamp validation stay the caller's job.
  pub fn entries(&self) -> impl Iterator<Item = (&str, &[u8])> {
    self.index.iter().filter_map(|(path, &(off, len))| {
      let bytes = self
        .store
        .as_bytes()
        .get(off as usize..off as usize + len as usize)?;
      Some((path.as_ref(), bytes))
    })
  }
}

fn read_sidecar(path: &Path, pack_len: usize) -> Option<(HashMap<Box<str>, Entry>, usize)> {
  let bytes = fs::read(path).ok()?;
  if bytes.len() < 24 || &bytes[0..4] != IDX_MAGIC {
    return None;
  }
  if u32::from_le_bytes(bytes[4..8].try_into().ok()?) != PACK_VERSION {
    return None;
  }
  let covered = u64::from_le_bytes(bytes[8..16].try_into().ok()?) as usize;
  if covered > pack_len {
    return None; // sidecar from a different pack generation
  }
  let count = u64::from_le_bytes(bytes[16..24].try_into().ok()?) as usize;
  let mut entries = HashMap::with_capacity(count);
  let mut at = 24usize;
  for _ in 0..count {
    let path_len = u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?) as usize;
    let path = std::str::from_utf8(bytes.get(at + 4..at + 4 + path_len)?).ok()?;
    let off_at = at + 4 + path_len;
    let off = u64::from_le_bytes(bytes.get(off_at..off_at + 8)?.try_into().ok()?);
    let len = u32::from_le_bytes(bytes.get(off_at + 8..off_at + 12)?.try_into().ok()?);
    if off as usize + len as usize > pack_len {
      return None;
    }
    entries.insert(path.into(), (off, len));
    at = off_at + 12;
  }
  Some((entries, covered))
}

/// One message from the extraction pipeline to the pack thread: a freshly encoded product
/// (new parse, or a loose file being consolidated). Products replayed straight from the
/// pack send **nothing** — their entries are carried into the new sidecar in bulk at
/// [`PackWriter::finish`], from the live path set. At kernel scale the per-file reuse
/// message was 72k sends against one channel; the profile showed every worker blocked on it.
pub struct PackMsg {
  pub path: String,
  pub body: Vec<u8>,
}

/// Where one canonical entry's body currently lives: freshly appended to this run's pack
/// (an offset into the local file), or carried from the prior generation's pack (fetched by
/// path through its mapped [`PackReader`]).
enum BodySource {
  Appended(Entry),
  Reused,
}

/// Write side: builds **this run's** pack in `dir` (a staging/new-generation directory) —
/// never mutating the prior generation's pack, which is only *read* through `reader`. Feed it
/// with [`PackWriter::sink`] clones from any thread; call [`PackWriter::finish`] after the
/// pipeline ends to publish the canonical pack + sidecar.
pub struct PackWriter {
  dir: PathBuf,
  rx: crossbeam_channel::Receiver<PackMsg>,
  tx: Option<crossbeam_channel::Sender<PackMsg>>,
  reader: Option<Arc<PackReader>>,
}

impl PackWriter {
  pub fn new(dir: &Path, reader: Option<Arc<PackReader>>) -> Self {
    let (tx, rx) = crossbeam_channel::bounded(1024);
    Self {
      dir: dir.to_path_buf(),
      rx,
      tx: Some(tx),
      reader,
    }
  }

  pub fn sink(&self) -> crossbeam_channel::Sender<PackMsg> {
    self.tx.as_ref().expect("sink before finish").clone()
  }

  /// Drain every append (streamed to disk as it arrives — bounded memory), carry entries for
  /// every path in `live` that was not re-appended from the prior generation's pack, then
  /// publish the **canonical** pack (entries sorted by path — a pure function of the
  /// `(path, body)` set) plus its sidecar, via `.tmp` + rename. The prior pack is never
  /// touched: reused bodies are copied out of `reader`'s mapping, so the previous generation
  /// stays complete for any reader still holding it. Call only after every
  /// [`PackWriter::sink`] clone is dropped.
  pub fn finish(self, live: impl IntoIterator<Item = String>) -> io::Result<()> {
    let mut this = self;
    drop(this.tx.take());
    // Fresh spool for this run's appended records (magic + version header first). A side
    // file, not `products.pack` itself: the reader may be mapping a same-named prior pack in
    // this very directory (legacy flat layout, tests), and truncating it in place would
    // clobber the bodies reuse is about to copy. The canonical pack lands via tmp + rename at
    // the end, so `products.pack` is only ever a complete prior pack or a complete new one.
    let pack_path = this.dir.join(PACK_FILE);
    let spool_path = this.dir.join("products.pack.spool");
    let mut file = fs::File::create(&spool_path)?;
    file.write_all(PACK_MAGIC)?;
    file.write_all(&PACK_VERSION.to_le_bytes())?;
    let mut at = 8u64;
    let mut out = BufWriter::with_capacity(1 << 20, file);

    let mut entries: Vec<(String, BodySource)> = Vec::new();
    let mut appended: std::collections::HashSet<Box<str>> = std::collections::HashSet::new();
    while let Ok(PackMsg { path, body }) = this.rx.recv() {
      out.write_all(&(path.len() as u32).to_le_bytes())?;
      out.write_all(path.as_bytes())?;
      out.write_all(&(body.len() as u32).to_le_bytes())?;
      out.write_all(&body)?;
      let body_at = at + 4 + path.len() as u64 + 4;
      appended.insert(path.as_str().into());
      entries.push((path, BodySource::Appended((body_at, body.len() as u32))));
      at = body_at + body.len() as u64;
    }
    out.flush()?;
    drop(out);
    // Bulk reuse: every live path not re-appended carries over from the prior pack.
    if let Some(reader) = &this.reader {
      for path in live {
        if !appended.contains(path.as_str()) && reader.entry(&path).is_some() {
          entries.push((path, BodySource::Reused));
        }
      }
    }

    // Canonical order: sort by path so the published bytes are a pure function of the
    // `(path, body)` set — independent of worker completion order and incremental history
    // (this is what makes an incremental generation converge byte-for-byte to a from-scratch
    // build of the same tree). Paths are unique per entry (one product per file; reuse skips
    // re-appended paths), so this is a total, machine-independent order.
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    let spooled = fs::read(&spool_path)?;
    let tmp = this.dir.join("products.pack.tmp");
    let mut out = BufWriter::with_capacity(1 << 20, fs::File::create(&tmp)?);
    out.write_all(PACK_MAGIC)?;
    out.write_all(&PACK_VERSION.to_le_bytes())?;
    let mut new_at = 8u64;
    let mut final_entries: Vec<(String, Entry)> = Vec::with_capacity(entries.len());
    for (path, source) in entries {
      let body: &[u8] = match &source {
        BodySource::Appended((off, len)) => spooled
          .get(*off as usize..(*off + *len as u64) as usize)
          .ok_or_else(|| io::Error::other("appended pack entry out of bounds"))?,
        BodySource::Reused => this
          .reader
          .as_ref()
          .and_then(|r| r.get(&path))
          .ok_or_else(|| io::Error::other("reused pack entry vanished from prior pack"))?,
      };
      out.write_all(&(path.len() as u32).to_le_bytes())?;
      out.write_all(path.as_bytes())?;
      out.write_all(&(body.len() as u32).to_le_bytes())?;
      out.write_all(body)?;
      let body_at = new_at + 4 + path.len() as u64 + 4;
      new_at = body_at + body.len() as u64;
      final_entries.push((path, (body_at, body.len() as u32)));
    }
    out.flush()?;
    drop(out);
    fs::rename(&tmp, &pack_path)?;
    let _ = fs::remove_file(&spool_path);
    let entries = final_entries;

    let covered = fs::metadata(&pack_path)?.len();
    let idx_tmp = this.dir.join("products.idx.tmp");
    let mut idx = BufWriter::with_capacity(1 << 20, fs::File::create(&idx_tmp)?);
    idx.write_all(IDX_MAGIC)?;
    idx.write_all(&PACK_VERSION.to_le_bytes())?;
    idx.write_all(&covered.to_le_bytes())?;
    idx.write_all(&(entries.len() as u64).to_le_bytes())?;
    for (path, (off, len)) in &entries {
      idx.write_all(&(path.len() as u32).to_le_bytes())?;
      idx.write_all(path.as_bytes())?;
      idx.write_all(&off.to_le_bytes())?;
      idx.write_all(&len.to_le_bytes())?;
    }
    idx.flush()?;
    drop(idx);
    fs::rename(&idx_tmp, this.dir.join(PACK_INDEX))?;
    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn scratch_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("vorpal-pack-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
  }

  #[test]
  fn appends_reads_reuses_and_recovers() {
    let dir = scratch_dir("basic");
    let writer = PackWriter::new(&dir, None);
    let sink = writer.sink();
    sink
      .send(PackMsg {
        path: "a.rs".into(),
        body: b"alpha-bytes".to_vec(),
      })
      .unwrap();
    sink
      .send(PackMsg {
        path: "b.rs".into(),
        body: b"beta".to_vec(),
      })
      .unwrap();
    drop(sink);
    writer.finish(Vec::new()).unwrap();

    let reader = PackReader::open(&dir).unwrap();
    assert_eq!(reader.get("a.rs"), Some(&b"alpha-bytes"[..]));
    assert_eq!(reader.get("b.rs"), Some(&b"beta"[..]));
    assert_eq!(reader.get("missing.rs"), None);

    // Second generation: reuse a, replace b, and survive without a sidecar.
    let reader = Arc::new(reader);
    let writer = PackWriter::new(&dir, Some(reader.clone()));
    let sink = writer.sink();
    sink
      .send(PackMsg {
        path: "b.rs".into(),
        body: b"beta-v2".to_vec(),
      })
      .unwrap();
    drop(sink);
    // `a.rs` is carried by the bulk-reuse path: live but not re-appended.
    writer
      .finish(vec!["a.rs".to_string(), "b.rs".to_string()])
      .unwrap();
    fs::remove_file(dir.join(PACK_INDEX)).unwrap(); // killed-run recovery: no sidecar
    let recovered = PackReader::open(&dir).unwrap();
    assert_eq!(recovered.get("a.rs"), Some(&b"alpha-bytes"[..]));
    assert_eq!(recovered.get("b.rs"), Some(&b"beta-v2"[..]));

    let _ = fs::remove_dir_all(&dir);
  }

  #[test]
  fn publish_is_byte_identical_regardless_of_arrival_order() {
    // Determinism contract: the same (path, body) set produces byte-identical pack + sidecar
    // no matter the order records arrive in. This is what makes indexes fleet-coherent.
    let products = [
      ("z/last.rs", b"zzz".as_slice()),
      ("a/first.rs", b"aaaa".as_slice()),
      ("m/mid.rs", b"mm".as_slice()),
      ("a/second.rs", b"a2".as_slice()),
    ];
    let build = |tag: &str, order: &[usize]| -> (Vec<u8>, Vec<u8>) {
      let dir = scratch_dir(tag);
      let writer = PackWriter::new(&dir, None);
      let sink = writer.sink();
      for &i in order {
        sink
          .send(PackMsg {
            path: products[i].0.into(),
            body: products[i].1.to_vec(),
          })
          .unwrap();
      }
      drop(sink);
      writer.finish(Vec::new()).unwrap();
      let pack = fs::read(dir.join(PACK_FILE)).unwrap();
      let idx = fs::read(dir.join(PACK_INDEX)).unwrap();
      let _ = fs::remove_dir_all(&dir);
      (pack, idx)
    };
    let (pack_a, idx_a) = build("order-a", &[0, 1, 2, 3]);
    let (pack_b, idx_b) = build("order-b", &[3, 2, 1, 0]);
    let (pack_c, idx_c) = build("order-c", &[2, 0, 3, 1]);
    assert_eq!(pack_a, pack_b, "pack differs by arrival order");
    assert_eq!(pack_a, pack_c, "pack differs by arrival order");
    assert_eq!(idx_a, idx_b, "sidecar differs by arrival order");
    assert_eq!(idx_a, idx_c, "sidecar differs by arrival order");
    // And the canonical order is genuinely sorted: first live record is a/first.rs.
    let first = &pack_a[12..12 + "a/first.rs".len()];
    assert_eq!(first, b"a/first.rs");

    // A reused-entry publish (second generation, no re-append) must reproduce the same bytes.
    let dir = scratch_dir("reuse-canonical");
    let w1 = PackWriter::new(&dir, None);
    let s1 = w1.sink();
    for &i in &[1usize, 3, 0, 2] {
      s1.send(PackMsg { path: products[i].0.into(), body: products[i].1.to_vec() }).unwrap();
    }
    drop(s1);
    w1.finish(Vec::new()).unwrap();
    let gen1 = fs::read(dir.join(PACK_FILE)).unwrap();
    let reader = Arc::new(PackReader::open(&dir).unwrap());
    let w2 = PackWriter::new(&dir, Some(reader));
    drop(w2.sink()); // no appends: everything is carried by bulk reuse
    w2.finish(products.iter().map(|(p, _)| p.to_string())).unwrap();
    let gen2 = fs::read(dir.join(PACK_FILE)).unwrap();
    assert_eq!(gen1, gen2, "no-change reuse re-publish is not byte-stable");
    let _ = fs::remove_dir_all(&dir);
  }

  #[test]
  fn torn_tail_record_is_dropped_not_fatal() {
    let dir = scratch_dir("torn");
    let writer = PackWriter::new(&dir, None);
    let sink = writer.sink();
    sink
      .send(PackMsg {
        path: "ok.rs".into(),
        body: b"whole".to_vec(),
      })
      .unwrap();
    drop(sink);
    writer.finish(Vec::new()).unwrap();
    // Simulate a crash mid-append: a half-written record after the covered length.
    let mut bytes = fs::read(dir.join(PACK_FILE)).unwrap();
    bytes.extend_from_slice(&(9u32).to_le_bytes());
    bytes.extend_from_slice(b"torn"); // path shorter than declared
    fs::write(dir.join(PACK_FILE), &bytes).unwrap();
    fs::remove_file(dir.join(PACK_INDEX)).unwrap();
    let reader = PackReader::open(&dir).unwrap();
    assert_eq!(reader.get("ok.rs"), Some(&b"whole"[..]));
    let _ = fs::remove_dir_all(&dir);
  }
}
