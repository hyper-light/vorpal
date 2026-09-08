//! Chunk-scoped parsing: parse only the top-level statements that can hold a match.
//!
//! A pattern match is one syntax node, so it lies inside exactly one direct child of the
//! parse root (the only node spanning two is the root itself, which no literal-bearing
//! pattern matches). The product records the start byte of every direct root child (v22
//! `cuts`); the prefilter's anchor literal names the byte offsets a match must contain.
//! Chunk `i` is the text from cut `i` to cut `i + 1` (the trailing gap — whitespace and
//! comments, which are not cuts — rides with it, so the scanner sees the newlines it
//! needs and a comment is parsed with the chunk it follows), and the chunks holding an anchor occurrence are
//! handed to tree-sitter **one at a time**, each as a single included range. A chunk parsed
//! alone reproduces its child exactly — the grammar is context-free and the chunk is a
//! complete node followed by end of input — so the match set equals the whole-file parse's,
//! pinned by the oracle in `tests/chunks.rs` and the kernel-scale parity check in
//! `evals/tgrep_bench.py`. One range per parse, never several: tree-sitter reads a set of
//! included ranges as one contiguous text, and a JavaScript statement without a semicolon
//! followed by a skipped chunk and then `(function(){…})()` would read as a call.
//!
//! The plan is only used for bytes whose digest matches the product (`Verified` reads):
//! cuts describe the indexed bytes, never a file that has since changed. And only when no
//! anchor chunk carries tree-sitter's `has_error` flag (recorded per cut): error recovery is
//! context-dependent, so a recovered child can parse differently alone than in its file —
//! the kernel-scale parity run found exactly that (`refcounted_kptr.c`, a
//! `__failure __msg(...)` line before a function) — and such a file parses whole.
//! `VORPAL_NO_CHUNK_PARSE=1` vetoes the plan for A/B runs.

use std::sync::OnceLock;

use vorpal_core::tree_sitter::{TSPoint, TSRange};
use vorpal_ingest::Cuts;

/// `VORPAL_NO_CHUNK_PARSE=1`: every candidate file parses whole.
pub fn chunk_parse_disabled() -> bool {
  static FLAG: OnceLock<bool> = OnceLock::new();
  *FLAG.get_or_init(|| std::env::var_os("VORPAL_NO_CHUNK_PARSE").is_some_and(|v| v == "1"))
}

/// Reusable scratch for [`chunks_with_anchors`]: the cut table, anchor offsets, and the
/// chunks of the file in hand.
#[derive(Default)]
pub struct ChunkScratch {
  pub cuts: Vec<u32>,
  /// Per cut: tree-sitter's `has_error` for that child.
  pub errors: Vec<bool>,
  pub positions: Vec<usize>,
  /// `(start, end)` of every chunk holding an anchor, ascending, from [`chunks_with_anchors`].
  pub chunks: Vec<(usize, usize)>,
  /// Consumers' per-file lists: chunks to parse, their memo keys, one chunk's relative
  /// starts, the file's match starts — kept here so a per-thread scratch reuses them all.
  pub to_parse: Vec<(usize, usize)>,
  pub chunk_keys: Vec<u64>,
  pub rel: Vec<u32>,
  pub starts: Vec<u32>,
  /// The file's definition spans `(start, end, node id)` for attribution.
  pub spans: Vec<(u32, u32, u64)>,
}

impl ChunkScratch {
  /// Clear every list, keeping capacity.
  pub fn reset(&mut self) {
    self.cuts.clear();
    self.errors.clear();
    self.positions.clear();
    self.chunks.clear();
    self.to_parse.clear();
    self.chunk_keys.clear();
    self.rel.clear();
    self.starts.clear();
    self.spans.clear();
  }
}

std::thread_local! {
  static SCRATCH_SLOT: std::cell::RefCell<Option<Box<ChunkScratch>>> = const { std::cell::RefCell::new(None) };
}

/// This thread's chunk scratch, cleared. Rayon's workers persist, so after the first file
/// no scan allocates a scratch list again. Give it back with [`give_scratch`].
pub fn take_scratch() -> Box<ChunkScratch> {
  let mut scratch = SCRATCH_SLOT
    .with(|slot| slot.borrow_mut().take())
    .unwrap_or_default();
  scratch.reset();
  scratch
}

