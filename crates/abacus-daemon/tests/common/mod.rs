//! Helpers shared by the L2 test binaries. A subdirectory module, so cargo does not build
//! it as a test target. Candidates for the testkit once more than one layer needs them.

#![allow(dead_code)]
#![allow(non_snake_case)]

use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use abacus_client::{AbacusClient, WaitState};
use abacus_tests::{
    describe_exit, role_args, role_command, unique_socket_path, wait_child, RawClient,
};

/// The daemon binary cargo built for this test target.
pub const BIN: &str = env!("CARGO_BIN_EXE_abacus");

/// `main.rs` default when no `--socket-path` is given.
pub const DEFAULT_SOCKET_PATH: &str = "/run/abacus-rts/abacus.sock";

pub fn bin() -> &'static Path {
    Path::new(BIN)
}

/// The daemon spawned with arbitrary arguments, stderr captured to a file. Killed on drop.
/// For argument-form and refusal tests that `ProcessDaemon` (which always passes
/// `--socket-path=`) cannot express.
pub struct RawDaemon {
    pub child: Child,
    pub stderr_path: PathBuf,
    pub socket: Option<PathBuf>,
}

impl RawDaemon {
    pub fn spawn(args: &[&str], label: &str, socket: Option<PathBuf>) -> Self {
        let stderr_path = unique_socket_path(label).with_extension("stderr");
        let stderr = File::create(&stderr_path).expect("create stderr file");
        let child = Command::new(BIN)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(stderr))
            .spawn()
            .unwrap_or_else(|e| panic!("spawn {BIN} {args:?} failed: {e}"));
        Self {
            child,
            stderr_path,
            socket,
        }
    }

    pub fn stderr(&self) -> String {
        std::fs::read_to_string(&self.stderr_path).unwrap_or_default()
    }
}

impl Drop for RawDaemon {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        let _ = std::fs::remove_file(&self.stderr_path);
        if let Some(p) = &self.socket {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// The pass condition every "survives" test shares: a fresh client can create a timer and
/// `wait_ms(wait_ms)` on it. Runs in a role child so an SDK abort (RTSTimeout) or a hung
/// create is contained: an abort shows as `signal 6`, a hang is killed at `deadline`. Never
/// call the SDK's `wait_ms` with a small budget in the test process itself; one stall would
/// take every test in the binary down.
pub fn fresh_client_waits(socket: &Path, wait_ms: u64, deadline: Duration) -> Result<(), String> {
    let mut child = role_command(
        "common::role__well_behaved_timer",
        &[
            socket.to_str().expect("utf-8 socket path"),
            &wait_ms.to_string(),
        ],
    )
    .stdout(Stdio::null())
    .stderr(Stdio::piped())
    .spawn()
    .map_err(|e| format!("spawn fresh-client role: {e}"))?;
    let status = wait_child(&mut child, deadline);
    let mut err = String::new();
    if let Some(mut s) = child.stderr.take() {
        let _ = s.read_to_string(&mut err);
    }
    let status = status?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "fresh client ended with {}: {}",
            describe_exit(&status),
            err.trim()
        ))
    }
}

/// [`fresh_client_waits`] with `wait_ms(20)` and a 3 s deadline. 20 ms leaves a 40 ms fatal
/// margin, enough that scheduling noise on a loaded host does not read as a dead daemon
/// (the fatal margin floor is what makes smaller waits fragile).
pub fn fresh_client_works(socket: &Path) -> Result<(), String> {
    fresh_client_waits(socket, 20, Duration::from_secs(3))
}

/// Role: connect to `args[0]`, create a timer, `wait_ms(args[1])`; exit 0 on delivery. Not
/// a test; spawned by `fresh_client_waits` from every binary that includes this module.
#[test]
#[ignore = "role: process entry point for common::fresh_client_waits"]
fn role__well_behaved_timer() {
    let Some(args) = role_args("common::role__well_behaved_timer") else {
        return;
    };
    let t0 = Instant::now();
    let mut client = AbacusClient::connect(Path::new(&args[0])).expect("connect");
    let timer = client
        .create_wait_timer("fresh-check")
        .expect("create timer");
    let ms: u64 = args[1].parse().expect("wait ms");
    let r = timer.wait_ms(ms).expect("wait_ms");
    eprintln!("role__well_behaved_timer: {r:?} after {:?}", t0.elapsed());
    assert!(
        matches!(r.state, WaitState::Normal | WaitState::Overrun),
        "timer state {r:?}"
    );
}

/// Read the socket until the daemon closes it (EOF or reset) or `deadline` passes.
/// `Ok(bytes)` on EOF, `Err` naming the bytes drained on timeout. Use this, not
/// `RawClient::peer_closed`, whenever a reply may still be queued: a peek sees the reply,
/// not the close behind it.
pub fn drained_to_eof(raw: &mut RawClient, deadline: Duration) -> Result<usize, String> {
    let start = Instant::now();
    let mut drained = 0usize;
    let mut buf = [0u8; 4096];
    loop {
        let remaining = deadline.saturating_sub(start.elapsed());
        if remaining.is_zero() {
            return Err(format!(
                "no EOF within {deadline:?}; drained {drained} bytes"
            ));
        }
        raw.set_read_timeout(Some(remaining.max(Duration::from_millis(1))));
        match raw.recv_raw(&mut buf) {
            Ok(0) => return Ok(drained),
            Ok(n) => drained += n,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                return Err(format!(
                    "no EOF within {deadline:?}; drained {drained} bytes"
                ))
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return Ok(drained),
        }
    }
}

/// Deadline left from `start` out of `total`, floored at 1 ms so waits never go negative.
pub fn remaining(start: Instant, total: Duration) -> Duration {
    total
        .saturating_sub(start.elapsed())
        .max(Duration::from_millis(1))
}
