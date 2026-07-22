//! The length-delimited frame layer: a 16-byte explicit-little-endian header followed by an opaque
//! payload (a postcard-encoded [`crate::msg::Message`] or raw bulk bytes). Runs over any byte pipe
//! — SSH stdout, a k8s exec WebSocket, a docker attach stream, a vsock socket.
//!
//! Header layout (all little-endian, no padding, 16 bytes total):
//! ```text
//!   0  magic:u16   2  version:u8  3  flags:u8
//!   4  channel:u16 6  msg_type:u16
//!   8  len:u32     12 checksum:u32
//! ```
//! `channel` multiplexes independent streams (0 = control, N = per-shard result stream).
//! `msg_type` mirrors the message discriminant so a receiver can skip a frame it does not care
//! about — or a future one it cannot decode — purely by `len`, without decoding the body.

use crate::hash::checksum32;

/// `"VP"` in little-endian bytes.
pub const MAGIC: u16 = 0x5650;
/// Protocol major version. A mismatch is rejected outright (no partial-compat matrix).
pub const PROTOCOL_VERSION: u8 = 1;
/// Bytes in the fixed frame header.
pub const FRAME_HEADER_LEN: usize = 16;
/// Default per-frame ceiling used until a smaller `Caps.max_frame` is negotiated (64 MiB).
pub const DEFAULT_MAX_FRAME: u32 = 64 * 1024 * 1024;

/// Frame header flag bits.
pub mod flag {
  /// Payload is zstd-compressed.
  pub const ZSTD: u8 = 1 << 0;
  /// Payload is a structured record (vs. an opaque rendered fragment).
  pub const STRUCTURED: u8 = 1 << 1;
  /// A payload checksum is present and must be verified.
  pub const CHECKSUM: u8 = 1 << 2;
}

/// Errors from framing. `Incomplete` is a normal control-flow signal for a streaming reader (feed
/// more bytes and retry), not a fault.
#[derive(Debug, thiserror::Error)]
pub enum WireError {
  #[error("incomplete frame: have {have} bytes, need {need}")]
  Incomplete { have: usize, need: usize },
  #[error("bad magic {0:#06x} (not a vorpal frame)")]
  BadMagic(u16),
  #[error("unsupported protocol version {0} (this build speaks {PROTOCOL_VERSION})")]
  BadVersion(u8),
  #[error("frame length {len} exceeds negotiated ceiling {max}")]
  TooLarge { len: u32, max: u32 },
  #[error("payload checksum mismatch (corruption or truncation)")]
  Checksum,
  #[error("message codec: {0}")]
  Codec(String),
  #[error("wire io: {0}")]
  Io(String),
}

/// The decoded fixed header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
  pub flags: u8,
  pub channel: u16,
  pub msg_type: u16,
  pub len: u32,
  pub checksum: u32,
}

impl FrameHeader {
  /// Append the 16-byte little-endian encoding of this header to `out`.
  pub fn write_bytes(&self, out: &mut Vec<u8>) {
    out.extend_from_slice(&MAGIC.to_le_bytes());
    out.push(PROTOCOL_VERSION);
    out.push(self.flags);
    out.extend_from_slice(&self.channel.to_le_bytes());
    out.extend_from_slice(&self.msg_type.to_le_bytes());
    out.extend_from_slice(&self.len.to_le_bytes());
    out.extend_from_slice(&self.checksum.to_le_bytes());
  }

  /// Decode a header from the first 16 bytes of `buf`, validating magic and protocol version.
  pub fn read_bytes(buf: &[u8]) -> Result<FrameHeader, WireError> {
    if buf.len() < FRAME_HEADER_LEN {
      return Err(WireError::Incomplete { have: buf.len(), need: FRAME_HEADER_LEN });
    }
    let magic = u16::from_le_bytes([buf[0], buf[1]]);
    if magic != MAGIC {
      return Err(WireError::BadMagic(magic));
    }
    let version = buf[2];
    if version != PROTOCOL_VERSION {
      return Err(WireError::BadVersion(version));
    }
    Ok(FrameHeader {
      flags: buf[3],
      channel: u16::from_le_bytes([buf[4], buf[5]]),
      msg_type: u16::from_le_bytes([buf[6], buf[7]]),
      len: u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]),
      checksum: u32::from_le_bytes([buf[12], buf[13], buf[14], buf[15]]),
    })
  }
}

/// One decoded frame: routing metadata plus the owned payload bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
  pub channel: u16,
  pub msg_type: u16,
  pub flags: u8,
  pub payload: Vec<u8>,
}

impl Frame {
  /// A checksummed frame carrying `payload`.
  pub fn new(channel: u16, msg_type: u16, payload: Vec<u8>) -> Self {
    Self { channel, msg_type, flags: flag::CHECKSUM, payload }
  }

