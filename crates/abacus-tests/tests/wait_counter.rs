//! L1: WaitCounter contract (SURFACE.md "WaitCounter", CONTRACTS.md field semantics, wake
//! outcomes, TTL rules), daemon as a thread.

#![allow(non_snake_case)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use abacus_client::{SdkError, WaitResult, WaitState, WatchedWord};
use abacus_core::clock::monotonic_now_nanos;
use abacus_tests::{
    attach_words, interlock_words, on_thread, recv_within, wait_for_value, ThreadDaemon,
};

const MS: u64 = 1_000_000;

fn fires_on_word(label: &str, word: WatchedWord) {
    let d = ThreadDaemon::start(label);
    let mut client = d.client();
    let src = client.create_interlock("src").expect("create src");
    let counter = Arc::new(
        client
            .create_wait_counter("c", "src", word)
            .expect("create counter"),
    );
    let c = counter.clone();
    let rx = on_thread(move || {
        let t0 = Instant::now();
        (c.wait_until(5, 500), t0.elapsed())
    });
    std::thread::sleep(Duration::from_millis(5)); // ordering only
    let t_advance = Instant::now();
    match word {
        WatchedWord::OpenCount => src.open(5).expect("open"),
        WatchedWord::ClosedCount => src.close(5).expect("close"),
    }
    let (r, _) = recv_within(&rx, Duration::from_millis(300))
        .unwrap_or_else(|e| panic!("counter never fired: {e}; peek={:?}", counter.peek()));
    let latency = t_advance.elapsed();
    let r = match r {
        Ok(r) => r,
        Err(e) => panic!("wait_until failed: {e}; peek={:?}", counter.peek()),
    };
    assert!(
        matches!(r.state, WaitState::Normal | WaitState::Overrun),
        "unexpected state {r:?}"
    );
    assert_eq!(
        r.completed_at, 5,
        "daemon stamped {} not the watched value 5",
        r.completed_at
    );
    assert!(
        latency < Duration::from_millis(50),
        "fired {latency:?} after the advance"
    );
}

/// CONTRACTS.md daemon contract: Open WaitCounter fires when the watched closed_count
/// reaches open_count; closed_count is stamped to the watched value.
#[test]
fn wait_counter__fires_on_closed_word() {
    fires_on_word("wc-closed", WatchedWord::ClosedCount);
}

/// CONTRACTS.md daemon contract: same with watched_word = open_count.
#[test]
fn wait_counter__fires_on_open_word() {
    fires_on_word("wc-open", WatchedWord::OpenCount);
}

/// LIFECYCLE.md wait contract: a target already crossed fires on the next cycle, stamped
/// with the watched value (Overrun, since it overshot).
#[test]
fn wait_counter__already_crossed_fires_next_cycle() {
    let d = ThreadDaemon::start("wc-crossed");
    let mut client = d.client();
    let src = client.create_interlock("src").expect("create src");
    src.close(10).expect("close");
    let counter = client
        .create_wait_counter("c", "src", WatchedWord::ClosedCount)
        .expect("create counter");
    let t0 = Instant::now();
    let r = counter.wait_until(5, 100);
    let elapsed = t0.elapsed();
    let r = match r {
        Ok(r) => r,
        Err(e) => panic!("wait_until failed: {e}; peek={:?}", counter.peek()),
    };
    assert_eq!(r.state, WaitState::Overrun, "{r:?}");
    assert_eq!(r.completed_at, 10, "{r:?}");
    assert!(
        elapsed < Duration::from_millis(20),
        "took {elapsed:?}, more than a couple of cycles"
    );
}

