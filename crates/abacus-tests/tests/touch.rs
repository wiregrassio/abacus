//! L1: background touch thread TTL contract (SURFACE.md "Background touch thread",
//! keepalive TTL), daemon as a thread.

#![allow(non_snake_case)]

use std::time::{Duration, Instant};

use abacus_core::clock::monotonic_now_nanos;
use abacus_core::interlock::SENTINEL;
use abacus_tests::{attach_words, interlock_words, wait_for, ThreadDaemon};

const MS: u64 = 1_000_000;

/// The default touch thread leaves a steady-state TTL that survives a 100 ms stall
/// of the owning process (CFS quota period): the TTL floor is 200 ms (5x the 40 ms touch
/// interval). Steady state first, then the touch thread is stopped to simulate the stall;
/// the interlock is still alive 100 ms later.
#[test]
fn touch__default_ttl_survives_100ms_stall() {
    let d = ThreadDaemon::start("touch-stall");
    let mut client = d.client();
    let mut il = client.create_interlock("s").expect("create");
    let view = attach_words(d.socket_path(), "s");
    let (_, _, exp0) = interlock_words(&view);
    wait_for(Duration::from_millis(300), Duration::from_millis(5), || {
        interlock_words(&view).2 > exp0
    })
    .expect("touch thread never armed past the creation TTL");
    il.stop_touch_thread();
    let stall_start = Instant::now();
    let ttl_left_ms = (interlock_words(&view)
        .2
        .saturating_sub(monotonic_now_nanos()))
        / MS;
    let reaped = wait_for(Duration::from_millis(100), Duration::from_millis(2), || {
        interlock_words(&view).0 == SENTINEL || interlock_words(&view).2 == SENTINEL
    });
    assert!(
        reaped.is_err(),
        "reaped {reaped:?} into a 100 ms stall (TTL left at stall start: {ttl_left_ms} ms); words={:?}",
        interlock_words(&view)
    );
    assert!(
        stall_start.elapsed() >= Duration::from_millis(100),
        "stall window not observed: {:?}",
        stall_start.elapsed()
    );
    assert!(
        il.touch(50).is_ok(),
        "touch after the stall failed; words={:?}",
        interlock_words(&view)
    );
}
