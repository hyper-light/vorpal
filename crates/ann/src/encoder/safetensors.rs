//! Strict safetensors loader for the vendored encoder weights (F32 or F16, one
//! dtype per file), plus the deterministic f32→f16 converter the installer uses.
//!
//! Layout: 8-byte LE header length, JSON tensor table, then the data region.
//! Validation is total and typed — every span must sit inside the data region
//! with `product(shape) × dtype size` bytes and dtype-aligned starts; a file
//! mixing dtypes (which the installer never writes) is refused. F32 files serve
//! ZERO-COPY from the cold mapping (`StoreKind::VectorsFull` — the OS pager
//! carries the ~550 MB working set). F16 files upconvert ONCE at open into an
//! owned f32 arena: halved disk, full-size RSS while a handle is live — the
//! stated f16 trade, with the f16-native kernel as the recorded follow-up lead.

use std::collections::HashMap;
use std::io::Read;
use std::path::Path;

use crate::encoder::f16::{f16_bits_to_f32, f32_to_f16_bits};

enum TensorStorage {
  /// All-F32 file: slices come straight from the mapping.
  Mapped(vorpal_mem::MappedStore),
  /// All-F16 file: one owned f32 arena built at open; table offsets address it.
  Owned(Vec<f32>),
}

pub struct SafeTensors {
  storage: TensorStorage,
  data_start: usize,
  /// name → (offset, length, shape). For `Mapped`: BYTES into the data region.
  /// For `Owned`: F32 CELLS into the arena.
  table: HashMap<String, (usize, usize, Vec<usize>)>,
}

/// Parse and validate a safetensors header: returns (data_start, table entries
/// as (name, dtype, shape, byte start, byte end)) with bounds/shape/alignment
/// checked against `data_len`.
#[allow(clippy::type_complexity)]
fn parse_header(
  bytes: &[u8],
) -> Result<(usize, Vec<(String, String, Vec<usize>, usize, usize)>), String> {
  if bytes.len() < 8 {
    return Err("safetensors: shorter than its header length field".to_string());
  }
  let header_len = u64::from_le_bytes(
    bytes[0..8]
      .try_into()
      .map_err(|_| "safetensors: unreadable header length".to_string())?,
  );
  let data_start = usize::try_from(header_len)
    .ok()
    .and_then(|len| len.checked_add(8))
    .filter(|start| *start <= bytes.len())
    .ok_or("safetensors: header length outside the file")?;
  let header: serde_json::Value = serde_json::from_slice(&bytes[8..data_start])
    .map_err(|e| format!("safetensors header parse: {e}"))?;
  let object = header
    .as_object()
    .ok_or("safetensors: header is not an object")?;
  let data_len = bytes.len() - data_start;
  let mut entries = Vec::with_capacity(object.len());
  for (name, meta) in object {
    if name == "__metadata__" {
      continue;
    }
    let dtype = meta
      .get("dtype")
      .and_then(|d| d.as_str())
      .ok_or_else(|| format!("safetensors: tensor {name} has no dtype"))?;
    let cell_bytes = match dtype {
      "F32" => 4usize,
      "F16" => 2usize,
      other => return Err(format!("safetensors: tensor {name} has unsupported dtype {other}")),
    };
    let shape: Vec<usize> = meta
      .get("shape")
      .and_then(|s| s.as_array())
      .ok_or_else(|| format!("safetensors: tensor {name} has no shape"))?
      .iter()
      .map(|d| d.as_u64().and_then(|d| usize::try_from(d).ok()))
      .collect::<Option<_>>()
      .ok_or_else(|| format!("safetensors: tensor {name} has a bad dimension"))?;
    let offsets = meta
      .get("data_offsets")
      .and_then(|o| o.as_array())
      .filter(|o| o.len() == 2)
      .ok_or_else(|| format!("safetensors: tensor {name} has no data_offsets"))?;
    let byte = |i: usize| -> Result<usize, String> {
      offsets[i]
        .as_u64()
        .and_then(|b| usize::try_from(b).ok())
        .ok_or_else(|| format!("safetensors: tensor {name} has a bad offset"))
    };
    let (start, end) = (byte(0)?, byte(1)?);
    if start > end || end > data_len {
      return Err(format!("safetensors: tensor {name} spans outside the data region"));
    }
    let cells = shape
      .iter()
      .try_fold(1usize, |product, &d| product.checked_mul(d))
      .ok_or_else(|| format!("safetensors: tensor {name} shape overflows"))?;
    if cells.checked_mul(cell_bytes) != Some(end - start) {
      return Err(format!("safetensors: tensor {name} bytes disagree with its shape"));
    }
    if (data_start + start) % cell_bytes != 0 {
      return Err(format!("safetensors: tensor {name} is not {cell_bytes}-aligned (malformed writer)"));
    }
    entries.push((name.clone(), dtype.to_string(), shape, start, end));
  }
  Ok((data_start, entries))
}

