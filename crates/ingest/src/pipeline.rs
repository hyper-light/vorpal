//! The bounded streaming orchestrator, decoupled from how files are parsed (§3.4), with a
//! two-pass linking step that resolves references into `calls`/`references` edges (§3.3).

use std::collections::HashMap;
use std::io;
use std::path::Path;

use vorpal_kg::{EdgeType, Kg, KgWriter, NodeId, SymbolKind};
use vorpal_resolve::{Reference, ResolveStats, Resolver, Symbol, SymbolTable, resolve_all};

/// Turns one file's source into KG nodes/edges via the writer, appending any references it finds
/// to `references` for later resolution. Implementors own their parse tree locally.
pub trait FileExtractor {
  fn extract_into<'i>(
    &self,
    interner: &'i vorpal_resolve::Interner,
    path: &str,
    source: &str,
    writer: &mut KgWriter,
    references: &mut Vec<Reference<'i>>,
  );

  /// Whether this extractor handles `path` (default: all files). Directory ingestion skips files
  /// for which this is false, avoiding reads of unsupported types.
  fn handles(&self, _path: &str) -> bool {
    true
  }
}

/// Running totals for an ingest session.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct IngestStats {
  pub indexed: u64,
  pub skipped: u64,
  pub bytes: u64,
}

/// Per-file result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileOutcome {
  Indexed,
  Skipped,
}

/// A single-writer ingest sink: reads files, applies content-hash skip, drives extraction into a
/// [`KgWriter`], buffers references, and seals a queryable [`Kg`] (optionally after linking).
pub struct Ingestor<'i, E: FileExtractor> {
  interner: &'i vorpal_resolve::Interner,
  extractor: E,
  writer: KgWriter,
  references: Vec<Reference<'i>>,
  seen: HashMap<String, [u8; 32]>,
  stats: IngestStats,
}

impl<'i, E: FileExtractor> Ingestor<'i, E> {
  pub fn new(interner: &'i vorpal_resolve::Interner, extractor: E) -> Self {
    Self {
      interner,
      extractor,
      writer: KgWriter::new(),
      references: Vec::new(),
      seen: HashMap::new(),
      stats: IngestStats::default(),
    }
  }

  /// Ingest one in-memory source. Content-hash skip (§3.4): if `path` was last seen with the same
  /// bytes, it is not re-parsed.
  pub fn ingest_source(&mut self, path: &str, source: &str) -> FileOutcome {
    let hash = *blake3::hash(source.as_bytes()).as_bytes();
    self.stats.bytes += source.len() as u64;
    if self.seen.get(path) == Some(&hash) {
      self.stats.skipped += 1;
      return FileOutcome::Skipped;
    }
    self.seen.insert(path.to_owned(), hash);
    self.extractor.extract_into(
      self.interner,
      path,
      source,
      &mut self.writer,
      &mut self.references,
    );
    self.stats.indexed += 1;
    FileOutcome::Indexed
  }

  /// Read a file (bounded by its size — the only transient buffer) and ingest it.
  pub fn ingest_file(&mut self, path: &Path) -> io::Result<FileOutcome> {
    let source = std::fs::read_to_string(path)?;
    Ok(self.ingest_source(&path.to_string_lossy(), &source))
  }

  /// Ingest a pre-extracted [`FileProduct`] (freshly built or replayed from the incremental
  /// cache) — defines the file's entities and re-attributes its references by entity path.
  /// Takes the product by value: its reference strings are moved into the buffered
  /// [`Reference`]s, so the single-writer apply stage clones nothing.
  pub fn ingest_product(&mut self, path: &str, product: crate::FileProduct) {
    apply_product(self.interner, path, product, &mut self.writer, &mut self.references);
  }

  /// Recursively ingest a directory, respecting `.gitignore`, skipping files the extractor does
  /// not handle. Bounded: one file is read at a time. Per-file read errors (e.g. non-UTF-8) are
  /// skipped so a stray file cannot abort the walk.
  pub fn ingest_dir(&mut self, root: &Path) -> io::Result<()> {
    for entry in ignore::Walk::new(root) {
      let entry = entry.map_err(io::Error::other)?;
      if !entry.file_type().is_some_and(|t| t.is_file()) {
        continue;
      }
      let path = entry.path();
      if !self.extractor.handles(path.to_string_lossy().as_ref()) {
        continue;
      }
      let _ = self.ingest_file(path);
    }
    Ok(())
  }

  pub fn stats(&self) -> IngestStats {
    self.stats
  }

  /// Distinct entities interned so far.
  pub fn node_count(&self) -> usize {
    self.writer.node_count()
  }

  /// References buffered so far, awaiting resolution.
  pub fn pending_references(&self) -> usize {
    self.references.len()
  }

  /// Seal definitions + containment only (buffered references are dropped unresolved).
  pub fn seal(self) -> Kg {
    self.writer.seal()
  }

  /// Two-pass link + seal (§3.3): build the symbol table from interned definitions, resolve every
  /// buffered reference, inject the resolved edges, then seal. Returns the graph and resolution
  /// stats. Unresolvable references produce no edge — they are counted, never faked.
  pub fn link_and_seal(self, resolver: &Resolver) -> (Kg, ResolveStats) {
    link_writer(self.interner, self.writer, self.references, resolver)
  }
}

/// The global linking tail shared by [`Ingestor::link_and_seal`] and the sharded commit path:
/// symbol table from the writer's definitions, resolve every reference, inject the resolved
/// edges, seal. Takes the references by value so the ~hundreds of MB of reference strings —
/// and the symbol table — are freed **before** seal allocates the segment buffer: at kernel
/// scale those two corpses pinned ~1 GB through the peak-memory moment for nothing.
/// Where the (rebased) reference stream goes during a streamed apply: RAM for callers that
/// want the vector, or the disk spill for bulk builds — at kernel scale the in-RAM vector
/// was ~220 MB of peak footprint that resolution only ever reads once, sequentially.
/// One traceable call-site argument riding beside its reference through the apply→link
/// hand-off (G-M3). `from` is the caller entity (rebased with its reference); the (from,
/// span) pair keys the join against resolved call edges at link time.
pub(crate) struct ArgRec {
  pub(crate) from: NodeId,
  pub(crate) span: (u32, u32),
  pub(crate) index: u16,
  pub(crate) class: u8,
  /// The call had a receiver (`x.foo(...)`) — drives the Python self/cls offset when the
  /// callee's parameter ledger starts with one.
  pub(crate) has_receiver: bool,
  pub(crate) expr: Option<Box<str>>,
  /// Keyword name at the call site (`f(x=1)`), bound to the callee's parameter position at
  /// link time (G-M5).
  pub(crate) kw: Option<Box<str>>,
}

/// One callee's ordered parameter names (Python only — the kwarg/self binding ledger).
/// Splat entries keep their sigils (`*args`, `**kwargs`) so they can never match a keyword.
pub(crate) struct ParamRec {
  pub(crate) entity: NodeId,
  pub(crate) names: Box<[Box<str>]>,
}

/// One `function name → declared return type` row (G-M5 chained-call typing). Name-keyed —
/// no id to rebase — and deduplicated/poisoned when the link-time ledger builds.
pub(crate) struct RetRec {
  pub(crate) name: Box<str>,
  pub(crate) ret: Box<str>,
}

/// One signed definition's near-clone sketch (v16), keyed by its entity id — rebased with
/// its shard like a parameter ledger.
pub(crate) struct SigRec {
  pub(crate) entity: NodeId,
  pub(crate) shingles: u32,
  pub(crate) sketch: [u8; crate::signature::BINS],
}

/// One HTTP client call site riding beside its file's references (v17), keyed by the
/// calling entity — rebased with its shard.
pub(crate) struct ReqRec {
  pub(crate) from: NodeId,
  pub(crate) method: Box<str>,
  pub(crate) path: Box<str>,
  pub(crate) span: (u32, u32),
}

/// Side rows a shard hands the absorber beside its references: call-site argument records,
/// callee parameter ledgers, return ledgers, near-clone sketches, and request records —
/// one struct so the absorb/committer plumbing keeps a fixed shape as side data grows.
#[derive(Default)]
pub(crate) struct FlowSidecar {
  pub(crate) args: Vec<ArgRec>,
  pub(crate) params: Vec<ParamRec>,
  pub(crate) rets: Vec<RetRec>,
  pub(crate) sigs: Vec<SigRec>,
  pub(crate) requests: Vec<ReqRec>,
}

/// Append-only arg spill (staging `.args.spill`): written by the absorber in absorb order,
/// loaded once at link into the join map. Process-private scratch — no versioning.
pub(crate) struct ArgSpillWriter {
  file: std::io::BufWriter<std::fs::File>,
  path: std::path::PathBuf,
  count: u64,
}

impl ArgSpillWriter {
  fn create(path: &std::path::Path) -> io::Result<Self> {
    Ok(Self {
      file: std::io::BufWriter::new(std::fs::File::create(path)?),
      path: path.to_path_buf(),
      count: 0,
    })
  }

  fn push(&mut self, rec: &ArgRec) -> io::Result<()> {
    use std::io::Write;
    self.file.write_all(&rec.from.raw().to_le_bytes())?;
    self.file.write_all(&rec.span.0.to_le_bytes())?;
    self.file.write_all(&rec.span.1.to_le_bytes())?;
    self.file.write_all(&rec.index.to_le_bytes())?;
    self.file.write_all(&[rec.class, rec.has_receiver as u8])?;
    let expr = rec.expr.as_deref().unwrap_or("");
    self.file.write_all(&(expr.len() as u16).to_le_bytes())?;
    self.file.write_all(expr.as_bytes())?;
    let kw = rec.kw.as_deref().unwrap_or("");
    self.file.write_all(&[kw.len() as u8])?;
    self.file.write_all(kw.as_bytes())?;
    self.count += 1;
    Ok(())
  }

  fn finish(mut self) -> io::Result<(std::path::PathBuf, u64)> {
    use std::io::Write;
    self.file.flush()?;
    Ok((self.path, self.count))
  }
}

/// The link-time argument join: one flat record vector sorted by (from, span), looked up by
/// binary search — measured against a HashMap<(from,span), Vec<..>> shape, the per-key
/// vectors and hashing dominated the join's cost at ~500k records. Holds launch-language
/// traceable args only; freed with the link phase; the spill file is deleted after load.
pub(crate) struct ArgJoin {
  records: Vec<ArgRec>,
  /// `(from << 64) | (span.0 << 32) | span.1` per record, sorted — the probe compares one
  /// u128 instead of extracting a 3-tuple, which halved the join's share of link resolve.
  keys: Vec<u128>,
}

#[inline]
fn arg_key(from: u64, span: (u32, u32)) -> u128 {
  ((from as u128) << 64) | ((span.0 as u128) << 32) | span.1 as u128
}

impl ArgJoin {
  pub(crate) fn empty() -> Self {
    Self {
      records: Vec::new(),
      keys: Vec::new(),
    }
  }

  pub(crate) fn is_empty(&self) -> bool {
    self.records.is_empty()
  }

  #[inline]
  pub(crate) fn get(&self, from: u64, span: (u32, u32)) -> &[ArgRec] {
    let key = arg_key(from, span);
    let start = self.keys.partition_point(|&k| k < key);
    let end = start + self.keys[start..].iter().take_while(|&&k| k == key).count();
    &self.records[start..end]
  }
}

