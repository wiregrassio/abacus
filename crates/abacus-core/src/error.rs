//! Error vocabulary shared by the daemon and the SDK: daemon-domain conditions, transport and
//! protocol faults, and daemon startup errors.

use std::fmt;

// -- Daemon-domain conditions --

/// A daemon-domain outcome that is not success. Carried across the wire as an error response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Condition {
    /// The interlock is terminated: reaped by the daemon or freed by a client.
    InterlockReaped,
    /// No interlock with this name is in the registry.
    InterlockNotFound {
        /// The name that was looked up.
        name: String,
    },
    /// The daemon could not allocate an interlock.
    AllocationFailed {
        /// The syscall that failed.
        step: AllocationStep,
        /// The errno it reported.
        errno: i32,
    },
    /// The request was well-formed on the wire but semantically invalid.
    InvalidRequest {
        /// Why it was rejected.
        message: String,
    },
}

/// The syscall inside interlock allocation or fd handout that failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AllocationStep {
    /// `memfd_create` failed.
    MemfdCreate,
    /// `ftruncate` to the interlock size failed.
    Ftruncate,
    /// `fcntl(F_ADD_SEALS)` failed.
    Seal,
    /// `mmap` of the interlock failed.
    Mmap,
    /// `dup` of the interlock fd for handout failed.
    Dup,
}

impl fmt::Display for Condition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InterlockReaped => write!(f, "InterlockReaped"),
            Self::InterlockNotFound { name } => write!(f, "InterlockNotFound: {name}"),
            Self::AllocationFailed { step, errno } => {
                write!(f, "AllocationFailed at {step:?}, errno={errno}")
            }
            Self::InvalidRequest { message } => {
                write!(f, "InvalidRequest: {message}")
            }
        }
    }
}

impl std::error::Error for Condition {}

/// Result alias for daemon-domain operations.
pub type Result<T> = std::result::Result<T, Condition>;

// -- Transport errors --

/// The I/O operation an errno was reported from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IoOperation {
    /// `bind` on the listening socket.
    Bind,
    /// `listen` on the listening socket.
    Listen,
    /// `accept` on the listening socket.
    Accept,
    /// `connect` to the daemon socket.
    Connect,
    /// `sendmsg` with SCM_RIGHTS.
    SendMsg,
    /// `recvmsg` with SCM_RIGHTS.
    RecvMsg,
    /// `read` on a stream.
    Read,
    /// `write` on a stream.
    Write,
    /// `stat` on the socket path.
    Stat,
    /// `unlink` of the socket path.
    Unlink,
    /// Setting non-blocking mode on a stream.
    SetNonBlocking,
    /// Setting a read or write timeout on a stream.
    SetTimeout,
    /// `chmod` of the socket path.
    Chmod,
    /// `chown` of the socket path.
    Chown,
    /// `ppoll` in the daemon loop.
    Poll,
}

/// A malformed frame or payload. Any protocol fault closes the connection it arrived on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtocolFault {
    /// The payload ended before a field it declared.
    Truncated {
        /// Bytes required to decode the field being read.
        needed: usize,
        /// Bytes actually present.
        have: usize,
    },
    /// A frame or string exceeded its maximum size.
    FrameTooLarge {
        /// The declared length.
        len: usize,
        /// The maximum allowed.
        max: usize,
    },
    /// The version byte is not the one this build speaks.
    UnsupportedVersion {
        /// The version byte received.
        version: u8,
    },
    /// The tag byte names no request or response.
    UnknownTag {
        /// The tag byte received.
        tag: u8,
    },
    /// A string field was not valid UTF-8.
    InvalidUtf8,
    /// A field the tier requires was absent at encode time.
    MissingField {
        /// The field name.
        field: &'static str,
    },
    /// A response carried more fds than its tag allows (e.g. an Error frame with an fd).
    UnexpectedFd,
}

impl fmt::Display for ProtocolFault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated { needed, have } => {
                write!(f, "truncated: needed {needed}, have {have}")
            }
            Self::FrameTooLarge { len, max } => {
                write!(f, "frame too large: {len} > {max}")
            }
            Self::UnsupportedVersion { version } => {
                write!(f, "unsupported version: {version}")
            }
            Self::UnknownTag { tag } => write!(f, "unknown tag: 0x{tag:02x}"),
            Self::InvalidUtf8 => write!(f, "invalid UTF-8"),
            Self::MissingField { field } => write!(f, "missing field: {field}"),
            Self::UnexpectedFd => write!(f, "unexpected fd on response"),
        }
    }
}

/// A failure below the request level: framing, sockets, fd passing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportError {
    /// A malformed frame or payload.
    Protocol {
        /// The fault.
        fault: ProtocolFault,
    },
    /// A syscall failed.
    Io {
        /// Which operation.
        operation: IoOperation,
        /// Its errno. `ETIMEDOUT` when a client-side transport timeout elapsed.
        errno: i32,
    },
    /// The peer closed the connection.
    ConnectionClosed,
    /// The socket path is already in use.
    SocketPathOccupied {
        /// The path.
        path: String,
        /// True if a daemon answered on it; false if a non-socket object sits there.
        live_daemon: bool,
    },
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Protocol { fault } => write!(f, "protocol fault: {fault}"),
            Self::Io { operation, errno } => {
                write!(f, "I/O error in {operation:?}: errno {errno}")
            }
            Self::ConnectionClosed => write!(f, "connection closed by peer"),
            Self::SocketPathOccupied { path, live_daemon } => {
                if *live_daemon {
                    write!(f, "socket path occupied by a live daemon: {path}")
                } else {
                    write!(f, "socket path occupied by a non-socket object: {path}")
                }
            }
        }
    }
}

impl std::error::Error for TransportError {}

// -- Daemon startup errors --

/// Why the daemon could not start or keep running.
#[derive(Debug)]
pub enum StartupError {
    /// The listening socket could not be set up.
    Transport(TransportError),
    /// The clock interlock could not be allocated.
    Allocation(Condition),
    /// `ppoll` failed with an errno other than `EINTR`.
    EventLoopFailed {
        /// The errno.
        errno: i32,
    },
}

impl fmt::Display for StartupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(e) => write!(f, "transport: {e}"),
            Self::Allocation(e) => write!(f, "allocation: {e}"),
            Self::EventLoopFailed { errno } => {
                write!(f, "event loop: ppoll failed permanently, errno={errno}")
            }
        }
    }
}

impl std::error::Error for StartupError {}

impl From<TransportError> for StartupError {
    fn from(e: TransportError) -> Self {
        Self::Transport(e)
    }
}

impl From<Condition> for StartupError {
    fn from(e: Condition) -> Self {
        Self::Allocation(e)
    }
}