pub fn give_scratch(scratch: Box<ChunkScratch>) {
  SCRATCH_SLOT.with(|slot| {
    let mut slot = slot.borrow_mut();
    if slot.is_none() {
      *slot = Some(scratch);
    }
  });
}

/// The chunks of `bytes` that hold one of `scratch.positions` (ascending anchor offsets),
/// into `scratch.chunks`. `false` when the file cannot be chunked: no cut table, cuts that
/// do not describe these bytes, a single chunk, or no anchor.
pub fn chunks_with_anchors(bytes: &[u8], cuts: Cuts<'_>, scratch: &mut ChunkScratch) -> bool {
  scratch.chunks.clear();
  if chunk_parse_disabled() || scratch.positions.is_empty() {
    return false;
  }
  scratch.cuts.clear();
  scratch.errors.clear();
  for (offset, has_error) in cuts {
    scratch.cuts.push(offset);
    scratch.errors.push(has_error);
  }
  let cuts = &scratch.cuts;
  if cuts.len() < 2 {
    return false; // one child (or none): the chunk is the file
  }
  let len = bytes.len();
  if cuts.iter().any(|&c| c as usize > len) {
    return false; // cuts do not describe these bytes
  }
  // Chunk of an offset: the last cut at or before it (offsets before the first cut belong
  // to a head chunk starting at 0).
  let chunk_of = |pos: usize| -> usize {
    match cuts.binary_search(&(pos as u32)) {
      Ok(i) => i + 1,
      Err(i) => i, // i == 0 → head chunk
    }
  };
  let chunk_start = |i: usize| -> usize {
    if i == 0 { 0 } else { cuts[i - 1] as usize }
  };
  let chunk_end = |i: usize| -> usize {
    if i < cuts.len() { cuts[i] as usize } else { len }
  };
  let mut last_chunk: Option<usize> = None;
  for &pos in &scratch.positions {
    let chunk = chunk_of(pos);
    if last_chunk == Some(chunk) {
      continue;
    }
    last_chunk = Some(chunk);
    // Chunk `i > 0` is child `i - 1`; a recovered child parses with its file.
    if chunk > 0 && scratch.errors[chunk - 1] {
      scratch.chunks.clear();
      return false;
    }
    let (start, end) = (chunk_start(chunk), chunk_end(chunk));
    if start < end {
      scratch.chunks.push((start, end));
    }
  }
  !scratch.chunks.is_empty()
}

/// The single included range for one chunk, with exact points (tree-sitter positions every
/// node from them). `row`/`line_start`/`cursor` carry the newline count forward across
/// ascending chunks so each file is scanned for newlines once.
pub struct PointCursor {
  row: usize,
  line_start: usize,
  cursor: usize,
}

impl Default for PointCursor {
  fn default() -> Self {
    PointCursor {
      row: 0,
      line_start: 0,
      cursor: 0,
    }
  }
}

impl PointCursor {
  fn advance(&mut self, bytes: &[u8], to: usize) -> TSPoint {
    for nl in memchr::memchr_iter(b'\n', &bytes[self.cursor..to]) {
      self.row += 1;
      self.line_start = self.cursor + nl + 1;
    }
    self.cursor = to;
    TSPoint {
      row: self.row,
      column: to - self.line_start,
    }
  }
  /// The range of `[start, end)`; chunks must be handed over ascending.
  pub fn range(&mut self, bytes: &[u8], start: usize, end: usize) -> TSRange {
    let start_point = self.advance(bytes, start);
    let end_point = self.advance(bytes, end);
    TSRange {
      start_byte: start,
      end_byte: end,
      start_point,
      end_point,
    }
  }
}

// ---------------------------------------------------------------------------------------
// The chunk memo: verified match sets keyed by the chunk's bytes and the pattern.
//
// Identical bytes under the same grammar and pattern parse to the same tree and match the
// same nodes, so a chunk's match set is a function of (chunk bytes, pattern). The daemon
// keeps the sets it has computed: a repeated pattern re-verifies only the chunks whose
// bytes moved and replays the rest without reading a parse tree — query cost proportional
// to the edit, the compose lanes' law applied to search. Match offsets are stored relative
// to the chunk start so a chunk that moved (an edit above it) still hits.
//
// Bounded by bytes (`VORPAL_CHUNK_MEMO_BYTES`, default 64 MiB — a knob, recorded beside the
// measurement in BENCHMARKS): when full, the oldest half of every shard is dropped.
// Sharded by chunk hash so eighteen scanning threads do not queue on one lock.