pub(crate) fn load_arg_spill(path: &std::path::Path, expected: u64) -> io::Result<ArgJoin> {
  use std::io::Read;
  let mut bytes = Vec::new();
  std::fs::File::open(path)?.read_to_end(&mut bytes)?;
  let _ = std::fs::remove_file(path);
  let mut records: Vec<ArgRec> = Vec::with_capacity(expected as usize);
  let mut off = 0usize;
  let mut seen = 0u64;
  while off < bytes.len() {
    let take = |o: usize, n: usize| -> io::Result<&[u8]> {
      bytes
        .get(o..o + n)
        .ok_or_else(|| io::Error::other("arg spill truncated"))
    };
    let from = u64::from_le_bytes(take(off, 8)?.try_into().expect("8B"));
    let s0 = u32::from_le_bytes(take(off + 8, 4)?.try_into().expect("4B"));
    let s1 = u32::from_le_bytes(take(off + 12, 4)?.try_into().expect("4B"));
    let index = u16::from_le_bytes(take(off + 16, 2)?.try_into().expect("2B"));
    let class = take(off + 18, 1)?[0];
    let has_receiver = take(off + 19, 1)?[0] != 0;
    let expr_len = u16::from_le_bytes(take(off + 20, 2)?.try_into().expect("2B")) as usize;
    let expr_bytes = take(off + 22, expr_len)?;
    let expr = if expr_len == 0 {
      None
    } else {
      Some(
        std::str::from_utf8(expr_bytes)
          .map_err(|_| io::Error::other("arg spill: non-utf8 expression"))?
          .into(),
      )
    };
    off += 22 + expr_len;
    let kw_len = take(off, 1)?[0] as usize;
    let kw_bytes = take(off + 1, kw_len)?;
    let kw = if kw_len == 0 {
      None
    } else {
      Some(
        std::str::from_utf8(kw_bytes)
          .map_err(|_| io::Error::other("arg spill: non-utf8 keyword"))?
          .into(),
      )
    };
    off += 1 + kw_len;
    seen += 1;
    records.push(ArgRec {
      from: NodeId::new(from),
      span: (s0, s1),
      index,
      class,
      has_receiver,
      expr,
      kw,
    });
  }
  if seen != expected {
    return Err(io::Error::other(format!(
      "arg spill holds {seen} records, absorber wrote {expected} — torn scratch"
    )));
  }
  let keys = {
    use rayon::prelude::*;
    // Stable by (key, index): records per call site keep their capture order.
    records.par_sort_by_key(|r| (r.from.raw(), r.span.0, r.span.1, r.index));
    records
      .par_iter()
      .map(|r| arg_key(r.from.raw(), r.span))
      .collect()
  };
  Ok(ArgJoin { records, keys })
}

/// Append-only parameter-ledger spill (`.params.spill`), one record per Python entity with
/// parameters: entity u64 · count u16 · (len u8 · bytes)*. Same lifecycle as the arg spill.
pub(crate) struct ParamSpillWriter {
  file: std::io::BufWriter<std::fs::File>,
  path: std::path::PathBuf,
  count: u64,
}

impl ParamSpillWriter {
  fn create(path: &std::path::Path) -> io::Result<Self> {
    Ok(Self {
      file: std::io::BufWriter::new(std::fs::File::create(path)?),
      path: path.to_path_buf(),
      count: 0,
    })
  }

  fn push(&mut self, rec: &ParamRec) -> io::Result<()> {
    use std::io::Write;
    self.file.write_all(&rec.entity.raw().to_le_bytes())?;
    self.file.write_all(&(rec.names.len() as u16).to_le_bytes())?;
    for name in rec.names.iter() {
      self.file.write_all(&[name.len() as u8])?;
      self.file.write_all(name.as_bytes())?;
    }
    self.count += 1;
    Ok(())
  }

  fn finish(mut self) -> io::Result<(std::path::PathBuf, u64)> {
    use std::io::Write;
    self.file.flush()?;
    Ok((self.path, self.count))
  }
}

/// Append-only return-ledger spill (`.rets.spill`): (len u8 · name)(len u8 · ret) rows.
/// Name-keyed, so the absorber never rebases them; the link pass folds them into the
/// resolver's `ChainReturns` (dedup + disagreement poison there).
pub(crate) struct RetSpillWriter {
  file: std::io::BufWriter<std::fs::File>,
  path: std::path::PathBuf,
  count: u64,
}

impl RetSpillWriter {
  fn create(path: &std::path::Path) -> io::Result<Self> {
    Ok(Self {
      file: std::io::BufWriter::new(std::fs::File::create(path)?),
      path: path.to_path_buf(),
      count: 0,
    })
  }

  fn push(&mut self, rec: &RetRec) -> io::Result<()> {
    use std::io::Write;
    self.file.write_all(&[rec.name.len() as u8])?;
    self.file.write_all(rec.name.as_bytes())?;
    self.file.write_all(&[rec.ret.len() as u8])?;
    self.file.write_all(rec.ret.as_bytes())?;
    self.count += 1;
    Ok(())
  }

  fn finish(mut self) -> io::Result<(std::path::PathBuf, u64)> {
    use std::io::Write;
    self.file.flush()?;
    Ok((self.path, self.count))
  }
}

pub(crate) fn load_ret_spill(
  path: &std::path::Path,
  expected: u64,
) -> io::Result<Vec<(Box<str>, Box<str>)>> {
  use std::io::Read;
  let mut bytes = Vec::new();
  std::fs::File::open(path)?.read_to_end(&mut bytes)?;
  let _ = std::fs::remove_file(path);
  let mut rows = Vec::with_capacity(expected as usize);
  let mut off = 0usize;
  let mut seen = 0u64;
  while off < bytes.len() {
    let take = |o: usize, n: usize| -> io::Result<&[u8]> {
      bytes
        .get(o..o + n)
        .ok_or_else(|| io::Error::other("ret spill truncated"))
    };
    let name_len = take(off, 1)?[0] as usize;
    let name = std::str::from_utf8(take(off + 1, name_len)?)
      .map_err(|_| io::Error::other("ret spill: non-utf8 name"))?;
    off += 1 + name_len;
    let ret_len = take(off, 1)?[0] as usize;
    let ret = std::str::from_utf8(take(off + 1, ret_len)?)
      .map_err(|_| io::Error::other("ret spill: non-utf8 type"))?;
    off += 1 + ret_len;
    rows.push((Box::from(name), Box::from(ret)));
    seen += 1;
  }
  if seen != expected {
    return Err(io::Error::other(format!(
      "ret spill holds {seen} records, absorber wrote {expected} — torn scratch"
    )));
  }
  Ok(rows)
}

/// entity id → ordered parameter names, binary-searched (sorted by entity; one row per
/// entity — later duplicates for the same id cannot exist because each entity's defining
/// file lands exactly once per build).
pub(crate) struct ParamTable {
  rows: Vec<(u64, Box<[Box<str>]>)>,
}

impl ParamTable {
  pub(crate) fn empty() -> Self {
    Self { rows: Vec::new() }
  }

  pub(crate) fn is_empty(&self) -> bool {
    self.rows.is_empty()
  }

  pub(crate) fn get(&self, entity: u64) -> Option<&[Box<str>]> {
    let at = self.rows.binary_search_by_key(&entity, |(id, _)| *id).ok()?;
    Some(&self.rows[at].1)
  }
}

pub(crate) fn load_param_spill(path: &std::path::Path, expected: u64) -> io::Result<ParamTable> {
  use std::io::Read;
  let mut bytes = Vec::new();
  std::fs::File::open(path)?.read_to_end(&mut bytes)?;
  let _ = std::fs::remove_file(path);
  let mut rows: Vec<(u64, Box<[Box<str>]>)> = Vec::with_capacity(expected as usize);
  let mut off = 0usize;
  let mut seen = 0u64;
  while off < bytes.len() {
    let take = |o: usize, n: usize| -> io::Result<&[u8]> {
      bytes
        .get(o..o + n)
        .ok_or_else(|| io::Error::other("param spill truncated"))
    };
    let entity = u64::from_le_bytes(take(off, 8)?.try_into().expect("8B"));
    let count = u16::from_le_bytes(take(off + 8, 2)?.try_into().expect("2B")) as usize;
    off += 10;
    let mut names = Vec::with_capacity(count);
    for _ in 0..count {
      let len = take(off, 1)?[0] as usize;
      let name = std::str::from_utf8(take(off + 1, len)?)
        .map_err(|_| io::Error::other("param spill: non-utf8 name"))?;
      names.push(Box::from(name));
      off += 1 + len;
    }
    rows.push((entity, names.into_boxed_slice()));
    seen += 1;
  }
  if seen != expected {
    return Err(io::Error::other(format!(
      "param spill holds {seen} records, absorber wrote {expected} — torn scratch"
    )));
  }
  rows.sort_unstable_by_key(|(id, _)| *id);
  Ok(ParamTable { rows })
}

/// Fixed-width sketch spill (`.sigs.spill`): entity u64 · shingles u32 · 64 sketch bytes.
/// Same lifecycle as the arg spill.
pub(crate) struct SigSpillWriter {
  file: std::io::BufWriter<std::fs::File>,
  path: std::path::PathBuf,
  count: u64,
}

const SIG_RECORD_LEN: usize = 8 + 4 + crate::signature::BINS;

impl SigSpillWriter {
  fn create(path: &std::path::Path) -> io::Result<Self> {
    Ok(Self {
      file: std::io::BufWriter::new(std::fs::File::create(path)?),
      path: path.to_path_buf(),
      count: 0,
    })
  }

  fn push(&mut self, rec: &SigRec) -> io::Result<()> {
    use std::io::Write;
    self.file.write_all(&rec.entity.raw().to_le_bytes())?;
    self.file.write_all(&rec.shingles.to_le_bytes())?;
    self.file.write_all(&rec.sketch)?;
    self.count += 1;
    Ok(())
  }

  fn finish(mut self) -> io::Result<(std::path::PathBuf, u64)> {
    use std::io::Write;
    self.file.flush()?;
    Ok((self.path, self.count))
  }
}

pub(crate) fn load_sig_spill(
  path: &std::path::Path,
  expected: u64,
) -> io::Result<Vec<crate::similar::SigRow>> {
  use std::io::Read;
  let mut bytes = Vec::new();
  std::fs::File::open(path)?.read_to_end(&mut bytes)?;
  let _ = std::fs::remove_file(path);
  if bytes.len() % SIG_RECORD_LEN != 0 || (bytes.len() / SIG_RECORD_LEN) as u64 != expected {
    return Err(io::Error::other(format!(
      "sig spill holds {} bytes for {expected} records of {SIG_RECORD_LEN} — torn scratch",
      bytes.len()
    )));
  }
  Ok(
    bytes
      .chunks_exact(SIG_RECORD_LEN)
      .map(|rec| {
        let mut sketch = [0u8; crate::signature::BINS];
        sketch.copy_from_slice(&rec[12..]);
        crate::similar::SigRow {
          node: u64::from_le_bytes(rec[..8].try_into().expect("8B")),
          shingles: u32::from_le_bytes(rec[8..12].try_into().expect("4B")),
          sketch,
        }
      })
      .collect(),
  )
}

/// Request spill (`.reqs.spill`): from u64 · span u32×2 · method len u8 + bytes ·
/// path len u16 + bytes. Same lifecycle as the arg spill.
pub(crate) struct ReqSpillWriter {
  file: std::io::BufWriter<std::fs::File>,
  path: std::path::PathBuf,
  count: u64,
}

impl ReqSpillWriter {
  fn create(path: &std::path::Path) -> io::Result<Self> {
    Ok(Self {
      file: std::io::BufWriter::new(std::fs::File::create(path)?),
      path: path.to_path_buf(),
      count: 0,
    })
  }

