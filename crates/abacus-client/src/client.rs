//! AbacusClient: the connection to the daemon, the SDK error type, and the create and attach
//! calls that hand back typed handles.

use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use abacus_core::error::{Condition, IoOperation, ProtocolFault, TransportError};
use abacus_core::interlock::{
    interlock_map, interlock_map_barrier, interlock_map_clock, interlock_map_counter,
    interlock_map_cron, interlock_map_timer, InterlockHandle,
};
use abacus_wire::{
    decode_response, encode_request, expected_fd_count, read_exact, recv_prefix_with_fds,
    write_all, Request, Response, ERR_ALLOCATION_FAILED, ERR_INTERLOCK_NOT_FOUND,
    ERR_INTERLOCK_REAPED, ERR_INVALID_REQUEST, MAX_MESSAGE_SIZE,
};

use crate::interlock::{AttachedInterlock, AttachedWaitCounter, ClockHandle, Interlock};
use crate::process_clock::ProcessClock;
use crate::touch::Keepalive;
use crate::types::{TimeoutPolicy, WatchedWord, DEFAULT_TRANSPORT_TIMEOUT, MIN_FATAL_MARGIN_MS};
use crate::wait_barrier::WaitBarrier;
use crate::wait_counter::WaitCounter;
use crate::wait_cron::WaitCron;
use crate::wait_timer::WaitTimer;

// -- SDK error type --

/// Every way an SDK call can fail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SdkError {
    /// The interlock is terminated: TTL lapsed, name claimed by another create, freed, or its
    /// watched target died. Recreate and resume, or crash.
    InterlockReaped,
    /// Attach to a name not in the registry, or a watched name that does not exist.
    InterlockNotFound {
        /// The name.
        name: String,
    },
    /// The daemon could not allocate the interlock.
    AllocationFailed {
        /// The daemon's message.
        message: String,
    },
    /// A transport-level failure: connection, framing, protocol, timeout.
    Transport(TransportError),
    /// mmap of the received fd failed.
    MmapFailed {
        /// The failure.
        message: String,
    },
    /// The request was rejected: bad tier, missing field, reserved name, limit reached, or a
    /// bad argument caught SDK-side.
    InvalidRequest {
        /// Why.
        message: String,
    },
    /// The daemon answered with something the SDK did not expect.
    UnexpectedResponse {
        /// What.
        message: String,
    },
    /// A WaitTimer's fatal margin elapsed without delivery, under `TimeoutPolicy::Error`.
    DeliveryTimeout,
}

impl std::fmt::Display for SdkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InterlockReaped => write!(f, "interlock reaped"),
            Self::InterlockNotFound { name } => write!(f, "interlock not found: {name}"),
            Self::AllocationFailed { message } => write!(f, "allocation failed: {message}"),
            Self::Transport(e) => write!(f, "transport: {e}"),
            Self::MmapFailed { message } => write!(f, "mmap failed: {message}"),
            Self::InvalidRequest { message } => write!(f, "invalid request: {message}"),
            Self::UnexpectedResponse { message } => write!(f, "unexpected response: {message}"),
            Self::DeliveryTimeout => write!(
                f,
                "DeliveryTimeout: daemon did not deliver within the margin"
            ),
        }
    }
}

impl std::error::Error for SdkError {}

impl From<TransportError> for SdkError {
    fn from(e: TransportError) -> Self {
        Self::Transport(e)
    }
}

impl From<Condition> for SdkError {
    fn from(c: Condition) -> Self {
        match c {
            Condition::InterlockReaped => Self::InterlockReaped,
            Condition::InterlockNotFound { name } => Self::InterlockNotFound { name },
            Condition::AllocationFailed { .. } => Self::AllocationFailed {
                message: c.to_string(),
            },
            Condition::InvalidRequest { message } => Self::InvalidRequest { message },
        }
    }
}

/// Result alias for SDK calls.
pub type Result<T> = std::result::Result<T, SdkError>;

// -- Client connection transport --

/// Blocking UDS connection to the daemon with read and write timeouts.
struct ClientConn {
    stream: UnixStream,
    timeout: Duration,
    poisoned: bool,
}

