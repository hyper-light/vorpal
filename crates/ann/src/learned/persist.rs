//! `ann.model.bin` — the VMD1 persisted form of a trained [`LearnedModel`]. Warm-tier
//! sidecar rules apply: machine-local, provenance-gated, never part of a generation's
//! content id; a torn/foreign/corrupt file must read as an ERROR (the caller routes to
//! the lexical fallback and says so), never as a partial model.
//!
//! Bytes are DETERMINISTIC for a given model (the double-warm byte-identity gate):
//! word terms serialize in row order, the exact gram table in slot order, frequencies
//! sorted by term; every float is little-endian bit-exact. The file seals itself with
//! an xxh3-128 checksum over everything before the trailer; the checksum doubles as
//! the tier's `weights_hash` in `ann.model.json`.
//!
//! Layout (little-endian):
//! ```text
//! "VMD1" | version u32 | dim u32 | word_count u32 | gram_mode u8 (0 exact, 1 bucket)
//! | gram_slots u64 | freq_count u64 | abtt_components u32 | sentence_components u32
//! | usif_a f64
//! | word terms (len u32 + utf8, row order)
//! | word_rows f32 × word_count·dim (COMPOSED word vectors: factor row + Σ gram rows)
//! | gram terms (exact mode only: len u32 + utf8, slot order)
//! | gram_rows f32 × gram_slots·dim
//! | frequencies (len u32 + utf8 + p f64, sorted by term)
//! | abtt mean f32 × dim | abtt components f32 × abtt_components·dim
//! | sentence lambdas f32 × sentence_components
//! | sentence components f32 × sentence_components·dim
//! | xxh3_128 u128 over all preceding bytes
//! ```

use std::collections::HashMap;
use std::path::Path;

use crate::learned::model::{GramTable, LearnedModel};
use crate::learned::pool::{Abtt, SentenceComponents, UsifWeighting};

/// VMD format version — bumped on ANY layout or semantics change so stale files can
/// never silently deserialize (docs INDEX_FORMAT row: mismatch → lexical fallback →
/// re-warm retrains). v2: `word_rows` carry COMPOSED word vectors (factor row + Σ gram
/// rows, precomputed at train). v3: the zero-copy layout — aligned numeric sections,
/// offset/permutation tables, term blobs (a v2 file under a v3 reader would misparse
/// past the header, which is why the freshness gate is the FULL open, never a prefix
/// check: a forgotten-bump incident proved prefix gates and readers can drift).
pub const LEARNED_MODEL_VERSION: u32 = 3;

const MAGIC: &[u8; 4] = b"VMD1";

