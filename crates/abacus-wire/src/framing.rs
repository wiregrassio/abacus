//! Length-prefixed framing over a Unix stream.
//!
//! `FrameReader` is the non-blocking receive path: it accumulates whatever bytes are available
//! and yields a frame only once the whole frame is present, so a peer that sends part of a
//! frame and pauses costs the reader nothing but buffer space. `read_exact` and `write_all`
//! are the blocking paths for a client talking to the daemon.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;

use abacus_core::error::{IoOperation, ProtocolFault, TransportError};

use crate::codec::{LENGTH_PREFIX_BYTES, MAX_FRAME_BYTES, MAX_MESSAGE_SIZE};

/// What one non-blocking read into a `FrameReader` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadOutcome {
    /// Bytes were appended to the buffer.
    Progress,
    /// Nothing was available; try again after the next poll.
    WouldBlock,
}

/// Per-connection receive buffer. Bounded at one maximum frame.
#[derive(Debug, Default)]
pub struct FrameReader {
    buf: Vec<u8>,
}

impl FrameReader {
    /// An empty reader.
    pub fn new() -> Self {
        Self {
            buf: Vec::with_capacity(MAX_FRAME_BYTES),
        }
    }

    /// Bytes buffered and not yet consumed by `next_frame`.
    pub fn buffered(&self) -> usize {
        self.buf.len()
    }

    /// Read once from a non-blocking stream and append whatever arrived.
    ///
    /// `Err(ConnectionClosed)` on EOF. `Err(Io)` on any errno other than `EAGAIN` and `EINTR`.
    /// Never blocks and never loops on partial data.
    pub fn fill(&mut self, stream: &mut UnixStream) -> Result<ReadOutcome, TransportError> {
        let start = self.buf.len();
        if start >= MAX_FRAME_BYTES {
            // Full without a complete frame: the prefix claims more than MAX_MESSAGE_SIZE.
            // next_frame reports that as FrameTooLarge.
            return Ok(ReadOutcome::Progress);
        }
        self.buf.resize(MAX_FRAME_BYTES, 0);
        loop {
            match stream.read(&mut self.buf[start..]) {
                Ok(0) => {
                    self.buf.truncate(start);
                    return Err(TransportError::ConnectionClosed);
                }
                Ok(n) => {
                    self.buf.truncate(start + n);
                    return Ok(ReadOutcome::Progress);
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    self.buf.truncate(start);
                    return Ok(ReadOutcome::WouldBlock);
                }
                Err(e) => {
                    self.buf.truncate(start);
                    return Err(TransportError::Io {
                        operation: IoOperation::Read,
                        errno: e.raw_os_error().unwrap_or(0),
                    });
                }
            }
        }
    }

    /// Take the next complete frame's payload out of the buffer, if one is present.
    ///
    /// `Ok(None)` while the frame is incomplete. `Err(Protocol(FrameTooLarge))` as soon as a
    /// prefix declares more than `MAX_MESSAGE_SIZE`, without waiting for the bytes.
    pub fn next_frame(&mut self) -> Result<Option<Vec<u8>>, TransportError> {
        if self.buf.len() < LENGTH_PREFIX_BYTES {
            return Ok(None);
        }
        let mut prefix = [0u8; LENGTH_PREFIX_BYTES];
        prefix.copy_from_slice(&self.buf[..LENGTH_PREFIX_BYTES]);
        let len = u32::from_le_bytes(prefix) as usize;
        if len > MAX_MESSAGE_SIZE {
            return Err(TransportError::Protocol {
                fault: ProtocolFault::FrameTooLarge {
                    len,
                    max: MAX_MESSAGE_SIZE,
                },
            });
        }
        let total = LENGTH_PREFIX_BYTES + len;
        if self.buf.len() < total {
            return Ok(None);
        }
        let payload = self.buf[LENGTH_PREFIX_BYTES..total].to_vec();
        self.buf.drain(..total);
        Ok(Some(payload))
    }
}

/// Write the whole buffer to a stream.
///
/// `EAGAIN` is returned as `Io { Write, EAGAIN }`: on a non-blocking daemon socket it means the
/// peer is not draining its receive buffer, and on a client socket with a send timeout it means
/// the timeout elapsed. Either way the caller treats it as a dead connection.
pub fn write_all(stream: &mut UnixStream, buf: &[u8]) -> Result<(), TransportError> {
    let mut written: usize = 0;
    while written < buf.len() {
        match stream.write(&buf[written..]) {
            Ok(0) => {
                return Err(TransportError::Io {
                    operation: IoOperation::Write,
                    errno: libc::EPIPE,
                });
            }
            Ok(n) => written += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                return Err(TransportError::Io {
                    operation: IoOperation::Write,
                    errno: e.raw_os_error().unwrap_or(0),
                });
            }
        }
    }
    Ok(())
}

