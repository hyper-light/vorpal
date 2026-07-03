//! Builds an immutable `.vseg` segment (HOT columns) from in-memory columns (§9.1).

use std::path::Path;

use xxhash_rust::xxh3::xxh3_64;

use crate::error::SegmentError;
use crate::format::{
  ALIGN, ColumnDesc, ColumnPlacement, DIR_ENTRY_LEN, FOOTER_LEN, FOOTER_MAGIC, HEADER_HASH_OFF,
  HEADER_LEN, Header, LogicalType, align_up, write_header,
};

struct PendingColumn {
  name: String,
  type_tag: u8,
  stride: u32,
  data: Vec<u8>,
  min: [u8; 8],
  max: [u8; 8],
}

/// Accumulates equal-length HOT columns, then serializes one sealed segment.
pub struct SegmentBuilder {
  logical_id_base: u64,
  row_count: Option<u64>,
  columns: Vec<PendingColumn>,
}

impl SegmentBuilder {
  /// Start a segment whose first row has `NodeId(logical_id_base)`.
  pub fn new(logical_id_base: u64) -> Self {
    Self {
      logical_id_base,
      row_count: None,
      columns: Vec::new(),
    }
  }

  pub fn row_count(&self) -> Option<u64> {
    self.row_count
  }

  fn set_or_check_rows(&mut self, n: u64) -> Result<(), SegmentError> {
    match self.row_count {
      None => {
        self.row_count = Some(n);
        Ok(())
      }
      Some(expected) if expected == n => Ok(()),
      Some(expected) => Err(SegmentError::RowMismatch { expected, got: n }),
    }
  }

  pub fn add_u8(&mut self, name: &str, values: &[u8]) -> Result<&mut Self, SegmentError> {
    self.set_or_check_rows(values.len() as u64)?;
    let (min, max) = minmax(values.iter().copied(), |v| pad8(&[v]));
    self.push(name, LogicalType::U8, 1, values.to_vec(), min, max);
    Ok(self)
  }

  pub fn add_u32(&mut self, name: &str, values: &[u32]) -> Result<&mut Self, SegmentError> {
    self.set_or_check_rows(values.len() as u64)?;
    let (min, max) = minmax(values.iter().copied(), |v| pad8(&v.to_ne_bytes()));
    self.push(
      name,
      LogicalType::U32,
      4,
      bytemuck::cast_slice(values).to_vec(),
      min,
      max,
    );
    Ok(self)
  }

  pub fn add_u64(&mut self, name: &str, values: &[u64]) -> Result<&mut Self, SegmentError> {
    self.set_or_check_rows(values.len() as u64)?;
    let (min, max) = minmax(values.iter().copied(), |v| v.to_ne_bytes());
    self.push(
      name,
      LogicalType::U64,
      8,
      bytemuck::cast_slice(values).to_vec(),
      min,
      max,
    );
    Ok(self)
  }

  /// A fixed-width opaque byte column (e.g. quantized vector codes). `data.len()` must be a
  /// multiple of `stride`; row count is `data.len() / stride`.
  pub fn add_bytes(
    &mut self,
    name: &str,
    stride: u32,
    data: &[u8],
  ) -> Result<&mut Self, SegmentError> {
    if stride == 0 || data.len() % stride as usize != 0 {
      return Err(SegmentError::RaggedColumn {
        len: data.len(),
        stride,
      });
    }
    self.set_or_check_rows((data.len() / stride as usize) as u64)?;
    self.push(
      name,
      LogicalType::Bytes,
      stride,
      data.to_vec(),
      [0; 8],
      [0; 8],
    );
    Ok(self)
  }

  fn push(
    &mut self,
    name: &str,
    ty: LogicalType,
    stride: u32,
    data: Vec<u8>,
    min: [u8; 8],
    max: [u8; 8],
  ) {
    self.columns.push(PendingColumn {
      name: name.to_string(),
      type_tag: ty.tag(),
      stride,
      data,
      min,
      max,
    });
  }

  /// Serialize the sealed segment to bytes (header + directory + HOT stripes + footer), computing
  /// the per-column xxh3, the header blake3, and the whole-segment blake3.
  pub fn build(&self) -> Result<Vec<u8>, SegmentError> {
    let row_count = self.row_count.ok_or(SegmentError::Empty)?;
    let ncols = self.columns.len();
    let dir_offset = HEADER_LEN as u64;
    let dir_len = (ncols * DIR_ENTRY_LEN) as u64;

    // Lay out 64 B-aligned HOT stripes and resolve each column's descriptor.
    let mut cursor = align_up(dir_offset + dir_len, ALIGN);
    let mut descs = Vec::with_capacity(ncols);
    for col in &self.columns {
      let data_offset = align_up(cursor, ALIGN);
      let data_len = col.data.len() as u64;
      descs.push(ColumnDesc {
        name_hash: xxh3_64(col.name.as_bytes()),
        type_tag: col.type_tag,
        placement: ColumnPlacement::Hot.tag(),
        stride: col.stride,
        data_offset,
        data_len,
        xxh3: xxh3_64(&col.data),
        min: col.min,
        max: col.max,
      });
      cursor = data_offset + data_len;
    }
    let footer_offset = align_up(cursor, ALIGN);
    let total = footer_offset + FOOTER_LEN as u64;

    let mut buf = vec![0u8; total as usize];

    write_header(
      &mut buf,
      &Header {
        row_count,
        logical_id_base: self.logical_id_base,
        column_count: ncols as u32,
        column_dir_offset: dir_offset,
        column_dir_len: dir_len,
        footer_offset,
      },
    );
    // Header blake3 over [0, HEADER_HASH_OFF); nothing after the header can change it.
    let hh = blake3::hash(&buf[0..HEADER_HASH_OFF]);
    buf[HEADER_HASH_OFF..HEADER_LEN].copy_from_slice(hh.as_bytes());

    for (i, desc) in descs.iter().enumerate() {
      desc.encode(&mut buf, dir_offset as usize + i * DIR_ENTRY_LEN);
    }
    for (col, desc) in self.columns.iter().zip(&descs) {
      let off = desc.data_offset as usize;
      buf[off..off + col.data.len()].copy_from_slice(&col.data);
    }

    let foff = footer_offset as usize;
    buf[foff..foff + 8].copy_from_slice(FOOTER_MAGIC);
    let sh = blake3::hash(&buf[0..foff]);
    buf[foff + 8..foff + 40].copy_from_slice(sh.as_bytes());

    Ok(buf)
  }

  /// Build and atomically write to `path` (temp + rename), mirroring segment publish (§9.7).
  pub fn write_to(&self, path: &Path) -> Result<(), SegmentError> {
    let bytes = self.build()?;
    let tmp = path.with_extension("vseg.tmp");
    std::fs::write(&tmp, &bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
  }
}

/// Fold a min/max zone-map over an iterator, projecting each value to its 8-byte native key.
fn minmax<T: Copy + PartialOrd>(
  mut it: impl Iterator<Item = T>,
  key: impl Fn(T) -> [u8; 8],
) -> ([u8; 8], [u8; 8]) {
  let Some(first) = it.next() else {
    return ([0; 8], [0; 8]);
  };
  let (mut lo, mut hi) = (first, first);
  for v in it {
    if v < lo {
      lo = v;
    }
    if v > hi {
      hi = v;
    }
  }
  (key(lo), key(hi))
}

fn pad8(bytes: &[u8]) -> [u8; 8] {
  let mut out = [0u8; 8];
  out[..bytes.len()].copy_from_slice(bytes);
  out
}