  fn push(&mut self, rec: &ReqRec) -> io::Result<()> {
    use std::io::Write;
    self.file.write_all(&rec.from.raw().to_le_bytes())?;
    self.file.write_all(&rec.span.0.to_le_bytes())?;
    self.file.write_all(&rec.span.1.to_le_bytes())?;
    self.file.write_all(&[rec.method.len() as u8])?;
    self.file.write_all(rec.method.as_bytes())?;
    self.file.write_all(&(rec.path.len() as u16).to_le_bytes())?;
    self.file.write_all(rec.path.as_bytes())?;
    self.count += 1;
    Ok(())
  }

  fn finish(mut self) -> io::Result<(std::path::PathBuf, u64)> {
    use std::io::Write;
    self.file.flush()?;
    Ok((self.path, self.count))
  }
}

pub(crate) fn load_req_spill(
  path: &std::path::Path,
  expected: u64,
) -> io::Result<Vec<crate::requests::ReqRow>> {
  use std::io::Read;
  let mut bytes = Vec::new();
  std::fs::File::open(path)?.read_to_end(&mut bytes)?;
  let _ = std::fs::remove_file(path);
  let mut rows = Vec::with_capacity(expected as usize);
  let mut off = 0usize;
  let mut seen = 0u64;
  while off < bytes.len() {
    let take = |o: usize, n: usize| -> io::Result<&[u8]> {
      bytes
        .get(o..o + n)
        .ok_or_else(|| io::Error::other("request spill truncated"))
    };
    let from = u64::from_le_bytes(take(off, 8)?.try_into().expect("8B"));
    let s0 = u32::from_le_bytes(take(off + 8, 4)?.try_into().expect("4B"));
    let s1 = u32::from_le_bytes(take(off + 12, 4)?.try_into().expect("4B"));
    let method_len = take(off + 16, 1)?[0] as usize;
    let method = std::str::from_utf8(take(off + 17, method_len)?)
      .map_err(|_| io::Error::other("request spill: non-utf8 method"))?;
    off += 17 + method_len;
    let path_len = u16::from_le_bytes(take(off, 2)?.try_into().expect("2B")) as usize;
    let url = std::str::from_utf8(take(off + 2, path_len)?)
      .map_err(|_| io::Error::other("request spill: non-utf8 path"))?;
    off += 2 + path_len;
    rows.push(crate::requests::ReqRow {
      from,
      method: Box::from(method),
      path: Box::from(url),
      span: (s0, s1),
    });
    seen += 1;
  }
  if seen != expected {
    return Err(io::Error::other(format!(
      "request spill holds {seen} records, absorber wrote {expected} — torn scratch"
    )));
  }
  Ok(rows)
}

enum RefSink<'a, 'i> {
  Ram(&'a mut Vec<Reference<'i>>, &'a mut FlowSidecar),
  Spill(
    &'a mut vorpal_resolve::RefSpillWriter<'i>,
    Option<FlowSpillWriters<'a>>,
  ),
}

/// The five flow spill writers a bulk build streams side rows into.
pub(crate) struct FlowSpillWriters<'a> {
  args: &'a mut ArgSpillWriter,
  params: &'a mut ParamSpillWriter,
  rets: &'a mut RetSpillWriter,
  sigs: &'a mut SigSpillWriter,
  requests: &'a mut ReqSpillWriter,
}

impl<'i> RefSink<'_, 'i> {
  fn consume(
    &mut self,
    shard_references: Vec<Reference<'i>>,
    shard_flow: FlowSidecar,
    id_base: u64,
  ) -> io::Result<()> {
    let rebased = shard_references.into_iter().map(|mut reference| {
      reference.from = NodeId::new(reference.from.raw() + id_base);
      reference
    });
    let rebased_args = shard_flow.args.into_iter().map(|mut rec| {
      rec.from = NodeId::new(rec.from.raw() + id_base);
      rec
    });
    let rebased_params = shard_flow.params.into_iter().map(|mut rec| {
      rec.entity = NodeId::new(rec.entity.raw() + id_base);
      rec
    });
    let rebased_sigs = shard_flow.sigs.into_iter().map(|mut rec| {
      rec.entity = NodeId::new(rec.entity.raw() + id_base);
      rec
    });
    let rebased_requests = shard_flow.requests.into_iter().map(|mut rec| {
      rec.from = NodeId::new(rec.from.raw() + id_base);
      rec
    });
    match self {
      RefSink::Ram(references, flow) => {
        references.extend(rebased);
        flow.args.extend(rebased_args);
        flow.params.extend(rebased_params);
        flow.rets.extend(shard_flow.rets);
        flow.sigs.extend(rebased_sigs);
        flow.requests.extend(rebased_requests);
        Ok(())
      }
      RefSink::Spill(writer, flow_writers) => {
        for reference in rebased {
          writer.push(&reference)?;
        }
        if let Some(writers) = flow_writers {
          for rec in rebased_args {
            writers.args.push(&rec)?;
          }
          for rec in rebased_params {
            writers.params.push(&rec)?;
          }
          for rec in &shard_flow.rets {
            writers.rets.push(rec)?;
          }
          for rec in rebased_sigs {
            writers.sigs.push(&rec)?;
          }
          for rec in rebased_requests {
            writers.requests.push(&rec)?;
          }
        }
        Ok(())
      }
    }
  }
}

/// Merge one completed shard into the global writer, rebasing its buffered references by
/// the id base the absorb assigns — the single absorption step both the rolling path and
/// the leftover tail share, so their outputs are identical by construction.
fn absorb_shard<'i>(
  writer: &mut KgWriter,
  sink: &mut RefSink<'_, 'i>,
  shard_writer: KgWriter,
  shard_references: Vec<Reference<'i>>,
  shard_flow: FlowSidecar,
) -> io::Result<()> {
  let id_base = writer.absorb(shard_writer);
  sink.consume(shard_references, shard_flow, id_base)
}

/// Routes exactly like extraction: the parameter ledger is collected for files the registry
/// hands to the Python grammar (kwarg call-site binding is a Python-shaped semantic).
fn is_python_path(path: &str) -> bool {
  matches!(
    vorpal_lang_registry::from_path(std::path::Path::new(path)),
    Some(vorpal_lang_registry::SgLang::Builtin(vorpal_language::SupportLang::Python))
  )
}

/// Emit a phase stamp to stderr when `VORPAL_PHASE_TRACE` is set — for correlating RSS
/// timelines with pipeline phases during memory profiling.
pub fn phase_trace(label: &str) {
  vorpal_kg::phase_stamp(label);
}

/// Hand freed-but-retained allocator pages back to the OS at a phase boundary. On macOS the
/// default malloc keeps freed pages dirty in per-thread magazines, so a build's peak
/// footprint reads as (largest phase) + (every earlier phase's retained garbage) even when
/// the live set shrank between phases. One `malloc_zone_pressure_relief` sweep at each seam
/// makes the footprint track the live set instead. Elsewhere this is a no-op — the peak
/// figures we publish are honest live-set peaks, not allocator accidents.
pub fn release_freed_pages() {
  #[cfg(target_os = "macos")]
  {
    unsafe extern "C" {
      fn malloc_zone_pressure_relief(zone: *mut std::ffi::c_void, goal: usize) -> usize;
    }
    unsafe {
      malloc_zone_pressure_relief(std::ptr::null_mut(), 0);
    }
  }
}

pub fn link_writer<'i>(
  interner: &'i vorpal_resolve::Interner,
  mut writer: KgWriter,
  references: Vec<Reference<'i>>,
  resolver: &Resolver,
) -> (Kg, ResolveStats) {
  phase_trace("link: table build start");
  let mut table = build_symbol_table(interner, &writer);
  // The table build's transients (per-shard pair vectors, the finalize sort buffer) just
  // died — return their pages before resolution allocates the edge lists.
  release_freed_pages();
  phase_trace("link: resolve start");
  // Import-binding pre-pass (§3.3 scope step): resolve the qualifier-carrying imports first,
  // so bare uses in an importing file inherit its import-proven targets.
  let qualified: Vec<Reference<'i>> = references
    .iter()
    .filter(|r| {
      r.kind == vorpal_resolve::RefKind::Import && r.form == vorpal_resolve::RefForm::Static
    })
    .copied()
    .collect();
  // Root-relative imports (`<linux/export.h>`) resolve by suffix with corpus-learned
  // include roots — learn them before anything consults the path prober.
  phase_trace("link: include-roots learn");
  table.learn_include_roots(interner, &references);
  phase_trace("link: import-binding seed");
  vorpal_resolve::seed_import_bindings(interner, &mut table, &qualified, resolver);
  drop(qualified);
  // Include-reachability pre-pass (the candidate law's macro gate): file→file
  // edges from every resolved path-form import, closed transitively — macro
  // candidates then bind by inclusion, exactly like the preprocessor.
  phase_trace("link: include-reach build");
  let reach = vorpal_resolve::build_include_reach(interner, &table, &references);
  phase_trace("link: resolve refs");
  let (edges, stats) = resolve_all(interner, &table, &references, resolver, Some(&reach));
  phase_trace("link: resolve done");
  drop(table);
  drop(references);
  // The largest transient of the run (references + table) just died — return its pages
  // before compaction and seal allocate theirs.
  release_freed_pages();
  for edge in &edges {
    writer.add_edge(
      edge.from,
      edge.to,
      edge.edge.with_confidence(edge.confidence),
    );
  }
  drop(edges);
  phase_trace("link: seal start");
  let kg = writer.seal();
  release_freed_pages();
  phase_trace("link: seal done");
  (kg, stats)
}

/// [`link_writer`] over a spilled reference stream: the same table build, resolution, and
/// seal, with references streamed off disk in bounded chunks (identical output — chunking is
/// invisible to per-reference resolution) and the spill deleted once resolved. Also returns
/// the per-edge evidence rows (span, resolver reason, confidence, candidate count) resolution
/// produced — the caller persists them as the generation's `evidence.bin` sidecar (§5), so
/// every persisted relation can answer "why does this exist?".
pub fn link_writer_spilled<'i>(
  interner: &'i vorpal_resolve::Interner,
  writer: KgWriter,
  spill: vorpal_resolve::RefSpill<'i>,
  resolver: &Resolver,
) -> io::Result<(Kg, ResolveStats, Vec<vorpal_kg::EvidenceRow>)> {
  let (kg, stats, evidence, _flows, _similar, _requests) =
    link_writer_spilled_with_flows(interner, writer, spill, resolver, None)?;
  Ok((kg, stats, evidence))
}

/// [`link_writer_spilled`] plus the data-flow join (G-M3): the arg spill written beside the
/// reference spill joins resolved CALLS edges by (from, span) — every traceable argument at
/// a resolved call becomes a `dataflow.bin` row, and each (caller, callee) pair with at
/// least one traceable argument gains one `DATA_FLOWS` edge carrying the call's confidence.
/// Bind one call-site argument to a callee parameter position (G-M5). Keyword arguments
/// bind by exact name against the callee's parameter ledger — a miss (typo, **kwargs
/// absorption, no ledger) is the honest sentinel, never a guessed position. Positional
/// arguments keep their index, shifted past an explicit self/cls when the call had a
/// receiver and the ledger proves the parameter is there.
fn bind_param_index(rec: &ArgRec, callee_params: Option<&[Box<str>]>) -> u16 {
  const NO_PARAM: u16 = u16::MAX;
  match (rec.kw.as_deref(), callee_params) {
    (Some(kw), Some(params)) => params
      .iter()
      .position(|p| p.as_ref() == kw)
      .map(|at| at as u16)
      .unwrap_or(NO_PARAM),
    (Some(_), None) => NO_PARAM,
    (None, Some(params)) => {
      let offset = (rec.has_receiver
        && matches!(params.first().map(|p| p.as_ref()), Some("self") | Some("cls")))
        as u16;
      rec.index + offset
    }
    // No ledger (non-Python callee, or an external): positional v1 semantics.
    (None, None) => rec.index,
  }
}