impl ClientConn {
    fn connect(path: &Path, timeout: Duration) -> std::result::Result<Self, TransportError> {
        let stream = UnixStream::connect(path).map_err(|e| TransportError::Io {
            operation: IoOperation::Connect,
            errno: e.raw_os_error().unwrap_or(0),
        })?;
        let some = Some(timeout);
        stream
            .set_read_timeout(some)
            .map_err(|e| TransportError::Io {
                operation: IoOperation::SetTimeout,
                errno: e.raw_os_error().unwrap_or(0),
            })?;
        stream
            .set_write_timeout(some)
            .map_err(|e| TransportError::Io {
                operation: IoOperation::SetTimeout,
                errno: e.raw_os_error().unwrap_or(0),
            })?;
        Ok(Self {
            stream,
            timeout,
            poisoned: false,
        })
    }

    /// On a blocking socket with SO_RCVTIMEO and SO_SNDTIMEO, EAGAIN means the timeout elapsed.
    fn map_timeout(e: TransportError) -> TransportError {
        match e {
            TransportError::Io { operation, errno } if errno == libc::EAGAIN => {
                TransportError::Io {
                    operation,
                    errno: libc::ETIMEDOUT,
                }
            }
            other => other,
        }
    }

    fn send_frame(&mut self, frame: &[u8]) -> std::result::Result<(), TransportError> {
        write_all(&mut self.stream, frame).map_err(Self::map_timeout)
    }

    fn recv_response(&mut self) -> std::result::Result<(Response, Vec<OwnedFd>), TransportError> {
        let (prefix, fds) = recv_prefix_with_fds(&mut self.stream).map_err(Self::map_timeout)?;
        let payload_len = u32::from_le_bytes(prefix) as usize;
        if payload_len > MAX_MESSAGE_SIZE {
            return Err(TransportError::Protocol {
                fault: ProtocolFault::FrameTooLarge {
                    len: payload_len,
                    max: MAX_MESSAGE_SIZE,
                },
            });
        }
        let mut payload = vec![0u8; payload_len];
        read_exact(&mut self.stream, &mut payload).map_err(Self::map_timeout)?;
        let response =
            decode_response(&payload).map_err(|fault| TransportError::Protocol { fault })?;
        let needed = expected_fd_count(&response);
        if fds.len() > needed {
            return Err(TransportError::Protocol {
                fault: ProtocolFault::UnexpectedFd,
            });
        }
        if fds.len() < needed {
            return Err(TransportError::Protocol {
                fault: ProtocolFault::Truncated {
                    needed,
                    have: fds.len(),
                },
            });
        }
        Ok((response, fds))
    }

    fn send_recv(&mut self, req: &Request) -> Result<(Response, Vec<OwnedFd>)> {
        if self.poisoned {
            return Err(SdkError::Transport(TransportError::Io {
                operation: IoOperation::Read,
                errno: libc::ECONNRESET,
            }));
        }
        let frame = encode_request(req)
            .map_err(|fault| SdkError::Transport(TransportError::Protocol { fault }))?;
        if let Err(e) = self.send_frame(&frame) {
            self.poisoned = true;
            return Err(e.into());
        }
        match self.recv_response() {
            Ok(resp) => Ok(resp),
            Err(e) => {
                self.poisoned = true;
                Err(e.into())
            }
        }
    }
}

// -- AbacusClient --

/// A client connection to the Abacus daemon.
///
/// Holds the clock handle attached on connect, one keepalive thread shared by every handle
/// this client creates, and the timeout policy handed to WaitTimers.
pub struct AbacusClient {
    conn: ClientConn,
    clock: ClockHandle,
    keepalive: Keepalive,
    timeout_policy: TimeoutPolicy,
    min_fatal_margin_ms: u64,
}

impl AbacusClient {
    /// Connect to the daemon at `socket_path` with the default 1 s transport timeout and
    /// attach to the clock.
    pub fn connect(socket_path: &Path) -> Result<Self> {
        Self::connect_with_timeout(socket_path, DEFAULT_TRANSPORT_TIMEOUT)
    }

