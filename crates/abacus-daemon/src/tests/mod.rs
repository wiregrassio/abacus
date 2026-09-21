//! L0 unit tests for abacus-daemon: registry and transport. No daemon loop, no event loop;
//! the registry is constructed directly and transport tests use a Server on a unique temp
//! path. Wire codec tests live in `abacus-wire`. One claim per test; the name is the claim.

#![allow(non_snake_case)]

mod registry;
mod transport;

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// A socket path no other test in this process uses.
fn unique_path(label: &str) -> PathBuf {
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "abacus-l0-{label}-{}-{id}.sock",
        std::process::id()
    ))
}

/// errno as set by the last libc call on this thread.
fn last_errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// Raise RLIMIT_NOFILE's soft limit to `min(to, hard)`; returns the effective soft limit.
/// The registry holds one memfd per interlock, so thousand-interlock tests need headroom
/// over the 1024 default.
fn raise_fd_limit(to: u64) -> u64 {
    let mut lim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit/setrlimit on a local struct.
    unsafe {
        assert_eq!(
            libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim),
            0,
            "getrlimit failed"
        );
        lim.rlim_cur = to.min(lim.rlim_max);
        assert_eq!(
            libc::setrlimit(libc::RLIMIT_NOFILE, &lim),
            0,
            "setrlimit failed"
        );
    }
    lim.rlim_cur
}
