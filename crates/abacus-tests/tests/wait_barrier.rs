//! L1: WaitBarrier contract (SURFACE.md "WaitBarrier", LIFECYCLE.md wait contract
//! evaluation), daemon as a thread.

#![allow(non_snake_case)]

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use abacus_client::{SdkError, WaitState, WatchedWord};
use abacus_core::clock::monotonic_now_nanos;
use abacus_core::interlock::SENTINEL;
use abacus_daemon::registry::{Registry, Tier};
use abacus_tests::{on_thread, recv_within, wait_for, ThreadDaemon};

/// LIFECYCLE.md wait contract (WaitBarrier): fires only when every condition is met. One of
/// two conditions met: no fire in 20 ms. Both met: fires.
#[test]
fn wait_barrier__fires_only_when_all_conditions_met() {
    let d = ThreadDaemon::start("wb-all");
    let mut client = d.client();
    let a = client.create_interlock("a").expect("create a");
    let b = client.create_interlock("b").expect("create b");
    let barrier = Arc::new(
        client
            .create_wait_barrier(
                "all",
                vec![
                    ("a".to_string(), WatchedWord::ClosedCount, 3),
                    ("b".to_string(), WatchedWord::ClosedCount, 3),
                ],
            )
            .expect("create barrier"),
    );
    a.close(3).expect("close");
    let fired_early = wait_for(Duration::from_millis(20), Duration::from_millis(1), || {
        barrier.peek().1 != 0
    });
    assert!(
        fired_early.is_err(),
        "barrier fired with one of two conditions met after {fired_early:?}: peek={:?}",
        barrier.peek()
    );
    let w = barrier.clone();
    let rx = on_thread(move || w.wait());
    b.close(3).expect("close");
    let r = recv_within(&rx, Duration::from_millis(300)).unwrap_or_else(|e| {
        panic!(
            "barrier never fired with both met: {e}; peek={:?}",
            barrier.peek()
        )
    });
    match r {
        Ok(r) => assert!(r.completed_at > 0, "fired with completed_at 0: {r:?}"),
        Err(e) => panic!("wait failed: {e}; peek={:?}", barrier.peek()),
    }
}

/// LIFECYCLE.md daemon evaluation: one check per 1 ms cycle for all conditions, so the
/// barrier fires within 5 ms of the last condition being met.
#[test]
fn wait_barrier__fires_within_5ms_of_last_condition() {
    let d = ThreadDaemon::start("wb-5ms");
    let mut client = d.client();
    let a = client.create_interlock("a").expect("create a");
    let b = client.create_interlock("b").expect("create b");
    let barrier = Arc::new(
        client
            .create_wait_barrier(
                "all",
                vec![
                    ("a".to_string(), WatchedWord::OpenCount, 1),
                    ("b".to_string(), WatchedWord::OpenCount, 1),
                ],
            )
            .expect("create barrier"),
    );
    a.open(1).expect("open");
    let w = barrier.clone();
    let rx = on_thread(move || {
        let r = w.wait();
        (r, Instant::now())
    });
    std::thread::sleep(Duration::from_millis(5)); // ordering only
    let t_last = Instant::now();
    b.open(1).expect("open");
    let (r, t_fired) = recv_within(&rx, Duration::from_millis(300))
        .unwrap_or_else(|e| panic!("barrier never fired: {e}; peek={:?}", barrier.peek()));
    let latency = t_fired.duration_since(t_last);
    match r {
        Ok(r) => assert!(r.completed_at > 0, "{r:?}"),
        Err(e) => panic!("wait failed: {e}; peek={:?}", barrier.peek()),
    }
    assert!(
        latency < Duration::from_millis(5),
        "barrier fired {latency:?} after the last condition; peek={:?}",
        barrier.peek()
    );
}

/// SURFACE.md WaitBarrier: a barrier reaped mid-wait returns InterlockReaped. A/// recreate from a second client reaps it while the waiter is blocked; the reap's futex
/// wake lands on the post-wake expiration check, which reports it.
#[test]
fn wait_barrier__reaped_returns_error() {
    let d = ThreadDaemon::start("wb-reaped");
    let mut client = d.client();
    let _a = client.create_interlock("a").expect("create a");
    let conditions = vec![("a".to_string(), WatchedWord::ClosedCount, 100)];
    let barrier = Arc::new(
        client
            .create_wait_barrier("all", conditions.clone())
            .expect("create barrier"),
    );
    let w = barrier.clone();
    let rx = on_thread(move || w.wait());
    std::thread::sleep(Duration::from_millis(5)); // ordering only
    let mut client2 = d.client();
    let _replacement = client2
        .create_wait_barrier("all", conditions)
        .expect("recreate");
    let r = recv_within(&rx, Duration::from_millis(300)).unwrap_or_else(|e| {
        panic!(
            "waiter never returned after reap: {e}; peek={:?}",
            barrier.peek()
        )
    });
    assert!(
        matches!(r, Err(SdkError::InterlockReaped)),
        "reaped barrier returned {r:?}; peek={:?}",
        barrier.peek()
    );
}

