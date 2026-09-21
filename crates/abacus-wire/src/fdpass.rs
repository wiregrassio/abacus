//! SCM_RIGHTS fd passing: send a frame with fds attached, receive a frame prefix with fds.

use std::mem;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;

use abacus_core::error::{IoOperation, ProtocolFault, TransportError};

use crate::codec::LENGTH_PREFIX_BYTES;
use crate::framing::{read_exact, write_all};

/// Maximum fds attached to one message. The protocol attaches at most one.
pub const MAX_FDS_PER_MESSAGE: usize = 1;

/// Bytes of control-message space needed to carry `n` fds, aligned like `CMSG_SPACE`.
pub const fn cmsg_space_for_fds(n: usize) -> usize {
    let data_len = n * mem::size_of::<RawFd>();
    let hdr_align = (mem::size_of::<libc::cmsghdr>() + mem::size_of::<usize>() - 1)
        & !(mem::size_of::<usize>() - 1);
    let data_align = (data_len + mem::size_of::<usize>() - 1) & !(mem::size_of::<usize>() - 1);
    hdr_align + data_align
}

#[repr(C)]
struct CmsgBuf {
    _align: [libc::cmsghdr; 0],
    buf: [u8; cmsg_space_for_fds(MAX_FDS_PER_MESSAGE)],
}

impl CmsgBuf {
    fn new() -> Self {
        Self {
            _align: [],
            buf: [0u8; cmsg_space_for_fds(MAX_FDS_PER_MESSAGE)],
        }
    }
    fn as_mut_ptr(&mut self) -> *mut u8 {
        self.buf.as_mut_ptr()
    }
    fn len(&self) -> usize {
        self.buf.len()
    }
}

