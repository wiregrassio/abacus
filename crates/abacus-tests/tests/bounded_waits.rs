//! Focused tests for bounded interlock, attached-interlock, and clock futex waits.

#![allow(non_snake_case)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use abacus_client::SdkError;
use abacus_tests::{on_thread, recv_within, ThreadDaemon};

/// A never-advancing interlock: wait_open_for returns Ok(None) after the timeout.
#[test]
fn interlock__wait_open_for_times_out() {
    let d = ThreadDaemon::start("s12-open");
    let mut client = d.client();
    let il = client.create_interlock("w").expect("create");
    let t0 = Instant::now();
    let r = il.wait_open_for(1000, Duration::from_millis(50));
    let elapsed = t0.elapsed();
    assert!(
        matches!(r, Ok(None)),
        "wait_open_for on a still word returned {r:?}"
    );
    assert!(
        elapsed >= Duration::from_millis(50) && elapsed < Duration::from_millis(120),
        "wait_open_for(50 ms) returned after {elapsed:?}"
    );
}

/// Same for wait_close_for.
#[test]
fn interlock__wait_close_for_times_out() {
    let d = ThreadDaemon::start("s12-close");
    let mut client = d.client();
    let il = client.create_interlock("w").expect("create");
    let t0 = Instant::now();
    let r = il.wait_close_for(1000, Duration::from_millis(50));
    let elapsed = t0.elapsed();
    assert!(
        matches!(r, Ok(None)),
        "wait_close_for on a still word returned {r:?}"
    );
    assert!(
        elapsed >= Duration::from_millis(50) && elapsed < Duration::from_millis(120),
        "wait_close_for(50 ms) returned after {elapsed:?}"
    );
}

/// An advance before the timeout returns Ok(Some(value)) at once.
#[test]
fn interlock__wait_open_for_returns_value_on_advance() {
    let d = ThreadDaemon::start("s12-advance");
    let mut client = d.client();
    let il = Arc::new(client.create_interlock("w").expect("create"));
    let waiter = il.clone();
    let rx = on_thread(move || waiter.wait_open_for(5, Duration::from_secs(2)));
    std::thread::sleep(Duration::from_millis(5)); // ordering only
    let t0 = Instant::now();
    il.open(5).expect("open");
    let r = recv_within(&rx, Duration::from_millis(300)).expect("wait_open_for never returned");
    assert!(matches!(r, Ok(Some(v)) if v >= 5), "returned {r:?}");
    assert!(
        t0.elapsed() < Duration::from_millis(50),
        "woke after {:?}",
        t0.elapsed()
    );
}

/// AttachedInterlock has the same bounded waits.
#[test]
fn attached__wait_open_for_times_out() {
    let d = ThreadDaemon::start("s12-attached");
    let mut client = d.client();
    let _owner = client.create_interlock("w").expect("create");
    let attached = client.attach_interlock("w").expect("attach");
    let t0 = Instant::now();
    let r = attached.wait_open_for(1000, Duration::from_millis(50));
    let elapsed = t0.elapsed();
    assert!(
        matches!(r, Ok(None)),
        "attached wait_open_for returned {r:?}"
    );
    assert!(
        elapsed >= Duration::from_millis(50) && elapsed < Duration::from_millis(120),
        "{elapsed:?}"
    );
}

/// ClockHandle: a target the clock reaches within the timeout returns Some.
#[test]
fn clock__wait_open_for_reaches_near_target() {
    let d = ThreadDaemon::start("s12-clock");
    let client = d.client();
    let clock = client.clock();
    let target = clock.now_ms() + 10;
    let r = clock.wait_open_for(target, Duration::from_millis(200));
    assert!(
        matches!(r, Ok(Some(v)) if v >= target),
        "clock wait_open_for returned {r:?}"
    );
    let far = clock.now_ms() + 10_000;
    let r = clock.wait_open_for(far, Duration::from_millis(20));
    assert!(
        matches!(r, Ok(None)),
        "clock wait_open_for on a far target returned {r:?}"
    );
}

/// A reaped interlock is still an error, not a timeout.
#[test]
fn interlock__wait_open_for_reaped_is_error() {
    let d = ThreadDaemon::start("s12-reaped");
    let mut client = d.client();
    let mut il = client.create_interlock("w").expect("create");
    il.stop_touch_thread();
    let r = il.wait_open_for(1000, Duration::from_millis(400));
    assert!(
        matches!(r, Err(SdkError::InterlockReaped)),
        "lapsed interlock returned {r:?}"
    );
}
