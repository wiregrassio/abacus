//! L1: ProcessClock contract (SURFACE.md "ProcessClock", LIFECYCLE.md ProcessClock
//! pattern), daemon as a thread.

#![allow(non_snake_case)]

use std::time::{Duration, Instant};

use abacus_client::{WaitState, WatchedWord};
use abacus_tests::{wait_for, ThreadDaemon};

/// LIFECYCLE.md ProcessClock pattern: closed_count is the start time and never changes;
/// open_count advances with each touch; value is uptime in ms.
#[test]
fn process_clock__uptime_advances_and_start_is_fixed() {
    let d = ThreadDaemon::start("pc-uptime");
    let mut client = d.client();
    let pc = client
        .create_process_clock("pc")
        .expect("create process clock");
    let start = pc.start_time_ms();
    assert!(start > 0, "start_time_ms is zero");
    assert!(pc.last_seen_ms() >= start);
    let elapsed = wait_for(Duration::from_millis(400), Duration::from_millis(2), || {
        pc.uptime_ms() >= 50
    })
    .unwrap_or_else(|e| {
        panic!(
            "uptime never reached 50 ms: {e}; uptime={} last_seen={} start={}",
            pc.uptime_ms(),
            pc.last_seen_ms(),
            start
        )
    });
    assert!(
        elapsed < Duration::from_millis(200),
        "uptime 50 ms took {elapsed:?}"
    );
    assert_eq!(pc.start_time_ms(), start, "start time moved");
    assert_eq!(
        pc.uptime_ms(),
        (pc.last_seen_ms() - pc.start_time_ms()) as i64,
        "uptime is not last_seen - start"
    );
    assert!(!pc.is_reaped());
}

/// CONTRACTS.md UDS surface: recreating the name reaps the old process clock;
/// its touch thread observes the reap and is_reaped() turns true.
#[test]
fn process_clock__is_reaped_after_recreate() {
    let d = ThreadDaemon::start("pc-recreate");
    let mut client = d.client();
    let old = client.create_process_clock("pc").expect("create 1");
    assert!(!old.is_reaped(), "fresh process clock reports reaped");
    let _new = client.create_process_clock("pc").expect("create 2");
    let elapsed = wait_for(Duration::from_millis(300), Duration::from_millis(2), || {
        old.is_reaped()
    })
    .unwrap_or_else(|e| {
        panic!(
            "old process clock never reported reaped: {e}; uptime={}",
            old.uptime_ms()
        )
    });
    assert!(
        elapsed < Duration::from_millis(150),
        "is_reaped took {elapsed:?} (touch interval is 40 ms)"
    );
}

/// LIFECYCLE.md ProcessClock pattern: a WaitCounter watching the process clock's open_count
/// fires at an uptime threshold.
#[test]
fn process_clock__watcher_fires_at_uptime_threshold() {
    let d = ThreadDaemon::start("pc-watch");
    let mut client = d.client();
    let pc = client
        .create_process_clock("pc")
        .expect("create process clock");
    let counter = client
        .create_wait_counter("uptime-50", "pc", WatchedWord::OpenCount)
        .expect("create counter");
    let target = pc.start_time_ms() + 50;
    let t0 = Instant::now();
    let r = counter.wait_until(target, 500);
    let elapsed = t0.elapsed();
    let r = match r {
        Ok(r) => r,
        Err(e) => panic!("wait_until failed: {e}; peek={:?}", counter.peek()),
    };
    assert!(
        matches!(r.state, WaitState::Normal | WaitState::Overrun),
        "unexpected state {r:?}"
    );
    assert!(
        r.completed_at >= target,
        "completed_at {} below target {target}",
        r.completed_at
    );
    assert!(
        elapsed >= Duration::from_millis(10) && elapsed < Duration::from_millis(200),
        "uptime watcher fired after {elapsed:?}; uptime={}",
        pc.uptime_ms()
    );
}