const MEMO_SHARDS: usize = 64;

struct MemoShard {
  map: std::collections::HashMap<(u64, u64), (u64, Box<[u32]>)>,
  bytes: usize,
  tick: u64,
}

struct ChunkMemo {
  shards: Vec<std::sync::Mutex<MemoShard>>,
  per_shard_budget: usize,
}

static MEMO: OnceLock<ChunkMemo> = OnceLock::new();

fn memo() -> &'static ChunkMemo {
  MEMO.get_or_init(|| {
    let budget = std::env::var("VORPAL_CHUNK_MEMO_BYTES")
      .ok()
      .and_then(|v| v.parse::<usize>().ok())
      .unwrap_or(64 << 20);
    ChunkMemo {
      shards: (0..MEMO_SHARDS)
        .map(|_| {
          std::sync::Mutex::new(MemoShard {
            map: std::collections::HashMap::new(),
            bytes: 0,
            tick: 0,
          })
        })
        .collect(),
      per_shard_budget: (budget / MEMO_SHARDS).max(4096),
    }
  })
}

/// The pattern half of a memo key: language, pattern source, selector and context.
pub fn pattern_key(lang: &str, pattern: &str, selector: Option<&str>, context: Option<&str>) -> u64 {
  let mut h = xxhash_rust::xxh3::Xxh3::new();
  h.update(lang.as_bytes());
  h.update(&[0]);
  h.update(pattern.as_bytes());
  h.update(&[0]);
  h.update(selector.unwrap_or("").as_bytes());
  h.update(&[0]);
  h.update(context.unwrap_or("").as_bytes());
  h.digest()
}

/// The chunk half of a memo key.
#[inline]
pub fn chunk_key(chunk: &[u8]) -> u64 {
  xxhash_rust::xxh3::xxh3_64(chunk)
}

fn entry_bytes(starts: usize) -> usize {
  40 + starts * 4
}

/// The memoized match starts (relative to the chunk start) for `(pattern, chunk)`.
pub fn memo_get(pattern: u64, chunk: u64, out: &mut Vec<u32>) -> bool {
  let shard = &memo().shards[(chunk as usize) % MEMO_SHARDS];
  let mut guard = shard.lock().unwrap_or_else(|p| p.into_inner());
  guard.tick += 1;
  let tick = guard.tick;
  match guard.map.get_mut(&(pattern, chunk)) {
    Some(entry) => {
      entry.0 = tick;
      out.extend_from_slice(&entry.1);
      true
    }
    None => false,
  }
}

/// Record the verified match starts (relative to the chunk start) for `(pattern, chunk)`.
pub fn memo_put(pattern: u64, chunk: u64, starts: &[u32]) {
  let m = memo();
  let shard = &m.shards[(chunk as usize) % MEMO_SHARDS];
  let mut guard = shard.lock().unwrap_or_else(|p| p.into_inner());
  let cost = entry_bytes(starts.len());
  if guard.bytes + cost > m.per_shard_budget && !guard.map.is_empty() {
    // Drop the least recently used half of this shard.
    let mut ticks: Vec<u64> = guard.map.values().map(|(t, _)| *t).collect();
    ticks.sort_unstable();
    let cutoff = ticks[ticks.len() / 2];
    let mut freed = 0usize;
    guard.map.retain(|_, (t, starts)| {
      let keep = *t > cutoff;
      if !keep {
        freed += entry_bytes(starts.len());
      }
      keep
    });
    guard.bytes = guard.bytes.saturating_sub(freed);
  }
  guard.tick += 1;
  let tick = guard.tick;
  if guard.map.insert((pattern, chunk), (tick, starts.into())).is_none() {
    guard.bytes += cost;
  }
}

/// Bytes the memo currently holds (for `health`).
pub fn memo_bytes() -> usize {
  memo().shards.iter().map(|s| s.lock().unwrap_or_else(|p| p.into_inner()).bytes).sum()
}