/// What a spilled link yields: the sealed graph, resolution stats, evidence rows, data-flow
/// rows, and the near-clone pairing report.
pub type LinkedGraph = (
  Kg,
  ResolveStats,
  Vec<vorpal_kg::EvidenceRow>,
  Vec<vorpal_kg::DataflowRow>,
  crate::similar::SimilarReport,
  crate::requests::RequestReport,
);

pub fn link_writer_spilled_with_flows<'i>(
  interner: &'i vorpal_resolve::Interner,
  mut writer: KgWriter,
  spill: vorpal_resolve::RefSpill<'i>,
  resolver: &Resolver,
  flow_spill: Option<FlowSpill>,
) -> io::Result<LinkedGraph> {
  // Near-clone sketches (v16): paired on their own thread while the table builds and
  // resolution runs — they need no writer state, only the spill rows.
  let sig_rows: Vec<crate::similar::SigRow> = match &flow_spill {
    Some(spill) if spill.sigs.1 > 0 => load_sig_spill(&spill.sigs.0, spill.sigs.1)?,
    Some(spill) => {
      let _ = std::fs::remove_file(&spill.sigs.0);
      Vec::new()
    }
    None => Vec::new(),
  };
  let req_rows: Vec<crate::requests::ReqRow> = match &flow_spill {
    Some(spill) if spill.requests.1 > 0 => load_req_spill(&spill.requests.0, spill.requests.1)?,
    Some(spill) => {
      let _ = std::fs::remove_file(&spill.requests.0);
      Vec::new()
    }
    None => Vec::new(),
  };
  // The pairing needs nothing below — it starts now and overlaps the spill loads, the table
  // build, and resolution; its result is joined once resolution has finished.
  let (link, pairing) = std::thread::scope(|scope| {
    let pairing = scope.spawn(move || crate::similar::similar_pairs(sig_rows));
    let link = link_resolve(interner, &mut writer, &spill, resolver, flow_spill);
    (
      link,
      pairing
        .join()
        .map_err(|_| io::Error::other("similarity pairing panicked")),
    )
  });
  let (stats, evidence, flows) = link?;
  let (similar_pairs, similar_report) = pairing?;
  phase_trace("link: resolve done");
  // Symmetric near-clone edges, sorted pairs — deterministic edge-log order.
  for &(a, b, confidence) in &similar_pairs {
    let label = vorpal_kg::EdgeType::SIMILAR_TO.with_confidence(confidence);
    writer.add_edge(NodeId::new(a), NodeId::new(b), label);
    writer.add_edge(NodeId::new(b), NodeId::new(a), label);
  }
  drop(similar_pairs);
  // Request → route edges (ADOPTION #25 slice 2): literal client URLs against Route
  // templates; unique matches only, everything else counted on the report.
  let request_report = if req_rows.is_empty() {
    crate::requests::RequestReport::default()
  } else {
    let mut routes: Vec<(u64, String)> = Vec::new();
    writer.for_each_definition(|id, name, _path, kind, _exported| {
      if matches!(kind, vorpal_kg::SymbolKind::Route | vorpal_kg::SymbolKind::Channel) {
        routes.push((id.raw(), name.to_string()));
      }
    });
    let (matched, report) = crate::requests::match_requests(&routes, &req_rows);
    for &(from, to, confidence) in &matched.requests {
      writer.add_edge(
        NodeId::new(from),
        NodeId::new(to),
        vorpal_kg::EdgeType::REQUESTS.with_confidence(confidence),
      );
    }
    for &(from, to, confidence) in &matched.notifies {
      writer.add_edge(
        NodeId::new(from),
        NodeId::new(to),
        vorpal_kg::EdgeType::NOTIFIES.with_confidence(confidence),
      );
    }
    report
  };
  drop(req_rows);
  let _ = spill.remove();
  // The link transients (spill chunks + table + arg join) just died — return their pages
  // before compaction and seal allocate theirs.
  release_freed_pages();
  phase_trace("link: seal start");
  let kg = writer.seal();
  release_freed_pages();
  phase_trace("link: seal done");
  Ok((kg, stats, evidence, flows, similar_report, request_report))
}

/// The resolution half of a spilled link: load the flow spills, build the table, resolve
/// every reference into the writer's edge log, collect evidence and data-flow rows.
fn link_resolve<'i>(
  interner: &'i vorpal_resolve::Interner,
  writer: &mut KgWriter,
  spill: &vorpal_resolve::RefSpill<'i>,
  resolver: &Resolver,
  flow_spill: Option<FlowSpill>,
) -> io::Result<(ResolveStats, Vec<vorpal_kg::EvidenceRow>, Vec<vorpal_kg::DataflowRow>)> {
  let arg_join: ArgJoin = match &flow_spill {
    Some(spill) if spill.args.1 > 0 => load_arg_spill(&spill.args.0, spill.args.1)?,
    Some(spill) => {
      let _ = std::fs::remove_file(&spill.args.0);
      ArgJoin::empty()
    }
    None => ArgJoin::empty(),
  };
  let param_table: ParamTable = match &flow_spill {
    Some(spill) if spill.params.1 > 0 => load_param_spill(&spill.params.0, spill.params.1)?,
    Some(spill) => {
      let _ = std::fs::remove_file(&spill.params.0);
      ParamTable::empty()
    }
    None => ParamTable::empty(),
  };
  // The chained-call return ledger (G-M5): name-keyed, disagreements poisoned at build.
  let chain: Option<vorpal_resolve::ChainReturns<'i>> = match &flow_spill {
    Some(spill) if spill.rets.1 > 0 => {
      let rows = load_ret_spill(&spill.rets.0, spill.rets.1)?;
      Some(vorpal_resolve::ChainReturns::build(interner, rows))
    }
    Some(spill) => {
      let _ = std::fs::remove_file(&spill.rets.0);
      None
    }
    None => None,
  };
  let mut flows: Vec<vorpal_kg::DataflowRow> = Vec::new();
  let mut flow_pairs: std::collections::HashSet<(u64, u64)> = std::collections::HashSet::new();
  phase_trace("link: table build start");
  let mut table = build_symbol_table(interner, writer);
  release_freed_pages();
  phase_trace("link: resolve start");
  // Import-binding pre-pass (§3.3 scope step): the spill retained the import references
  // in RAM, so bare uses in an importing file inherit its import-proven targets. The
  // same retained slice feeds the include-reachability oracle (the candidate law's
  // macro gate) — path-form imports build the file→file graph, closed transitively.
  phase_trace("link: include-roots learn");
  table.learn_include_roots(interner, spill.imports());
  phase_trace("link: import-binding seed");
  vorpal_resolve::seed_import_bindings(interner, &mut table, spill.imports(), resolver);
  phase_trace("link: include-reach build");
  let reach = vorpal_resolve::build_include_reach(interner, &table, spill.imports());
  phase_trace("link: include-reach done");
  // Edges stream straight into the writer's edge log, in resolution order — the collected
  // edge vector was ~90 MB alive under the seal at kernel scale. Evidence rows are collected
  // alongside (24 bytes per emitted edge; they must all exist before the canonical sort that
  // makes the sidecar deterministic, so streaming them out is not an option).
  let mut evidence: Vec<vorpal_kg::EvidenceRow> = Vec::new();
  let stats = {
    let evidence = std::cell::RefCell::new(&mut evidence);
    vorpal_resolve::resolve_all_spilled_into(
      interner,
      &table,
      spill,
      resolver,
      chain.as_ref(),
      Some(&reach),
      |edge| {
        writer.add_edge(
          edge.from,
          edge.to,
          edge.edge.with_confidence(edge.confidence),
        );
        if edge.edge.base() == vorpal_kg::EdgeType::CALLS && !arg_join.is_empty() {
          let records = arg_join.get(edge.from.raw(), edge.span);
          if !records.is_empty() {
            // One DATA_FLOWS edge per (caller, callee) pair; one row per traceable arg.
            if flow_pairs.insert((edge.from.raw(), edge.to.raw())) {
              writer.add_edge(
                edge.from,
                edge.to,
                vorpal_kg::EdgeType::DATA_FLOWS.with_confidence(edge.confidence),
              );
            }
            let callee_params = if param_table.is_empty() {
              None
            } else {
              param_table.get(edge.to.raw())
            };
            for rec in records {
              flows.push(vorpal_kg::DataflowRow {
                from: edge.from.raw() as u32,
                to: edge.to.raw() as u32,
                span: edge.span,
                arg_index: rec.index,
                param_index: bind_param_index(rec, callee_params),
                class: rec.class,
                expr: rec.expr.as_deref().map(str::to_string),
              });
            }
          }
        }
        let (alt_ids, alt_count) = edge.alternatives;
        evidence.borrow_mut().push(vorpal_kg::EvidenceRow {
          from: edge.from.raw() as u32,
          to: edge.to.raw() as u32,
          name_hash: edge.name_hash,
          etype: edge.edge.base().0,
          reason: edge.reason as u8,
          confidence: edge.confidence,
          outcome: vorpal_kg::EvidenceOutcome::Edge,
          candidates: edge.candidates,
          span_start: edge.span.0,
          span_end: edge.span.1,
          alternatives: alt_ids[..alt_count as usize].to_vec(),
        });
      },
      |unresolved| {
        // No-edge outcomes are evidence too (07-29 §4): "why is there no edge here?" is
        // answerable from the sidecar instead of only aggregate counts.
        evidence.borrow_mut().push(vorpal_kg::EvidenceRow {
          from: unresolved.from.raw() as u32,
          to: vorpal_kg::NO_EDGE,
          name_hash: unresolved.name_hash,
          etype: unresolved.etype.base().0,
          reason: 0,
          confidence: 0,
          outcome: if unresolved.external {
            vorpal_kg::EvidenceOutcome::External
          } else {
            vorpal_kg::EvidenceOutcome::Masked
          },
          candidates: unresolved.candidates,
          span_start: unresolved.span.0,
          span_end: unresolved.span.1,
          alternatives: Vec::new(),
        });
      },
    )?
  };
  drop(table);
  drop(arg_join);
  drop(param_table);
  drop(chain);
  drop(flow_pairs);
  Ok((stats, evidence, flows))
}

/// Fewer files than this per shard and the fan-out overhead outweighs the win: small trees
/// take the single-writer path outright.
const MIN_FILES_PER_SHARD: usize = 16;

/// Upper bound on files per streaming shard (see `stream_apply_impl`'s sizing comment).
const SHARD_CAP_FILES: usize = 64;

