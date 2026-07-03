//! Reads and verifies an immutable `.vseg` segment, with O(1) HOT-column point access (§9.1).

use std::path::Path;

use vorpal_mem::{AccessPattern, Hotness, MappedStore, ResourcePolicy, StoreKind};
use xxhash_rust::xxh3::xxh3_64;

use crate::error::SegmentError;
use crate::format::{
  ColumnDesc, DIR_ENTRY_LEN, FOOTER_MAGIC, HEADER_HASH_OFF, HEADER_LEN, Header, LogicalType,
};
use crate::id::NodeId;

enum Backing {
  Owned(Vec<u8>),
  Mapped(MappedStore),
}

impl Backing {
  fn bytes(&self) -> &[u8] {
    match self {
      Backing::Owned(v) => v,
      Backing::Mapped(m) => m.as_bytes(),
    }
  }
}

/// An opened, parsed segment. Metadata (header + column directory) is resident; column payloads
/// stay in the mmap / buffer and are accessed zero-copy.
pub struct Segment {
  backing: Backing,
  header: Header,
  columns: Vec<ColumnDesc>,
}

impl Segment {
  /// Open a segment from an owned byte buffer (in-RAM build or test).
  pub fn open_owned(bytes: Vec<u8>) -> Result<Self, SegmentError> {
    let (header, columns) = Self::parse(&bytes)?;
    Ok(Self {
      backing: Backing::Owned(bytes),
      header,
      columns,
    })
  }

  /// Open a segment from an adaptive mmap ([`vorpal_mem::MappedStore`]).
  pub fn open_mapped(store: MappedStore) -> Result<Self, SegmentError> {
    let (header, columns) = Self::parse(store.as_bytes())?;
    Ok(Self {
      backing: Backing::Mapped(store),
      header,
      columns,
    })
  }

  /// Open a segment file, mmapped hot + random-access under the given policy (§8.2).
  pub fn open_file(path: &Path, policy: &ResourcePolicy) -> Result<Self, SegmentError> {
    let store = MappedStore::map_file(
      path,
      StoreKind::NodesHot,
      AccessPattern::Random,
      Hotness::Hot,
      policy,
    )?;
    Self::open_mapped(store)
  }

  fn parse(bytes: &[u8]) -> Result<(Header, Vec<ColumnDesc>), SegmentError> {
    let header = Header::parse(bytes)?;
    let mut columns = Vec::with_capacity(header.column_count as usize);
    for i in 0..header.column_count as usize {
      let off = header.column_dir_offset as usize + i * DIR_ENTRY_LEN;
      let desc = ColumnDesc::decode(bytes, off);
      // Bounds so later slicing is infallible.
      let end = desc
        .data_offset
        .checked_add(desc.data_len)
        .ok_or(SegmentError::Corrupt("column extent overflow"))?;
      if desc.data_offset < HEADER_LEN as u64 || end > header.footer_offset {
        return Err(SegmentError::Corrupt("column data out of bounds"));
      }
      if desc.stride != 0 && desc.data_len != desc.stride as u64 * header.row_count {
        return Err(SegmentError::Corrupt("column data_len != stride*rows"));
      }
      columns.push(desc);
    }
    Ok((header, columns))
  }

  pub fn row_count(&self) -> u64 {
    self.header.row_count
  }

  pub fn logical_id_base(&self) -> u64 {
    self.header.logical_id_base
  }

  pub fn column_count(&self) -> usize {
    self.columns.len()
  }

  pub fn bytes(&self) -> &[u8] {
    self.backing.bytes()
  }

  /// The row index for `id` within this segment, if it belongs here (§9.2).
  pub fn contains_id(&self, id: NodeId) -> Option<u64> {
    id.local_row(self.header.logical_id_base, self.header.row_count)
  }