/// Serialize `model` to deterministic VMD1 bytes (checksum trailer included).
pub fn model_to_bytes(model: &LearnedModel) -> Result<Vec<u8>, String> {
  let dim = model.dim;
  let word_count = model.word_terms.len();
  let (gram_mode, gram_slots, exact_grams) = match &model.gram_table {
    GramTable::Exact(map) => {
      // Invert to slot order; the map must be a bijection onto 0..len.
      let mut terms = vec![None::<&str>; map.len()];
      for (gram, &slot) in map {
        let cell = terms
          .get_mut(slot as usize)
          .ok_or_else(|| format!("gram slot {slot} out of range {}", map.len()))?;
        if cell.is_some() {
          return Err(format!("duplicate gram slot {slot}"));
        }
        *cell = Some(gram.as_str());
      }
      let terms: Vec<&str> = terms
        .into_iter()
        .enumerate()
        .map(|(slot, term)| term.ok_or_else(|| format!("gram slot {slot} unassigned")))
        .collect::<Result<_, String>>()?;
      (0u8, map.len(), Some(terms))
    }
    GramTable::Bucketed(buckets) => (1u8, *buckets, None),
  };
  if model.word_rows.len() != word_count * dim {
    return Err("word row table shape mismatch".to_string());
  }
  if model.gram_rows.len() != gram_slots * dim {
    return Err("gram row table shape mismatch".to_string());
  }

  // v3 layout: fixed 56-byte header, then 8-aligned numeric sections in one
  // deterministic order, then offset/permutation tables, then raw term blobs — every
  // section boundary derivable from the header (+ the offsets' final entries for the
  // blobs), so the zero-copy view casts sections in place with no scan.
  let mut bytes = Vec::new();
  bytes.extend_from_slice(MAGIC);
  bytes.extend_from_slice(&LEARNED_MODEL_VERSION.to_le_bytes());
  bytes.extend_from_slice(&(dim as u32).to_le_bytes());
  bytes.extend_from_slice(&(word_count as u32).to_le_bytes());
  bytes.extend_from_slice(&(gram_mode as u32).to_le_bytes());
  bytes.extend_from_slice(&(model.abtt.components.len() as u32).to_le_bytes());
  bytes.extend_from_slice(&(model.sentence.components.len() as u32).to_le_bytes());
  bytes.extend_from_slice(&0u32.to_le_bytes()); // reserved
  bytes.extend_from_slice(&(gram_slots as u64).to_le_bytes());
  bytes.extend_from_slice(&(model.frequencies.len() as u64).to_le_bytes());
  bytes.extend_from_slice(&model.usif.a.to_le_bytes());

  // Frequencies sorted by term: HashMap order must never reach the bytes, and the
  // sorted order IS the mapped view's direct binary-search order.
  let mut frequencies: Vec<(&String, &f64)> = model.frequencies.iter().collect();
  frequencies.sort_by(|a, b| a.0.cmp(b.0));
  for (_, probability) in &frequencies {
    bytes.extend_from_slice(&probability.to_le_bytes());
  }
  for value in &model.word_rows {
    bytes.extend_from_slice(&value.to_le_bytes());
  }
  for value in &model.gram_rows {
    bytes.extend_from_slice(&value.to_le_bytes());
  }
  if model.abtt.mean.len() != dim {
    return Err("ABTT mean shape mismatch".to_string());
  }
  for value in &model.abtt.mean {
    bytes.extend_from_slice(&value.to_le_bytes());
  }
  for component in &model.abtt.components {
    if component.len() != dim {
      return Err("ABTT component shape mismatch".to_string());
    }
    for value in component {
      bytes.extend_from_slice(&value.to_le_bytes());
    }
  }
  if model.sentence.lambdas.len() != model.sentence.components.len() {
    return Err("sentence component shape mismatch".to_string());
  }
  for value in &model.sentence.lambdas {
    bytes.extend_from_slice(&value.to_le_bytes());
  }
  for component in &model.sentence.components {
    if component.len() != dim {
      return Err("sentence component shape mismatch".to_string());
    }
    for value in component {
      bytes.extend_from_slice(&value.to_le_bytes());
    }
  }
  while bytes.len() % 8 != 0 {
    bytes.push(0);
  }

  // String sections: u64 prefix-offset tables (with a leading 0 sentinel), u32
  // sorted-permutation tables for in-place binary search, then the raw term blobs.
  // Word/gram terms stay in ROW/SLOT order (their float rows bind by position);
  // frequencies are term-sorted already, so they need no permutation. Bucketed gram
  // tables carry no strings at all.
  let word_terms: Vec<&str> = model.word_terms.iter().map(String::as_str).collect();
  let gram_terms: Vec<&str> = exact_grams.unwrap_or_default();
  let freq_terms: Vec<&str> = frequencies.iter().map(|(term, _)| term.as_str()).collect();
  let write_offsets = |bytes: &mut Vec<u8>, terms: &[&str]| {
    let mut offset = 0u64;
    bytes.extend_from_slice(&offset.to_le_bytes());
    for term in terms {
      offset += term.len() as u64;
      bytes.extend_from_slice(&offset.to_le_bytes());
    }
  };
  let sorted_by_bytes = |terms: &[&str]| -> Vec<u32> {
    let mut order: Vec<u32> = (0..terms.len() as u32).collect();
    order.sort_by(|&a, &b| terms[a as usize].as_bytes().cmp(terms[b as usize].as_bytes()));
    order
  };
  write_offsets(&mut bytes, &word_terms);
  if gram_mode == 0 {
    write_offsets(&mut bytes, &gram_terms);
  }
  write_offsets(&mut bytes, &freq_terms);
  for index in sorted_by_bytes(&word_terms) {
    bytes.extend_from_slice(&index.to_le_bytes());
  }
  if gram_mode == 0 {
    for index in sorted_by_bytes(&gram_terms) {
      bytes.extend_from_slice(&index.to_le_bytes());
    }
  }
  for term in &word_terms {
    bytes.extend_from_slice(term.as_bytes());
  }
  for term in &gram_terms {
    bytes.extend_from_slice(term.as_bytes());
  }
  for term in &freq_terms {
    bytes.extend_from_slice(term.as_bytes());
  }

  let checksum = xxhash_rust::xxh3::xxh3_128(&bytes);
  bytes.extend_from_slice(&checksum.to_le_bytes());
  Ok(bytes)
}

/// Atomically persist `model` at `path` (tmp + rename). Returns the checksum — the
/// tier's `weights_hash`.
pub fn save_model(model: &LearnedModel, path: &Path) -> Result<u128, String> {
  let bytes = model_to_bytes(model)?;
  let checksum_offset = bytes.len() - 16;
  let checksum = u128::from_le_bytes(
    bytes[checksum_offset..]
      .try_into()
      .map_err(|_| "checksum trailer slice")?,
  );
  let tmp = path.with_extension("bin.tmp");
  std::fs::write(&tmp, &bytes).map_err(|e| format!("writing {}: {e}", tmp.display()))?;
  std::fs::rename(&tmp, path).map_err(|e| format!("committing {}: {e}", path.display()))?;
  Ok(checksum)
}

