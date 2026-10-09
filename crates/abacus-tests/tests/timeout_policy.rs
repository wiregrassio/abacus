//! Tests timer TTL margins and zero-duration waits.

#![allow(non_snake_case)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use abacus_client::{WaitState, MIN_FATAL_MARGIN_MS};
use abacus_core::clock::monotonic_now_nanos;
use abacus_tests::{
    attach_words, interlock_words, on_thread, recv_within, wait_for, wait_for_value, ThreadDaemon,
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
