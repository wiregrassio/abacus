//! Tests timeout-policy behavior, timer TTL margins, zero-duration waits, daemon stalls,
//! and daemon restart semantics.

#![allow(non_snake_case)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use abacus_client::{SdkError, TimeoutPolicy, WaitState, MIN_FATAL_MARGIN_MS};
use abacus_core::clock::monotonic_now_nanos;
use abacus_tests::{
    abacus_binary, attach_words, interlock_words, on_thread, recv_within, wait_for, wait_for_value,
    ProcessDaemon, ThreadDaemon,
};

const MS: u64 = 1_000_000;

/// TTL rules: wait_ms(W) arms the TTL to W + max(W, MIN_FATAL_MARGIN_MS) so the interlock
/// cannot be reaped while legitimately waiting. Above the floor the margin is 2W; at or
/// below it the margin is W + floor. Observed through expiration_ns while the wait is in
/// flight.
#[test]
fn wait_timer__ttl_is_2x_wait_or_floor() {
    let d = ThreadDaemon::start("d4-ttl");
    let mut client = d.client();
    let timer = Arc::new(client.create_wait_timer("t").expect("create timer"));
    let view = attach_words(d.socket_path(), "t");

    // 2 * W above the floor: wait_ms(100) arms 200 ms.
    let t0 = monotonic_now_nanos();
    let t = timer.clone();
    let rx = on_thread(move || t.wait_ms(100));
    let (exp, _) = wait_for_value(Duration::from_millis(50), Duration::from_millis(1), || {
        let (_, _, e) = interlock_words(&view);
        if e >= t0 + 190 * MS {
            Some(e)
        } else {
            None
        }
    })
    .unwrap_or_else(|e| {
        panic!(
            "wait_ms(100) did not arm 2W: {e}; words={:?}",
            interlock_words(&view)
        )
    });
    assert!(
        exp <= t0 + 260 * MS,
        "wait_ms(100) armed {} ms ahead, expected about 200",
        (exp - t0) / MS
    );
    let r = recv_within(&rx, Duration::from_millis(500)).expect("wait_ms(100) never returned");
    assert!(r.is_ok(), "{r:?}");

    // Floor: wait_ms(5) arms at least W + MIN_FATAL_MARGIN_MS = 105.
    let t1 = monotonic_now_nanos();
    let expected_floor_margin = 5 + MIN_FATAL_MARGIN_MS;
    let r = timer.wait_ms(5).expect("wait_ms(5)");
    assert!(
        matches!(r.state, WaitState::Normal | WaitState::Overrun),
        "{r:?}"
    );
    let (_, _, exp) = interlock_words(&view);
    assert!(
        exp >= t1 + expected_floor_margin * MS,
        "after wait_ms(5) expiration is only {} ms past the call; expected at least {expected_floor_margin}",
        exp.saturating_sub(t1) / MS
    );

    // Explicit margin: wait_ms_with_margin(5, 300) arms 300 ms.
    let t2 = monotonic_now_nanos();
    let t = timer.clone();
    let rx = on_thread(move || t.wait_ms_with_margin(5, 300));
    let (exp, _) = wait_for_value(Duration::from_millis(50), Duration::from_millis(1), || {
        let (_, _, e) = interlock_words(&view);
        if e >= t2 + 290 * MS {
            Some(e)
        } else {
            None
        }
    })
    .unwrap_or_else(|e| panic!("wait_ms_with_margin(5, 300) did not arm 300 ms: {e}"));
    assert!(
        exp <= t2 + 360 * MS,
        "margin armed {} ms, expected about 300",
        (exp - t2) / MS
    );
    let r =
        recv_within(&rx, Duration::from_millis(500)).expect("wait_ms_with_margin never returned");
    assert!(r.is_ok(), "{r:?}");
}