struct Reader<'a> {
  bytes: &'a [u8],
  offset: usize,
}

impl<'a> Reader<'a> {
  fn take(&mut self, len: usize) -> Result<&'a [u8], String> {
    let slice = self
      .bytes
      .get(self.offset..self.offset + len)
      .ok_or_else(|| format!("model file truncated at offset {}", self.offset))?;
    self.offset += len;
    Ok(slice)
  }

  fn u32(&mut self) -> Result<u32, String> {
    Ok(u32::from_le_bytes(self.take(4)?.try_into().map_err(|_| "u32")?))
  }

  fn u64(&mut self) -> Result<u64, String> {
    Ok(u64::from_le_bytes(self.take(8)?.try_into().map_err(|_| "u64")?))
  }

  fn f64(&mut self) -> Result<f64, String> {
    Ok(f64::from_le_bytes(self.take(8)?.try_into().map_err(|_| "f64")?))
  }

  fn f32_vec(&mut self, count: usize) -> Result<Vec<f32>, String> {
    let bytes = self.take(count * 4)?;
    Ok(
      bytes
        .chunks_exact(4)
        .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
        .collect(),
    )
  }

  fn u32_vec(&mut self, count: usize) -> Result<Vec<u32>, String> {
    let bytes = self.take(count * 4)?;
    Ok(
      bytes
        .chunks_exact(4)
        .map(|chunk| u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
        .collect(),
    )
  }

  fn u64_vec(&mut self, count: usize) -> Result<Vec<u64>, String> {
    let bytes = self.take(count * 8)?;
    Ok(
      bytes
        .chunks_exact(8)
        .map(|chunk| {
          u64::from_le_bytes([
            chunk[0], chunk[1], chunk[2], chunk[3], chunk[4], chunk[5], chunk[6], chunk[7],
          ])
        })
        .collect(),
    )
  }

  fn f64_vec(&mut self, count: usize) -> Result<Vec<f64>, String> {
    let bytes = self.take(count * 8)?;
    Ok(
      bytes
        .chunks_exact(8)
        .map(|chunk| {
          f64::from_le_bytes([
            chunk[0], chunk[1], chunk[2], chunk[3], chunk[4], chunk[5], chunk[6], chunk[7],
          ])
        })
        .collect(),
    )
  }

  /// Skip (and verify) the zero padding up to the next 8-byte boundary.
  fn pad_to8(&mut self) -> Result<(), String> {
    let pad = (8 - self.offset % 8) % 8;
    let bytes = self.take(pad)?;
    if bytes.iter().any(|&byte| byte != 0) {
      return Err("nonzero section padding".to_string());
    }
    Ok(())
  }
}

/// Fixed v3 header length: magic + version + five u32 counters + reserved + two u64
/// counts + uSIF a.
const HEADER_BYTES: usize = 56;

/// The v3 header, parsed and sanity-checked — ONE parser shared by the owned loader
/// and the zero-copy view, so the two can never disagree about the layout.
struct Header {
  dim: usize,
  word_count: usize,
  gram_mode: u32,
  abtt_count: usize,
  sentence_count: usize,
  gram_slots: usize,
  freq_count: usize,
  usif_a: f64,
}

fn parse_header(body: &[u8]) -> Result<Header, String> {
  let mut reader = Reader {
    bytes: body,
    offset: 0,
  };
  if reader.take(4)? != MAGIC {
    return Err("bad model magic".to_string());
  }
  let version = reader.u32()?;
  if version != LEARNED_MODEL_VERSION {
    return Err(format!(
      "model version {version} ≠ supported {LEARNED_MODEL_VERSION}"
    ));
  }
  let dim = reader.u32()? as usize;
  let word_count = reader.u32()? as usize;
  let gram_mode = reader.u32()?;
  let abtt_count = reader.u32()? as usize;
  let sentence_count = reader.u32()? as usize;
  let _reserved = reader.u32()?;
  let gram_slots = reader.u64()? as usize;
  let freq_count = reader.u64()? as usize;
  let usif_a = reader.f64()?;
  if dim == 0 || !usif_a.is_finite() || usif_a <= 0.0 {
    return Err(format!("degenerate model header (dim {dim}, a {usif_a})"));
  }
  if gram_mode > 1 {
    return Err(format!("unknown gram mode {gram_mode}"));
  }
  Ok(Header {
    dim,
    word_count,
    gram_mode,
    abtt_count,
    sentence_count,
    gram_slots,
    freq_count,
    usif_a,
  })
}

