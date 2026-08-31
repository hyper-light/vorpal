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
/// rows, precomputed at train) — v1 files stored raw factor rows, which v2 readers
/// would silently misread as composed.
pub const LEARNED_MODEL_VERSION: u32 = 2;

const MAGIC: &[u8; 4] = b"VMD1";

/// Cheap header compatibility check over raw file bytes: magic + format version. The
/// warm-side freshness gate uses this WITHOUT deserializing (a checksum-intact file of
/// an older version must read as STALE, or a version bump would leave the tier wedged:
/// "fresh" to the builder, unloadable to every query).
pub fn model_bytes_compatible(bytes: &[u8]) -> bool {
  bytes.len() >= 8
    && &bytes[..4] == MAGIC
    && bytes[4..8] == LEARNED_MODEL_VERSION.to_le_bytes()
}

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

  let mut bytes = Vec::new();
  bytes.extend_from_slice(MAGIC);
  bytes.extend_from_slice(&LEARNED_MODEL_VERSION.to_le_bytes());
  bytes.extend_from_slice(&(dim as u32).to_le_bytes());
  bytes.extend_from_slice(&(word_count as u32).to_le_bytes());
  bytes.push(gram_mode);
  bytes.extend_from_slice(&(gram_slots as u64).to_le_bytes());
  bytes.extend_from_slice(&(model.frequencies.len() as u64).to_le_bytes());
  bytes.extend_from_slice(&(model.abtt.components.len() as u32).to_le_bytes());
  bytes.extend_from_slice(&(model.sentence.components.len() as u32).to_le_bytes());
  bytes.extend_from_slice(&model.usif.a.to_le_bytes());

  let write_string = |bytes: &mut Vec<u8>, text: &str| {
    bytes.extend_from_slice(&(text.len() as u32).to_le_bytes());
    bytes.extend_from_slice(text.as_bytes());
  };
  for term in &model.word_terms {
    write_string(&mut bytes, term);
  }
  for value in &model.word_rows {
    bytes.extend_from_slice(&value.to_le_bytes());
  }
  if let Some(terms) = exact_grams {
    for term in terms {
      write_string(&mut bytes, term);
    }
  }
  for value in &model.gram_rows {
    bytes.extend_from_slice(&value.to_le_bytes());
  }
  // Frequencies sorted by term: HashMap order must never reach the bytes.
  let mut frequencies: Vec<(&String, &f64)> = model.frequencies.iter().collect();
  frequencies.sort_by(|a, b| a.0.cmp(b.0));
  for (term, probability) in frequencies {
    write_string(&mut bytes, term);
    bytes.extend_from_slice(&probability.to_le_bytes());
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

  fn u8(&mut self) -> Result<u8, String> {
    Ok(self.take(1)?[0])
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

  fn string(&mut self) -> Result<String, String> {
    let len = self.u32()? as usize;
    let bytes = self.take(len)?;
    String::from_utf8(bytes.to_vec()).map_err(|_| "invalid utf8 in model string".to_string())
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
}

/// Load and FULLY validate a VMD1 model: magic, version, shapes, utf8, and the sealed
/// checksum. Every failure is a typed error — a damaged sidecar can only route to the
/// lexical fallback, never produce a partial model.
pub fn load_model(path: &Path) -> Result<(LearnedModel, u128), String> {
  let bytes = std::fs::read(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
  if bytes.len() < 16 + MAGIC.len() {
    return Err("model file too short".to_string());
  }
  let body_len = bytes.len() - 16;
  let stored = u128::from_le_bytes(bytes[body_len..].try_into().map_err(|_| "trailer")?);
  let computed = xxhash_rust::xxh3::xxh3_128(&bytes[..body_len]);
  if stored != computed {
    return Err("model checksum mismatch (torn or foreign ann.model.bin)".to_string());
  }
  let mut reader = Reader {
    bytes: &bytes[..body_len],
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
  let gram_mode = reader.u8()?;
  let gram_slots = reader.u64()? as usize;
  let freq_count = reader.u64()? as usize;
  let abtt_components = reader.u32()? as usize;
  let sentence_components = reader.u32()? as usize;
  let usif_a = reader.f64()?;
  if dim == 0 || !usif_a.is_finite() || usif_a <= 0.0 {
    return Err(format!("degenerate model header (dim {dim}, a {usif_a})"));
  }

  let mut word_terms = Vec::with_capacity(word_count);
  for _ in 0..word_count {
    word_terms.push(reader.string()?);
  }
  let word_rows = reader.f32_vec(word_count * dim)?;
  let gram_table = match gram_mode {
    0 => {
      let mut map = HashMap::with_capacity(gram_slots);
      for slot in 0..gram_slots {
        let gram = reader.string()?;
        if map.insert(gram, slot as u32).is_some() {
          return Err(format!("duplicate gram term at slot {slot}"));
        }
      }
      GramTable::Exact(map)
    }
    1 => GramTable::Bucketed(gram_slots),
    other => return Err(format!("unknown gram mode {other}")),
  };
  let gram_rows = reader.f32_vec(gram_slots * dim)?;
  let mut frequencies = HashMap::with_capacity(freq_count);
  for _ in 0..freq_count {
    let term = reader.string()?;
    let probability = reader.f64()?;
    if !(probability.is_finite() && probability >= 0.0) {
      return Err(format!("degenerate frequency for {term:?}: {probability}"));
    }
    frequencies.insert(term, probability);
  }
  let mean = reader.f32_vec(dim)?;
  let mut components = Vec::with_capacity(abtt_components);
  for _ in 0..abtt_components {
    components.push(reader.f32_vec(dim)?);
  }
  let lambdas = reader.f32_vec(sentence_components)?;
  let mut sentence_vectors = Vec::with_capacity(sentence_components);
  for _ in 0..sentence_components {
    sentence_vectors.push(reader.f32_vec(dim)?);
  }
  if reader.offset != body_len {
    return Err(format!(
      "model file has {} trailing bytes before the checksum",
      body_len - reader.offset
    ));
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
      usif: UsifWeighting { a: usif_a },
      abtt: Abtt { mean, components },
      sentence: SentenceComponents {
        lambdas,
        components: sentence_vectors,
      },
    },
    stored,
  ))
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