    /// Connect with an explicit transport timeout. Every create and attach that the daemon
    /// does not answer within `timeout` fails with `Transport(Io { errno: ETIMEDOUT })`.
    pub fn connect_with_timeout(socket_path: &Path, timeout: Duration) -> Result<Self> {
        let mut conn = ClientConn::connect(socket_path, timeout)?;
        let clock_handle = do_attach(&mut conn, "clock", interlock_map_clock)?;
        Ok(Self {
            conn,
            clock: ClockHandle::new(clock_handle),
            keepalive: Keepalive::new(),
            timeout_policy: TimeoutPolicy::default(),
            min_fatal_margin_ms: MIN_FATAL_MARGIN_MS,
        })
    }

    /// The policy WaitTimers created after this call use on a missed fatal margin.
    pub fn set_timeout_policy(&mut self, policy: TimeoutPolicy) {
        self.timeout_policy = policy;
    }

    /// The current timeout policy.
    pub fn timeout_policy(&self) -> TimeoutPolicy {
        self.timeout_policy
    }

    /// The fatal-margin floor WaitTimers created after this call use. Default
    /// `MIN_FATAL_MARGIN_MS`.
    pub fn set_min_fatal_margin_ms(&mut self, margin_ms: u64) {
        self.min_fatal_margin_ms = margin_ms;
    }

    /// The current fatal-margin floor.
    pub fn min_fatal_margin_ms(&self) -> u64 {
        self.min_fatal_margin_ms
    }

    /// The transport timeout this connection uses.
    pub fn transport_timeout(&self) -> Duration {
        self.conn.timeout
    }

    /// The keepalive shared by this client's handles.
    pub fn keepalive(&self) -> &Keepalive {
        &self.keepalive
    }

    /// Create a bare interlock (tier 0) with the keepalive running.
    pub fn create_interlock(&mut self, name: &str) -> Result<Interlock> {
        let handle = self.do_create(create_request(name, 0), interlock_map)?;
        Ok(Interlock::new(
            handle,
            self.clock.handle().clone(),
            self.keepalive.clone(),
        ))
    }

    /// Attach to an existing interlock by name: counters r/w, expiration r/o.
    ///
    /// "clock" is reserved; use `client.clock()`.
    pub fn attach_interlock(&mut self, name: &str) -> Result<AttachedInterlock> {
        if name == "clock" {
            return Err(SdkError::InvalidRequest {
                message: "use client.clock() to access the system clock".to_string(),
            });
        }
        let handle = do_attach(&mut self.conn, name, interlock_map)?;
        Ok(AttachedInterlock::new(handle, self.clock.handle().clone()))
    }

    /// Attach to a WaitCounter by name, read-only.
    ///
    /// The Attached response carries no tier, so the caller must know the name is a
    /// WaitCounter; attaching any other tier returns a handle with the wrong contract.
    pub fn attach_wait_counter(&mut self, name: &str) -> Result<AttachedWaitCounter> {
        let handle = do_attach(&mut self.conn, name, interlock_map_counter)?;
        Ok(AttachedWaitCounter::new(handle))
    }

    /// Create a WaitCounter (tier 1) watching `watched_word` of `watched_name`.
    pub fn create_wait_counter(
        &mut self,
        name: &str,
        watched_name: &str,
        watched_word: WatchedWord,
    ) -> Result<WaitCounter> {
        let mut req = create_request(name, 1);
        if let Request::CreateInterlock {
            watched_name: wn,
            watched_word: ww,
            ..
        } = &mut req
        {
            *wn = Some(watched_name.to_string());
            *ww = Some(watched_word.to_u8());
        }
        let handle = self.do_create(req, interlock_map_counter)?;
        Ok(WaitCounter::new(handle, &self.keepalive))
    }

    /// Create a WaitTimer (tier 2) watching the clock, with this client's timeout policy and
    /// fatal-margin floor.
    pub fn create_wait_timer(&mut self, name: &str) -> Result<WaitTimer> {
        let handle = self.do_create(create_request(name, 2), interlock_map_timer)?;
        Ok(WaitTimer::new(
            handle,
            self.clock.handle().clone(),
            &self.keepalive,
            self.timeout_policy,
            self.min_fatal_margin_ms,
        ))
    }

