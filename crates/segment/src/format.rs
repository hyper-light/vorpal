//! On-disk `.vseg` byte layout: constants, the column directory entry, and LE field helpers.
//!
//! Layout (§9.1):
//! ```text
//! [4 KiB header] [column directory] [HOT stripes, 64 B-aligned] [4 KiB footer]
//! ```
//! Metadata integers are little-endian; column payloads are native-endian (all supported targets
//! are little-endian, so these coincide) and read back zero-copy via `bytemuck`.

use crate::error::SegmentError;

pub(crate) const MAGIC: &[u8; 8] = b"VSEG0001";
pub(crate) const FOOTER_MAGIC: &[u8; 8] = b"VSEGEND1";
pub(crate) const FORMAT_VERSION: u32 = 1;

pub(crate) const HEADER_LEN: usize = 4096;
pub(crate) const FOOTER_LEN: usize = 4096;
/// Header bytes `[0..HEADER_HASH_OFF)` are covered by the header blake3, stored in the last 32.
pub(crate) const HEADER_HASH_OFF: usize = HEADER_LEN - 32;
pub(crate) const DIR_ENTRY_LEN: usize = 64;
/// HOT stripes and the footer are aligned to a cache line so a point lookup never straddles one.
pub(crate) const ALIGN: u64 = 64;

/// Column value type. `stride` is derived from this (or given explicitly for [`LogicalType::Bytes`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogicalType {
  U8,
  U32,
  U64,
  F32,
  /// Fixed-width opaque bytes (e.g. a 1-bit RaBitQ code row) — stride carried separately.
  Bytes,
}

impl LogicalType {
  pub(crate) fn tag(self) -> u8 {
    match self {
      LogicalType::U8 => 1,
      LogicalType::U32 => 2,
      LogicalType::U64 => 3,
      LogicalType::F32 => 4,
      LogicalType::Bytes => 5,
    }
  }

  pub(crate) fn from_tag(tag: u8) -> Option<Self> {
    Some(match tag {
      1 => LogicalType::U8,
      2 => LogicalType::U32,
      3 => LogicalType::U64,
      4 => LogicalType::F32,
      5 => LogicalType::Bytes,
      _ => return None,
    })
  }
}

/// Where a column lives. Only [`ColumnPlacement::Hot`] is implemented today (§9.1); WARM/COLD are
/// reserved for the codec layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnPlacement {
  Hot,
  Warm,
  Cold,
}

impl ColumnPlacement {
  pub(crate) fn tag(self) -> u8 {
    match self {
      ColumnPlacement::Hot => 0,
      ColumnPlacement::Warm => 1,
      ColumnPlacement::Cold => 2,
    }
  }
}

/// A parsed column-directory entry (64 bytes on disk).
#[derive(Debug, Clone, Copy)]
pub(crate) struct ColumnDesc {
  pub name_hash: u64,
  pub type_tag: u8,
  pub placement: u8,
  pub stride: u32,
  pub data_offset: u64,
  pub data_len: u64,
  pub xxh3: u64,
  /// Zone-map min/max, raw native bytes of the value type (zeroed for `Bytes`).
  pub min: [u8; 8],
  pub max: [u8; 8],
}

impl ColumnDesc {
  pub(crate) fn encode(&self, buf: &mut [u8], off: usize) {
    put_u64(buf, off, self.name_hash);
    buf[off + 8] = self.type_tag;
    buf[off + 9] = self.placement;
    // off+10..12 reserved
    put_u32(buf, off + 12, self.stride);
    put_u64(buf, off + 16, self.data_offset);
    put_u64(buf, off + 24, self.data_len);
    put_u64(buf, off + 32, self.xxh3);
    buf[off + 40..off + 48].copy_from_slice(&self.min);
    buf[off + 48..off + 56].copy_from_slice(&self.max);
    // off+56..64 reserved
  }