  /// A zero-copy view of the named HOT column, if present.
  pub fn column(&self, name: &str) -> Option<ColumnView<'_>> {
    let want = xxh3_64(name.as_bytes());
    let desc = self.columns.iter().find(|d| d.name_hash == want)?;
    let bytes = self.backing.bytes();
    let start = desc.data_offset as usize;
    let data = &bytes[start..start + desc.data_len as usize];
    Some(ColumnView {
      data,
      desc,
      row_count: self.header.row_count,
    })
  }

  /// Full torn-write / corruption gate for cold open (§9.7): header blake3 + whole-segment
  /// blake3-256. The segment hash covers every column, so it catches any corruption; it is the
  /// expensive-but-total check. Use [`Segment::verify_column_checksums`] when hashing a
  /// multi-GB segment in full is too costly.
  pub fn verify(&self) -> Result<(), SegmentError> {
    let bytes = self.backing.bytes();

    if blake3::hash(&bytes[0..HEADER_HASH_OFF]).as_bytes() != &bytes[HEADER_HASH_OFF..HEADER_LEN] {
      return Err(SegmentError::HeaderHash);
    }

    let foff = self.header.footer_offset as usize;
    if &bytes[foff..foff + 8] != FOOTER_MAGIC {
      return Err(SegmentError::BadFooterMagic);
    }
    if blake3::hash(&bytes[0..foff]).as_bytes() != &bytes[foff + 8..foff + 40] {
      return Err(SegmentError::SegmentHash);
    }
    Ok(())
  }

  /// The cheap per-column integrity path (§9.7 "per-block xxh3-64, decode fast path"): validates
  /// each column's xxh3 without the whole-segment blake3. Pinpoints the offending column.
  pub fn verify_column_checksums(&self) -> Result<(), SegmentError> {
    let bytes = self.backing.bytes();
    for desc in &self.columns {
      let start = desc.data_offset as usize;
      let data = &bytes[start..start + desc.data_len as usize];
      if xxh3_64(data) != desc.xxh3 {
        return Err(SegmentError::ColumnHash(desc.name_hash));
      }
    }
    Ok(())
  }
}

/// A zero-copy view over one HOT column's bytes.
pub struct ColumnView<'a> {
  data: &'a [u8],
  desc: &'a ColumnDesc,
  row_count: u64,
}

impl<'a> ColumnView<'a> {
  pub fn logical_type(&self) -> Option<LogicalType> {
    LogicalType::from_tag(self.desc.type_tag)
  }

  pub fn stride(&self) -> u32 {
    self.desc.stride
  }

  pub fn row_count(&self) -> u64 {
    self.row_count
  }

  /// Raw bytes of one row (`base + row·stride`) — the O(1), one-cache-line point lookup.
  pub fn row_bytes(&self, row: u64) -> Option<&'a [u8]> {
    if row >= self.row_count {
      return None;
    }
    let stride = self.desc.stride as usize;
    let start = row as usize * stride;
    Some(&self.data[start..start + stride])
  }

  pub fn get_u8(&self, row: u64) -> Option<u8> {
    self.row_bytes(row).map(|b| b[0])
  }

  pub fn get_u32(&self, row: u64) -> Option<u32> {
    self.row_bytes(row).map(bytemuck::pod_read_unaligned::<u32>)
  }

  pub fn get_u64(&self, row: u64) -> Option<u64> {
    self.row_bytes(row).map(bytemuck::pod_read_unaligned::<u64>)
  }

  /// Zero-copy typed slice of the whole column, when alignment permits (native-endian).
  pub fn as_slice<T: bytemuck::Pod>(&self) -> Option<&'a [T]> {
    let len = self.row_count as usize * size_of::<T>();
    bytemuck::try_cast_slice(&self.data[..len]).ok()
  }

  /// Fast per-column integrity check (xxh3) — the decode-time fast path (§9.7).
  pub fn verify(&self) -> bool {
    xxh3_64(self.data) == self.desc.xxh3
  }
}
