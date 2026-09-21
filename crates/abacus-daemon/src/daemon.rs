//! The daemon loop: ppoll to the next millisecond boundary or the next client byte, service
//! clients, then evaluate the registry once per boundary reached.

use std::os::fd::AsFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use abacus_core::clock::{monotonic_now_nanos, timespec_from_nanos, NANOS_PER_MS};
use abacus_core::error::{IoOperation, StartupError, TransportError};
use abacus_core::interlock::interlock_dup_fd;
use abacus_wire::{Request, Response};

use crate::registry::{Registry, Tier, DEFAULT_MAX_INTERLOCKS};
use crate::transport::{Connection, Server, SocketOptions};

/// Daemon configuration. Defaults match the `abacus` binary's flags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonConfig {
    /// UDS path to listen on.
    pub socket_path: PathBuf,
    /// Permission bits for the socket file.
    pub socket_mode: u32,
    /// Group to chown the socket file to, if any.
    pub socket_group: Option<String>,
    /// Cap on live interlocks, excluding the clock.
    pub max_interlocks: usize,
}

impl DaemonConfig {
    /// Defaults with the given socket path: mode 0660, group unchanged, 4096 interlocks.
    pub fn new(socket_path: PathBuf) -> Self {
        Self {
            socket_path,
            socket_mode: 0o660,
            socket_group: None,
            max_interlocks: DEFAULT_MAX_INTERLOCKS,
        }
    }
}

/// Run the daemon on `socket_path` with default configuration until `stop` is set.
///
/// Returns `Ok(())` after a clean stop; the socket file is removed on return. Returns an
/// error only if the socket or the clock cannot be set up, or `ppoll` fails permanently.
pub fn daemon_run(socket_path: &Path, stop: &AtomicBool) -> Result<(), StartupError> {
    daemon_run_with(&DaemonConfig::new(socket_path.to_path_buf()), stop)
}

/// Run the daemon with explicit configuration until `stop` is set. See `daemon_run`.
pub fn daemon_run_with(config: &DaemonConfig, stop: &AtomicBool) -> Result<(), StartupError> {
    let options = SocketOptions {
        mode: config.socket_mode,
        group: config.socket_group.clone(),
    };
    let server = Server::create(&config.socket_path, &options)?;
    let mut registry = Registry::with_limit(config.max_interlocks)?;
    set_timer_slack();

    eprintln!("abacus: daemon running on {}", config.socket_path.display());

    let anchor = monotonic_now_nanos();
    let mut next_due = anchor + NANOS_PER_MS;
    let mut clients: Vec<Connection> = Vec::new();
    let mut pollfds: Vec<libc::pollfd> = Vec::new();
    rebuild_pollfds(&mut pollfds, &server, &clients);

    loop {
        if stop.load(Ordering::Acquire) {
            break;
        }

        // Sleep to the next boundary, or until a client has bytes. Behind schedule (a heavy
        // cycle) means a zero timeout: evaluate now, then resume the grid.
        let now = monotonic_now_nanos();
        let timeout = timespec_from_nanos(next_due.saturating_sub(now));
        for pfd in pollfds.iter_mut() {
            pfd.revents = 0;
        }
        let poll_ret = unsafe {
            libc::ppoll(
                pollfds.as_mut_ptr(),
                pollfds.len() as libc::nfds_t,
                &timeout,
                std::ptr::null(),
            )
        };
        if poll_ret < 0 {
            let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
            if errno != libc::EINTR {
                return Err(StartupError::EventLoopFailed { errno });
            }
        }

        let mut changed = false;

        if poll_ret > 0 && pollfds[0].revents & libc::POLLIN != 0 {
            loop {
                match server.try_accept() {
                    Ok(Some(conn)) => {
                        if let Err(e) = conn.set_nonblocking(true) {
                            eprintln!("abacus: set_nonblocking error: {e}");
                            continue;
                        }
                        clients.push(conn);
                        changed = true;
                    }
                    Ok(None) => break,
                    Err(e) => {
                        eprintln!("abacus: accept error: {e}");
                        break;
                    }
                }
            }
        }

        // Readable first, hangup second: a client that sends a request and shuts down its
        // write side still gets its request read.
        // Only clients that were in this cycle's poll set have revents; ones accepted just
        // now are polled from the next cycle.
        let mut to_remove: Vec<usize> = Vec::new();
        if poll_ret > 0 {
            for i in 0..pollfds.len() - 1 {
                let revents = pollfds[i + 1].revents;
                if revents & libc::POLLIN != 0 {
                    if service_client(&mut clients[i], &mut registry) == ClientOutcome::Drop {
                        to_remove.push(i);
                    }
                } else if revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
                    // Client gone. Its interlocks die by TTL, not by disconnect.
                    to_remove.push(i);
                }
            }
        }
        for i in to_remove.into_iter().rev() {
            clients.remove(i);
            changed = true;
        }

        let now = monotonic_now_nanos();
        if now >= next_due {
            registry.tick(now);
            // The first boundary strictly after now. Boundaries missed during a stall are
            // subsumed by this evaluation, never replayed.
            next_due = anchor + ((now - anchor) / NANOS_PER_MS + 1) * NANOS_PER_MS;
        }

        if changed {
            rebuild_pollfds(&mut pollfds, &server, &clients);
        }
    }

    Ok(())
}

