//! Tests the stoppable daemon-loop API, socket cleanup, connection-state transition, and
//! wait-counter timeout after daemon shutdown.

#![allow(non_snake_case)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use abacus_client::{AbacusClient, WaitState, WatchedWord};
use abacus_tests::{unique_socket_path, wait_for, wait_for_daemon};

/// A thread daemon that can be stopped: the stoppable form of `ThreadDaemon`.
struct StoppableThreadDaemon {
    path: PathBuf,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl StoppableThreadDaemon {
    fn start(label: &str) -> Self {
        let path = unique_socket_path(label);
        let stop = Arc::new(AtomicBool::new(false));
        let (p, s) = (path.clone(), stop.clone());
        let thread = std::thread::Builder::new()
            .name(format!("daemon-{label}"))
            .spawn(move || {
                if let Err(e) = abacus_daemon::daemon::daemon_run(&p, &s) {
                    eprintln!("stoppable thread daemon {} failed: {e}", p.display());
                }
            })
            .expect("spawn daemon thread");
        wait_for_daemon(&path, Duration::from_secs(5))
            .unwrap_or_else(|e| panic!("daemon on {} did not come up: {e}", path.display()));
        Self {
            path,
            stop,
            thread: Some(thread),
        }
    }

    fn socket_path(&self) -> &Path {
        &self.path
    }

    /// Set the flag and join the loop. Returns how long the join took.
    fn stop(&mut self) -> Duration {
        self.stop.store(true, Ordering::Release);
        let t0 = Instant::now();
        if let Some(t) = self.thread.take() {
            t.join().expect("daemon thread panicked");
        }
        t0.elapsed()
    }
}

impl Drop for StoppableThreadDaemon {
    fn drop(&mut self) {
        self.stop();
        let _ = std::fs::remove_file(&self.path);
    }
}

/// The stop flag ends the loop within a few cycles and the socket file is removed.
#[test]
fn daemon__stop_flag_exits_loop_and_removes_socket() {
    let mut d = StoppableThreadDaemon::start("d14-stop");
    let path = d.socket_path().to_path_buf();
    assert!(path.exists());
    let took = d.stop();
    assert!(took < Duration::from_millis(50), "stop took {took:?}");
    wait_for(Duration::from_millis(100), Duration::from_millis(1), || {
        !path.exists()
    })
    .unwrap_or_else(|e| {
        panic!(
            "socket file {} still present after stop: {e}",
            path.display()
        )
    });
}

/// SURFACE.md Connection: is_connected() is true while the daemon runs and false
/// after it stops (peer closed).
#[test]
fn client__is_connected_true_then_false_after_daemon_stops() {
    let mut d = StoppableThreadDaemon::start("d14-connected");
    let client = AbacusClient::connect(d.socket_path()).expect("connect");
    assert!(
        client.is_connected(),
        "fresh connection reports disconnected"
    );
    d.stop();
    wait_for(Duration::from_millis(200), Duration::from_millis(1), || {
        !client.is_connected()
    })
    .unwrap_or_else(|e| panic!("is_connected stayed true after the daemon stopped: {e}"));
}

/// CONTRACTS.md wake outcomes: with the daemon stopped a WaitCounter's futex times out
/// and reports WaitState::Timeout (owner's decision), never an abort.
#[test]
fn wait_counter__timeout_after_daemon_stop_is_timeout_state() {
    let mut d = StoppableThreadDaemon::start("d14-counter");
    let mut client = AbacusClient::connect(d.socket_path()).expect("connect");
    let src = client.create_interlock("src").expect("create src");
    let counter = client
        .create_wait_counter("c", "src", WatchedWord::ClosedCount)
        .expect("create counter");
    d.stop();
    src.close(5).expect("close");
    let t0 = Instant::now();
    let r = counter.wait_until(5, 50).expect("wait_until");
    let elapsed = t0.elapsed();
    assert_eq!(
        r.state,
        WaitState::Timeout,
        "stopped daemon delivered: {r:?}"
    );
    assert!(
        elapsed >= Duration::from_millis(50) && elapsed < Duration::from_millis(120),
        "timeout after {elapsed:?}"
    );
}