/// §7.5 **sharded single-writer commit** over pre-extracted products: partition the
/// (path-sorted) product list into contiguous shards, apply each shard in its own private
/// [`KgWriter`] in parallel — one writer per shard, no locks — then absorb the shards in
/// order, rebasing node ids and buffered references. Contiguous slices + ordered absorption
/// reproduce the serial writer's id assignment exactly, so the sealed output is bit-identical
/// to a single-writer apply (pinned by test); cross-shard resolution still happens in the
/// global [`link_writer`] pass, which sees the merged table.
pub fn apply_products_sharded<'i>(
  interner: &'i vorpal_resolve::Interner,
  products: Vec<(String, crate::FileProduct)>,
) -> (KgWriter, Vec<Reference<'i>>) {
  use rayon::prelude::*;

  let threads = rayon::current_num_threads().max(1);
  // ~2 shards per thread for balance, floored so tiny trees stay serial.
  let shard_size = products
    .len()
    .div_ceil(threads * 2)
    .max(MIN_FILES_PER_SHARD);

  if products.len() <= shard_size {
    let mut writer = KgWriter::new();
    let mut references = Vec::new();
    for (path, product) in products {
      apply_product(interner, &path, product, &mut writer, &mut references);
    }
    return (writer, references);
  }

  let shards: Vec<(KgWriter, Vec<Reference<'i>>)> = products
    .into_par_iter()
    .chunks(shard_size)
    .map(|shard| {
      let mut writer = KgWriter::new();
      let mut references = Vec::new();
      for (path, product) in shard {
        apply_product(interner, &path, product, &mut writer, &mut references);
      }
      (writer, references)
    })
    .collect();

  let mut writer = KgWriter::new();
  let mut references = Vec::new();
  for (shard_writer, shard_references) in shards {
    let id_base = writer.absorb(shard_writer);
    references.extend(shard_references.into_iter().map(|mut reference| {
      reference.from = NodeId::new(reference.from.raw() + id_base);
      reference
    }));
  }
  (writer, references)
}

/// Apply one file's product to the writer: ingest its items, then push its references with
/// `from` resolved through the writer's canonical identity (entity path → fresh `NodeId`).
/// Consumes the product so name/qualifier strings move instead of cloning — this stage is the
/// serial single-writer section of the pipeline, so every allocation here is on the critical
/// path at any corpus size.
pub(crate) fn apply_product<'i>(
  interner: &'i vorpal_resolve::Interner,
  path: &str,
  product: crate::FileProduct,
  writer: &mut KgWriter,
  references: &mut Vec<Reference<'i>>,
) {
  apply_product_with_args(interner, path, product, writer, references, None);
}

pub(crate) fn apply_product_with_args<'i>(
  interner: &'i vorpal_resolve::Interner,
  path: &str,
  product: crate::FileProduct,
  writer: &mut KgWriter,
  references: &mut Vec<Reference<'i>>,
  mut flow_out: Option<&mut FlowSidecar>,
) {
  let crate::FileProduct {
    items,
    refs,
    entity_params,
    returns,
    signatures,
    requests,
    ..
  } = product;
  if let Some(flow_out) = flow_out.as_deref_mut() {
    for (name, ret) in &returns {
      flow_out.rets.push(RetRec {
        name: Box::from(name.as_str()),
        ret: Box::from(ret.as_str()),
      });
    }
  }
  let entity_params: Vec<(u32, Vec<&str>)> = entity_params
    .iter()
    .map(|(entity, params)| (*entity, params.iter().map(|(name, _)| name.as_str()).collect()))
    .collect();
  apply_parts(
    interner,
    path,
    &items,
    refs.iter().map(|r| {
      crate::product::RefView::bridge(
        r.from_entity_index,
        &r.name,
        r.kind,
        r.start,
        r.end,
        r.qualifier.as_deref(),
        r.form,
        r.alias.as_deref(),
        r.receiver.as_deref(),
        r.receiver_type.as_deref(),
        r.receiver_type_origin,
        &r.args,
      )
    }),
    &entity_params,
    signatures
      .iter()
      .map(|sig| (sig.entity_index, sig.shingles, &sig.sketch[..])),
    requests
      .iter()
      .map(|r| (r.from_entity_index, r.method.as_str(), r.path.as_str(), (r.start, r.end))),
    writer,
    references,
    flow_out,
  );
}

/// Apply a pack-replayed product straight from its mapped bytes: decode to views, apply —
/// no owned strings anywhere on the path (the replay profile showed decode's per-string
/// allocations as a top cost).
pub(crate) fn apply_product_view_with_args<'i>(
  interner: &'i vorpal_resolve::Interner,
  path: &str,
  view: &crate::ProductView<'_>,
  writer: &mut KgWriter,
  references: &mut Vec<Reference<'i>>,
  mut flow_out: Option<&mut FlowSidecar>,
) {
  if let Some(flow_out) = flow_out.as_deref_mut() {
    for (name, ret) in &view.returns {
      flow_out.rets.push(RetRec {
        name: Box::from(*name),
        ret: Box::from(*ret),
      });
    }
  }
  let entity_params: Vec<(u32, Vec<&str>)> = view
    .entity_params
    .iter()
    .map(|(entity, params)| (*entity, params.iter().map(|(name, _)| *name).collect()))
    .collect();
  apply_parts(
    interner,
    path,
    &view.items,
    view.refs.iter().copied(),
    &entity_params,
    view
      .signatures
      .iter()
      .map(|sig| (sig.entity_index, sig.shingles, sig.sketch)),
    view
      .requests
      .iter()
      .map(|r| (r.from_entity_index, r.method, r.path, (r.start, r.end))),
    writer,
    references,
    flow_out,
  );
}

/// The single application kernel both product forms share.
#[allow(clippy::too_many_arguments)] // the single shared application kernel: every input is load-bearing
fn apply_parts<'a, 'i>(
  interner: &'i vorpal_resolve::Interner,
  path: &str,
  items: &[vorpal_outline::model::OutlineItem<'_>],
  refs: impl Iterator<Item = crate::product::RefView<'a>>,
  entity_params: &[(u32, Vec<&str>)],
  signatures: impl Iterator<Item = (u32, u32, &'a [u8])>,
  requests: impl Iterator<Item = (u32, &'a str, &'a str, (u32, u32))>,
  writer: &mut KgWriter,
  references: &mut Vec<Reference<'i>>,
  mut flow_out: Option<&mut FlowSidecar>,
) {
  // Identity lookups below are scoped to this file's entities, and each path lands exactly
  // once (manifest invariant) — so the previous files' identity keys are dead weight.
  writer.forget_identity_scope();
  // The writer hands back each layout position's NodeId in walk order (spans[0] = the file
  // node, then items and members — the exact order product entity indices use), rendering
  // each identity path into one reused buffer as it walks. Attribution below is therefore
  // ARRAY INDEXING; the per-reference canonical lookup this replaces hashed (blake3) the
  // path+entity strings ~5.8M times per kernel link, and the per-file layout `Vec<String>`
  // it once consumed was ~9 % of stream-phase allocation samples. An out-of-range index —
  // a corrupt product — still simply drops the row.
  let spans = writer.ingest_file_with_spans(path, items);
  let id_at = |index: u32| spans.get(index as usize).map(|(_, id)| *id);
  // Intern the file's path once; every reference carries the 4-byte id.
  let path_id = interner.intern(path);
  // Callee parameter ledgers (G-M5): Python entities only — the one language whose call
  // sites bind by keyword and whose methods carry an explicit self/cls first parameter.
  if let Some(flow_out) = flow_out.as_deref_mut() {
    if !entity_params.is_empty() && is_python_path(path) {
      for (entity_index, names) in entity_params {
        if let Some(id) = id_at(*entity_index) {
          if !names.is_empty() {
            flow_out.params.push(ParamRec {
              entity: id,
              names: names.iter().map(|n| Box::from(*n)).collect(),
            });
          }
        }
      }
    }
  }
  // Near-clone sketches (v16): keyed to the writer's fresh node id, collected only where a
  // collector exists (the spilled index-build path).
  if let Some(flow_out) = flow_out.as_deref_mut() {
    for (entity_index, shingles, sketch) in signatures {
      let Ok(sketch) = <[u8; crate::signature::BINS]>::try_from(sketch) else {
        continue; // a corrupt width — the decoder guarantees 64, so this never fires
      };
      if let Some(id) = id_at(entity_index) {
        flow_out.sigs.push(SigRec {
          entity: id,
          shingles,
          sketch,
        });
      }
    }
  }
  // Request records (v17): keyed to the writer's fresh node id, collected only where a
  // collector exists (the spilled index-build path).
  if let Some(flow_out) = flow_out.as_deref_mut() {
    for (entity_index, method, url, span) in requests {
      if let Some(from) = id_at(entity_index) {
        flow_out.requests.push(ReqRec {
          from,
          method: Box::from(method),
          path: Box::from(url),
          span,
        });
      }
    }
  }
  for r in refs {
    if let Some(from) = id_at(r.from_entity_index) {
      // Traceable call-site arguments ride beside the reference (G-M3): lazily decoded off
      // the view, captured only where a collector exists (the spilled index-build path).
      if let Some(flow_out) = flow_out.as_deref_mut() {
        if crate::product::tag_refkind(r.kind) == vorpal_resolve::RefKind::Call
          && r.args_len() > 0
        {
          let has_receiver = r.receiver.is_some();
          for arg in r.args() {
            if arg.class <= 2 {
              flow_out.args.push(ArgRec {
                from,
                span: (r.start, r.end),
                index: arg.index,
                class: arg.class,
                has_receiver,
                expr: arg.expr.map(Box::from),
                kw: arg.kw_name.map(Box::from),
              });
            }
          }
        }
      }
      references.push(
        Reference::with_interned_path(
          interner,
          from,
          path_id,
          r.name,
          crate::product::tag_refkind(r.kind),
        )
        .with_evidence(r.start, r.end)
        .with_qualifier_ref(interner, r.qualifier)
        .with_alias_ref(interner, r.alias)
        .with_form(crate::product::tag_refform(r.form))
        .with_receiver_type_ref(interner, r.receiver_type, r.receiver_type_origin),
      );
    }
  }
}

/// Below this many definitions the table builds serially — fan-out costs more than it saves.
const MIN_DEFS_PER_SHARD: usize = 4096;

/// The owner id for members whose owner's name no reference ever interned: a reserved,
/// unparseable string (control character) that can never equal a real qualifier, preserving
/// "is a member" without admitting a match.
fn unmatchable_owner<'i>(interner: &'i vorpal_resolve::Interner) -> vorpal_resolve::NameId<'i> {
  interner.intern("\u{1}vorpal:unreferenced-owner")
}

fn build_symbol_table<'i>(
  interner: &'i vorpal_resolve::Interner,
  writer: &KgWriter,
) -> SymbolTable<'i> {
  // Derive each member's owner row from the containment edges (`Kg` for `Kg.load`) — the
  // target side of qualified-reference matching. Containment from a File node is not
  // ownership: top-level items match by module file instead. One cheap serial pass.
  let node_count = writer.node_count();
  let mut owner_of: Vec<Option<u32>> = vec![None; node_count];
  for (src, dst, etype) in writer.edge_log().iter() {
    let containment = etype.base() == EdgeType::DEFINES
      || etype.base() == EdgeType::HAS_METHOD
      || etype.base() == EdgeType::HAS_FIELD;
    if containment
      && writer
        .definition(src as usize)
        .map(|(_, _, _, kind, _)| kind)
        != Some(SymbolKind::File)
      && (dst as usize) < owner_of.len()
    {
      owner_of[dst as usize] = Some(src);
    }
  }

  // §7.5 sharded table build: contiguous row ranges each fill a private table on their own
  // thread, absorbed in row order — candidate lists end up in the exact order the serial
  // insertion produced (pinned by test). Small graphs build serially.
  let insert_range = |range: std::ops::Range<usize>| {
    let mut table = SymbolTable::new();
    table.reserve(range.len());
    for row in range {
      let (id, name, path, kind, exported) = writer.definition(row).expect("row < node_count");
      if kind == SymbolKind::File {
        // File nodes are the targets of path-form imports (`import "./util"`).
        table.insert_file(interner, path, id);
      } else if kind.is_resolution_candidate() {
        // The candidate law lives on SymbolKind (ONE definition for every table
        // feed): imports are wiring, macros are visibility-gated — see
        // `SymbolKind::is_resolution_candidate`.
        // Owners resolve by `peek`: an owner name no reference ever interned can never match
        // a qualifier, but member-ness must survive — the unmatchable sentinel keeps such
        // members out of the top-level (module-stem) matching path.
        let owner = owner_of[row]
          .and_then(|src| writer.definition(src as usize))
          .map(|(_, owner_name, _, _, _)| {
            interner
              .peek(owner_name)
              .unwrap_or_else(|| unmatchable_owner(interner))
          });
        table.insert_if_referenced(
          interner,
          name,
          Symbol {
            id,
            kind,
            path: interner.intern(path),
            exported,
            owner,
          },
        );
      }
    }
    table
  };

  if node_count <= MIN_DEFS_PER_SHARD {
    let mut table = insert_range(0..node_count);
    table.finalize();
    return table;
  }
  use rayon::prelude::*;
  vorpal_kg::phase_stamp("table: owner pass done");
  let threads = rayon::current_num_threads().max(1);
  let shard_size = node_count.div_ceil(threads * 2).max(MIN_DEFS_PER_SHARD);
  let starts: Vec<usize> = (0..node_count).step_by(shard_size).collect();
  let shards: Vec<SymbolTable> = starts
    .par_iter()
    .map(|&start| insert_range(start..(start + shard_size).min(node_count)))
    .collect();
  vorpal_kg::phase_stamp("table: shards built");
  let table = SymbolTable::from_shards(shards);
  vorpal_kg::phase_stamp("table: finalized");
  table
}