#[cfg(test)]
mod tests {
  use super::*;

  fn cuts_of(offsets: &[u32]) -> Vec<u8> {
    cuts_flagged(&offsets.iter().map(|&o| (o, false)).collect::<Vec<_>>())
  }

  fn cuts_flagged(cuts: &[(u32, bool)]) -> Vec<u8> {
    let mut raw = Vec::new();
    let mut prev = 0u32;
    for &(o, err) in cuts {
      let mut d = ((o - prev) << 1) | u32::from(err);
      prev = o;
      loop {
        let b = (d & 0x7f) as u8;
        d >>= 7;
        if d == 0 {
          raw.push(b);
          break;
        }
        raw.push(b | 0x80);
      }
    }
    raw
  }

  #[test]
  fn chunks_follow_anchors_flags_and_points() {
    // three top-level statements at 0, 10, 20 (each "fn x() {}\n" is 10 bytes)
    let src = b"fn a() {}\nfn b() {}\nfn c() {}\n";
    let raw = cuts_of(&[0, 10, 20]);
    let mut scratch = ChunkScratch::default();
    scratch.positions = vec![23]; // inside chunk 2
    assert!(chunks_with_anchors(src, Cuts::new(&raw), &mut scratch));
    assert_eq!(scratch.chunks, vec![(20, 30)]);
    // one chunk per anchor chunk, deduplicated, ascending
    scratch.positions = vec![3, 5, 12, 23];
    assert!(chunks_with_anchors(src, Cuts::new(&raw), &mut scratch));
    assert_eq!(scratch.chunks, vec![(0, 10), (10, 20), (20, 30)]);
    // no anchor, a single child, or cuts past the bytes (a different file) → no plan
    scratch.positions.clear();
    assert!(!chunks_with_anchors(src, Cuts::new(&raw), &mut scratch));
    scratch.positions = vec![3];
    assert!(!chunks_with_anchors(src, Cuts::new(&cuts_of(&[0])), &mut scratch));
    assert!(!chunks_with_anchors(b"short", Cuts::new(&raw), &mut scratch));
    // an anchor inside a child tree-sitter recovered → the file parses whole
    let flagged = cuts_flagged(&[(0, false), (10, true), (20, false)]);
    scratch.positions = vec![12];
    assert!(!chunks_with_anchors(src, Cuts::new(&flagged), &mut scratch));
    scratch.positions = vec![3, 23];
    assert!(chunks_with_anchors(src, Cuts::new(&flagged), &mut scratch));
    assert_eq!(scratch.chunks, vec![(0, 10), (20, 30)]);
    // per-chunk ranges carry exact points across ascending chunks
    let mut pc = PointCursor::default();
    let r0 = pc.range(src, 0, 10);
    let r2 = pc.range(src, 20, 30);
    assert_eq!((r0.start_byte, r0.end_byte), (0, 10));
    assert_eq!((r0.start_point.row, r0.end_point.row, r0.end_point.column), (0, 1, 0));
    assert_eq!((r2.start_point.row, r2.start_point.column, r2.end_point.row), (2, 0, 3));
  }

  #[test]
  fn memo_round_trips_and_stays_bounded() {
    let pattern = pattern_key("c", "kmalloc($A, $B)", None, None);
    let chunk = chunk_key(b"int f(void) { return kmalloc(1, 2); }\n");
    let mut out = Vec::new();
    assert!(!memo_get(pattern, chunk, &mut out));
    memo_put(pattern, chunk, &[21]);
    assert!(memo_get(pattern, chunk, &mut out));
    assert_eq!(out, vec![21]);
    // a different pattern over the same bytes is a different key
    assert!(!memo_get(pattern_key("c", "kfree($A)", None, None), chunk, &mut Vec::new()));
    // filling far past the per-shard budget evicts instead of growing without bound
    for i in 0..200_000u64 {
      memo_put(pattern, i.wrapping_mul(0x9E37_79B9_7F4A_7C15), &[1, 2, 3]);
    }
    assert!(memo_bytes() <= (64 << 20) + MEMO_SHARDS * 64, "{}", memo_bytes());
  }
}