    /// Create a WaitCron (tier 3): a recurring timer on the `interval_ms` grid aligned to the
    /// monotonic epoch.
    pub fn create_wait_cron(&mut self, name: &str, interval_ms: u64) -> Result<WaitCron> {
        if interval_ms == 0 {
            return Err(SdkError::InvalidRequest {
                message: "WaitCron interval_ms must be > 0".to_string(),
            });
        }
        let mut req = create_request(name, 3);
        if let Request::CreateInterlock { interval_ns, .. } = &mut req {
            *interval_ns = Some(interval_ms.saturating_mul(1_000_000));
        }
        let handle = self.do_create(req, interlock_map_cron)?;
        Ok(WaitCron::new(
            handle,
            self.clock.handle().clone(),
            &self.keepalive,
            interval_ms,
        ))
    }

    /// Create a WaitBarrier (tier 4) that fires when every `(watched_name, watched_word,
    /// threshold)` condition holds.
    pub fn create_wait_barrier(
        &mut self,
        name: &str,
        conditions: Vec<(String, WatchedWord, u64)>,
    ) -> Result<WaitBarrier> {
        let wire_conditions: Vec<(String, u8, u64)> = conditions
            .into_iter()
            .map(|(n, w, t)| (n, w.to_u8(), t))
            .collect();
        let mut req = create_request(name, 4);
        if let Request::CreateInterlock { conditions: c, .. } = &mut req {
            *c = Some(wire_conditions);
        }
        let handle = self.do_create(req, interlock_map_barrier)?;
        Ok(WaitBarrier::new(
            handle,
            self.clock.handle().clone(),
            &self.keepalive,
        ))
    }

    /// Create a ProcessClock: a bare interlock whose keepalive also stamps open_count with
    /// the clock, so other processes can read liveness and uptime.
    pub fn create_process_clock(&mut self, name: &str) -> Result<ProcessClock> {
        let handle = self.do_create(create_request(name, 0), interlock_map)?;
        Ok(ProcessClock::new(
            handle,
            self.clock.handle().clone(),
            &self.keepalive,
        ))
    }

    /// The system clock, attached on connect. Read-only.
    pub fn clock(&self) -> &ClockHandle {
        &self.clock
    }

    /// Whether the daemon connection is still open, without consuming data.
    ///
    /// A cheap probe to run before a create or attach when the daemon may have restarted:
    /// `MSG_PEEK | MSG_DONTWAIT` sees EOF (false) or EAGAIN or data (true). It does not
    /// prove the daemon is serving requests; only a completed request does.
    pub fn is_connected(&self) -> bool {
        let fd = self.conn.stream.as_raw_fd();
        let mut buf = [0u8; 1];
        let ret = unsafe {
            libc::recv(
                fd,
                buf.as_mut_ptr() as *mut libc::c_void,
                1,
                libc::MSG_PEEK | libc::MSG_DONTWAIT,
            )
        };
        if ret == 0 {
            return false;
        }
        if ret < 0 {
            let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
            return errno == libc::EAGAIN || errno == libc::EWOULDBLOCK;
        }
        true
    }

    // -- internal helpers --

    fn do_create(
        &mut self,
        req: Request,
        map_fn: fn(OwnedFd) -> std::result::Result<InterlockHandle, Condition>,
    ) -> Result<InterlockHandle> {
        let (resp, fds) = self.conn.send_recv(&req)?;
        match resp {
            Response::Created { .. } => map_handle(fds, map_fn),
            Response::Error { code, message } => Err(daemon_error(code, message)),
            _ => Err(SdkError::UnexpectedResponse {
                message: "unexpected response type for create".to_string(),
            }),
        }
    }
}

fn create_request(name: &str, tier: u8) -> Request {
    Request::CreateInterlock {
        name: name.to_string(),
        tier,
        watched_name: None,
        watched_word: None,
        interval_ns: None,
        conditions: None,
    }
}

fn do_attach(
    conn: &mut ClientConn,
    name: &str,
    map_fn: fn(OwnedFd) -> std::result::Result<InterlockHandle, Condition>,
) -> Result<InterlockHandle> {
    let req = Request::AttachInterlock {
        name: name.to_string(),
    };
    let (resp, fds) = conn.send_recv(&req)?;
    match resp {
        Response::Attached { .. } => map_handle(fds, map_fn),
        Response::Error { code, message } => Err(daemon_error(code, message)),
        _ => Err(SdkError::UnexpectedResponse {
            message: "unexpected response type for attach".to_string(),
        }),
    }
}