#[cfg(test)]
mod sharded_table_tests {
  use super::*;

  /// One shared session for this test module.
  fn itn() -> &'static vorpal_resolve::Interner {
    static INTERNER: std::sync::OnceLock<vorpal_resolve::Interner> = std::sync::OnceLock::new();
    INTERNER.get_or_init(vorpal_resolve::Interner::default)
  }
  use vorpal_kg::NodeDef;

  /// The serial specification: single-pass insertion via `for_each_definition`, exactly the
  /// pre-sharding algorithm. The sharded build must produce an equal table.
  fn serial_reference_table(writer: &KgWriter) -> SymbolTable<'static> {
    let mut names: Vec<String> = Vec::with_capacity(writer.node_count());
    let mut kinds: Vec<SymbolKind> = Vec::with_capacity(writer.node_count());
    writer.for_each_definition(|_, name, _, kind, _| {
      names.push(name.to_owned());
      kinds.push(kind);
    });
    let mut owner_of: Vec<Option<u32>> = vec![None; names.len()];
    for (src, dst, etype) in writer.edge_log().iter() {
      let containment = etype.base() == EdgeType::DEFINES
        || etype.base() == EdgeType::HAS_METHOD
        || etype.base() == EdgeType::HAS_FIELD;
      if containment
        && kinds.get(src as usize).copied() != Some(SymbolKind::File)
        && (dst as usize) < owner_of.len()
      {
        owner_of[dst as usize] = Some(src);
      }
    }
    let mut table = SymbolTable::new();
    writer.for_each_definition(|id, name, path, kind, exported| {
      if kind == SymbolKind::File {
        table.insert_file(itn(), path, id);
      } else if kind.is_resolution_candidate() {
        let owner = owner_of[id.raw() as usize].map(|src| {
          itn()
            .peek(&names[src as usize])
            .unwrap_or_else(|| unmatchable_owner(itn()))
        });
        table.insert_if_referenced(
          itn(),
          name,
          Symbol {
            id,
            kind,
            path: itn().intern(path),
            exported,
            owner,
          },
        );
      }
    });
    table.finalize();
    table
  }

  #[test]
  fn sharded_table_build_equals_the_serial_specification() {
    // Referenced-only inserts key off the interner: intern every name this corpus uses (as
    // reference construction would have during commit) so the oracle exercises real,
    // non-empty tables regardless of what other tests interned first.
    for j in 0..4usize {
      itn().intern(&format!("Item{j}"));
      itn().intern(&format!("member_{j}"));
    }
    // A writer big enough to force multiple shards (> MIN_DEFS_PER_SHARD definitions), with
    // files, items, members (owners), imports, duplicate names, and privates.
    let mut writer = KgWriter::new();
    for i in 0..800usize {
      let path = format!("src/file_{i:03}.rs");
      let file_id = writer.define(NodeDef {
        kind: SymbolKind::File,
        name: &path,
        entity_path: "",
        path: &path,
        signature: "",
        exported: true,
        content_hash: i as u64,
        span: (0, 0),
      });
      for j in 0..4usize {
        let item_name = format!("Item{j}");
        let item_id = writer.define(NodeDef {
          kind: if j % 2 == 0 {
            SymbolKind::Struct
          } else {
            SymbolKind::Function
          },
          name: &item_name,
          entity_path: &item_name,
          path: &path,
          signature: "sig",
          exported: j % 3 != 0,
          content_hash: (i * 10 + j) as u64,
          span: (0, 0),
        });
        writer.add_edge(file_id, item_id, EdgeType::DEFINES);
        let member_name = format!("member_{j}");
        let entity = format!("{item_name}.{member_name}");
        let member_id = writer.define(NodeDef {
          kind: SymbolKind::Method,
          name: &member_name,
          entity_path: &entity,
          path: &path,
          signature: "msig",
          exported: true,
          content_hash: (i * 100 + j) as u64,
          span: (0, 0),
        });
        writer.add_edge(item_id, member_id, EdgeType::HAS_METHOD);
      }
      let import_name = format!("imported_{i}");
      writer.define(NodeDef {
        kind: SymbolKind::Import,
        name: &import_name,
        entity_path: &import_name,
        path: &path,
        signature: "",
        exported: false,
        content_hash: i as u64 + 7,
        span: (0, 0),
      });
    }
    assert!(
      writer.node_count() > MIN_DEFS_PER_SHARD,
      "corpus must force the sharded path ({} defs)",
      writer.node_count()
    );

    assert_eq!(
      build_symbol_table(itn(), &writer),
      serial_reference_table(&writer),
      "sharded table diverged from the serial specification"
    );
  }
}

// --- §7.5 bounded streaming: byte-budget admission → MPMC stages → sharded commit ---------

/// In-flight byte budget (§7.5 byte-budget admission): discovery reserves a file's bytes
/// before it is read and the committer releases them once its product has been applied, so
/// peak transient memory is bounded by `capacity` regardless of corpus size. Reservation is a
/// CAS on a cache-padded atomic (the hot path); exhaustion parks on a condvar until a release
/// makes room. A single item larger than the whole budget reserves the full capacity instead
/// of deadlocking — progress over precision for the pathological case.
pub struct ByteBudget {
  capacity: u64,
  used: crossbeam_utils::CachePadded<std::sync::atomic::AtomicU64>,
  peak: crossbeam_utils::CachePadded<std::sync::atomic::AtomicU64>,
  gate: std::sync::Mutex<()>,
  room: std::sync::Condvar,
}

impl ByteBudget {
  pub fn new(capacity: u64) -> Self {
    Self {
      capacity: capacity.max(1),
      used: crossbeam_utils::CachePadded::new(std::sync::atomic::AtomicU64::new(0)),
      peak: crossbeam_utils::CachePadded::new(std::sync::atomic::AtomicU64::new(0)),
      gate: std::sync::Mutex::new(()),
      room: std::sync::Condvar::new(),
    }
  }

  /// Reserve `bytes` (clamped to capacity), blocking until they fit. Release with the SAME
  /// `bytes` value: both sides apply the identical clamp, so accounts always balance — even
  /// for a file larger than the whole budget.
  pub fn reserve(&self, bytes: u64) -> u64 {
    use std::sync::atomic::Ordering;
    let want = bytes.clamp(1, self.capacity);
    loop {
      let current = self.used.load(Ordering::Acquire);
      if current + want <= self.capacity {
        if self
          .used
          .compare_exchange(current, current + want, Ordering::AcqRel, Ordering::Acquire)
          .is_ok()
        {
          self.peak.fetch_max(current + want, Ordering::AcqRel);
          return want;
        }
        continue; // CAS race: retry immediately.
      }
      // No room: park until a release, then re-check.
      let guard = self.gate.lock().unwrap();
      if self.used.load(Ordering::Acquire) + want <= self.capacity {
        continue; // Released between the check and the lock.
      }
      #[cfg(feature = "alloc-ledger")]
      vorpal_kg::ledger::BUDGET_PARKS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
      let _guard = self.room.wait(guard).unwrap();
    }
  }

  pub fn release(&self, bytes: u64) {
    use std::sync::atomic::Ordering;
    let amount = bytes.clamp(1, self.capacity); // the mirror of reserve's clamp
    self.used.fetch_sub(amount, Ordering::AcqRel);
    let _guard = self.gate.lock().unwrap();
    self.room.notify_all();
  }

  /// High-water mark of concurrent reservations — the observable proof that admission
  /// actually bounded in-flight bytes (asserted by tests, useful as telemetry).
  pub fn peak(&self) -> u64 {
    self.peak.load(std::sync::atomic::Ordering::Acquire)
  }
}

/// Per-worker reusable buffers (§7.5 per-worker arenas, realized as scratch reuse): the two
/// dominant per-file allocations — the source read buffer and the product encode buffer —
/// amortize to zero across a worker's lifetime. Contents that must outlive a file (product
/// strings) are copied out exactly once, as everywhere else in the pipeline; the parse tree
/// itself lives in tree-sitter's allocator and is out of scratch's reach.
#[derive(Default)]
pub struct ExtractScratch {
  pub source: String,
  pub encode: Vec<u8>,
}

impl ExtractScratch {
  /// Retained-capacity bound per buffer. Reuse makes the common case allocation-free, but an
  /// unbounded buffer pins the *largest file the worker ever saw* for the rest of the run —
  /// across a pool of workers on a corpus with 10–20 MB generated headers, hundreds of MB of
  /// dead high-water. Oversized buffers are released after use; the next giant file simply
  /// reallocates (rare by construction — that's what makes the buffer *scratch*).
  const RETAIN_LIMIT: usize = 2 * 1024 * 1024;

  /// Read `path` into the reused source buffer (replacing its contents), UTF-8-validated
  /// exactly like `fs::read_to_string`.
  pub fn read_source(&mut self, path: &Path) -> io::Result<&str> {
    use std::io::Read;
    self.source.clear();
    if self.source.capacity() > Self::RETAIN_LIMIT {
      self.source.shrink_to(Self::RETAIN_LIMIT);
    }
    if self.encode.capacity() > Self::RETAIN_LIMIT {
      self.encode.clear();
      self.encode.shrink_to(Self::RETAIN_LIMIT);
    }
    std::fs::File::open(path)?.read_to_string(&mut self.source)?;
    Ok(&self.source)
  }
}

/// One entry's streaming outcome, produced by the caller's work closure.
pub enum StreamWork {
  /// Freshly parsed this run.
  Parsed(String, crate::FileProduct),
  /// Freshly parsed and already encoded to stamped `.vpb` bytes
  /// (`OutlineExtractor::extract_product_encoded`): the committer decodes views straight off
  /// the bytes and applies them — no owned product ever exists. Single-owner all the way:
  /// the buffer MOVES worker → committer → pack thread (the committer forwards it into the
  /// pack sink after applying; the pack's canonical path-sort makes arrival order
  /// irrelevant), so nothing is shared and nothing is copied.
  ParsedEncoded(String, Vec<u8>),
  /// Replayed from the incremental cache.
  Replayed(String, crate::FileProduct),
  /// Replayed from the products pack: only the path travels — the committer decodes views
  /// straight out of the mapped pack and applies without materializing a product. The
  /// producer must have validated the entry (stamps + a full view decode).
  ReplayedPacked(String),
  /// Not extractable (unreadable, unsupported) — skipped, exactly like the batch path.
  Skipped,
}