/// SURFACE.md WaitTimer as amended: wait_ms(0) with the clock past the last target
/// returns the current state at once instead of arming a zero deadline and aborting.
#[test]
fn wait_timer__wait_ms_zero_does_not_abort() {
    let d = ThreadDaemon::start("s6-zero");
    let mut client = d.client();
    let timer = client.create_wait_timer("z").expect("create timer");
    let created_at = client.clock().now_ms();
    wait_for(Duration::from_millis(200), Duration::from_millis(1), || {
        client.clock().now_ms() > created_at + 1
    })
    .expect("clock did not advance");
    let t0 = Instant::now();
    let r = timer.wait_ms(0);
    let elapsed = t0.elapsed();
    assert!(r.is_ok(), "wait_ms(0) returned {r:?}");
    assert!(
        elapsed < Duration::from_millis(2),
        "wait_ms(0) took {elapsed:?}"
    );
    let past = client.clock().now_ms() - 50;
    let r = timer.wait_until(past);
    assert!(r.is_ok(), "wait_until(past) returned {r:?}");
}

/// Under TimeoutPolicy::Error a daemon that stops delivering produces
/// Err(SdkError::InterlockReaped) once the clock TTL lapses, and the process lives. The
/// daemon is stopped with SIGSTOP for the duration.
#[test]
fn wait_timer__rts_timeout_is_an_error_under_error_policy() {
    let mut d = ProcessDaemon::start(&abacus_binary(), "d4-error-policy");
    let mut client = d.client();
    client.set_timeout_policy(TimeoutPolicy::Error);
    let timer = client.create_wait_timer("t").expect("create timer");
    d.kill(libc::SIGSTOP);
    let t0 = Instant::now();
    let r = timer.wait_ms(5);
    let elapsed = t0.elapsed();
    d.kill(libc::SIGCONT);
    assert!(
        matches!(r, Err(SdkError::InterlockReaped)),
        "stopped daemon: wait_ms(5) returned {r:?} after {elapsed:?}"
    );
    let expected_margin = MIN_FATAL_MARGIN_MS + 5;
    assert!(
        elapsed >= Duration::from_millis(expected_margin)
            && elapsed < Duration::from_millis(expected_margin + 40),
        "InterlockReaped after {elapsed:?}, expected about {expected_margin} ms"
    );
}

/// ARCHITECTURE.md, a daemon restart wakes nobody; the
/// waiter learns of it through InterlockReaped once the clock TTL lapses under
/// TimeoutPolicy::Error, and a fresh client connects and creates on the new daemon.
#[test]
fn daemon__restart_wakes_nobody_and_timers_time_out() {
    let mut d = ProcessDaemon::start(&abacus_binary(), "d4-restart");
    let mut client = d.client();
    client.set_timeout_policy(TimeoutPolicy::Error);
    let timer = Arc::new(client.create_wait_timer("t").expect("create timer"));
    let t = timer.clone();
    let rx = on_thread(move || {
        let t0 = Instant::now();
        (t.wait_ms(50), t0.elapsed())
    });
    std::thread::sleep(Duration::from_millis(5)); // ordering only
    d.restart();
    let (r, elapsed) = recv_within(&rx, Duration::from_millis(500)).unwrap_or_else(|e| {
        panic!(
            "waiter never returned after restart: {e}; peek={:?}",
            timer.peek()
        )
    });
    assert!(
        matches!(r, Err(SdkError::InterlockReaped)),
        "waiter across a restart returned {r:?} after {elapsed:?}"
    );
    assert!(
        elapsed >= Duration::from_millis(100) && elapsed < Duration::from_millis(200),
        "InterlockReaped after {elapsed:?}, expected about the fatal margin"
    );
    assert!(
        !client.is_connected(),
        "old connection still reports connected after restart"
    );
    let mut fresh = d.client();
    let fresh_timer = fresh
        .create_wait_timer("t2")
        .expect("create on restarted daemon");
    let r = fresh_timer.wait_ms(20).expect("wait on restarted daemon");
    assert!(
        matches!(r.state, WaitState::Normal | WaitState::Overrun),
        "{r:?}"
    );
}