/// Load and FULLY validate a v3 model into the OWNED form: magic, version, shapes,
/// padding, sorted-table order, utf8, and the sealed checksum. Every failure is a
/// typed error — a damaged sidecar can only route to the lexical fallback, never
/// produce a partial model. (The query side uses [`ModelView::open`] instead; this
/// materialized form serves training round-trips and tests.)
pub fn load_model(path: &Path) -> Result<(LearnedModel, u128), String> {
  let bytes = std::fs::read(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
  if bytes.len() < 16 + HEADER_BYTES {
    return Err("model file too short".to_string());
  }
  let body_len = bytes.len() - 16;
  let stored = u128::from_le_bytes(bytes[body_len..].try_into().map_err(|_| "trailer")?);
  let computed = xxhash_rust::xxh3::xxh3_128(&bytes[..body_len]);
  if stored != computed {
    return Err("model checksum mismatch (torn or foreign ann.model.bin)".to_string());
  }
  let body = &bytes[..body_len];
  let header = parse_header(body)?;
  let dim = header.dim;
  let exact = header.gram_mode == 0;
  let mut reader = Reader {
    bytes: body,
    offset: HEADER_BYTES,
  };
  let freq_values = reader.f64_vec(header.freq_count)?;
  let cells = |rows: usize| -> Result<usize, String> {
    rows
      .checked_mul(dim)
      .ok_or_else(|| "model section size overflow".to_string())
  };
  let word_rows = reader.f32_vec(cells(header.word_count)?)?;
  let gram_rows = reader.f32_vec(cells(header.gram_slots)?)?;
  let mean = reader.f32_vec(dim)?;
  let mut components = Vec::with_capacity(header.abtt_count);
  for _ in 0..header.abtt_count {
    components.push(reader.f32_vec(dim)?);
  }
  let lambdas = reader.f32_vec(header.sentence_count)?;
  let mut sentence_vectors = Vec::with_capacity(header.sentence_count);
  for _ in 0..header.sentence_count {
    sentence_vectors.push(reader.f32_vec(dim)?);
  }
  reader.pad_to8()?;
  let word_offsets = reader.u64_vec(header.word_count + 1)?;
  let gram_offsets = if exact {
    reader.u64_vec(header.gram_slots + 1)?
  } else {
    Vec::new()
  };
  let freq_offsets = reader.u64_vec(header.freq_count + 1)?;
  let word_sorted = reader.u32_vec(header.word_count)?;
  let gram_sorted = if exact {
    reader.u32_vec(header.gram_slots)?
  } else {
    Vec::new()
  };
  let read_terms = |reader: &mut Reader, offsets: &[u64]| -> Result<Vec<String>, String> {
    let total = offsets.last().copied().unwrap_or(0) as usize;
    let blob = reader.take(total)?;
    let mut terms = Vec::with_capacity(offsets.len().saturating_sub(1));
    for window in offsets.windows(2) {
      let (start, end) = (window[0] as usize, window[1] as usize);
      if end < start || end > total {
        return Err("term offsets not monotone".to_string());
      }
      terms.push(
        String::from_utf8(blob[start..end].to_vec())
          .map_err(|_| "invalid utf8 in model string".to_string())?,
      );
    }
    Ok(terms)
  };
  let word_terms = read_terms(&mut reader, &word_offsets)?;
  let gram_terms = read_terms(&mut reader, &gram_offsets)?;
  let freq_terms = read_terms(&mut reader, &freq_offsets)?;
  if reader.offset != body_len {
    return Err(format!(
      "model file has {} trailing bytes before the checksum",
      body_len - reader.offset
    ));
  }
  // The sorted permutations must actually sort their terms (writer-bug insurance —
  // the mapped view binary-searches them blind).
  let check_sorted = |terms: &[String], order: &[u32]| -> Result<(), String> {
    if order.len() != terms.len() {
      return Err("sorted table length mismatch".to_string());
    }
    for window in order.windows(2) {
      let (a, b) = (window[0] as usize, window[1] as usize);
      if terms
        .get(a)
        .zip(terms.get(b))
        .is_none_or(|(x, y)| x.as_bytes() >= y.as_bytes())
      {
        return Err("sorted table out of order".to_string());
      }
    }
    if terms.len() == 1 && order.first().copied() != Some(0) {
      return Err("sorted table out of range".to_string());
    }
    Ok(())
  };
  check_sorted(&word_terms, &word_sorted)?;
  check_sorted(&gram_terms, &gram_sorted)?;
  for window in freq_terms.windows(2) {
    if window[0].as_bytes() >= window[1].as_bytes() {
      return Err("frequency terms not sorted".to_string());
    }
  }

  let gram_table = if exact {
    let mut map = HashMap::with_capacity(header.gram_slots);
    for (slot, gram) in gram_terms.into_iter().enumerate() {
      if map.insert(gram, slot as u32).is_some() {
        return Err(format!("duplicate gram term at slot {slot}"));
      }
    }
    GramTable::Exact(map)
  } else {
    GramTable::Bucketed(header.gram_slots)
  };
  let mut frequencies = HashMap::with_capacity(header.freq_count);
  for (term, probability) in freq_terms.into_iter().zip(freq_values) {
    if !(probability.is_finite() && probability >= 0.0) {
      return Err(format!("degenerate frequency for {term:?}: {probability}"));
    }
    frequencies.insert(term, probability);
  }
  let word_ids: HashMap<String, u32> = word_terms
    .iter()
    .enumerate()
    .map(|(id, term)| (term.clone(), id as u32))
    .collect();
  if word_ids.len() != word_terms.len() {
    return Err("duplicate word terms in model".to_string());
  }
  Ok((
    LearnedModel {
      dim,
      word_terms,
      word_ids,
      word_rows,
      gram_table,
      gram_rows,
      frequencies,
      usif: UsifWeighting { a: header.usif_a },
      abtt: Abtt { mean, components },
      sentence: SentenceComponents {
        lambdas,
        components: sentence_vectors,
      },
    },
    stored,
  ))
}

use std::cmp::Ordering;
use std::sync::Arc;

use vorpal_mem::{
  AccessPattern, CorpusProbe, Hotness, MappedStore, PodColumn, ResourcePolicy, StoreKind,
};

use crate::learned::model::{TokenLexicon, embed_text_via};

/// Zero-copy, checksum-verified view over a persisted v3 model: the bulk tables
/// (word/gram rows, frequency values, offset + permutation tables) are typed casts
/// into ONE read-only mapping — pages fault in as queries touch them — and only the
/// tiny post-processing sections (ABTT, sentence PCs, uSIF a) materialize (a few KB).
/// Lookups binary-search the sorted permutations against raw term BYTES. The view
/// runs the SAME generic pipeline as the owned model
/// ([`crate::learned::model::TokenLexicon`]), so mapped and owned embeddings are
/// bit-identical — pinned by test.
pub struct ModelView {
  store: Arc<MappedStore>,
  dim: usize,
  word_count: usize,
  gram_slots: usize,
  freq_count: usize,
  /// `Some(buckets)` when the gram table is hash-bucketed (no strings on disk).
  bucketed_slots: Option<usize>,
  freq_values: PodColumn<f64>,
  word_rows: PodColumn<f32>,
  gram_rows: PodColumn<f32>,
  word_offsets: PodColumn<u64>,
  gram_offsets: Option<PodColumn<u64>>,
  freq_offsets: PodColumn<u64>,
  word_sorted: PodColumn<u32>,
  gram_sorted: Option<PodColumn<u32>>,
  word_blob: (usize, usize),
  gram_blob: (usize, usize),
  freq_blob: (usize, usize),
  usif: UsifWeighting,
  abtt: Abtt,
  sentence: SentenceComponents,
}

impl ModelView {
  /// Map, checksum, and section the file: O(hash) once, then every lookup is a
  /// binary search over mapped bytes and every row read a mapped slice — nothing
  /// bulk materializes. All section arithmetic is checked (a self-checksummed
  /// FOREIGN file must fail typed, never overflow), offset tables are validated
  /// monotone up front, and out-of-range permutation entries can only miss
  /// (`Option`), never read outside their section.
  pub fn open(path: &Path) -> Result<(ModelView, u128), String> {
    let store = Arc::new(
      MappedStore::map_file(
        path,
        StoreKind::VectorsFull,
        AccessPattern::Random,
        Hotness::Hot,
        &ResourcePolicy::probe(CorpusProbe::new(0, 0)),
      )
      .map_err(|e| format!("mapping {}: {e}", path.display()))?,
    );
    let bytes = store.as_bytes();
    if bytes.len() < 16 + HEADER_BYTES {
      return Err("model file too short".to_string());
    }
    let body_len = bytes.len() - 16;
    let stored = u128::from_le_bytes(bytes[body_len..].try_into().map_err(|_| "trailer")?);
    let computed = xxhash_rust::xxh3::xxh3_128(&bytes[..body_len]);
    if stored != computed {
      return Err("model checksum mismatch (torn or foreign ann.model.bin)".to_string());
    }
    let header = parse_header(&bytes[..body_len])?;
    let dim = header.dim;
    let exact = header.gram_mode == 0;

    // Section walk — mirrors the writer exactly; the tiling equality at the end
    // seals the agreement. Checked arithmetic throughout.
    let overflow = || "model section size overflow".to_string();
    let mul = |a: usize, b: usize| a.checked_mul(b).ok_or_else(overflow);
    let add = |a: usize, b: usize| a.checked_add(b).ok_or_else(overflow);
    let freq_values_at = HEADER_BYTES;
    let word_rows_at = add(freq_values_at, mul(header.freq_count, 8)?)?;
    let gram_rows_at = add(word_rows_at, mul(mul(header.word_count, dim)?, 4)?)?;
    let abtt_mean_at = add(gram_rows_at, mul(mul(header.gram_slots, dim)?, 4)?)?;
    let abtt_comp_at = add(abtt_mean_at, mul(dim, 4)?)?;
    let lambdas_at = add(abtt_comp_at, mul(mul(header.abtt_count, dim)?, 4)?)?;
    let sent_comp_at = add(lambdas_at, mul(header.sentence_count, 4)?)?;
    let pad_at = add(sent_comp_at, mul(mul(header.sentence_count, dim)?, 4)?)?;
    let word_offsets_at = add(pad_at, (8 - pad_at % 8) % 8)?;
    let gram_offsets_at = add(word_offsets_at, mul(header.word_count + 1, 8)?)?;
    let freq_offsets_at = add(
      gram_offsets_at,
      if exact {
        mul(header.gram_slots + 1, 8)?
      } else {
        0
      },
    )?;
    let word_sorted_at = add(freq_offsets_at, mul(header.freq_count + 1, 8)?)?;
    let gram_sorted_at = add(word_sorted_at, mul(header.word_count, 4)?)?;
    let blobs_at = add(
      gram_sorted_at,
      if exact { mul(header.gram_slots, 4)? } else { 0 },
    )?;
    if blobs_at > body_len {
      return Err("model sections exceed the file".to_string());
    }
    if bytes
      .get(pad_at..word_offsets_at)
      .is_none_or(|pad| pad.iter().any(|&byte| byte != 0))
    {
      return Err("nonzero section padding".to_string());
    }
    let column_f64 = |offset: usize, count: usize| -> Result<PodColumn<f64>, String> {
      PodColumn::from_mapped_le::<8>(&store, offset, count * 8, f64::from_le_bytes)
        .map_err(|e| format!("model section: {e}"))
    };
    let column_f32 = |offset: usize, count: usize| -> Result<PodColumn<f32>, String> {
      PodColumn::from_mapped_le::<4>(&store, offset, count * 4, f32::from_le_bytes)
        .map_err(|e| format!("model section: {e}"))
    };
    let column_u64 = |offset: usize, count: usize| -> Result<PodColumn<u64>, String> {
      PodColumn::from_mapped_le::<8>(&store, offset, count * 8, u64::from_le_bytes)
        .map_err(|e| format!("model section: {e}"))
    };
    let column_u32 = |offset: usize, count: usize| -> Result<PodColumn<u32>, String> {
      PodColumn::from_mapped_le::<4>(&store, offset, count * 4, u32::from_le_bytes)
        .map_err(|e| format!("model section: {e}"))
    };
    let freq_values = column_f64(freq_values_at, header.freq_count)?;
    let word_rows = column_f32(word_rows_at, mul(header.word_count, dim)?)?;
    let gram_rows = column_f32(gram_rows_at, mul(header.gram_slots, dim)?)?;
    let word_offsets = column_u64(word_offsets_at, header.word_count + 1)?;
    let gram_offsets = if exact {
      Some(column_u64(gram_offsets_at, header.gram_slots + 1)?)
    } else {
      None
    };
    let freq_offsets = column_u64(freq_offsets_at, header.freq_count + 1)?;
    let word_sorted = column_u32(word_sorted_at, header.word_count)?;
    let gram_sorted = if exact {
      Some(column_u32(gram_sorted_at, header.gram_slots)?)
    } else {
      None
    };
    let bounded = |offsets: &PodColumn<u64>| -> Result<usize, String> {
      let mut previous = 0u64;
      for (index, &offset) in offsets.iter().enumerate() {
        if index == 0 && offset != 0 {
          return Err("offset table must start at 0".to_string());
        }
        if offset < previous {
          return Err("term offsets not monotone".to_string());
        }
        previous = offset;
      }
      Ok(previous as usize)
    };
    let word_blob_len = bounded(&word_offsets)?;
    let gram_blob_len = match &gram_offsets {
      Some(offsets) => bounded(offsets)?,
      None => 0,
    };
    let freq_blob_len = bounded(&freq_offsets)?;
    let word_blob = (blobs_at, word_blob_len);
    let gram_blob = (add(blobs_at, word_blob_len)?, gram_blob_len);
    let freq_blob = (add(gram_blob.0, gram_blob_len)?, freq_blob_len);
    if add(freq_blob.0, freq_blob_len)? != body_len {
      return Err("model sections do not tile the file".to_string());
    }
    // The tiny post-processing sections materialize (a few KB at the D2 clamp).
    let f32s = |start: usize, count: usize| -> Vec<f32> {
      bytes[start..start + count * 4]
        .chunks_exact(4)
        .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
        .collect()
    };
    let mean = f32s(abtt_mean_at, dim);
    let mut components = Vec::with_capacity(header.abtt_count);
    for index in 0..header.abtt_count {
      components.push(f32s(abtt_comp_at + index * dim * 4, dim));
    }
    let lambdas = f32s(lambdas_at, header.sentence_count);
    let mut sentence_vectors = Vec::with_capacity(header.sentence_count);
    for index in 0..header.sentence_count {
      sentence_vectors.push(f32s(sent_comp_at + index * dim * 4, dim));
    }
    Ok((
      ModelView {
        store: store.clone(),
        dim,
        word_count: header.word_count,
        gram_slots: header.gram_slots,
        freq_count: header.freq_count,
        bucketed_slots: (!exact).then_some(header.gram_slots),
        freq_values,
        word_rows,
        gram_rows,
        word_offsets,
        gram_offsets,
        freq_offsets,
        word_sorted,
        gram_sorted,
        word_blob,
        gram_blob,
        freq_blob,
        usif: UsifWeighting { a: header.usif_a },
        abtt: Abtt { mean, components },
        sentence: SentenceComponents {
          lambdas,
          components: sentence_vectors,
        },
      },
      stored,
    ))
  }

  pub fn dim(&self) -> usize {
    self.dim
  }

  /// Embed arbitrary text through the ONE generic pipeline — bit-identical to the
  /// owned model's [`LearnedModel::embed_text`].
  pub fn embed_text(&self, text: &str, out: &mut [f32]) {
    embed_text_via(self, text, out);
  }

  fn term_bytes(&self, blob: (usize, usize), offsets: &PodColumn<u64>, index: usize) -> Option<&[u8]> {
    let start = *offsets.get(index)? as usize;
    let end = *offsets.get(index + 1)? as usize;
    if end < start || end > blob.1 {
      return None;
    }
    self.store.as_bytes().get(blob.0 + start..blob.0 + end)
  }

  /// Binary search over a term table: `order` present = permuted (word/gram terms
  /// stay in row/slot order), absent = the table is stored sorted (frequencies).
  /// Returns the ROW/SLOT index of the match.
  fn search_sorted(
    &self,
    blob: (usize, usize),
    offsets: &PodColumn<u64>,
    order: Option<&PodColumn<u32>>,
    count: usize,
    needle: &[u8],
  ) -> Option<u32> {
    let mut low = 0usize;
    let mut high = count;
    while low < high {
      let mid = low + (high - low) / 2;
      let index = match order {
        Some(order) => *order.get(mid)? as usize,
        None => mid,
      };
      match self.term_bytes(blob, offsets, index)?.cmp(needle) {
        Ordering::Less => low = mid + 1,
        Ordering::Greater => high = mid,
        Ordering::Equal => return Some(index as u32),
      }
    }
    None
  }
}

impl TokenLexicon for ModelView {
  fn dim(&self) -> usize {
    self.dim
  }
  fn word_row(&self, token: &str) -> Option<&[f32]> {
    let row = self.search_sorted(
      self.word_blob,
      &self.word_offsets,
      Some(&self.word_sorted),
      self.word_count,
      token.as_bytes(),
    )? as usize;
    self.word_rows.get(row * self.dim..(row + 1) * self.dim)
  }
  fn gram_slot(&self, gram: &str) -> Option<u32> {
    if let Some(buckets) = self.bucketed_slots {
      // The SAME bucketing formula the owned table uses — one source.
      return GramTable::Bucketed(buckets).slot(gram);
    }
    self.search_sorted(
      self.gram_blob,
      self.gram_offsets.as_ref()?,
      self.gram_sorted.as_ref(),
      self.gram_slots,
      gram.as_bytes(),
    )
  }
  fn gram_row(&self, slot: u32) -> Option<&[f32]> {
    let start = slot as usize * self.dim;
    self.gram_rows.get(start..start + self.dim)
  }
  fn frequency(&self, token: &str) -> f64 {
    self
      .search_sorted(
        self.freq_blob,
        &self.freq_offsets,
        None,
        self.freq_count,
        token.as_bytes(),
      )
      .and_then(|index| self.freq_values.get(index as usize).copied())
      .unwrap_or(0.0)
  }
  fn usif(&self) -> &UsifWeighting {
    &self.usif
  }
  fn abtt(&self) -> &Abtt {
    &self.abtt
  }
  fn sentence(&self) -> &SentenceComponents {
    &self.sentence
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::learned::model::{TrainResources, tests_support};

  /// Per-test scratch: cargo runs tests concurrently, so a shared directory would let
  /// one test's cleanup race another's training.
  fn trained(tag: &str) -> LearnedModel {
    let resources = TrainResources {
      scratch_dir: {
        let dir =
          std::env::temp_dir().join(format!("vorpal-persist-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
      },
      page_bytes: 4096,
      progress: |_| {},
      arena_chunk_bytes: 64 * 1024,
    };
    let (model, _) = LearnedModel::train(&tests_support::corpus, 42, &resources).unwrap();
    let _ = std::fs::remove_dir_all(&resources.scratch_dir);
    model
  }

  #[test]
  fn round_trip_preserves_embeddings_bitwise_and_bytes_are_deterministic() {
    let model = trained("round-trip");
    let bytes_a = model_to_bytes(&model).unwrap();
    let bytes_b = model_to_bytes(&model).unwrap();
    assert_eq!(bytes_a, bytes_b, "serialization must be byte-deterministic");

    let path = std::env::temp_dir().join(format!("vorpal-vmd1-{}.bin", std::process::id()));
    let checksum = save_model(&model, &path).unwrap();
    let (loaded, stored) = load_model(&path).unwrap();
    assert_eq!(checksum, stored);
    assert_eq!(loaded.dim, model.dim);

    for text in ["socket buffer", "grammar", "sockets oov compose", ""] {
      let mut original = vec![0.0f32; model.dim];
      let mut reloaded = vec![0.0f32; model.dim];
      model.embed_text(text, &mut original);
      loaded.embed_text(text, &mut reloaded);
      assert_eq!(
        original.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
        reloaded.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
        "embedding drifted after round trip for {text:?}"
      );
    }
    let _ = std::fs::remove_file(&path);
  }

  #[test]
  fn mapped_view_embeds_bit_identically_to_the_owned_model() {
    let model = trained("view-parity");
    let dir = std::env::temp_dir().join(format!("vorpal-persist-view-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("ann.model.bin");
    let saved = save_model(&model, &path).unwrap();
    let (view, opened) = ModelView::open(&path).unwrap();
    assert_eq!(saved, opened, "open must return the sealed checksum");
    assert_eq!(view.dim(), model.dim);
    // In-vocab, OOV (gram composition), empty, multi-token — every path through the
    // shared pipeline must produce the SAME BITS from the mapped view.
    for text in [
      "socket buffer alloc",
      "wholly_unknown_zzqv token",
      "",
      "packet recv socket close",
      "socket",
    ] {
      let mut owned = vec![0.0f32; model.dim];
      let mut mapped = vec![0.0f32; model.dim];
      model.embed_text(text, &mut owned);
      view.embed_text(text, &mut mapped);
      assert_eq!(
        owned.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
        mapped.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
        "mapped view diverged on {text:?}"
      );
    }
    // A flipped byte anywhere in the body must refuse the mapped open too.
    let mut tampered = std::fs::read(&path).unwrap();
    let middle = tampered.len() / 2;
    tampered[middle] ^= 0x01;
    std::fs::write(&path, &tampered).unwrap();
    match ModelView::open(&path) {
      Err(error) => assert!(error.contains("checksum"), "{error}"),
      Ok(_) => panic!("tampered model must not open"),
    }
    let _ = std::fs::remove_dir_all(&dir);
  }

  #[test]
  fn corruption_and_truncation_are_typed_errors() {
    let model = trained("corruption");
    let path = std::env::temp_dir().join(format!("vorpal-vmd1-bad-{}.bin", std::process::id()));
    save_model(&model, &path).unwrap();
    let mut bytes = std::fs::read(&path).unwrap();

    // Flip one payload byte → checksum mismatch.
    bytes[40] ^= 0x01;
    std::fs::write(&path, &bytes).unwrap();
    match load_model(&path) {
      Err(error) => assert!(error.contains("checksum"), "{error}"),
      Ok(_) => panic!("a corrupted model file must never load"),
    }

    // Truncate → typed error, never a partial model.
    bytes[40] ^= 0x01;
    std::fs::write(&path, &bytes[..bytes.len() / 2]).unwrap();
    assert!(load_model(&path).is_err());

    // Wrong magic.
    let mut wrong = bytes.clone();
    wrong[0] = b'X';
    std::fs::write(&path, &wrong).unwrap();
    assert!(load_model(&path).is_err());
    let _ = std::fs::remove_file(&path);
  }
}
