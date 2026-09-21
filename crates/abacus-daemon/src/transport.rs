//! Server-side UDS transport: the listening socket and per-client connections.

use std::os::fd::{AsRawFd, BorrowedFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

use abacus_core::error::{IoOperation, TransportError};
use abacus_wire::{
    decode_request, encode_response, send_frame_with_fds, write_all, FrameReader, ReadOutcome,
    Request, Response,
};

const LISTEN_BACKLOG: i32 = 64;

/// Ownership and permission bits applied to the socket file after bind.
#[derive(Debug, Clone)]
pub struct SocketOptions {
    /// Permission bits, e.g. `0o660`.
    pub mode: u32,
    /// Group name to chown the socket to. `None` leaves the group as created.
    pub group: Option<String>,
}

impl Default for SocketOptions {
    fn default() -> Self {
        Self {
            mode: 0o660,
            group: None,
        }
    }
}

// -- Server --

/// The listening socket. Removes the socket file on drop.
#[derive(Debug)]
pub struct Server {
    listener: UnixListener,
    path: PathBuf,
}

impl Server {
    /// Bind and listen on `path`, replacing a stale socket file and refusing a live one.
    /// The socket file gets `options.mode` and, if set, `options.group` after binding.
    pub fn create(path: &Path, options: &SocketOptions) -> Result<Self, TransportError> {
        if path.exists() {
            if is_socket_file(path)? {
                if probe_connect_succeeds(path) {
                    return Err(TransportError::SocketPathOccupied {
                        path: path.to_string_lossy().into_owned(),
                        live_daemon: true,
                    });
                }
                let _ = std::fs::remove_file(path);
            } else {
                return Err(TransportError::SocketPathOccupied {
                    path: path.to_string_lossy().into_owned(),
                    live_daemon: false,
                });
            }
        }

        let listener = UnixListener::bind(path).map_err(|e| TransportError::Io {
            operation: IoOperation::Bind,
            errno: e.raw_os_error().unwrap_or(0),
        })?;
        let server = Self {
            listener,
            path: path.to_path_buf(),
        };

        server.apply_socket_options(options)?;

        server
            .listener
            .set_nonblocking(true)
            .map_err(|e| TransportError::Io {
                operation: IoOperation::SetNonBlocking,
                errno: e.raw_os_error().unwrap_or(0),
            })?;

        let rc = unsafe { libc::listen(server.listener.as_raw_fd(), LISTEN_BACKLOG) };
        if rc != 0 {
            return Err(TransportError::Io {
                operation: IoOperation::Listen,
                errno: last_errno(),
            });
        }

        Ok(server)
    }

    fn apply_socket_options(&self, options: &SocketOptions) -> Result<(), TransportError> {
        use std::os::unix::fs::PermissionsExt;
        if let Some(group) = &options.group {
            let gid = resolve_group(group).ok_or(TransportError::Io {
                operation: IoOperation::Chown,
                errno: libc::EINVAL,
            })?;
            let c_path =
                std::ffi::CString::new(self.path.as_os_str().as_encoded_bytes()).map_err(|_| {
                    TransportError::Io {
                        operation: IoOperation::Chown,
                        errno: libc::EINVAL,
                    }
                })?;
            let rc = unsafe { libc::chown(c_path.as_ptr(), u32::MAX, gid) };
            if rc != 0 {
                return Err(TransportError::Io {
                    operation: IoOperation::Chown,
                    errno: last_errno(),
                });
            }
        }
        std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(options.mode)).map_err(
            |e| TransportError::Io {
                operation: IoOperation::Chmod,
                errno: e.raw_os_error().unwrap_or(0),
            },
        )
    }

    /// Accept one pending connection, or `None` if there is none.
    pub fn try_accept(&self) -> Result<Option<Connection>, TransportError> {
        match self.listener.accept() {
            Ok((stream, _)) => Ok(Some(Connection::new(stream))),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
            Err(e) => Err(TransportError::Io {
                operation: IoOperation::Accept,
                errno: e.raw_os_error().unwrap_or(0),
            }),
        }
    }

    /// The listening fd, for the poll set.
    pub fn as_raw_fd(&self) -> RawFd {
        self.listener.as_raw_fd()
    }

    /// The socket path.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn resolve_group(name: &str) -> Option<libc::gid_t> {
    let c_name = std::ffi::CString::new(name).ok()?;
    let mut grp: libc::group = unsafe { std::mem::zeroed() };
    let mut buf = vec![0u8; 4096];
    let mut result: *mut libc::group = std::ptr::null_mut();
    let rc = unsafe {
        libc::getgrnam_r(
            c_name.as_ptr(),
            &mut grp,
            buf.as_mut_ptr() as *mut libc::c_char,
            buf.len(),
            &mut result,
        )
    };
    if rc != 0 || result.is_null() {
        return None;
    }
    Some(grp.gr_gid)
}