/// SURFACE.md WaitCounter: wait_until is CAS-max; a target below the current open_count is
/// a no-op and the call returns the current state at once.
#[test]
fn wait_counter__target_below_current_returns_immediately() {
    let d = ThreadDaemon::start("wc-below");
    let mut client = d.client();
    let src = client.create_interlock("src").expect("create src");
    src.close(5).expect("close");
    let counter = client
        .create_wait_counter("c", "src", WatchedWord::ClosedCount)
        .expect("create counter");
    let first = counter.wait_until(5, 100).expect("first wait");
    assert_eq!(first.completed_at, 5, "{first:?}");
    let t0 = Instant::now();
    let r = counter.wait_until(3, 100);
    let elapsed = t0.elapsed();
    let r = match r {
        Ok(r) => r,
        Err(e) => panic!("wait_until(3) failed: {e}; peek={:?}", counter.peek()),
    };
    assert_eq!(
        counter.peek().0,
        5,
        "target was lowered: peek={:?}",
        counter.peek()
    );
    assert_eq!(
        r,
        WaitResult {
            completed_at: 5,
            state: WaitState::Normal
        },
        "{r:?}"
    );
    assert!(elapsed < Duration::from_millis(5), "took {elapsed:?}");
}

/// SURFACE.md WaitCounter: CAS-max on open_count preserves the largest target under
/// concurrent wait_until calls.
#[test]
fn wait_counter__cas_max_keeps_largest_target_under_contention() {
    let d = ThreadDaemon::start("wc-cas");
    let mut client = d.client();
    let _src = client.create_interlock("src").expect("create src");
    let counter = Arc::new(
        client
            .create_wait_counter("c", "src", WatchedWord::ClosedCount)
            .expect("create counter"),
    );
    let rxs: Vec<_> = (0..8u64)
        .map(|i| {
            let c = counter.clone();
            on_thread(move || c.wait_until(10 + i, 10))
        })
        .collect();
    for (i, rx) in rxs.iter().enumerate() {
        let r = recv_within(rx, Duration::from_millis(500))
            .unwrap_or_else(|e| panic!("thread {i} never returned: {e}"));
        match r {
            Ok(r) => assert_eq!(r.state, WaitState::Timeout, "thread {i}: {r:?}"),
            Err(e) => panic!("thread {i} failed: {e}"),
        }
    }
    assert_eq!(
        counter.peek().0,
        17,
        "largest target not kept: peek={:?}",
        counter.peek()
    );
}

/// CONTRACTS.md wake outcomes: a futex timeout on a WaitCounter is WaitState::Timeout, not
/// an error and not an abort. timeout_ms is the futex cadence.
#[test]
fn wait_counter__timeout_returns_timeout_state_not_error() {
    let d = ThreadDaemon::start("wc-timeout");
    let mut client = d.client();
    let _src = client.create_interlock("src").expect("create src");
    let counter = client
        .create_wait_counter("c", "src", WatchedWord::ClosedCount)
        .expect("create counter");
    let t0 = Instant::now();
    let r = counter.wait_until(100, 20);
    let elapsed = t0.elapsed();
    match r {
        Ok(WaitResult {
            state: WaitState::Timeout,
            completed_at,
        }) => {
            assert_eq!(completed_at, 0, "completed_at moved without delivery");
        }
        other => panic!("expected Ok(Timeout), got {other:?} after {elapsed:?}"),
    }
    assert!(
        elapsed >= Duration::from_millis(20) && elapsed < Duration::from_millis(60),
        "timeout returned after {elapsed:?}, expected about 20 ms"
    );
}

/// SURFACE.md WaitCounter: a reaped counter raises InterlockReaped instead of returning a
/// WaitResult. A recreate of the name from a second client reaps it while it
/// waits: the loop checks both words for SENTINEL on every pass and returns
/// InterlockReaped rather than classifying (SENTINEL, SENTINEL) as a delivered wait.
#[test]
fn wait_counter__reaped_returns_error() {
    let d = ThreadDaemon::start("wc-reaped");
    let mut client = d.client();
    let _src = client.create_interlock("src").expect("create src");
    let counter = Arc::new(
        client
            .create_wait_counter("c", "src", WatchedWord::ClosedCount)
            .expect("create counter"),
    );
    let c = counter.clone();
    let rx = on_thread(move || c.wait_until(100, 500));
    std::thread::sleep(Duration::from_millis(5)); // ordering only
    let mut client2 = d.client();
    let _replacement = client2
        .create_wait_counter("c", "src", WatchedWord::ClosedCount)
        .expect("recreate");
    let r = recv_within(&rx, Duration::from_millis(300)).unwrap_or_else(|e| {
        panic!(
            "waiter never returned after reap: {e}; peek={:?}",
            counter.peek()
        )
    });
    assert!(
        matches!(r, Err(SdkError::InterlockReaped)),
        "reaped counter returned {r:?}; peek={:?}",
        counter.peek()
    );
}