  pub(crate) fn decode(buf: &[u8], off: usize) -> Self {
    let mut min = [0u8; 8];
    let mut max = [0u8; 8];
    min.copy_from_slice(&buf[off + 40..off + 48]);
    max.copy_from_slice(&buf[off + 48..off + 56]);
    Self {
      name_hash: get_u64(buf, off),
      type_tag: buf[off + 8],
      placement: buf[off + 9],
      stride: get_u32(buf, off + 12),
      data_offset: get_u64(buf, off + 16),
      data_len: get_u64(buf, off + 24),
      xxh3: get_u64(buf, off + 32),
      min,
      max,
    }
  }
}

/// Parsed header fields.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Header {
  pub row_count: u64,
  pub logical_id_base: u64,
  pub column_count: u32,
  pub column_dir_offset: u64,
  pub column_dir_len: u64,
  pub footer_offset: u64,
}

impl Header {
  pub(crate) fn parse(bytes: &[u8]) -> Result<Self, SegmentError> {
    if bytes.len() < HEADER_LEN + FOOTER_LEN {
      return Err(SegmentError::TooSmall(bytes.len()));
    }
    if &bytes[0..8] != MAGIC {
      return Err(SegmentError::BadMagic);
    }
    let version = get_u32(bytes, 8);
    if version != FORMAT_VERSION {
      return Err(SegmentError::BadVersion(version));
    }
    let header = Header {
      row_count: get_u64(bytes, 16),
      logical_id_base: get_u64(bytes, 24),
      column_count: get_u32(bytes, 32),
      column_dir_offset: get_u64(bytes, 36),
      column_dir_len: get_u64(bytes, 44),
      footer_offset: get_u64(bytes, 52),
    };
    header.validate(bytes.len())?;
    Ok(header)
  }

  fn validate(&self, len: usize) -> Result<(), SegmentError> {
    let footer_end = self
      .footer_offset
      .checked_add(FOOTER_LEN as u64)
      .ok_or(SegmentError::Corrupt("footer offset overflow"))?;
    if footer_end > len as u64 {
      return Err(SegmentError::Corrupt("footer past end"));
    }
    let dir_end = self
      .column_dir_offset
      .checked_add(self.column_dir_len)
      .ok_or(SegmentError::Corrupt("dir offset overflow"))?;
    if self.column_dir_offset < HEADER_LEN as u64 || dir_end > self.footer_offset {
      return Err(SegmentError::Corrupt("directory out of bounds"));
    }
    if self.column_dir_len != self.column_count as u64 * DIR_ENTRY_LEN as u64 {
      return Err(SegmentError::Corrupt("directory length mismatch"));
    }
    Ok(())
  }
}

pub(crate) fn write_header(buf: &mut [u8], header: &Header) {
  buf[0..8].copy_from_slice(MAGIC);
  put_u32(buf, 8, FORMAT_VERSION);
  put_u32(buf, 12, 0); // flags
  put_u64(buf, 16, header.row_count);
  put_u64(buf, 24, header.logical_id_base);
  put_u32(buf, 32, header.column_count);
  put_u64(buf, 36, header.column_dir_offset);
  put_u64(buf, 44, header.column_dir_len);
  put_u64(buf, 52, header.footer_offset);
}

#[inline]
pub(crate) fn align_up(v: u64, align: u64) -> u64 {
  (v + align - 1) & !(align - 1)
}

pub(crate) fn put_u32(buf: &mut [u8], off: usize, v: u32) {
  buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

pub(crate) fn put_u64(buf: &mut [u8], off: usize, v: u64) {
  buf[off..off + 8].copy_from_slice(&v.to_le_bytes());
}

pub(crate) fn get_u32(buf: &[u8], off: usize) -> u32 {
  u32::from_le_bytes(buf[off..off + 4].try_into().expect("4 bytes"))
}

pub(crate) fn get_u64(buf: &[u8], off: usize) -> u64 {
  u64::from_le_bytes(buf[off..off + 8].try_into().expect("8 bytes"))
}