fn map_handle(
    fds: Vec<OwnedFd>,
    map_fn: fn(OwnedFd) -> std::result::Result<InterlockHandle, Condition>,
) -> Result<InterlockHandle> {
    let fd = extract_single_fd(fds)?;
    map_fn(fd).map_err(|e| SdkError::MmapFailed {
        message: e.to_string(),
    })
}

/// Map a daemon error code to the SDK error. Codes are defined once, in `abacus-wire`.
fn daemon_error(code: u8, message: String) -> SdkError {
    match code {
        ERR_INTERLOCK_REAPED => SdkError::InterlockReaped,
        ERR_INTERLOCK_NOT_FOUND => SdkError::InterlockNotFound { name: message },
        ERR_ALLOCATION_FAILED => SdkError::AllocationFailed { message },
        ERR_INVALID_REQUEST => SdkError::InvalidRequest { message },
        _ => SdkError::UnexpectedResponse {
            message: format!("unknown daemon error 0x{code:02x}: {message}"),
        },
    }
}

fn extract_single_fd(mut fds: Vec<OwnedFd>) -> Result<OwnedFd> {
    if fds.len() != 1 {
        return Err(SdkError::Transport(TransportError::Protocol {
            fault: ProtocolFault::Truncated {
                needed: 1,
                have: fds.len(),
            },
        }));
    }
    Ok(fds.remove(0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use abacus_core::error::AllocationStep;

    #[test]
    fn conditions_convert_to_sdk_errors() {
        assert_eq!(
            SdkError::from(Condition::InterlockReaped),
            SdkError::InterlockReaped
        );
        assert_eq!(
            SdkError::from(Condition::InterlockNotFound { name: "n".into() }),
            SdkError::InterlockNotFound { name: "n".into() }
        );
        assert!(matches!(
            SdkError::from(Condition::AllocationFailed {
                step: AllocationStep::Seal,
                errno: 1
            }),
            SdkError::AllocationFailed { message } if message.contains("Seal")
        ));
        assert_eq!(
            SdkError::from(Condition::InvalidRequest {
                message: "m".into()
            }),
            SdkError::InvalidRequest {
                message: "m".into()
            }
        );
    }

    #[test]
    fn daemon_error_codes_map() {
        assert_eq!(
            daemon_error(ERR_INTERLOCK_REAPED, String::new()),
            SdkError::InterlockReaped
        );
        assert_eq!(
            daemon_error(ERR_INTERLOCK_NOT_FOUND, "x".into()),
            SdkError::InterlockNotFound { name: "x".into() }
        );
        assert_eq!(
            daemon_error(ERR_ALLOCATION_FAILED, "a".into()),
            SdkError::AllocationFailed {
                message: "a".into()
            }
        );
        assert_eq!(
            daemon_error(ERR_INVALID_REQUEST, "i".into()),
            SdkError::InvalidRequest {
                message: "i".into()
            }
        );
        assert!(matches!(
            daemon_error(0x7f, String::new()),
            SdkError::UnexpectedResponse { .. }
        ));
    }

    #[test]
    fn every_variant_displays() {
        let variants = [
            SdkError::InterlockReaped,
            SdkError::InterlockNotFound { name: "n".into() },
            SdkError::AllocationFailed {
                message: "m".into(),
            },
            SdkError::Transport(TransportError::ConnectionClosed),
            SdkError::MmapFailed {
                message: "m".into(),
            },
            SdkError::InvalidRequest {
                message: "m".into(),
            },
            SdkError::UnexpectedResponse {
                message: "m".into(),
            },
            SdkError::DeliveryTimeout,
        ];
        for v in variants {
            assert!(!v.to_string().is_empty());
        }
    }

    #[test]
    fn connect_to_missing_socket_is_transport_error() {
        let path = std::env::temp_dir().join("abacus-no-such-socket.sock");
        let err = AbacusClient::connect(&path)
            .err()
            .expect("connect must fail");
        assert!(matches!(
            err,
            SdkError::Transport(TransportError::Io {
                operation: IoOperation::Connect,
                ..
            })
        ));
    }
}
