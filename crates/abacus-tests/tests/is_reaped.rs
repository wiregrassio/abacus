//! Tests `is_reaped()` across owning handle types, replacement, and TTL expiry.

#![allow(non_snake_case)]

use std::time::Duration;

use abacus_client::WatchedWord;
use abacus_tests::{wait_for, ThreadDaemon};

const DEADLINE: Duration = Duration::from_millis(300);
const POLL: Duration = Duration::from_millis(2);

/// A live interlock reports is_reaped() == false.
#[test]
fn interlock__is_reaped_false_while_alive() {
    let d = ThreadDaemon::start("s8-alive");
    let mut client = d.client();
    let il = client.create_interlock("a").expect("create");
    let flipped = wait_for(Duration::from_millis(150), POLL, || il.is_reaped());
    assert!(
        flipped.is_err(),
        "live interlock reported reaped after {flipped:?}"
    );
}

/// After a recreate the old Interlock handle reports is_reaped() without a
/// failing call.
#[test]
fn interlock__is_reaped_after_recreate() {
    let d = ThreadDaemon::start("s8-il");
    let mut client = d.client();
    let old = client.create_interlock("n").expect("create 1");
    assert!(!old.is_reaped());
    let _new = client.create_interlock("n").expect("create 2");
    wait_for(DEADLINE, POLL, || old.is_reaped()).unwrap_or_else(|e| {
        panic!(
            "old Interlock never reported reaped: {e}; peek={:?}",
            old.peek()
        )
    });
}

/// Same for WaitCounter.
#[test]
fn wait_counter__is_reaped_after_recreate() {
    let d = ThreadDaemon::start("s8-wc");
    let mut client = d.client();
    let _src = client.create_interlock("src").expect("create src");
    let old = client
        .create_wait_counter("c", "src", WatchedWord::ClosedCount)
        .expect("create 1");
    assert!(!old.is_reaped());
    let _new = client
        .create_wait_counter("c", "src", WatchedWord::ClosedCount)
        .expect("create 2");
    wait_for(DEADLINE, POLL, || old.is_reaped()).unwrap_or_else(|e| {
        panic!(
            "old WaitCounter never reported reaped: {e}; peek={:?}",
            old.peek()
        )
    });
}

/// Same for WaitTimer.
#[test]
fn wait_timer__is_reaped_after_recreate() {
    let d = ThreadDaemon::start("s8-wt");
    let mut client = d.client();
    let old = client.create_wait_timer("t").expect("create 1");
    assert!(!old.is_reaped());
    let _new = client.create_wait_timer("t").expect("create 2");
    wait_for(DEADLINE, POLL, || old.is_reaped()).unwrap_or_else(|e| {
        panic!(
            "old WaitTimer never reported reaped: {e}; peek={:?}",
            old.peek()
        )
    });
}

/// Same for WaitCron.
#[test]
fn wait_cron__is_reaped_after_recreate() {
    let d = ThreadDaemon::start("s8-cron");
    let mut client = d.client();
    let old = client.create_wait_cron("c", 10).expect("create 1");
    assert!(!old.is_reaped());
    let _new = client.create_wait_cron("c", 10).expect("create 2");
    wait_for(DEADLINE, POLL, || old.is_reaped()).unwrap_or_else(|e| {
        panic!(
            "old WaitCron never reported reaped: {e}; peek={:?}",
            old.peek()
        )
    });
}

/// Same for WaitBarrier.
#[test]
fn wait_barrier__is_reaped_after_recreate() {
    let d = ThreadDaemon::start("s8-wb");
    let mut client = d.client();
    let _a = client.create_interlock("a").expect("create a");
    let conditions = vec![("a".to_string(), WatchedWord::ClosedCount, 100)];
    let old = client
        .create_wait_barrier("b", conditions.clone())
        .expect("create 1");
    assert!(!old.is_reaped());
    let _new = client
        .create_wait_barrier("b", conditions)
        .expect("create 2");
    wait_for(DEADLINE, POLL, || old.is_reaped()).unwrap_or_else(|e| {
        panic!(
            "old WaitBarrier never reported reaped: {e}; peek={:?}",
            old.peek()
        )
    });
}

/// A TTL lapse (touch thread stopped) is also reported.
#[test]
fn interlock__is_reaped_after_ttl_lapse() {
    let d = ThreadDaemon::start("s8-lapse");
    let mut client = d.client();
    let mut il = client.create_interlock("l").expect("create");
    il.stop_touch_thread();
    wait_for(Duration::from_millis(400), POLL, || il.is_reaped()).unwrap_or_else(|e| {
        panic!(
            "lapsed interlock never reported reaped: {e}; peek={:?}",
            il.peek()
        )
    });
}