/// wait_until(target, 0) must return WaitState::Timeout immediately instead of blocking
/// forever. Before the fix, timeout_ms 0 converts to timeout_nanos 0, which futex_wait
/// interprets as "block indefinitely."
#[test]
fn wait_counter__wait_until_zero_timeout_returns_immediately() {
    let d = ThreadDaemon::start("wc-zero-timeout");
    let mut client = d.client();
    let _src = client.create_interlock("src").expect("create src");
    let counter = client
        .create_wait_counter("c", "src", WatchedWord::ClosedCount)
        .expect("create counter");
    let rx = on_thread(move || {
        let t0 = Instant::now();
        let r = counter.wait_until(100, 0);
        (r, t0.elapsed())
    });
    let (r, elapsed) = recv_within(&rx, Duration::from_secs(1))
        .unwrap_or_else(|e| panic!("wait_until(100, 0) blocked instead of returning: {e}"));
    match r {
        Ok(WaitResult {
            state: WaitState::Timeout,
            ..
        }) => {}
        other => panic!(
            "wait_until(100, 0) should return Timeout immediately, got {other:?} after {elapsed:?}"
        ),
    }
    assert!(
        elapsed < Duration::from_millis(50),
        "wait_until(100, 0) took {elapsed:?}, expected immediate return"
    );
}

/// CONTRACTS.md TTL rules and SURFACE.md WaitCounter: wait_until sets the TTL to
/// 2 * timeout_ms (read from expiration_ns while the wait is in flight).
#[test]
fn wait_counter__ttl_is_2x_timeout() {
    let d = ThreadDaemon::start("wc-ttl");
    let mut client = d.client();
    let _src = client.create_interlock("src").expect("create src");
    let counter = Arc::new(
        client
            .create_wait_counter("c", "src", WatchedWord::ClosedCount)
            .expect("create counter"),
    );
    let view = attach_words(d.socket_path(), "c");
    let t0 = monotonic_now_nanos();
    let c = counter.clone();
    let rx = on_thread(move || c.wait_until(100, 100));
    let (exp, _) = wait_for_value(Duration::from_millis(50), Duration::from_millis(1), || {
        let (_, _, e) = interlock_words(&view);
        if e >= t0 + 150 * MS {
            Some(e)
        } else {
            None
        }
    })
    .unwrap_or_else(|e| {
        panic!(
            "expiration never reached 2x timeout: {e}; ahead by {} ms",
            (interlock_words(&view).2.saturating_sub(t0)) / MS
        )
    });
    assert!(
        exp <= t0 + 250 * MS,
        "expiration {} ms ahead of the call, expected about 200",
        (exp - t0) / MS
    );
    let r = recv_within(&rx, Duration::from_millis(500)).expect("wait never returned");
    assert!(
        matches!(
            r,
            Ok(WaitResult {
                state: WaitState::Timeout,
                ..
            })
        ),
        "{r:?}"
    );
}

/// CONTRACTS.md create call: watched_name may be "clock"; the counter fires when the clock's
/// open_count reaches the target.
#[test]
fn wait_counter__watching_clock_by_name_works() {
    let d = ThreadDaemon::start("wc-clock");
    let mut client = d.client();
    let counter = client
        .create_wait_counter("c", "clock", WatchedWord::OpenCount)
        .expect("create counter watching clock by name");
    let target = client.clock().now_ms() + 20;
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
        elapsed >= Duration::from_millis(10) && elapsed < Duration::from_millis(100),
        "fired after {elapsed:?} for a 20 ms clock target"
    );
}
