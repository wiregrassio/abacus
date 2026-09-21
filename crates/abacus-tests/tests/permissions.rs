//! L1: CONTRACTS.md permission model, the runtime cells. Every "no method" cell is a
//! compile-fail doctest in `src/permissions.rs`; this file proves the cells that exist do
//! what the row says. Daemon as a thread.

#![allow(non_snake_case)]

use std::time::Duration;

use abacus_client::{SdkError, WaitState, WatchedWord};
use abacus_core::clock::monotonic_now_nanos;
use abacus_core::interlock::SENTINEL;
use abacus_tests::{attach_words, interlock_words, wait_for, ThreadDaemon};

const MS: u64 = 1_000_000;

/// Row "Creator (Interlock)": open_count r/w, closed_count r/w, expiration_ns r/w via touch,
/// free() writes SENTINEL to expiration_ns.
fn creator_interlock_row(d: &ThreadDaemon, name: &str) {
    let mut client = d.client();
    let mut il = client.create_interlock(name).expect("create");
    let view = attach_words(d.socket_path(), name);
    il.open(2).expect("open");
    il.close(1).expect("close");
    assert_eq!(il.peek(), (2, 1), "creator counters not writable");
    let (_, _, exp_before) = interlock_words(&view);
    il.touch(700).expect("creator touch");
    let (_, _, exp_after) = interlock_words(&view);
    assert!(
        exp_after > exp_before && exp_after >= monotonic_now_nanos() + 600 * MS,
        "creator touch did not extend expiration: {exp_before} -> {exp_after}"
    );
    il.free();
    let (_, _, exp) = interlock_words(&view);
    assert_eq!(
        exp, SENTINEL,
        "creator free() did not write SENTINEL to expiration_ns"
    );
}

/// Row "Creator (WaitCounter/WaitTimer)": open_count r/w (the target), closed_count r/o
/// (daemon-written, no close method), expiration_ns r/w via touch, free() writes SENTINEL
/// to expiration_ns.
fn creator_wait_types_row(d: &ThreadDaemon, prefix: &str) {
    let mut client = d.client();
    let src_name = format!("{prefix}-src");
    let counter_name = format!("{prefix}-counter");
    let timer_name = format!("{prefix}-timer");
    let src = client.create_interlock(&src_name).expect("create src");
    let mut counter = client
        .create_wait_counter(&counter_name, &src_name, WatchedWord::ClosedCount)
        .expect("create counter");
    let counter_view = attach_words(d.socket_path(), &counter_name);
    let r = counter.wait_until(7, 1).expect("set target");
    assert_eq!(r.state, WaitState::Timeout, "{r:?}");
    assert_eq!(counter.peek().0, 7, "creator could not write the target");
    src.close(7).expect("close");
    let r = counter.wait_until(7, 200).expect("delivery");
    assert_eq!(
        counter.peek().1,
        7,
        "daemon did not write closed_count: {r:?}"
    );
    let (_, _, exp_before) = interlock_words(&counter_view);
    counter.touch(700).expect("counter touch");
    let (_, _, exp_after) = interlock_words(&counter_view);
    assert!(
        exp_after > exp_before,
        "counter touch did not extend: {exp_before} -> {exp_after}"
    );
    counter.free();
    assert_eq!(
        interlock_words(&counter_view).2,
        SENTINEL,
        "counter free() did not write SENTINEL"
    );

    let mut timer = client.create_wait_timer(&timer_name).expect("create timer");
    let timer_view = attach_words(d.socket_path(), &timer_name);
    let r = timer.wait_ms(20).expect("wait_ms");
    assert!(
        r.completed_at >= timer.peek().0,
        "daemon did not stamp closed_count: {r:?}"
    );
    let (_, _, exp_before) = interlock_words(&timer_view);
    timer.touch(700).expect("timer touch");
    let (_, _, exp_after) = interlock_words(&timer_view);
    assert!(
        exp_after > exp_before,
        "timer touch did not extend: {exp_before} -> {exp_after}"
    );
    timer.free();
    assert_eq!(
        interlock_words(&timer_view).2,
        SENTINEL,
        "timer free() did not write SENTINEL"
    );
}

