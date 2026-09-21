//! L1: WaitCron contract (SURFACE.md "WaitCron", LIFECYCLE.md wait contract evaluation),
//! daemon as a thread, plus one process daemon where a stall must be induced.

#![allow(non_snake_case)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use abacus_client::{SdkError, WaitState};
use abacus_tests::{abacus_binary, on_thread, recv_within, ProcessDaemon, ThreadDaemon};

/// SURFACE.md WaitCron: fires on monotonic-epoch grid lines with no phase drag. 50 fires at
/// 10 ms: last - first within one interval of 490, and every completed_at on the grid or
/// reported Overrun.
#[test]
fn wait_cron__fires_on_grid_and_does_not_drift() {
    let d = ThreadDaemon::start("cron-grid");
    let mut client = d.client();
    let cron = client.create_wait_cron("c", 10).expect("create cron");
    let mut fires = Vec::with_capacity(50);
    for i in 0..50 {
        let r = cron
            .wait()
            .unwrap_or_else(|e| panic!("wait {i} failed: {e}; peek={:?}", cron.peek()));
        fires.push(r);
    }
    let first = fires[0].completed_at;
    let last = fires[49].completed_at;
    let span = last - first;
    assert!(
        (480..=500).contains(&span),
        "grid drifted: last - first = {span} ms over 49 intervals; fires={:?}",
        fires.iter().map(|f| f.completed_at).collect::<Vec<_>>()
    );
    let off_grid_normal: Vec<_> = fires
        .iter()
        .filter(|f| f.completed_at % 10 != 0 && f.state != WaitState::Overrun)
        .map(|f| f.completed_at)
        .collect();
    assert!(
        off_grid_normal.is_empty(),
        "{} of 50 fires were off grid and reported Normal: {off_grid_normal:?}",
        off_grid_normal.len()
    );
}

/// SURFACE.md WaitCron: the daemon re-arms after each wake, so the client loops wait()
/// with no create or attach per cycle. 50 waits at 10 ms complete under 600 ms.
#[test]
fn wait_cron__wait_again_without_uds_round_trip() {
    let d = ThreadDaemon::start("cron-loop");
    let mut client = d.client();
    let cron = client.create_wait_cron("c", 10).expect("create cron");
    let t0 = Instant::now();
    for i in 0..50 {
        cron.wait()
            .unwrap_or_else(|e| panic!("wait {i} failed: {e}; peek={:?}", cron.peek()));
    }
    let elapsed = t0.elapsed();
    assert!(
        elapsed < Duration::from_millis(600),
        "50 waits took {elapsed:?}"
    );
    assert!(
        elapsed >= Duration::from_millis(490),
        "50 waits took only {elapsed:?}"
    );
}

/// SURFACE.md WaitCron as amended: a fire off the grid (completed_at not a multiple of
/// the interval) reports Overrun. The daemon is stopped for 15 ms so the next fire lands off
/// grid deterministically.
#[test]
fn wait_cron__reports_overrun_when_off_grid() {
    let mut d = ProcessDaemon::start(&abacus_binary(), "cron-offgrid");
    let mut client = d.client();
    let cron = Arc::new(client.create_wait_cron("c", 10).expect("create cron"));
    cron.wait().expect("first fire");
    let c = cron.clone();
    let rx = on_thread(move || c.wait());
    d.kill(libc::SIGSTOP);
    std::thread::sleep(Duration::from_millis(15)); // the stall under test
    d.kill(libc::SIGCONT);
    let r = recv_within(&rx, Duration::from_millis(300)).unwrap_or_else(|e| {
        panic!(
            "cron never fired after the stall: {e}; peek={:?}",
            cron.peek()
        )
    });
    let r = match r {
        Ok(r) => r,
        Err(e) => panic!("wait failed: {e}; peek={:?}", cron.peek()),
    };
    assert!(
        r.completed_at % 10 != 0,
        "stall did not push the fire off grid: {r:?}; peek={:?}",
        cron.peek()
    );
    assert_eq!(
        r.state,
        WaitState::Overrun,
        "off-grid fire at {} reported {:?}",
        r.completed_at,
        r.state
    );
}

/// After SIGKILL of the daemon, a WaitCron::wait() that has no local deadline must
/// return Err(InterlockReaped) within a bounded time (the clock TTL) instead of hanging
/// forever. Before the fix, the client keepalive re-arms the interlock's own expiration,
/// so the `exp < now` check in the wait loop never fires, and the daemon (the sole writer
/// of closed_count) is dead, so the SENTINEL check never fires either.
#[test]
fn wait_cron__daemon_death_returns_reaped_not_hang() {
    let mut d = ProcessDaemon::start(&abacus_binary(), "cron-death");
    let mut client = d.client();
    let cron = Arc::new(client.create_wait_cron("c", 10).expect("create cron"));
    // Consume one fire to confirm the cron is alive and ticking.
    cron.wait().expect("first fire");
    let c = cron.clone();
    let rx = on_thread(move || c.wait());
    // Give the wait thread time to enter the futex loop.
    std::thread::sleep(Duration::from_millis(5));
    d.kill(libc::SIGKILL);
    // The wait must return InterlockReaped within 500 ms (clock TTL + margin).
    let r = recv_within(&rx, Duration::from_millis(500)).unwrap_or_else(|e| {
        panic!(
            "cron.wait() hung after daemon SIGKILL: {e}; peek={:?}",
            cron.peek()
        )
    });
    assert!(
        matches!(r, Err(SdkError::InterlockReaped)),
        "expected InterlockReaped after daemon death, got {r:?}; peek={:?}",
        cron.peek()
    );
}

/// SURFACE.md WaitCron: after free() the next wait() returns InterlockReaped.
#[test]
fn wait_cron__free_then_wait_is_reaped() {
    let d = ThreadDaemon::start("cron-free");
    let mut client = d.client();
    let mut cron = client.create_wait_cron("c", 10).expect("create cron");
    cron.wait().expect("first fire");
    cron.free();
    let r = cron.wait();
    assert!(
        matches!(r, Err(SdkError::InterlockReaped)),
        "wait after free returned {r:?}; peek={:?}",
        cron.peek()
    );
}