// -- Connection --

/// One accepted client. Non-blocking; requests are assembled from whatever bytes each poll
/// delivers and decoded only when a frame is complete.
#[derive(Debug)]
pub struct Connection {
    stream: UnixStream,
    reader: FrameReader,
}

impl Connection {
    fn new(stream: UnixStream) -> Self {
        Self {
            stream,
            reader: FrameReader::new(),
        }
    }

    /// Set the stream to non-blocking mode.
    pub fn set_nonblocking(&self, nonblocking: bool) -> Result<(), TransportError> {
        self.stream
            .set_nonblocking(nonblocking)
            .map_err(|e| TransportError::Io {
                operation: IoOperation::SetNonBlocking,
                errno: e.raw_os_error().unwrap_or(0),
            })
    }

    /// The client fd, for the poll set.
    pub fn as_raw_fd(&self) -> RawFd {
        self.stream.as_raw_fd()
    }

    /// Read once from the socket into the receive buffer. Never blocks.
    pub fn read_available(&mut self) -> Result<ReadOutcome, TransportError> {
        self.reader.fill(&mut self.stream)
    }

    /// Decode the next complete request in the buffer, if any.
    pub fn next_request(&mut self) -> Result<Option<Request>, TransportError> {
        match self.reader.next_frame()? {
            Some(payload) => decode_request(&payload)
                .map(Some)
                .map_err(|fault| TransportError::Protocol { fault }),
            None => Ok(None),
        }
    }

    /// Send a response with no fd.
    pub fn send_response(&mut self, resp: &Response) -> Result<(), TransportError> {
        let frame = encode_response(resp).map_err(|fault| TransportError::Protocol { fault })?;
        write_all(&mut self.stream, &frame)
    }

    /// Send a response with fds attached.
    pub fn send_response_with_fds(
        &mut self,
        resp: &Response,
        fds: &[BorrowedFd<'_>],
    ) -> Result<(), TransportError> {
        let frame = encode_response(resp).map_err(|fault| TransportError::Protocol { fault })?;
        send_frame_with_fds(&mut self.stream, &frame, fds)
    }
}

// -- helpers --

fn last_errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

fn is_socket_file(path: &Path) -> Result<bool, TransportError> {
    use std::os::unix::fs::FileTypeExt;
    let meta = std::fs::metadata(path).map_err(|e| TransportError::Io {
        operation: IoOperation::Stat,
        errno: e.raw_os_error().unwrap_or(0),
    })?;
    Ok(meta.file_type().is_socket())
}

fn probe_connect_succeeds(path: &Path) -> bool {
    UnixStream::connect(path).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn tmp_path(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "abacus-transport-{label}-{}-{}.sock",
            std::process::id(),
            abacus_core::clock::monotonic_now_nanos()
        ))
    }

    #[test]
    fn socket_gets_requested_mode() {
        let path = tmp_path("mode");
        let opts = SocketOptions {
            mode: 0o600,
            group: None,
        };
        let server = Server::create(&path, &opts).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        drop(server);
        assert!(!path.exists(), "socket file removed on drop");
    }

    #[test]
    fn live_socket_is_refused_and_stale_is_replaced() {
        let path = tmp_path("occupied");
        let first = Server::create(&path, &SocketOptions::default()).unwrap();
        let err = Server::create(&path, &SocketOptions::default()).unwrap_err();
        assert!(matches!(
            err,
            TransportError::SocketPathOccupied {
                live_daemon: true,
                ..
            }
        ));
        drop(first);
        let stale = std::os::unix::net::UnixListener::bind(&path).unwrap();
        drop(stale);
        assert!(path.exists());
        let second = Server::create(&path, &SocketOptions::default());
        assert!(second.is_ok(), "stale socket file should be replaced");
    }

    #[test]
    fn non_socket_file_is_refused() {
        let path = tmp_path("plainfile");
        std::fs::write(&path, b"x").unwrap();
        let err = Server::create(&path, &SocketOptions::default()).unwrap_err();
        assert!(matches!(
            err,
            TransportError::SocketPathOccupied {
                live_daemon: false,
                ..
            }
        ));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn unknown_group_is_an_error() {
        let path = tmp_path("group");
        let opts = SocketOptions {
            mode: 0o660,
            group: Some("abacus-no-such-group-xyz".into()),
        };
        let err = Server::create(&path, &opts).unwrap_err();
        assert!(matches!(
            err,
            TransportError::Io {
                operation: IoOperation::Chown,
                ..
            }
        ));
        let _ = std::fs::remove_file(&path);
    }
}
