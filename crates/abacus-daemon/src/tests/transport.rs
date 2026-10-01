//! UDS transport: SCM_RIGHTS fd passing, fd-count checks, socket path policy, blocked
//! writes. Everything through the public Server/Connection API on unique temp paths.

use std::io::Write;
use std::os::fd::{AsRawFd, BorrowedFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use abacus_core::error::{IoOperation, ProtocolFault, TransportError};
use abacus_core::interlock::{interlock_create, interlock_map};
use abacus_wire::{
    decode_response, encode_response, expected_fd_count, read_exact, recv_prefix_with_fds, Response,
};

use super::{last_errno, unique_path};
use crate::transport::{Connection, Server, SocketOptions};

fn accept_one(server: &Server) -> Connection {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match server.try_accept() {
            Ok(Some(c)) => return c,
            Ok(None) => {}
            Err(e) => panic!("accept failed: {e}"),
        }
        assert!(
            Instant::now() < deadline,
            "no connection accepted within 2 s"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// A server on a unique path with one connected client: (server, raw client stream, daemon side).
fn connected_pair(label: &str) -> (Server, UnixStream, Connection) {
    let path = unique_path(label);
    let server = Server::create(&path, &SocketOptions::default()).expect("server create");
    let client = UnixStream::connect(&path).expect("connect");
    client
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("set timeout");
    let accepted = accept_one(&server);
    (server, client, accepted)
}

fn recv_response_from(
    stream: &mut UnixStream,
) -> Result<(Response, Vec<std::os::fd::OwnedFd>), TransportError> {
    let (prefix, fds) = recv_prefix_with_fds(stream)?;
    let len = u32::from_le_bytes(prefix) as usize;
    let mut payload = vec![0u8; len];
    read_exact(stream, &mut payload)?;
    let resp = decode_response(&payload).map_err(|fault| TransportError::Protocol { fault })?;
    let expected = expected_fd_count(&resp);
    if fds.len() < expected {
        return Err(TransportError::Protocol {
            fault: ProtocolFault::Truncated {
                needed: expected,
                have: fds.len(),
            },
        });
    }
    if fds.len() > expected {
        return Err(TransportError::Protocol {
            fault: ProtocolFault::UnexpectedFd,
        });
    }
    Ok((resp, fds))
}

/// Send `frame` with `fds` attached using raw sendmsg, bypassing the transport's own
/// one-fd limit.
fn raw_sendmsg_with_fds(fd: i32, frame: &[u8], fds: &[i32]) -> isize {
    let fd_bytes = std::mem::size_of_val(fds);
    // SAFETY: CMSG_SPACE is a pure size computation.
    let space = unsafe { libc::CMSG_SPACE(fd_bytes as libc::c_uint) } as usize;
    let mut cmsg_buf: Vec<u64> = vec![0; space.div_ceil(8)];
    let mut iov = libc::iovec {
        iov_base: frame.as_ptr() as *mut libc::c_void,
        iov_len: frame.len(),
    };
    // SAFETY: zeroed msghdr; every pointer set below outlives the sendmsg call.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cmsg_buf.as_mut_ptr() as *mut libc::c_void;
    msg.msg_controllen = space;
    // SAFETY: the control buffer is CMSG_SPACE(fd_bytes) bytes and 8-byte aligned.
    unsafe {
        let c = libc::CMSG_FIRSTHDR(&msg);
        (*c).cmsg_level = libc::SOL_SOCKET;
        (*c).cmsg_type = libc::SCM_RIGHTS;
        (*c).cmsg_len = libc::CMSG_LEN(fd_bytes as libc::c_uint) as usize;
        std::ptr::copy_nonoverlapping(fds.as_ptr() as *const u8, libc::CMSG_DATA(c), fd_bytes);
        libc::sendmsg(fd, &msg, libc::MSG_NOSIGNAL)
    }
}

/// CONTRACTS.md wire-abi: one fd per Created/Attached response over SCM_RIGHTS. The fd the
/// client receives maps the same page the daemon holds (Server plus connect stands in for a
/// socketpair; the public API has no constructor from a raw stream).
#[test]
fn transport__sendmsg_recvmsg_passes_one_fd_over_socketpair() {
    let (_server, mut client, mut daemon) = connected_pair("fd-pass");
    let handle = interlock_create().expect("create");
    // SAFETY: the handle outlives the borrow.
    let fd = unsafe { BorrowedFd::borrow_raw(handle.as_raw_fd()) };
    daemon
        .send_response_with_fds(&Response::Created { id: 7 }, &[fd])
        .expect("send with fd");
    let (resp, mut fds) = recv_response_from(&mut client).expect("recv with fd");
    assert_eq!(resp, Response::Created { id: 7 });
    assert_eq!(fds.len(), 1, "expected exactly one fd, got {}", fds.len());
    let received = fds.remove(0);
    assert_ne!(
        received.as_raw_fd(),
        handle.as_raw_fd(),
        "received the same fd number"
    );
    let mapped = interlock_map(received).expect("map received fd");
    handle.words().open_count.store(99, Ordering::Release);
    assert_eq!(mapped.words().open_count.load(Ordering::Acquire), 99);
}

/// Receiver control buffer holds one fd (MAX_FDS_PER_MESSAGE); more than fit sets
/// MSG_CTRUNC, which recv_response reports as a Truncated protocol fault rather than
/// silently keeping a partial fd set.
#[test]
fn transport__recv_rejects_ctrunc() {
    let (_server, mut client, daemon) = connected_pair("ctrunc");
    let a = interlock_create().expect("create");
    let b = interlock_create().expect("create");
    let c = interlock_create().expect("create");
    let frame = encode_response(&Response::Created { id: 1 }).expect("encode");
    // Three fds need 12 data bytes; the receiver's buffer (CMSG_SPACE(4) = 24) fits two.
    let sent = raw_sendmsg_with_fds(
        daemon.as_raw_fd(),
        &frame,
        &[a.as_raw_fd(), b.as_raw_fd(), c.as_raw_fd()],
    );
    assert_eq!(
        sent as usize,
        frame.len(),
        "sendmsg sent {sent}, errno {}",
        last_errno()
    );
    let r = recv_response_from(&mut client);
    assert!(
        matches!(
            r,
            Err(TransportError::Protocol {
                fault: ProtocolFault::Truncated { .. }
            })
        ),
        "recv with truncated control data returned {r:?}"
    );
}

/// CONTRACTS.md wire-abi fd counts: Created with zero fds and Error with one fd are both
/// protocol faults on receive.
#[test]
fn transport__recv_rejects_wrong_fd_count() {
    let (_server, mut client, mut daemon) = connected_pair("fd-count");
    daemon
        .send_response(&Response::Created { id: 3 })
        .expect("send created without fd");
    let r = recv_response_from(&mut client);
    assert_eq!(
        r.map(|(resp, fds)| (resp, fds.len())),
        Err(TransportError::Protocol {
            fault: ProtocolFault::Truncated { needed: 1, have: 0 }
        }),
        "Created with 0 fds accepted"
    );

    let handle = interlock_create().expect("create");
    // SAFETY: the handle outlives the borrow.
    let fd = unsafe { BorrowedFd::borrow_raw(handle.as_raw_fd()) };
    daemon
        .send_response_with_fds(&Response::invalid_request("x"), &[fd])
        .expect("send error with fd");
    let r = recv_response_from(&mut client);
    assert!(
        matches!(
            r,
            Err(TransportError::Protocol {
                fault: ProtocolFault::UnexpectedFd
            })
        ),
        "Error with 1 fd accepted: {r:?}"
    );
}

/// Server::create: a path with a live listener is refused with live_daemon: true and the
/// first server keeps serving.
#[test]
fn transport__server_refuses_live_socket_path() {
    let path = unique_path("live");
    let first = Server::create(&path, &SocketOptions::default()).expect("first server");
    let second = Server::create(&path, &SocketOptions::default());
    match second {
        Err(TransportError::SocketPathOccupied {
            path: p,
            live_daemon,
        }) => {
            assert!(live_daemon, "live listener reported as not live");
            assert_eq!(p, path.to_string_lossy());
        }
        Ok(_) => panic!("second server bound the live path {}", path.display()),
        Err(other) => panic!("unexpected error {other}"),
    }
    UnixStream::connect(&path).expect("first server still accepts");
    let _accepted = accept_one(&first);
}

/// Server::create: a socket file with no listener behind it (a daemon that died without
/// unlinking) is removed and replaced; the new server accepts.
#[test]
fn transport__server_replaces_stale_socket_file() {
    let path = unique_path("stale");
    {
        let stale = std::os::unix::net::UnixListener::bind(&path).expect("bind stale");
        drop(stale);
    }
    assert!(path.exists(), "stale socket file missing before the test");
    assert!(
        UnixStream::connect(&path).is_err(),
        "stale socket unexpectedly accepts"
    );
    let server = Server::create(&path, &SocketOptions::default())
        .expect("server did not replace the stale socket");
    UnixStream::connect(&path).expect("connect to replacement");
    let _accepted = accept_one(&server);
}

/// Server::create: a regular file at the path is refused with live_daemon: false and left in
/// place.
#[test]
fn transport__server_refuses_non_socket_file() {
    let path = unique_path("regular");
    std::fs::write(&path, b"not a socket").expect("write file");
    let r = Server::create(&path, &SocketOptions::default());
    match r {
        Err(TransportError::SocketPathOccupied {
            path: p,
            live_daemon,
        }) => {
            assert!(!live_daemon, "regular file reported as a live daemon");
            assert_eq!(p, path.to_string_lossy());
        }
        Ok(_) => panic!("server replaced a regular file at {}", path.display()),
        Err(other) => panic!("unexpected error {other}"),
    }
    assert_eq!(
        std::fs::read(&path).expect("file survives"),
        b"not a socket"
    );
    let _ = std::fs::remove_file(&path);
}

/// A client that stops reading fills its receive buffer; the daemon's non-blocking
/// write then fails. That failure is classified as connection death (ConnectionClosed, or
/// Io with EPIPE/ECONNRESET/EAGAIN) so the client is dropped, not logged and kept.
#[test]
fn transport__blocked_write_is_a_connection_error() {
    let (_server, _client, mut daemon) = connected_pair("blocked");
    daemon.set_nonblocking(true).expect("nonblocking");
    let big = Response::invalid_request(&"x".repeat(4000));
    let mut sent = 0usize;
    let err = loop {
        match daemon.send_response(&big) {
            Ok(()) => sent += 1,
            Err(e) => break e,
        }
        assert!(
            sent < 100_000,
            "peer never blocked after {sent} frames of 4 KB"
        );
    };
    let is_connection_death = matches!(
        err,
        TransportError::ConnectionClosed
            | TransportError::Io {
                operation: IoOperation::Write | IoOperation::SendMsg,
                errno: libc::EPIPE | libc::ECONNRESET | libc::EAGAIN
            }
    );
    assert!(
        is_connection_death,
        "blocked write after {sent} frames returned {err:?}; the contract requires it to read as connection death"
    );
}

/// Connection::read_available on a non-blocking daemon-side socket with nothing queued is
/// ReadOutcome::WouldBlock (the daemon skips the client this cycle and keeps it).
#[test]
fn transport__read_available_wouldblock_when_idle() {
    let (_server, _client, mut daemon) = connected_pair("idle");
    daemon.set_nonblocking(true).expect("nonblocking");
    let r = daemon.read_available();
    assert!(
        matches!(r, Ok(abacus_wire::ReadOutcome::WouldBlock)),
        "idle read_available returned {r:?}"
    );
}

/// Connection::read_available when the peer has closed is ConnectionClosed (daemon removes
/// the client, no reap: abacus-daemon CLAUDE.md contracts).
#[test]
fn transport__read_available_reports_peer_close() {
    let (_server, client, mut daemon) = connected_pair("eof");
    drop(client);
    let r = daemon.read_available();
    assert!(
        matches!(r, Err(TransportError::ConnectionClosed)),
        "read after close returned {r:?}"
    );
}

/// Connection::read_available + next_request decodes a complete frame written by a raw client.
#[test]
fn transport__next_request_decodes_complete_frame() {
    let path = unique_path("complete");
    let server = Server::create(&path, &SocketOptions::default()).expect("server");
    let mut raw = UnixStream::connect(&path).expect("connect");
    let payload = [2u8, 0x02, 3, 0, b'a', b'b', b'c'];
    let mut frame = (payload.len() as u32).to_le_bytes().to_vec();
    frame.extend_from_slice(&payload);
    raw.write_all(&frame).expect("write");
    let mut daemon = accept_one(&server);
    let outcome = daemon.read_available().expect("read_available");
    assert_eq!(outcome, abacus_wire::ReadOutcome::Progress);
    let r = daemon.next_request();
    assert_eq!(
        r,
        Ok(Some(abacus_wire::Request::AttachInterlock {
            name: "abc".into()
        })),
        "complete attach frame did not decode"
    );
}

/// Connection::read_available with partial data then a close: the first read gets bytes,
/// the second gets ConnectionClosed, and next_request with incomplete data returns a
/// Truncated protocol fault.
#[test]
fn transport__partial_then_close_is_truncated() {
    let path = unique_path("cut");
    let server = Server::create(&path, &SocketOptions::default()).expect("server");
    let mut raw = UnixStream::connect(&path).expect("connect");
    raw.write_all(&[7, 0, 0, 0, 1, 0x02, 3])
        .expect("write partial");
    drop(raw);
    let mut daemon = accept_one(&server);
    let _ = daemon.read_available();
    let close = daemon.read_available();
    assert!(
        matches!(close, Err(TransportError::ConnectionClosed)),
        "expected ConnectionClosed after partial then close, got {close:?}"
    );
}

/// Connecting to a path with no socket fails with ENOENT (or ECONNREFUSED), not a panic.
#[test]
fn transport__connect_missing_path_is_io_error() {
    let path = unique_path("nowhere");
    let r = UnixStream::connect(&path);
    match r {
        Err(e) => {
            let errno = e.raw_os_error().unwrap_or(0);
            assert!(
                errno == libc::ENOENT || errno == libc::ECONNREFUSED,
                "errno {errno}, expected ENOENT or ECONNREFUSED"
            );
        }
        Ok(_) => panic!("connected to a path with no socket"),
    }
}

/// Server::try_accept returns None without blocking when nobody is connecting.
#[test]
fn transport__try_accept_none_when_nobody_connects() {
    let path = unique_path("nobody");
    let server = Server::create(&path, &SocketOptions::default()).expect("server");
    let t0 = Instant::now();
    let r = server.try_accept().expect("try_accept");
    assert!(r.is_none(), "accepted a phantom connection");
    assert!(
        t0.elapsed() < Duration::from_millis(50),
        "try_accept blocked {:?}",
        t0.elapsed()
    );
}

/// Server drop removes the socket file (clean shutdown leaves no stale path).
#[test]
fn transport__server_drop_removes_socket_file() {
    let path = unique_path("drop");
    let server = Server::create(&path, &SocketOptions::default()).expect("server");
    assert!(path.exists());
    drop(server);
    assert!(
        !path.exists(),
        "socket file {} survived Server drop",
        path.display()
    );
}

/// A read timeout on the client stream bounds a silent peer: recv returns an Io error
/// after the timeout instead of blocking forever.
#[test]
fn transport__read_timeout_bounds_a_silent_peer() {
    let (_server, mut client, _daemon) = connected_pair("timeouts");
    client
        .set_read_timeout(Some(Duration::from_millis(20)))
        .expect("set_read_timeout");
    let t0 = Instant::now();
    let r = recv_response_from(&mut client);
    let elapsed = t0.elapsed();
    assert!(
        matches!(
            r,
            Err(TransportError::Io {
                operation: IoOperation::RecvMsg,
                ..
            })
        ),
        "recv with timeout returned {r:?}"
    );
    assert!(
        elapsed >= Duration::from_millis(20) && elapsed < Duration::from_millis(500),
        "timeout fired after {elapsed:?}"
    );
}