/// The raw header (8-byte length + JSON tensor table) and the file length — the
/// structural identity the encoder handle hashes at open (`model_identity`).
pub(crate) fn header_bytes(path: &Path) -> Result<(Vec<u8>, u64), String> {
  let mut file = std::fs::File::open(path).map_err(|e| format!("weights {}: {e}", path.display()))?;
  let file_len = file
    .metadata()
    .map_err(|e| format!("weights {}: {e}", path.display()))?
    .len();
  let mut length = [0u8; 8];
  file
    .read_exact(&mut length)
    .map_err(|e| format!("weights {}: header length: {e}", path.display()))?;
  let header_len = usize::try_from(u64::from_le_bytes(length))
    .ok()
    .filter(|len| (*len as u64) + 8 <= file_len)
    .ok_or("safetensors: header length outside the file")?;
  let mut header = vec![0u8; 8 + header_len];
  header[..8].copy_from_slice(&length);
  file
    .read_exact(&mut header[8..])
    .map_err(|e| format!("weights {}: header: {e}", path.display()))?;
  Ok((header, file_len))
}

impl SafeTensors {
  pub fn open(path: &Path) -> Result<SafeTensors, String> {
    let file_len = std::fs::metadata(path)
      .map_err(|e| format!("weights {}: {e}", path.display()))?
      .len();
    let policy = vorpal_mem::ResourcePolicy::probe(vorpal_mem::CorpusProbe::new(file_len, 1));
    let store = vorpal_mem::MappedStore::map_file(
      path,
      vorpal_mem::StoreKind::VectorsFull,
      vorpal_mem::AccessPattern::Random,
      vorpal_mem::Hotness::Cold,
      &policy,
    )
    .map_err(|e| format!("mapping {}: {e}", path.display()))?;
    let (data_start, entries) = parse_header(store.as_bytes())?;
    let f16_count = entries.iter().filter(|(_, dtype, ..)| dtype == "F16").count();
    if f16_count != 0 && f16_count != entries.len() {
      return Err("safetensors: mixed F16/F32 dtypes (the installer never writes this)".to_string());
    }
    let mut table = HashMap::with_capacity(entries.len());
    if f16_count == 0 {
      for (name, _, shape, start, end) in entries {
        table.insert(name, (start, end - start, shape));
      }
      return Ok(SafeTensors {
        storage: TensorStorage::Mapped(store),
        data_start,
        table,
      });
    }
    // All-F16: upconvert once into a single arena, in ascending file-offset
    // order (deterministic layout).
    let total_cells: usize = entries.iter().map(|(.., start, end)| (end - start) / 2).sum();
    let mut arena = Vec::with_capacity(total_cells);
    let mut ordered: Vec<&(String, String, Vec<usize>, usize, usize)> = entries.iter().collect();
    ordered.sort_by_key(|(.., start, _)| *start);
    let bytes = store.as_bytes();
    let mut placed: HashMap<&str, (usize, usize)> = HashMap::with_capacity(entries.len());
    for (name, _, _, start, end) in ordered {
      let cell_start = arena.len();
      for pair in bytes[data_start + start..data_start + end].chunks_exact(2) {
        let bits = u16::from_le_bytes([pair[0], pair[1]]);
        arena.push(f16_bits_to_f32(bits));
      }
      placed.insert(name.as_str(), (cell_start, arena.len() - cell_start));
    }
    for (name, _, shape, ..) in &entries {
      let (cell_start, cell_len) = placed[name.as_str()];
      table.insert(name.clone(), (cell_start, cell_len, shape.clone()));
    }
    Ok(SafeTensors {
      storage: TensorStorage::Owned(arena),
      data_start: 0,
      table,
    })
  }

  fn tensor(&self, name: &str) -> Result<(&[f32], &[usize]), String> {
    let (offset, len, shape) = self
      .table
      .get(name)
      .ok_or_else(|| format!("safetensors: no tensor named {name}"))?;
    let floats: &[f32] = match &self.storage {
      TensorStorage::Mapped(store) => {
        let start = self.data_start + offset;
        bytemuck::try_cast_slice(&store.as_bytes()[start..start + len])
          .map_err(|e| format!("safetensors: cast {name}: {e}"))?
      }
      TensorStorage::Owned(arena) => &arena[*offset..offset + len],
    };
    Ok((floats, shape))
  }