/// SURFACE.md WaitBarrier: wait() on a barrier that was already reaped before the call
/// returns InterlockReaped. The entry check tests both words for SENTINEL before comparing
/// closed_count against open_count, so an already-reaped barrier never reads as a delivered
/// wait.
#[test]
fn wait_barrier__wait_on_already_reaped_returns_error() {
    let d = ThreadDaemon::start("wb-pre-reaped");
    let mut client = d.client();
    let _a = client.create_interlock("a").expect("create a");
    let conditions = vec![("a".to_string(), WatchedWord::ClosedCount, 100)];
    let barrier = client
        .create_wait_barrier("all", conditions.clone())
        .expect("create barrier");
    let mut client2 = d.client();
    let _replacement = client2
        .create_wait_barrier("all", conditions)
        .expect("recreate");
    wait_for(Duration::from_millis(20), Duration::from_millis(1), || {
        barrier.peek() == (SENTINEL, SENTINEL)
    })
    .unwrap_or_else(|e| panic!("old barrier not reaped: {e}; peek={:?}", barrier.peek()));
    let r = barrier.wait();
    assert!(
        matches!(r, Err(SdkError::InterlockReaped)),
        "wait() on an already reaped barrier returned {r:?}"
    );
}

#[test]
fn registry__evaluate_barrier_stamps_per_contract() {
    let mut reg = Registry::new().expect("registry");
    let (_, src) = reg
        .create("src".to_string(), Tier::Interlock, None, None, None, None)
        .expect("create src");
    let (_, barrier) = reg
        .create(
            "b".to_string(),
            Tier::WaitBarrier,
            None,
            None,
            None,
            Some(vec![("src".to_string(), 1u8, 3u64)]),
        )
        .expect("create barrier");
    let now_ms = || monotonic_now_nanos() / 1_000_000;
    assert_eq!(
        (
            barrier.words().open_count.load(Ordering::Acquire),
            barrier.words().closed_count.load(Ordering::Acquire)
        ),
        (1, 0),
        "init table: barrier starts at (1, 0)"
    );
    reg.evaluate_all(now_ms(), monotonic_now_nanos());
    assert_eq!(
        barrier.words().closed_count.load(Ordering::Acquire),
        0,
        "barrier fired with the condition unmet"
    );
    src.words().closed_count.store(3, Ordering::Release);
    reg.evaluate_all(now_ms(), monotonic_now_nanos());
    let (open, closed) = (
        barrier.words().open_count.load(Ordering::Acquire),
        barrier.words().closed_count.load(Ordering::Acquire),
    );
    assert_eq!(
        (open, closed),
        (1, 1),
        "fire must stamp closed_count = open_count, got ({open}, {closed})"
    );

    barrier.words().open_count.store(2, Ordering::Release);
    reg.evaluate_all(now_ms(), monotonic_now_nanos());
    let (open, closed) = (
        barrier.words().open_count.load(Ordering::Acquire),
        barrier.words().closed_count.load(Ordering::Acquire),
    );
    assert_eq!(
        (open, closed),
        (2, 2),
        "rearmed barrier did not fire exactly: ({open}, {closed})"
    );
}

#[test]
fn wait_barrier__reports_normal_and_rearms() {
    let d = ThreadDaemon::start("s4-rearm");
    let mut client = d.client();
    let a = client.create_interlock("a").expect("create a");
    let b = client.create_interlock("b").expect("create b");
    let barrier = client
        .create_wait_barrier(
            "all",
            vec![
                ("a".to_string(), WatchedWord::ClosedCount, 3),
                ("b".to_string(), WatchedWord::ClosedCount, 3),
            ],
        )
        .expect("create barrier");
    a.close(3).expect("close a");
    b.close(3).expect("close b");
    let clock_before = client.clock().now_ms();
    let r = barrier.wait().expect("first fire");
    let clock_after = client.clock().now_ms();
    assert_eq!(
        r.state,
        WaitState::Normal,
        "first fire: {r:?}; peek={:?}",
        barrier.peek()
    );
    assert!(
        r.completed_at >= clock_before && r.completed_at <= clock_after + 1,
        "completed_at {} is not the clock at wake ({clock_before}..{clock_after})",
        r.completed_at
    );
    let (open, closed) = barrier.peek();
    assert_eq!(
        open,
        closed,
        "fire did not stamp exactly: peek={:?}",
        barrier.peek()
    );

    barrier.rearm().expect("rearm");
    assert_eq!(
        barrier.peek().0,
        open + 1,
        "rearm did not add 1 to open_count"
    );
    let r = barrier.wait().expect("second fire");
    assert_eq!(r.state, WaitState::Normal, "second fire: {r:?}");
    let (open2, closed2) = barrier.peek();
    assert_eq!(
        (open2, closed2),
        (open + 1, open + 1),
        "second fire stamp: {:?}",
        barrier.peek()
    );

    let strict = client
        .create_wait_barrier(
            "strict",
            vec![("a".to_string(), WatchedWord::ClosedCount, 10)],
        )
        .expect("create strict");
    let fired = wait_for(Duration::from_millis(20), Duration::from_millis(1), || {
        strict.peek().1 != 0
    });
    assert!(
        fired.is_err(),
        "barrier fired with threshold unmet after {fired:?}"
    );
}
