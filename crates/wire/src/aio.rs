//! Async frame transport over any tokio byte stream (an SSH channel, a k8s exec WebSocket, a
//! docker attach stream, a subprocess pipe). The mirror of [`crate::io`] for the coordinator's
//! async transport layer — the grammar-sliced agent stays on the blocking `io` module, so this is
//! feature-gated (`tokio`) and never pulled into the pushed binary.
//!
//! Same resource-safety guarantees as the blocking reader: the declared length is validated
//! against the negotiated ceiling **before** any allocation, and the payload checksum is verified
//! before a frame is surfaced.

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::frame::{FRAME_HEADER_LEN, Frame, FrameHeader, WireError, flag};
use crate::hash::checksum32;
use crate::msg::Message;

/// Reads length-delimited frames from an async byte stream.
pub struct AsyncFrameReader<R> {
  reader: R,
  max_frame: u32,
}

impl<R: AsyncRead + Unpin> AsyncFrameReader<R> {
  pub fn new(reader: R, max_frame: u32) -> Self {
    Self { reader, max_frame }
  }

  /// Consume the reader, returning the underlying stream (e.g. to reuse the channel).
  pub fn into_inner(self) -> R {
    self.reader
  }

  /// Read the next frame. `Ok(None)` on a clean EOF **at a frame boundary**; EOF inside a frame is
  /// an error (truncation is never silently dropped).
  pub async fn read_frame(&mut self) -> Result<Option<Frame>, WireError> {
    let mut header_buf = [0u8; FRAME_HEADER_LEN];
    let mut filled = 0usize;
    while filled < FRAME_HEADER_LEN {
      let n = self
        .reader
        .read(&mut header_buf[filled..])
        .await
        .map_err(|e| WireError::Io(e.to_string()))?;
      if n == 0 {
        if filled == 0 {
          return Ok(None);
        }
        return Err(WireError::Incomplete { have: filled, need: FRAME_HEADER_LEN });
      }
      filled += n;
    }
    let header = FrameHeader::read_bytes(&header_buf)?;
    if header.len > self.max_frame {
      return Err(WireError::TooLarge { len: header.len, max: self.max_frame });
    }
    let mut payload = vec![0u8; header.len as usize];
    self.reader.read_exact(&mut payload).await.map_err(|e| WireError::Io(e.to_string()))?;
    if header.flags & flag::CHECKSUM != 0 && checksum32(&payload) != header.checksum {
      return Err(WireError::Checksum);
    }
    Ok(Some(Frame { channel: header.channel, msg_type: header.msg_type, flags: header.flags, payload }))
  }

  /// Read the next frame and decode it as a [`Message`].
  pub async fn read_message(&mut self) -> Result<Option<(u16, Message)>, WireError> {
    match self.read_frame().await? {
      None => Ok(None),
      Some(frame) => Ok(Some((frame.channel, Message::decode(&frame.payload)?))),
    }
  }
}

/// Writes frames to an async byte stream, flushing per frame so results stream live.
pub struct AsyncFrameWriter<W> {
  writer: W,
  scratch: Vec<u8>,
  max_frame: u32,
}

impl<W: AsyncWrite + Unpin> AsyncFrameWriter<W> {
  pub fn new(writer: W) -> Self {
    Self::with_max_frame(writer, crate::DEFAULT_MAX_FRAME)
  }

  pub fn with_max_frame(writer: W, max_frame: u32) -> Self {
    Self { writer, scratch: Vec::with_capacity(4096), max_frame }
  }

  pub fn into_inner(self) -> W {
    self.writer
  }

  pub async fn write_frame(&mut self, frame: &Frame) -> Result<(), WireError> {
    if frame.payload.len() > self.max_frame as usize {
      return Err(WireError::TooLarge {
        len: u32::try_from(frame.payload.len()).unwrap_or(u32::MAX),
        max: self.max_frame,
      });
    }
    self.scratch.clear();
    frame.write(&mut self.scratch);
    self.writer.write_all(&self.scratch).await.map_err(|e| WireError::Io(e.to_string()))?;
    self.writer.flush().await.map_err(|e| WireError::Io(e.to_string()))
  }

  pub async fn write_message(&mut self, channel: u16, msg: &Message) -> Result<(), WireError> {
    let frame = msg.to_frame(channel)?;
    self.write_frame(&frame).await
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::msg::{Control, Message};

  // A `DuplexStream` cross-connects the two ends (write on one → read on the other) and is both
  // `AsyncRead` and `AsyncWrite`, so the tests use whole ends (no `split`, whose shared handle
  // would keep the stream alive and never signal EOF): write on `server`, read on `client`.

  #[tokio::test]
  async fn frames_round_trip_over_a_duplex() {
    let (client, server) = tokio::io::duplex(4096);
    let mut writer = AsyncFrameWriter::new(server);
    let mut reader = AsyncFrameReader::new(client, crate::DEFAULT_MAX_FRAME);
    writer.write_message(0, &Message::Control(Control::Ping { seq: 7 })).await.unwrap();
    writer.write_message(3, &Message::Control(Control::Pong { seq: 7 })).await.unwrap();
    let (ch1, m1) = reader.read_message().await.unwrap().unwrap();
    assert_eq!(ch1, 0);
    assert_eq!(m1, Message::Control(Control::Ping { seq: 7 }));
    let (ch2, m2) = reader.read_message().await.unwrap().unwrap();
    assert_eq!(ch2, 3);
    assert_eq!(m2, Message::Control(Control::Pong { seq: 7 }));
  }

  #[tokio::test]
  async fn clean_eof_at_boundary_is_none() {
    let (client, server) = tokio::io::duplex(4096);
    {
      let mut writer = AsyncFrameWriter::new(server);
      writer.write_message(0, &Message::Control(Control::Ping { seq: 1 })).await.unwrap();
      // `server` (the whole write end) is dropped here → the read end sees EOF after one frame.
    }
    let mut reader = AsyncFrameReader::new(client, crate::DEFAULT_MAX_FRAME);
    assert!(reader.read_message().await.unwrap().is_some());
    assert!(reader.read_message().await.unwrap().is_none(), "clean EOF at boundary");
  }

  #[tokio::test]
  async fn oversized_frame_is_rejected_before_payload_allocation() {
    let (client, mut server) = tokio::io::duplex(64);
    // Hand-write a header claiming a 4 GiB payload.
    let mut hostile = Vec::new();
    crate::frame::FrameHeader { flags: 0, channel: 0, msg_type: 1, len: u32::MAX, checksum: 0 }
      .write_bytes(&mut hostile);
    server.write_all(&hostile).await.unwrap();
    server.flush().await.unwrap();
    let mut reader = AsyncFrameReader::new(client, 1024);
    assert!(matches!(reader.read_frame().await, Err(WireError::TooLarge { len: u32::MAX, max: 1024 })));
  }
}