  /// A `[rows, cols]` tensor, shape-checked.
  pub fn matrix(&self, name: &str, rows: usize, cols: usize) -> Result<&[f32], String> {
    let (floats, shape) = self.tensor(name)?;
    if shape != [rows, cols] {
      return Err(format!(
        "safetensors: {name} has shape {shape:?}, expected [{rows}, {cols}]"
      ));
    }
    Ok(floats)
  }

  /// A `[len]` tensor, shape-checked.
  pub fn vector(&self, name: &str, len: usize) -> Result<&[f32], String> {
    let (floats, shape) = self.tensor(name)?;
    if shape != [len] {
      return Err(format!(
        "safetensors: {name} has shape {shape:?}, expected [{len}]"
      ));
    }
    Ok(floats)
  }
}

/// Does `path` hold an (all-)F16 safetensors file? Reads only the header.
pub fn safetensors_is_f16(path: &Path) -> Result<bool, String> {
  let mut file = std::fs::File::open(path).map_err(|e| format!("opening {}: {e}", path.display()))?;
  let mut length = [0u8; 8];
  file
    .read_exact(&mut length)
    .map_err(|e| format!("reading {}: {e}", path.display()))?;
  let header_len = usize::try_from(u64::from_le_bytes(length))
    .map_err(|_| "safetensors: header length outside the file".to_string())?;
  let mut header = vec![0u8; header_len];
  file
    .read_exact(&mut header)
    .map_err(|e| format!("reading {}: {e}", path.display()))?;
  let value: serde_json::Value =
    serde_json::from_slice(&header).map_err(|e| format!("safetensors header parse: {e}"))?;
  let object = value
    .as_object()
    .ok_or("safetensors: header is not an object")?;
  let mut saw_f16 = false;
  for (name, meta) in object {
    if name == "__metadata__" {
      continue;
    }
    match meta.get("dtype").and_then(|d| d.as_str()) {
      Some("F16") => saw_f16 = true,
      Some("F32") => return Ok(false),
      other => return Err(format!("safetensors: unsupported dtype {other:?}")),
    }
  }
  Ok(saw_f16)
}

/// Deterministically convert an all-F32 safetensors file to all-F16
/// (round-to-nearest-even; tensor names sorted, offsets repacked densely, header
/// space-padded so the data region stays 8-aligned) — the installer's
/// `semantic-f16` step. The converted file reopens through [`SafeTensors::open`]
/// like any other.
pub fn convert_safetensors_f32_to_f16(source: &Path, target: &Path) -> Result<(), String> {
  let bytes = std::fs::read(source).map_err(|e| format!("reading {}: {e}", source.display()))?;
  let (data_start, mut entries) = parse_header(&bytes)?;
  if entries.iter().any(|(_, dtype, ..)| dtype != "F32") {
    return Err("f16 conversion: source is not an all-F32 safetensors file".to_string());
  }
  entries.sort_by(|a, b| a.0.cmp(&b.0)); // name order — deterministic layout
  let mut header = serde_json::Map::new();
  let mut running = 0usize;
  for (name, _, shape, start, end) in &entries {
    let cells = (end - start) / 4;
    header.insert(
      name.clone(),
      serde_json::json!({
        "dtype": "F16",
        "shape": shape,
        "data_offsets": [running * 2, (running + cells) * 2],
      }),
    );
    running += cells;
  }
  let mut header_text =
    serde_json::to_string(&serde_json::Value::Object(header)).map_err(|e| format!("f16 header: {e}"))?;
  while (8 + header_text.len()) % 8 != 0 {
    header_text.push(' ');
  }
  let mut out = Vec::with_capacity(8 + header_text.len() + running * 2);
  out.extend_from_slice(&(header_text.len() as u64).to_le_bytes());
  out.extend_from_slice(header_text.as_bytes());
  for (_, _, _, start, end) in &entries {
    for quad in bytes[data_start + start..data_start + end].chunks_exact(4) {
      let value = f32::from_le_bytes([quad[0], quad[1], quad[2], quad[3]]);
      out.extend_from_slice(&f32_to_f16_bits(value).to_le_bytes());
    }
  }
  std::fs::write(target, out).map_err(|e| format!("writing {}: {e}", target.display()))
}
