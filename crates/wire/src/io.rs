//! Blocking frame transport over any `Read`/`Write` byte pipe (agent stdio, subprocess pipes).
//!
//! The read path is resource-safe against a malformed or hostile peer: the declared length is
//! validated against the negotiated ceiling **before** any allocation (§1), and the payload
//! checksum is verified before a frame is surfaced.

use std::io::{self, Read, Write};

use crate::frame::{FRAME_HEADER_LEN, Frame, FrameHeader, WireError, flag};
use crate::hash::checksum32;
use crate::msg::Message;

/// Reads length-delimited frames from a byte stream.
pub struct FrameReader<R: Read> {
  reader: R,
  /// Per-frame ceiling (from negotiated `Caps.max_frame`).
  max_frame: u32,
}

impl<R: Read> FrameReader<R> {
  pub fn new(reader: R, max_frame: u32) -> Self {
    Self { reader, max_frame }
  }

  /// Read the next frame. Returns `Ok(None)` on a clean EOF **at a frame boundary**; EOF inside a
  /// frame is an error (truncation is never silently dropped).
  pub fn read_frame(&mut self) -> Result<Option<Frame>, WireError> {
    let mut header_buf = [0u8; FRAME_HEADER_LEN];
    // Distinguish clean EOF (zero bytes) from mid-header truncation.
    let mut filled = 0usize;
    while filled < FRAME_HEADER_LEN {
      match self.reader.read(&mut header_buf[filled..]) {
        Ok(0) => {
          if filled == 0 {
            return Ok(None);
          }
          return Err(WireError::Incomplete { have: filled, need: FRAME_HEADER_LEN });
        }
        Ok(n) => filled += n,
        Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
        Err(e) => return Err(WireError::Io(e.to_string())),
      }
    }
    let header = FrameHeader::read_bytes(&header_buf)?;
    if header.len > self.max_frame {
      return Err(WireError::TooLarge { len: header.len, max: self.max_frame });
    }
    let mut payload = vec![0u8; header.len as usize];
    self.reader.read_exact(&mut payload).map_err(|e| WireError::Io(e.to_string()))?;
    if header.flags & flag::CHECKSUM != 0 && checksum32(&payload) != header.checksum {
      return Err(WireError::Checksum);
    }
    Ok(Some(Frame {
      channel: header.channel,
      msg_type: header.msg_type,
      flags: header.flags,
      payload,
    }))
  }

  /// Read the next frame and decode it as a [`Message`]. Unknown `msg_type`s decode-fail; callers
  /// that want skip-by-len forward-compat should use [`FrameReader::read_frame`] and route on
  /// `msg_type` themselves.
  pub fn read_message(&mut self) -> Result<Option<(u16, Message)>, WireError> {
    match self.read_frame()? {
      None => Ok(None),
      Some(frame) => {
        let msg = Message::decode(&frame.payload)?;
        Ok(Some((frame.channel, msg)))
      }
    }
  }
}

/// Writes frames to a byte stream, flushing per frame so results stream live.
///
/// The writer enforces the same per-frame ceiling the peer's reader will apply: an oversized
/// payload fails **here**, with a clear error, instead of being written and then killing the
/// session at the receiver (or, past `u32::MAX`, silently truncating the length field and
/// desynchronizing the stream into checksum/magic errors).
pub struct FrameWriter<W: Write> {
  writer: W,
  scratch: Vec<u8>,
  /// Per-frame ceiling (the peer's negotiated `Caps.max_frame`).
  max_frame: u32,
}

impl<W: Write> FrameWriter<W> {
  pub fn new(writer: W) -> Self {
    Self::with_max_frame(writer, crate::DEFAULT_MAX_FRAME)
  }

  pub fn with_max_frame(writer: W, max_frame: u32) -> Self {
    Self { writer, scratch: Vec::with_capacity(4096), max_frame }
  }

  pub fn write_frame(&mut self, frame: &Frame) -> Result<(), WireError> {
    if frame.payload.len() > self.max_frame as usize {
      return Err(WireError::TooLarge {
        len: u32::try_from(frame.payload.len()).unwrap_or(u32::MAX),
        max: self.max_frame,
      });
    }
    self.scratch.clear();
    frame.write(&mut self.scratch);
    self.writer.write_all(&self.scratch).map_err(|e| WireError::Io(e.to_string()))?;
    self.writer.flush().map_err(|e| WireError::Io(e.to_string()))
  }

  pub fn write_message(&mut self, channel: u16, msg: &Message) -> Result<(), WireError> {
    let frame = msg.to_frame(channel)?;
    self.write_frame(&frame)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::msg::{Control, Message};

  #[test]
  fn frames_round_trip_over_a_pipe_like_buffer() {
    let mut wire = Vec::new();
    {
      let mut w = FrameWriter::new(&mut wire);
      w.write_message(0, &Message::Control(Control::Ping { seq: 1 })).unwrap();
      w.write_message(3, &Message::Control(Control::Pong { seq: 1 })).unwrap();
    }
    let mut r = FrameReader::new(wire.as_slice(), crate::DEFAULT_MAX_FRAME);
    let (ch1, m1) = r.read_message().unwrap().unwrap();
    assert_eq!(ch1, 0);
    assert_eq!(m1, Message::Control(Control::Ping { seq: 1 }));
    let (ch2, m2) = r.read_message().unwrap().unwrap();
    assert_eq!(ch2, 3);
    assert_eq!(m2, Message::Control(Control::Pong { seq: 1 }));
    assert!(r.read_message().unwrap().is_none(), "clean EOF at boundary");
  }

  #[test]
  fn truncated_stream_is_an_error_not_a_silent_eof() {
    let mut wire = Vec::new();
    FrameWriter::new(&mut wire)
      .write_message(0, &Message::Control(Control::Ping { seq: 9 }))
      .unwrap();
    // Cut inside the header.
    let mut r = FrameReader::new(&wire[..7], crate::DEFAULT_MAX_FRAME);
    assert!(matches!(r.read_frame(), Err(WireError::Incomplete { .. })));
    // Cut inside the payload.
    let mut r = FrameReader::new(&wire[..wire.len() - 1], crate::DEFAULT_MAX_FRAME);
    assert!(matches!(r.read_frame(), Err(WireError::Io(_))));
  }

  #[test]
  fn oversized_payload_is_rejected_at_the_writer_not_the_peer() {
    let mut wire = Vec::new();
    let mut w = FrameWriter::with_max_frame(&mut wire, 16);
    let frame = crate::frame::Frame::new(0, 1, vec![0u8; 17]);
    assert!(matches!(w.write_frame(&frame), Err(WireError::TooLarge { len: 17, max: 16 })));
    assert!(wire.is_empty(), "nothing may reach the stream after a size rejection");
    // At the ceiling exactly, the frame goes through.
    let mut w = FrameWriter::with_max_frame(&mut wire, 16);
    w.write_frame(&crate::frame::Frame::new(0, 1, vec![0u8; 16])).unwrap();
  }

  #[test]
  fn oversized_frame_is_rejected_before_payload_allocation() {
    let mut hostile = Vec::new();
    crate::frame::FrameHeader {
      flags: 0,
      channel: 0,
      msg_type: 1,
      len: u32::MAX,
      checksum: 0,
    }
    .write_bytes(&mut hostile);
    let mut r = FrameReader::new(hostile.as_slice(), 1024);
    assert!(matches!(r.read_frame(), Err(WireError::TooLarge { len: u32::MAX, max: 1024 })));
  }
}
