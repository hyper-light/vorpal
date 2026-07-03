//! Segment error type.

/// Errors from building, parsing, or verifying a `.vseg` segment.
#[derive(Debug, thiserror::Error)]
pub enum SegmentError {
  #[error("segment too small: {0} bytes")]
  TooSmall(usize),
  #[error("bad segment magic")]
  BadMagic,
  #[error("bad footer magic")]
  BadFooterMagic,
  #[error("unsupported segment format version {0}")]
  BadVersion(u32),
  #[error("corrupt segment: {0}")]
  Corrupt(&'static str),
  #[error("header blake3 mismatch (torn write or corruption)")]
  HeaderHash,
  #[error("segment blake3 mismatch (torn write or corruption)")]
  SegmentHash,
  #[error("column 0x{0:016x} xxh3 mismatch")]
  ColumnHash(u64),
  #[error("cannot build an empty segment (no columns)")]
  Empty,
  #[error("column row count mismatch: got {got}, expected {expected}")]
  RowMismatch { expected: u64, got: u64 },
  #[error("raw column length {len} is not a multiple of stride {stride}")]
  RaggedColumn { len: usize, stride: u32 },
  #[error(transparent)]
  Io(#[from] std::io::Error),
}