/// Fill the whole buffer from a blocking stream.
///
/// EOF before the first byte is `ConnectionClosed`; EOF mid-buffer is `Truncated`. A read
/// timeout surfaces as `Io { Read, EAGAIN }`.
pub fn read_exact(stream: &mut UnixStream, buf: &mut [u8]) -> Result<(), TransportError> {
    let total = buf.len();
    let mut filled: usize = 0;
    while filled < total {
        match stream.read(&mut buf[filled..]) {
            Ok(0) => {
                if filled == 0 {
                    return Err(TransportError::ConnectionClosed);
                }
                return Err(TransportError::Protocol {
                    fault: ProtocolFault::Truncated {
                        needed: total,
                        have: filled,
                    },
                });
            }
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                return Err(TransportError::Io {
                    operation: IoOperation::Read,
                    errno: e.raw_os_error().unwrap_or(0),
                });
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn pair() -> (UnixStream, UnixStream) {
        let (a, b) = UnixStream::pair().unwrap();
        b.set_nonblocking(true).unwrap();
        (a, b)
    }

    #[test]
    fn partial_frame_yields_nothing_until_complete() {
        let (mut tx, mut rx) = pair();
        let mut reader = FrameReader::new();
        let payload = [1u8, 2, 0, 3, b'a', b'b', b'c'];
        let frame = {
            let mut f = (payload.len() as u32).to_le_bytes().to_vec();
            f.extend_from_slice(&payload);
            f
        };
        tx.write_all(&frame[..2]).unwrap();
        assert_eq!(reader.fill(&mut rx).unwrap(), ReadOutcome::Progress);
        assert_eq!(reader.next_frame().unwrap(), None);
        assert_eq!(reader.fill(&mut rx).unwrap(), ReadOutcome::WouldBlock);
        tx.write_all(&frame[2..]).unwrap();
        assert_eq!(reader.fill(&mut rx).unwrap(), ReadOutcome::Progress);
        assert_eq!(reader.next_frame().unwrap(), Some(payload.to_vec()));
        assert_eq!(reader.next_frame().unwrap(), None);
        assert_eq!(reader.buffered(), 0);
    }

    #[test]
    fn two_frames_in_one_read_both_yield() {
        let (mut tx, mut rx) = pair();
        let mut reader = FrameReader::new();
        let mut bytes = Vec::new();
        for p in [&[9u8][..], &[8u8, 7][..]] {
            bytes.extend_from_slice(&(p.len() as u32).to_le_bytes());
            bytes.extend_from_slice(p);
        }
        tx.write_all(&bytes).unwrap();
        reader.fill(&mut rx).unwrap();
        assert_eq!(reader.next_frame().unwrap(), Some(vec![9]));
        assert_eq!(reader.next_frame().unwrap(), Some(vec![8, 7]));
        assert_eq!(reader.next_frame().unwrap(), None);
    }

    #[test]
    fn oversized_prefix_is_a_protocol_fault_immediately() {
        let (mut tx, mut rx) = pair();
        let mut reader = FrameReader::new();
        tx.write_all(&(u32::MAX).to_le_bytes()).unwrap();
        reader.fill(&mut rx).unwrap();
        assert!(matches!(
            reader.next_frame(),
            Err(TransportError::Protocol {
                fault: ProtocolFault::FrameTooLarge { .. }
            })
        ));
    }

    #[test]
    fn eof_is_connection_closed() {
        let (tx, mut rx) = pair();
        drop(tx);
        let mut reader = FrameReader::new();
        assert_eq!(reader.fill(&mut rx), Err(TransportError::ConnectionClosed));
    }

    #[test]
    fn blocked_write_reports_eagain() {
        let (mut tx, rx) = pair();
        tx.set_nonblocking(true).unwrap();
        let chunk = vec![0u8; 64 * 1024];
        let err = loop {
            match write_all(&mut tx, &chunk) {
                Ok(()) => continue,
                Err(e) => break e,
            }
        };
        assert_eq!(
            err,
            TransportError::Io {
                operation: IoOperation::Write,
                errno: libc::EAGAIN
            }
        );
        drop(rx);
    }
}