/// Counters and telemetry from a streaming run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamStats {
  pub parsed: u64,
  pub replayed: u64,
  /// High-water mark of in-flight reserved bytes (≤ the configured budget).
  pub peak_in_flight_bytes: u64,
}

/// What a shard committer receives for one admitted entry: its global sequence number and
/// either a product to apply or a skip marker (skips still advance the in-shard order).
enum Slot {
  Product {
    path: String,
    product: Box<crate::FileProduct>,
    parsed: bool,
    reserved: u64,
  },
  /// A fresh parse already encoded to `.vpb` bytes — applied as decoded views, like
  /// [`Slot::Packed`] but off the in-flight buffer instead of the mapped pack; the buffer
  /// then moves on into the pack sink.
  ProductBytes {
    path: String,
    bytes: Vec<u8>,
    reserved: u64,
  },
  Packed {
    path: String,
    reserved: u64,
  },
  Skipped,
}

/// §7.5 **bounded streaming ingest**: `discover → admit (byte budget) → extract (scoped
/// workers, per-worker scratch) → commit (single writer per shard, in-shard order)` joined by
/// bounded channels, so peak transient memory is O(budget + queue capacities), independent of
/// corpus size — a product exists in RAM only between extraction and application.
///
/// Ordering and therefore output are **identical to the batch path**: every admitted entry is
/// assigned its manifest-order sequence number up front; each shard's committer applies its
/// entries in sequence order (a reorder buffer absorbs out-of-order arrivals — bounded by the
/// byte budget, and the reason a straggler can never deadlock its shard); shard writers are
/// absorbed in shard order as they complete (rolling prefix absorption — the merged
/// writer grows during commit rather than doubling at the end). Pinned byte-for-byte by test, including under a
/// deliberately starved budget.
///
/// Workers are scoped threads borrowing `work` and the caller's state by `&` — no `'static`,
/// no `Arc` on the hot path (§7).
///
/// The first `Err` from `work` aborts the run and is returned; a partial graph is never
/// produced.
pub fn stream_apply<'i, F>(
  interner: &'i vorpal_resolve::Interner,
  entries: &[crate::FileStat],
  budget_bytes: u64,
  work: F,
) -> io::Result<(KgWriter, Vec<Reference<'i>>, StreamStats)>
where
  F: Fn(&crate::FileStat, &mut ExtractScratch) -> io::Result<StreamWork> + Sync,
{
  let mut references = Vec::new();
  // The in-RAM path has no data-flow consumer yet (flows are a spilled-build product; see
  // the G-M3 plan note) — flow rows are collected for order parity and dropped here, stated.
  let mut flow = FlowSidecar::default();
  let (writer, stats) = stream_apply_impl(
    interner,
    entries,
    budget_bytes,
    work,
    RefSink::Ram(&mut references, &mut flow),
    None,
    None,
    None,
  )?;
  Ok((writer, references, stats))
}

/// [`stream_apply`] with the reference stream spilled to `spill_path` instead of buffered in
/// RAM — the bulk-build configuration. Resolve the result with
/// [`vorpal_resolve::resolve_all_spilled`] (or [`link_writer_spilled`]), which streams the
/// file back in bounded chunks and deletes it.
pub fn stream_apply_spilled<'i, F>(
  interner: &'i vorpal_resolve::Interner,
  entries: &[crate::FileStat],
  budget_bytes: u64,
  spill_path: &std::path::Path,
  heap_stream_path: Option<&std::path::Path>,
  pack: Option<&crate::PackReader>,
  pack_out: Option<&crossbeam_channel::Sender<crate::PackMsg>>,
  work: F,
) -> io::Result<(
  KgWriter,
  vorpal_resolve::RefSpill<'i>,
  StreamStats,
  FlowSpill,
)>
where
  F: Fn(&crate::FileStat, &mut ExtractScratch) -> io::Result<StreamWork> + Sync,
{
  let mut spill_writer = vorpal_resolve::RefSpillWriter::create(interner, spill_path)?;
  // The flow spills ride beside the reference spill (G-M3/G-M5), same absorber, same order.
  let args_path = spill_path.with_extension("args");
  let mut arg_writer = ArgSpillWriter::create(&args_path)?;
  let params_path = spill_path.with_extension("params");
  let mut param_writer = ParamSpillWriter::create(&params_path)?;
  let rets_path = spill_path.with_extension("rets");
  let mut ret_writer = RetSpillWriter::create(&rets_path)?;
  let sigs_path = spill_path.with_extension("sigs");
  let mut sig_writer = SigSpillWriter::create(&sigs_path)?;
  let reqs_path = spill_path.with_extension("reqs");
  let mut req_writer = ReqSpillWriter::create(&reqs_path)?;
  let (writer, stats) = stream_apply_impl(
    interner,
    entries,
    budget_bytes,
    work,
    RefSink::Spill(
      &mut spill_writer,
      Some(FlowSpillWriters {
        args: &mut arg_writer,
        params: &mut param_writer,
        rets: &mut ret_writer,
        sigs: &mut sig_writer,
        requests: &mut req_writer,
      }),
    ),
    heap_stream_path,
    pack,
    pack_out,
  )?;
  let flow_spill = FlowSpill {
    args: arg_writer.finish()?,
    params: param_writer.finish()?,
    rets: ret_writer.finish()?,
    sigs: sig_writer.finish()?,
    requests: req_writer.finish()?,
  };
  Ok((writer, spill_writer.finish()?, stats, flow_spill))
}

/// The two flow scratch files a spilled build hands to link: (path, record count) each.
pub struct FlowSpill {
  pub(crate) args: (std::path::PathBuf, u64),
  pub(crate) params: (std::path::PathBuf, u64),
  pub(crate) rets: (std::path::PathBuf, u64),
  pub(crate) sigs: (std::path::PathBuf, u64),
  pub(crate) requests: (std::path::PathBuf, u64),
}