  /// Append the encoded frame (header + payload) to `out`.
  ///
  /// The payload must fit the header's `u32` length field; [`crate::io::FrameWriter`] enforces
  /// the negotiated ceiling (well below `u32::MAX`) before reaching this encoding.
  pub fn write(&self, out: &mut Vec<u8>) {
    debug_assert!(
      self.payload.len() <= u32::MAX as usize,
      "frame payload exceeds the u32 length field; enforce a ceiling before encoding"
    );
    let checksum =
      if self.flags & flag::CHECKSUM != 0 { checksum32(&self.payload) } else { 0 };
    let header = FrameHeader {
      flags: self.flags,
      channel: self.channel,
      msg_type: self.msg_type,
      len: self.payload.len() as u32,
      checksum,
    };
    header.write_bytes(out);
    out.extend_from_slice(&self.payload);
  }

  /// The encoded frame as a fresh buffer.
  pub fn to_vec(&self) -> Vec<u8> {
    let mut out = Vec::with_capacity(FRAME_HEADER_LEN + self.payload.len());
    self.write(&mut out);
    out
  }
}

/// Parse one frame from the front of `buf`, returning it and the number of bytes it consumed.
///
/// Resource-safety (a node is authenticated but not trusted): the declared `len` is checked against
/// `max_len` **before** any allocation, so an oversized length is rejected rather than reserved.
/// Returns [`WireError::Incomplete`] when `buf` does not yet hold the whole frame — the caller
/// should read more bytes and retry from the same offset.
pub fn parse_frame(buf: &[u8], max_len: u32) -> Result<(Frame, usize), WireError> {
  let header = FrameHeader::read_bytes(buf)?;
  if header.len > max_len {
    return Err(WireError::TooLarge { len: header.len, max: max_len });
  }
  let total = FRAME_HEADER_LEN + header.len as usize;
  if buf.len() < total {
    return Err(WireError::Incomplete { have: buf.len(), need: total });
  }
  let payload = &buf[FRAME_HEADER_LEN..total];
  if header.flags & flag::CHECKSUM != 0 && checksum32(payload) != header.checksum {
    return Err(WireError::Checksum);
  }
  let frame = Frame {
    channel: header.channel,
    msg_type: header.msg_type,
    flags: header.flags,
    payload: payload.to_vec(),
  };
  Ok((frame, total))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn frame_round_trips() {
    let f = Frame::new(7, 42, b"hello vorpal".to_vec());
    let bytes = f.to_vec();
    assert_eq!(bytes.len(), FRAME_HEADER_LEN + f.payload.len());
    let (got, consumed) = parse_frame(&bytes, DEFAULT_MAX_FRAME).unwrap();
    assert_eq!(consumed, bytes.len());
    assert_eq!(got, f);
  }

  #[test]
  fn parses_first_of_two_and_reports_consumed() {
    let a = Frame::new(0, 1, b"aaa".to_vec());
    let b = Frame::new(1, 2, b"bbbb".to_vec());
    let mut stream = a.to_vec();
    stream.extend_from_slice(&b.to_vec());
    let (got_a, n) = parse_frame(&stream, DEFAULT_MAX_FRAME).unwrap();
    assert_eq!(got_a, a);
    let (got_b, _) = parse_frame(&stream[n..], DEFAULT_MAX_FRAME).unwrap();
    assert_eq!(got_b, b);
  }

  #[test]
  fn incomplete_header_and_body_are_signalled() {
    let f = Frame::new(0, 1, b"payload".to_vec());
    let bytes = f.to_vec();
    // Short of a full header.
    assert!(matches!(parse_frame(&bytes[..8], DEFAULT_MAX_FRAME), Err(WireError::Incomplete { .. })));
    // Full header, truncated body.
    assert!(matches!(
      parse_frame(&bytes[..FRAME_HEADER_LEN + 2], DEFAULT_MAX_FRAME),
      Err(WireError::Incomplete { .. })
    ));
  }

  #[test]
  fn oversized_len_is_rejected_before_allocation() {
    // A hostile header claims a 4 GiB payload but only the header is present. Must be TooLarge,
    // never an attempt to reserve the claimed length.
    let mut buf = Vec::new();
    FrameHeader { flags: 0, channel: 0, msg_type: 1, len: u32::MAX, checksum: 0 }.write_bytes(&mut buf);
    assert!(matches!(parse_frame(&buf, 1024), Err(WireError::TooLarge { .. })));
  }

  #[test]
  fn corrupted_payload_fails_checksum() {
    let f = Frame::new(0, 1, b"important".to_vec());
    let mut bytes = f.to_vec();
    let last = bytes.len() - 1;
    bytes[last] ^= 0xFF; // flip a payload bit
    assert!(matches!(parse_frame(&bytes, DEFAULT_MAX_FRAME), Err(WireError::Checksum)));
  }

  #[test]
  fn bad_magic_and_version_are_rejected() {
    let f = Frame::new(0, 1, b"x".to_vec());
    let mut bytes = f.to_vec();
    bytes[0] = 0x00;
    assert!(matches!(parse_frame(&bytes, DEFAULT_MAX_FRAME), Err(WireError::BadMagic(_))));
    let mut bytes = f.to_vec();
    bytes[2] = 0xFE;
    assert!(matches!(parse_frame(&bytes, DEFAULT_MAX_FRAME), Err(WireError::BadVersion(_))));
  }
}
