//! Strict zero-copy safetensors loader for the vendored encoder weights (F32 only).
//!
//! Layout: 8-byte LE header length, JSON tensor table, then the data region.
//! Validation is total and typed — dtype must be F32, every span must sit inside
//! the data region with `product(shape) × 4` bytes, and every tensor start must be
//! 4-aligned (the HF writer 8-aligns the data section; a file that is not is
//! refused as malformed rather than silently copied). The mapping is read-only and
//! cold (`StoreKind::VectorsFull`): the OS pager carries the ~550 MB working set.

use std::collections::HashMap;
use std::path::Path;

pub struct SafeTensors {
  store: vorpal_mem::MappedStore,
  data_start: usize,
  /// name → (byte offset within the data region, byte length, shape).
  table: HashMap<String, (usize, usize, Vec<usize>)>,
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
    let bytes = store.as_bytes();
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
    let mut table = HashMap::with_capacity(object.len());
    for (name, meta) in object {
      if name == "__metadata__" {
        continue;
      }
      if meta.get("dtype").and_then(|d| d.as_str()) != Some("F32") {
        return Err(format!("safetensors: tensor {name} is not F32"));
      }
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
      if cells.checked_mul(4) != Some(end - start) {
        return Err(format!("safetensors: tensor {name} bytes disagree with its shape"));
      }
      if (data_start + start) % std::mem::align_of::<f32>() != 0 {
        return Err(format!("safetensors: tensor {name} is not 4-aligned (malformed writer)"));
      }
      table.insert(name.clone(), (start, end - start, shape));
    }
    Ok(SafeTensors {
      store,
      data_start,
      table,
    })
  }

  fn tensor(&self, name: &str) -> Result<(&[f32], &[usize]), String> {
    let (offset, len, shape) = self
      .table
      .get(name)
      .ok_or_else(|| format!("safetensors: no tensor named {name}"))?;
    let start = self.data_start + offset;
    let bytes = &self.store.as_bytes()[start..start + len];
    let floats =
      bytemuck::try_cast_slice(bytes).map_err(|e| format!("safetensors: cast {name}: {e}"))?;
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