/// Rebuild the pollfds vector in place: clear and refill without deallocating.
/// The server listener is always index 0; clients follow.
fn rebuild_pollfds(pollfds: &mut Vec<libc::pollfd>, server: &Server, clients: &[Connection]) {
    pollfds.clear();
    pollfds.reserve(1 + clients.len());
    pollfds.push(libc::pollfd {
        fd: server.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    });
    for client in clients {
        pollfds.push(libc::pollfd {
            fd: client.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        });
    }
}

// Drop the default 50 us timer slack for this process so ppoll wakes at the boundary, not
// up to 50 us after it. A daemon-internal choice, not deployment.
fn set_timer_slack() {
    let rc = unsafe { libc::prctl(libc::PR_SET_TIMERSLACK, 1u64) };
    if rc != 0 {
        eprintln!(
            "abacus: PR_SET_TIMERSLACK failed, errno={}",
            std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
        );
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClientOutcome {
    Keep,
    Drop,
}

/// Read what the client has sent, answer every complete request. A protocol fault, a dead
/// socket, or a client that will not take its response all end the connection.
fn service_client(client: &mut Connection, registry: &mut Registry) -> ClientOutcome {
    let mut closing = false;
    match client.read_available() {
        Ok(_) => {}
        Err(TransportError::ConnectionClosed) => closing = true,
        Err(e) => {
            eprintln!("abacus: read error, closing client: {e}");
            return ClientOutcome::Drop;
        }
    }
    loop {
        match client.next_request() {
            Ok(Some(request)) => {
                if let Err(e) = handle_request(client, registry, request) {
                    if !is_connection_error(&e) {
                        eprintln!("abacus: response error, closing client: {e}");
                    }
                    return ClientOutcome::Drop;
                }
            }
            Ok(None) => break,
            Err(TransportError::Protocol { fault }) => {
                // The stream is desynced past this point: answer once, then close.
                eprintln!("abacus: protocol fault, closing client: {fault}");
                let _ = client.send_response(&Response::invalid_request(&fault.to_string()));
                return ClientOutcome::Drop;
            }
            Err(e) => {
                eprintln!("abacus: request error, closing client: {e}");
                return ClientOutcome::Drop;
            }
        }
    }
    if closing {
        ClientOutcome::Drop
    } else {
        ClientOutcome::Keep
    }
}

fn handle_request(
    client: &mut Connection,
    registry: &mut Registry,
    request: Request,
) -> Result<(), TransportError> {
    match request {
        Request::CreateInterlock {
            name,
            tier,
            watched_name,
            watched_word,
            interval_ns,
            conditions,
        } => {
            let Some(tier) = Tier::from_wire(tier) else {
                return client
                    .send_response(&Response::invalid_request(&format!("invalid tier: {tier}")));
            };
            match registry.create(
                name,
                tier,
                watched_name,
                watched_word,
                interval_ns,
                conditions,
            ) {
                Ok((id, handle)) => match interlock_dup_fd(&handle) {
                    Ok(fd) => {
                        client.send_response_with_fds(&Response::Created { id }, &[fd.as_fd()])
                    }
                    Err(e) => client.send_response(&Response::from_condition(&e)),
                },
                Err(e) => client.send_response(&Response::from_condition(&e)),
            }
        }
        Request::AttachInterlock { name } => match registry.attach(&name) {
            Ok((id, handle)) => match interlock_dup_fd(&handle) {
                Ok(fd) => client.send_response_with_fds(&Response::Attached { id }, &[fd.as_fd()]),
                Err(e) => client.send_response(&Response::from_condition(&e)),
            },
            Err(e) => client.send_response(&Response::from_condition(&e)),
        },
    }
}

/// Whether a transport error means the connection is dead rather than the request bad.
/// `EAGAIN` on a write counts: the protocol is strict request/response, so a client that is
/// not draining its socket is broken.
fn is_connection_error(e: &TransportError) -> bool {
    match e {
        TransportError::ConnectionClosed => true,
        TransportError::Io { operation, errno } => {
            matches!(
                operation,
                IoOperation::Read
                    | IoOperation::Write
                    | IoOperation::SendMsg
                    | IoOperation::RecvMsg
            ) && matches!(
                *errno,
                libc::EPIPE | libc::ECONNRESET | libc::EAGAIN | libc::ENOTCONN
            )
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connection_errors_include_eagain_on_write() {
        for errno in [libc::EPIPE, libc::ECONNRESET, libc::EAGAIN, libc::ENOTCONN] {
            assert!(is_connection_error(&TransportError::Io {
                operation: IoOperation::Write,
                errno
            }));
        }
        assert!(is_connection_error(&TransportError::ConnectionClosed));
        assert!(!is_connection_error(&TransportError::Io {
            operation: IoOperation::Bind,
            errno: libc::EAGAIN
        }));
        assert!(!is_connection_error(&TransportError::Io {
            operation: IoOperation::Write,
            errno: libc::ENOMEM
        }));
    }
}