fn stream_apply_impl<'i, F>(
  interner: &'i vorpal_resolve::Interner,
  entries: &[crate::FileStat],
  budget_bytes: u64,
  work: F,
  mut sink: RefSink<'_, 'i>,
  heap_stream_path: Option<&std::path::Path>,
  pack: Option<&crate::PackReader>,
  pack_out: Option<&crossbeam_channel::Sender<crate::PackMsg>>,
) -> io::Result<(KgWriter, StreamStats)>
where
  F: Fn(&crate::FileStat, &mut ExtractScratch) -> io::Result<StreamWork> + Sync,
{
  let threads = std::env::var("VORPAL_INDEX_THREADS")
    .ok()
    .and_then(|v| v.parse().ok())
    .filter(|&n: &usize| n > 0)
    .unwrap_or_else(|| {
      std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
    });
  // Shards are deliberately small: sequential admission keeps the in-flight window narrow,
  // and with a handful of huge shards only one or two committers were ever active — the
  // replay profile showed every worker blocked on a full committer channel while one
  // committer applied a 2,267-file shard serially. Capping shards at 64 files spreads the
  // active window across every committer; output bytes are shard-size-independent (pinned
  // by the streamed≡batch identity tests). Env-tunable for experiments.
  let shard_size = entries.len().div_ceil((threads * 2).max(1)).clamp(
    MIN_FILES_PER_SHARD,
    std::env::var("VORPAL_SHARD_CAP")
      .ok()
      .and_then(|v| v.parse().ok())
      .unwrap_or(SHARD_CAP_FILES),
  );

  // Small trees: one thread, one writer, zero fan-out — the same output by definition.
  if entries.len() <= shard_size {
    let mut scratch = ExtractScratch::default();
    let mut writer = KgWriter::new();
    let mut references = Vec::new();
    let mut flow = FlowSidecar::default();
    let (mut parsed, mut replayed) = (0u64, 0u64);
    for entry in entries {
      match work(entry, &mut scratch)? {
        StreamWork::Parsed(path, product) => {
          parsed += 1;
          apply_product_with_args(interner, &path, product, &mut writer, &mut references, Some(&mut flow));
        }
        StreamWork::ParsedEncoded(path, bytes) => {
          {
            let view = crate::product::decode_product_view(&bytes).map_err(|e| {
              io::Error::other(format!("freshly encoded product failed to decode ({path}): {e}"))
            })?;
            apply_product_view_with_args(interner, &path, &view, &mut writer, &mut references, Some(&mut flow));
            parsed += 1;
          }
          if let Some(pack_out) = pack_out {
            pack_out
              .send(crate::PackMsg { path, body: bytes })
              .map_err(|_| io::Error::other("pack sink closed during streaming"))?;
          }
        }
        StreamWork::Replayed(path, product) => {
          replayed += 1;
          apply_product_with_args(interner, &path, product, &mut writer, &mut references, Some(&mut flow));
        }
        StreamWork::ReplayedPacked(path) => {
          if let Some(view) = pack
            .and_then(|p| p.get(&path))
            .and_then(|bytes| crate::product::decode_product_view(bytes).ok())
          {
            apply_product_view_with_args(interner, &path, &view, &mut writer, &mut references, Some(&mut flow));
            replayed += 1;
          }
        }
        StreamWork::Skipped => {}
      }
    }
    sink.consume(references, flow, 0)?;
    return Ok((
      writer,
      StreamStats {
        parsed,
        replayed,
        peak_in_flight_bytes: 0,
      },
    ));
  }

  let num_shards = entries.len().div_ceil(shard_size);
  // Half the workers commit: replay-heavy runs are apply-bound, and with threads/4 the
  // committers were the throughput ceiling once shards got small enough to keep them all fed.
  let committers = num_shards.min((threads / 2).max(1));
  let budget = ByteBudget::new(budget_bytes);
  let abort = std::sync::atomic::AtomicBool::new(false);
  let first_error: std::sync::Mutex<Option<io::Error>> = std::sync::Mutex::new(None);
  let fail = |err: io::Error| {
    abort.store(true, std::sync::atomic::Ordering::Release);
    first_error.lock().unwrap().get_or_insert(err);
  };

  // Admission → workers: a bounded MPMC of (sequence, entry); fixed capacity IS the
  // backpressure.
  // Queue DEPTH is decoupled from the byte budget: entries are 16-byte (sequence, &stat)
  // pairs whose source bytes are already reserved, so a deep queue costs kilobytes while
  // absorbing admission jitter — at threads*2 the queue drained in single-digit
  // milliseconds whenever admission paused, starving every parser.
  let (work_tx, work_rx) =
    crossbeam_channel::bounded::<(usize, &crate::FileStat)>((threads * 64).clamp(64, 4096));
  // Workers → committers: one bounded channel per committer thread; shard k routes to
  // committer k % committers.
  let (slot_txs, slot_rxs): (Vec<_>, Vec<_>) = (0..committers)
    .map(|_| crossbeam_channel::bounded::<(usize, Slot)>(64))
    .unzip();

  let total_sequences = entries.len();
  let mut writer = KgWriter::new();
  if let Some(path) = heap_stream_path {
    // The merged heap writes through to disk as shards absorb (~140 MB at kernel scale that
    // never touches anonymous memory); the link pass reads it back through a zero-copy map.
    writer.stream_heap_to(path)?;
  }
  // Committers → absorber: completed shards, for rolling prefix absorption. Unbounded so a
  // committer never blocks handing off a finished shard.
  let (done_tx, done_rx) = crossbeam_channel::unbounded::<(usize, KgWriter, Vec<Reference>, FlowSidecar)>();

  let outputs = std::thread::scope(|scope| {
    // Committers: each owns its assigned shards' writers outright (single writer per shard)
    // and drains its channel unconditionally into per-shard reorder buffers — receiving never
    // blocks on applying, which is what keeps a full shard channel impossible and the
    // backpressure cycle broken.
    let committer_handles: Vec<_> = slot_rxs
      .into_iter()
      .enumerate()
      .map(|(committer_index, slot_rx)| {
        let budget = &budget;
        let fail = &fail;
        let done_tx = done_tx.clone();
        scope.spawn(move || {
          let owned_shards: Vec<usize> =
            (committer_index..num_shards).step_by(committers).collect();
          let mut writers: HashMap<usize, (KgWriter, Vec<Reference>, FlowSidecar)> = owned_shards
            .iter()
            .map(|&shard| (shard, (KgWriter::new(), Vec::new(), FlowSidecar::default())))
            .collect();
          let mut pending: HashMap<usize, std::collections::BTreeMap<usize, Slot>> = owned_shards
            .iter()
            .map(|&shard| (shard, Default::default()))
            .collect();
          let mut next_expected: HashMap<usize, usize> = owned_shards
            .iter()
            .map(|&shard| (shard, shard * shard_size))
            .collect();
          let (mut parsed, mut replayed) = (0u64, 0u64);
          while let Ok((sequence, slot)) = slot_rx.recv() {
            let shard = sequence / shard_size;
            pending
              .get_mut(&shard)
              .expect("routed shard")
              .insert(sequence, slot);
            let expected = next_expected.get_mut(&shard).expect("routed shard");
            let queue = pending.get_mut(&shard).expect("routed shard");
            while let Some(slot) = queue.remove(expected) {
              *expected += 1;
              match slot {
                Slot::Product {
                  path,
                  product,
                  parsed: was_parsed,
                  reserved,
                } => {
                  let (writer, references, flow) = writers.get_mut(&shard).expect("owned shard");
                  apply_product_with_args(interner, &path, *product, writer, references, Some(flow));
                  budget.release(reserved);
                  if was_parsed {
                    parsed += 1;
                  } else {
                    replayed += 1;
                  }
                }
                Slot::ProductBytes { path, bytes, reserved } => {
                  // Bytes a worker encoded moments ago: decode views, apply, then MOVE the
                  // buffer on into the pack sink — single owner end to end. A decode failure
                  // is an internal bug, surfaced through the run's error path (never a
                  // silent drop, never a panic), and its bytes are never banked.
                  let decoded = match crate::product::decode_product_view(&bytes) {
                    Ok(view) => {
                      let (writer, references, flow) = writers.get_mut(&shard).expect("owned shard");
                      apply_product_view_with_args(interner, &path, &view, writer, references, Some(flow));
                      parsed += 1;
                      true
                    }
                    Err(e) => {
                      fail(io::Error::other(format!(
                        "freshly encoded product failed to decode ({path}): {e}"
                      )));
                      false
                    }
                  };
                  if decoded
                    && let Some(pack_out) = pack_out
                    && pack_out.send(crate::PackMsg { path, body: bytes }).is_err()
                  {
                    fail(io::Error::other("pack sink closed during streaming"));
                  }
                  budget.release(reserved);
                }
                Slot::Packed { path, reserved } => {
                  // Decode views straight out of the mapped pack and apply — validated by
                  // the producer, so a failure here is disk rot; the file is then absent
                  // from this build rather than fatal.
                  if let Some(view) = pack
                    .and_then(|p| p.get(&path))
                    .and_then(|bytes| crate::product::decode_product_view(bytes).ok())
                  {
                    let (writer, references, flow) = writers.get_mut(&shard).expect("owned shard");
                    apply_product_view_with_args(interner, &path, &view, writer, references, Some(flow));
                    replayed += 1;
                  }
                  budget.release(reserved);
                }
                Slot::Skipped => {}
              }
            }
            // Shard complete? Hand it off for rolling prefix absorption — the merged
            // writer grows while commit continues, instead of every shard coexisting
            // with its merged copy in one final doubling spike.
            let shard_end = ((shard + 1) * shard_size).min(total_sequences);
            if *expected == shard_end
              && let Some((shard_writer, shard_references, shard_flow)) = writers.remove(&shard)
            {
              let _ = done_tx.send((shard, shard_writer, shard_references, shard_flow));
            }
          }
          (writers, parsed, replayed)
        })
      })
      .collect();

    // Extraction workers: scoped, borrowing `work` by reference; per-worker scratch reused
    // across every file the worker touches.
    let worker_handles: Vec<_> = (0..threads)
      .map(|_| {
        let work_rx = work_rx.clone();
        // Each worker owns clones of the committer senders; when the last worker exits, the
        // channels close and committers drain to completion.
        let slot_txs: Vec<crossbeam_channel::Sender<(usize, Slot)>> = slot_txs.clone();
        let work = &work;
        let budget = &budget;
        let abort = &abort;
        let fail = &fail;
        scope.spawn(move || {
          let mut scratch = ExtractScratch::default();
          while let Ok((sequence, entry)) = work_rx.recv() {
            if abort.load(std::sync::atomic::Ordering::Acquire) {
              budget.release(entry.size);
              continue;
            }
            let reserved = entry.size; // released with the same value; clamps match
            let slot = match work(entry, &mut scratch) {
              Ok(StreamWork::Parsed(path, product)) => Slot::Product {
                path,
                product: Box::new(product),
                parsed: true,
                reserved,
              },
              Ok(StreamWork::ParsedEncoded(path, bytes)) => Slot::ProductBytes {
                path,
                bytes,
                reserved,
              },
              Ok(StreamWork::Replayed(path, product)) => Slot::Product {
                path,
                product: Box::new(product),
                parsed: false,
                reserved,
              },
              Ok(StreamWork::ReplayedPacked(path)) => Slot::Packed { path, reserved },
              Ok(StreamWork::Skipped) => {
                budget.release(reserved);
                Slot::Skipped
              }
              Err(err) => {
                budget.release(reserved);
                fail(err);
                continue;
              }
            };
            let shard = sequence / shard_size;
            let slot_tx = &slot_txs[shard % committers];
            #[cfg(feature = "alloc-ledger")]
            let sent = match slot_tx.try_send((sequence, slot)) {
              Ok(()) => Ok(()),
              Err(crossbeam_channel::TrySendError::Full(item)) => {
                // A full committer channel blocks this worker — the exact
                // chokepoint the parallelism audit counts.
                vorpal_kg::ledger::CHAN_FULL.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                slot_tx.send(item)
              }
              Err(crossbeam_channel::TrySendError::Disconnected(item)) => {
                Err(crossbeam_channel::SendError(item))
              }
            };
            #[cfg(not(feature = "alloc-ledger"))]
            let sent = slot_tx.send((sequence, slot));
            if sent.is_err() {
              break; // committer gone: only happens on abort/teardown
            }
          }
        })
      })
      .collect();
    drop(work_rx);
    drop(slot_txs);

    // Absorber: rolling prefix absorption on its OWN thread. Absorb is memcpy plus spill
    // IO; interleaved with admission it paused the feed for milliseconds at a time, and the
    // work queue drained under every pause — measured as the distributed stream-phase idle.
    // Shard k still merges exactly after shards 0..k with the same rebases, so output stays
    // bit-identical (pinned by test).
    let absorber = scope.spawn(move || {
      let mut writer = writer;
      let mut sink = sink;
      let mut holdback: std::collections::BTreeMap<
        usize,
        (KgWriter, Vec<Reference<'i>>, FlowSidecar),
      > = std::collections::BTreeMap::new();
      let mut next_absorb = 0usize;
      // First sink (spill IO) error: aborts absorption, surfaced after the scope joins.
      let mut sink_error: Option<io::Error> = None;
      while let Ok((shard, shard_writer, shard_references, shard_flow)) = done_rx.recv() {
        holdback.insert(shard, (shard_writer, shard_references, shard_flow));
        while let Some((shard_writer, shard_references, shard_flow)) = holdback.remove(&next_absorb)
        {
          if let Err(err) =
            absorb_shard(&mut writer, &mut sink, shard_writer, shard_references, shard_flow)
            && sink_error.is_none()
          {
            sink_error = Some(err);
          }
          next_absorb += 1;
        }
      }
      (writer, sink, holdback, next_absorb, sink_error)
    });

    // Admission, on the calling thread: manifest order, budget-gated — and nothing else.
    // The scaling probe rides admission: the byte budget ties admission rate to completion
    // rate, so a growing per-unit cost shows up here as a superlinear exponent (D7). The
    // unit is BYTES, not files — parse work scales with bytes, and real trees order their
    // paths with heavy byte skew (the kernel's midsection carries most of its bytes), which
    // a per-file fit would misread as a fake quadratic.
    let mut scaling = vorpal_kg::ScalingProbe::new("stream");
    let mut bytes_admitted: u64 = 0;
    for (sequence, entry) in entries.iter().enumerate() {
      if abort.load(std::sync::atomic::Ordering::Acquire) {
        break;
      }
      budget.reserve(entry.size.max(1));
      #[cfg(feature = "alloc-ledger")]
      let admitted = match work_tx.try_send((sequence, entry)) {
        Ok(()) => Ok(()),
        Err(crossbeam_channel::TrySendError::Full(item)) => {
          vorpal_kg::ledger::CHAN_FULL.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
          work_tx.send(item)
        }
        Err(crossbeam_channel::TrySendError::Disconnected(item)) => {
          Err(crossbeam_channel::SendError(item))
        }
      };
      #[cfg(not(feature = "alloc-ledger"))]
      let admitted = work_tx.send((sequence, entry));
      if admitted.is_err() {
        break;
      }
      bytes_admitted += entry.size.max(1);
      scaling.tick(bytes_admitted);
    }
    scaling.finish(bytes_admitted);
    drop(work_tx);
    // Drop the caller's sender so the absorber's drain ends when the committers exit.
    drop(done_tx);
    phase_trace("stream: admission done, draining completions");

    for handle in worker_handles {
      let _ = handle.join();
    }
    // Workers dropped their slot senders on exit; committers drain and finish.
    let committer_outputs: Vec<_> = committer_handles
      .into_iter()
      .map(|handle| handle.join().expect("committer panicked"))
      .collect();
    let absorbed = absorber.join().expect("absorber panicked");
    (committer_outputs, absorbed)
  });
  let (committer_outputs, (mut writer, mut sink, mut holdback, mut next_absorb, mut sink_error)) =
    outputs;

  if let Some(err) = first_error.into_inner().unwrap() {
    return Err(err);
  }

  let (mut parsed, mut replayed) = (0u64, 0u64);
  for (leftover, shard_parsed, shard_replayed) in committer_outputs {
    parsed += shard_parsed;
    replayed += shard_replayed;
    // Leftovers exist only when admission aborted mid-run; fold them in anyway so the
    // (discarded) result is still built deterministically.
    for (shard, writer_and_refs) in leftover {
      holdback.insert(shard, writer_and_refs);
    }
  }
  phase_trace("stream: absorb tail");
  while let Some((shard_writer, shard_references, shard_flow)) = holdback.remove(&next_absorb) {
    if let Err(err) =
      absorb_shard(&mut writer, &mut sink, shard_writer, shard_references, shard_flow)
      && sink_error.is_none()
    {
      sink_error = Some(err);
    }
    next_absorb += 1;
  }
  if let Some(err) = sink_error {
    return Err(err);
  }
  // The writer has absorbed its last shard: return growth slack before link stacks the
  // table and edge transients on top, and reopen a streamed heap for the link pass's reads.
  phase_trace("stream: consolidate (shrink + heap finalize)");
  writer.shrink_to_fit();
  writer.finalize_streamed_heap()?;
  Ok((
    writer,
    StreamStats {
      parsed,
      replayed,
      peak_in_flight_bytes: budget.peak(),
    },
  ))
}