fn last_errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// Send a complete frame with `fds` attached as SCM_RIGHTS on the first bytes.
///
/// A short `sendmsg` is completed with plain writes. `EAGAIN` from either step is returned as
/// an `Io` error for the caller to treat as a dead peer.
pub fn send_frame_with_fds(
    stream: &mut UnixStream,
    frame: &[u8],
    fds: &[BorrowedFd<'_>],
) -> Result<(), TransportError> {
    if fds.len() > MAX_FDS_PER_MESSAGE {
        return Err(TransportError::Protocol {
            fault: ProtocolFault::FrameTooLarge {
                len: fds.len(),
                max: MAX_FDS_PER_MESSAGE,
            },
        });
    }
    if fds.is_empty() {
        return write_all(stream, frame);
    }

    let raw_fds: Vec<RawFd> = fds.iter().map(|fd| fd.as_raw_fd()).collect();
    let fd_bytes = raw_fds.len() * mem::size_of::<RawFd>();

    let mut cmsg_buf = CmsgBuf::new();
    let cmsg_len = unsafe { libc::CMSG_SPACE(fd_bytes as libc::c_uint) } as usize;

    let mut iov = libc::iovec {
        iov_base: frame.as_ptr() as *mut libc::c_void,
        iov_len: frame.len(),
    };

    let mut msg: libc::msghdr = unsafe { mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cmsg_buf.as_mut_ptr() as *mut libc::c_void;
    msg.msg_controllen = cmsg_len;

    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        if cmsg.is_null() {
            return Err(TransportError::Io {
                operation: IoOperation::SendMsg,
                errno: libc::EINVAL,
            });
        }
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(fd_bytes as libc::c_uint) as usize;
        std::ptr::copy_nonoverlapping(
            raw_fds.as_ptr() as *const u8,
            libc::CMSG_DATA(cmsg),
            fd_bytes,
        );
    }

    let sent = sendmsg_retry(stream.as_raw_fd(), &msg)?;
    if sent < frame.len() {
        write_all(stream, &frame[sent..])?;
    }
    Ok(())
}

fn sendmsg_retry(fd: RawFd, msg: &libc::msghdr) -> Result<usize, TransportError> {
    loop {
        let rc = unsafe { libc::sendmsg(fd, msg as *const libc::msghdr, libc::MSG_NOSIGNAL) };
        if rc < 0 {
            let errno = last_errno();
            if errno == libc::EINTR {
                continue;
            }
            return Err(TransportError::Io {
                operation: IoOperation::SendMsg,
                errno,
            });
        }
        return Ok(rc as usize);
    }
}

/// Receive a frame's length prefix together with any SCM_RIGHTS fds attached to it.
///
/// Blocking. EOF before any byte is `ConnectionClosed`. A truncated control message
/// (`MSG_CTRUNC`) is a protocol fault. A short read of the prefix is completed with
/// `read_exact`.
pub fn recv_prefix_with_fds(
    stream: &mut UnixStream,
) -> Result<([u8; LENGTH_PREFIX_BYTES], Vec<OwnedFd>), TransportError> {
    let mut prefix_buf = [0u8; LENGTH_PREFIX_BYTES];
    let mut cmsg_buf = CmsgBuf::new();

    let mut iov = libc::iovec {
        iov_base: prefix_buf.as_mut_ptr() as *mut libc::c_void,
        iov_len: LENGTH_PREFIX_BYTES,
    };

    let mut msg: libc::msghdr = unsafe { mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cmsg_buf.as_mut_ptr() as *mut libc::c_void;
    msg.msg_controllen = cmsg_buf.len();

    let received = recvmsg_retry(stream.as_raw_fd(), &mut msg)?;
    if received == 0 {
        return Err(TransportError::ConnectionClosed);
    }

    let fds = extract_fds_from_cmsg(&msg);

    if msg.msg_flags & libc::MSG_CTRUNC != 0 {
        return Err(TransportError::Protocol {
            fault: ProtocolFault::Truncated {
                needed: MAX_FDS_PER_MESSAGE,
                have: fds.len(),
            },
        });
    }

    if received < LENGTH_PREFIX_BYTES {
        read_exact(stream, &mut prefix_buf[received..])?;
    }

    Ok((prefix_buf, fds))
}

fn recvmsg_retry(fd: RawFd, msg: &mut libc::msghdr) -> Result<usize, TransportError> {
    loop {
        let rc = unsafe { libc::recvmsg(fd, msg, libc::MSG_CMSG_CLOEXEC) };
        if rc < 0 {
            let errno = last_errno();
            if errno == libc::EINTR {
                continue;
            }
            return Err(TransportError::Io {
                operation: IoOperation::RecvMsg,
                errno,
            });
        }
        return Ok(rc as usize);
    }
}

fn extract_fds_from_cmsg(msg: &libc::msghdr) -> Vec<OwnedFd> {
    let mut fds = Vec::new();
    unsafe {
        let mut cmsg = libc::CMSG_FIRSTHDR(msg);
        while !cmsg.is_null() {
            if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                let data_ptr = libc::CMSG_DATA(cmsg);
                let data_len = (*cmsg).cmsg_len - libc::CMSG_LEN(0) as usize;
                let n_fds = data_len / mem::size_of::<RawFd>();
                for i in 0..n_fds {
                    let raw: RawFd = std::ptr::read_unaligned(
                        data_ptr.add(i * mem::size_of::<RawFd>()) as *const RawFd,
                    );
                    fds.push(OwnedFd::from_raw_fd(raw));
                }
            }
            cmsg = libc::CMSG_NXTHDR(msg, cmsg);
        }
    }
    fds
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::fd::AsFd;

    #[test]
    fn cmsg_space_matches_libc() {
        let libc_space = unsafe { libc::CMSG_SPACE(mem::size_of::<RawFd>() as libc::c_uint) };
        assert_eq!(cmsg_space_for_fds(1), libc_space as usize);
    }

    #[test]
    fn one_fd_crosses_a_socketpair() {
        let (mut tx, mut rx) = UnixStream::pair().unwrap();
        let (mut carried_a, mut carried_b) = UnixStream::pair().unwrap();
        let frame = [3u8, 0, 0, 0, 0xaa, 0xbb, 0xcc];
        send_frame_with_fds(&mut tx, &frame, &[carried_a.as_fd()]).unwrap();
        let (prefix, fds) = recv_prefix_with_fds(&mut rx).unwrap();
        assert_eq!(prefix, [3, 0, 0, 0]);
        assert_eq!(fds.len(), 1);
        let mut rest = [0u8; 3];
        read_exact(&mut rx, &mut rest).unwrap();
        assert_eq!(rest, [0xaa, 0xbb, 0xcc]);
        // The received fd is the same socket: bytes written through it arrive at carried_b.
        let mut received = UnixStream::from(fds.into_iter().next().unwrap());
        received.write_all(b"hi").unwrap();
        let mut got = [0u8; 2];
        carried_b.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"hi");
        carried_a.write_all(b"ok").unwrap();
    }

    #[test]
    fn no_fds_is_a_plain_write() {
        let (mut tx, mut rx) = UnixStream::pair().unwrap();
        send_frame_with_fds(&mut tx, &[1, 0, 0, 0, 9], &[]).unwrap();
        let (prefix, fds) = recv_prefix_with_fds(&mut rx).unwrap();
        assert_eq!(prefix, [1, 0, 0, 0]);
        assert!(fds.is_empty());
    }

    #[test]
    fn eof_is_connection_closed() {
        let (tx, mut rx) = UnixStream::pair().unwrap();
        drop(tx);
        assert_eq!(
            recv_prefix_with_fds(&mut rx).unwrap_err(),
            TransportError::ConnectionClosed
        );
    }
}