/// Row "Attacher (Interlock)": open_count r/w, closed_count r/w, expiration_ns r/o (no
/// touch: doctest), free() writes SENTINEL to open_count.
fn attacher_interlock_row(d: &ThreadDaemon, name: &str) {
    let mut client = d.client();
    let owner = client.create_interlock(name).expect("create");
    let attached = client.attach_interlock(name).expect("attach");
    attached.open(3).expect("open");
    attached.close(2).expect("close");
    assert_eq!(owner.peek(), (3, 2), "attacher counters not writable");
    assert_eq!(attached.value(), 1);
    attached.free();
    assert_eq!(
        owner.peek().0,
        SENTINEL,
        "attacher free() did not write SENTINEL to open_count"
    );
    wait_for(Duration::from_millis(20), Duration::from_millis(1), || {
        matches!(owner.touch(10), Err(SdkError::InterlockReaped))
    })
    .unwrap_or_else(|e| {
        panic!(
            "daemon did not finish the attacher's free: {e}; peek={:?}",
            owner.peek()
        )
    });
}

/// Row "Attacher (WaitCounter)": all words r/o; the view tracks the creator's target and
/// the daemon's delivery.
fn attacher_wait_counter_row(d: &ThreadDaemon, prefix: &str) {
    let mut client = d.client();
    let src_name = format!("{prefix}-src");
    let counter_name = format!("{prefix}-counter");
    let src = client.create_interlock(&src_name).expect("create src");
    let counter = client
        .create_wait_counter(&counter_name, &src_name, WatchedWord::ClosedCount)
        .expect("create counter");
    let view = client
        .attach_wait_counter(&counter_name)
        .expect("attach counter");
    counter.wait_until(4, 1).expect("set target");
    assert_eq!(view.peek().0, 4, "attached view does not see the target");
    src.close(4).expect("close");
    let r = counter.wait_until(4, 200).expect("delivery");
    assert_eq!(
        view.completed_at(),
        r.completed_at,
        "attached view does not see delivery"
    );
    assert_eq!(view.value(), 0);
}

/// Row "Attacher (clock)": r/o reads and futex waits work; nothing else exists (doctests).
fn attacher_clock_row(d: &ThreadDaemon) {
    let client = d.client();
    let clock = client.clock();
    let (now, start) = clock.peek();
    assert!(
        start > 0 && now >= start,
        "clock peek now={now} start={start}"
    );
    assert!(
        clock.now_ms() >= now,
        "now_ms went backward: {} < {now}",
        clock.now_ms()
    );
    assert_eq!(clock.start_time_ms(), start, "start_time_ms moved");
    assert!(
        clock.uptime_ms() >= (now - start) as i64,
        "uptime went backward"
    );
    let target = clock.now_ms() + 5;
    let v = clock.wait_open(target);
    assert!(
        matches!(v, Ok(t) if t >= target),
        "clock wait_open({target}) returned {v:?}"
    );
    let v = clock.wait_close(start);
    assert!(
        matches!(v, Ok(t) if t >= start),
        "clock wait_close({start}) returned {v:?}"
    );
}

/// CONTRACTS.md permission model: every runtime cell of every row, one daemon.
#[test]
fn permissions__table_rows() {
    let d = ThreadDaemon::start("perm-rows");
    creator_interlock_row(&d, "row1");
    creator_wait_types_row(&d, "row2");
    attacher_interlock_row(&d, "row3");
    attacher_wait_counter_row(&d, "row4");
    attacher_clock_row(&d);
}

/// Row "Creator (Interlock)".
#[test]
fn permissions__creator_interlock_row() {
    let d = ThreadDaemon::start("perm-creator-il");
    creator_interlock_row(&d, "il");
}

/// Row "Creator (WaitCounter/WaitTimer)".
#[test]
fn permissions__creator_wait_types_row() {
    let d = ThreadDaemon::start("perm-creator-wait");
    creator_wait_types_row(&d, "w");
}

/// Row "Attacher (Interlock)".
#[test]
fn permissions__attacher_interlock_row() {
    let d = ThreadDaemon::start("perm-attacher-il");
    attacher_interlock_row(&d, "il");
}

/// Row "Attacher (WaitCounter)".
#[test]
fn permissions__attacher_wait_counter_row() {
    let d = ThreadDaemon::start("perm-attacher-wc");
    attacher_wait_counter_row(&d, "wc");
}

/// Row "Attacher (clock)".
#[test]
fn permissions__attacher_clock_row() {
    let d = ThreadDaemon::start("perm-attacher-clock");
    attacher_clock_row(&d);
}
