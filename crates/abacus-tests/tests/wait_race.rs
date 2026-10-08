//! L1: WaitRace contract, daemon as a thread.

#![allow(non_snake_case)]

use std::time::Duration;

use abacus_client::{SdkError, WaitRace, WaitState, WatchedWord};
use abacus_tests::{on_thread, recv_within, ThreadDaemon};

/// With targets set on every counter, wait() returns the index of the first counter whose
/// closed_count reaches open_count, and its result.
#[test]
fn wait_race__returns_first_index() {
    let d = ThreadDaemon::start("race-first");
    let mut client = d.client();
    let sources: Vec<_> = (0..3)
        .map(|i| {
            client
                .create_interlock(&format!("s{i}"))
                .expect("create source")
        })
        .collect();
    let counters: Vec<_> = (0..3)
        .map(|i| {
            client
                .create_wait_counter(&format!("c{i}"), &format!("s{i}"), WatchedWord::ClosedCount)
                .expect("create counter")
        })
        .collect();
    for (i, c) in counters.iter().enumerate() {
        let r = c.wait_until(3, 1).expect("set target");
        assert_eq!(
            r.state,
            WaitState::Timeout,
            "counter {i} fired before any advance: {r:?}"
        );
        assert_eq!(c.peek().0, 3, "counter {i} target not set: {:?}", c.peek());
    }
    let race = WaitRace::new(counters);
    assert_eq!(race.len(), 3);
    assert!(!race.is_empty());
    let rx = on_thread(move || race.wait(2000));
    std::thread::sleep(Duration::from_millis(5)); // ordering only
    sources[1].close(3).expect("close");
    let result = recv_within(&rx, Duration::from_millis(300))
        .unwrap_or_else(|e| panic!("race never returned: {e}"))
        .expect("race wait");
    let (index, result) = result.expect("race timed out unexpectedly");
    assert_eq!(index, 1, "wrong winner: {index} with {result:?}");
    assert!(
        matches!(result.state, WaitState::Normal | WaitState::Overrun),
        "winner state {result:?}"
    );
    assert_eq!(result.completed_at, 3, "{result:?}");
}

fn three_counters(
    client: &mut abacus_client::AbacusClient,
) -> (
    Vec<abacus_client::Interlock>,
    Vec<abacus_client::WaitCounter>,
) {
    let sources: Vec<_> = (0..3)
        .map(|i| {
            client
                .create_interlock(&format!("s{i}"))
                .expect("create source")
        })
        .collect();
    let counters: Vec<_> = (0..3)
        .map(|i| {
            client
                .create_wait_counter(&format!("c{i}"), &format!("s{i}"), WatchedWord::ClosedCount)
                .expect("create counter")
        })
        .collect();

    for c in &counters {
        let r = c.wait_until(3, 1).expect("set target");
        assert_eq!(r.state, WaitState::Timeout, "{r:?}");
    }
    (sources, counters)
}

#[test]
fn wait_race__reaped_counter_is_error() {
    let d = ThreadDaemon::start("s5-reaped");
    let mut client = d.client();
    let (_sources, counters) = three_counters(&mut client);
    let race = WaitRace::new(counters);
    let rx = on_thread(move || race.wait(2000));
    std::thread::sleep(Duration::from_millis(5));
    let mut client2 = d.client();
    let _replacement = client2
        .create_wait_counter("c1", "s1", WatchedWord::ClosedCount)
        .expect("recreate");
    let r = recv_within(&rx, Duration::from_millis(300)).expect("race never returned after reap");
    assert!(
        matches!(r, Err(SdkError::InterlockReaped)),
        "race with a reaped counter returned {r:?}"
    );
}

/// A closed_count advance short of delivery must not win the race. Only a counter whose
/// classify_wake returns Some (closed_count >= open_count) can win.
#[test]
fn wait_race__sub_delivery_advance_does_not_win() {
    let d = ThreadDaemon::start("race-sub");
    let mut client = d.client();
    let (sources, counters) = three_counters(&mut client);
    let race = WaitRace::new(counters);
    let rx = on_thread(move || race.wait(2000));
    std::thread::sleep(Duration::from_millis(5));
    let mut client2 = d.client();
    let attached = client2.attach_interlock("c0").expect("attach c0");
    attached.close(1).expect("bump c0");
    std::thread::sleep(Duration::from_millis(5));
    sources[1].close(3).expect("close s1");
    let result = recv_within(&rx, Duration::from_millis(300))
        .unwrap_or_else(|e| panic!("race never returned: {e}"))
        .expect("race wait");
    let (index, result) = result.expect("race timed out unexpectedly");
    assert_eq!(
        index, 1,
        "sub-delivery advance on c0 won instead of delivered c1: {result:?}"
    );
    assert!(
        matches!(result.state, WaitState::Normal | WaitState::Overrun),
        "winner should be Normal or Overrun, got {result:?}"
    );
}

/// Under Error policy, stopping the daemon causes the clock TTL to lapse. WaitRace::wait
/// must detect the dead clock and return InterlockReaped, not hang forever.
#[test]
fn wait_race__daemon_death_returns_reaped_under_error_policy() {
    let mut d = ThreadDaemon::start("race-death");
    let mut client = d.client();
    let (_sources, counters) = three_counters(&mut client);
    let race = WaitRace::new(counters);
    let rx = on_thread(move || race.wait(5000));
    std::thread::sleep(Duration::from_millis(5));
    d.stop();
    let r = recv_within(&rx, Duration::from_secs(1)).expect("race hung after daemon death");
    assert!(
        matches!(r, Err(SdkError::InterlockReaped)),
        "race after daemon death returned {r:?}, expected InterlockReaped"
    );
}

/// A delivery that lands before wait() is called must be detected immediately: the counter
/// is already in the delivered state (closed_count >= open_count).
#[test]
fn wait_race__delivery_before_wait_wins_immediately() {
    let d = ThreadDaemon::start("race-pre");
    let mut client = d.client();
    let sources: Vec<_> = (0..3)
        .map(|i| {
            client
                .create_interlock(&format!("s{i}"))
                .expect("create source")
        })
        .collect();
    let counters: Vec<_> = (0..3)
        .map(|i| {
            client
                .create_wait_counter(&format!("c{i}"), &format!("s{i}"), WatchedWord::ClosedCount)
                .expect("create counter")
        })
        .collect();
    for c in &counters {
        let r = c.wait_until(3, 1).expect("set target");
        assert_eq!(r.state, WaitState::Timeout, "{r:?}");
    }

    // Deliver counter 2 before constructing the race.
    sources[2].close(3).expect("close s2");
    // Let the daemon deliver (one tick is 1 ms; 20 ms is generous).
    std::thread::sleep(Duration::from_millis(20));
    assert_eq!(
        counters[2].peek(),
        (3, 3),
        "counter 2 should be delivered by now"
    );

    let race = WaitRace::new(counters);
    // The delivery happened before wait; it must still win.
    let result = race.wait(1000).expect("race wait").expect("race timed out");
    assert_eq!(result.0, 2, "pre-delivered counter 2 should win, got {result:?}");
    assert!(
        matches!(result.1.state, WaitState::Normal | WaitState::Overrun),
        "winner state {result:?}"
    );
}

/// wait(0) polls once and returns Ok(None) when no counter is delivered.
#[test]
fn wait_race__timeout_zero_polls_once() {
    let d = ThreadDaemon::start("race-poll");
    let mut client = d.client();
    let (_sources, counters) = three_counters(&mut client);
    let race = WaitRace::new(counters);
    let t0 = std::time::Instant::now();
    let result = race.wait(0).expect("race wait");
    let elapsed = t0.elapsed();
    assert!(result.is_none(), "no counter was delivered, expected None, got {result:?}");
    assert!(
        elapsed < Duration::from_millis(50),
        "timeout 0 should return quickly, took {elapsed:?}"
    );
}

/// wait with a short timeout returns Ok(None) when no counter fires.
#[test]
fn wait_race__timeout_expiry_returns_none() {
    let d = ThreadDaemon::start("race-exp");
    let mut client = d.client();
    let (_sources, counters) = three_counters(&mut client);
    let race = WaitRace::new(counters);
    let t0 = std::time::Instant::now();
    let result = race.wait(50).expect("race wait");
    let elapsed = t0.elapsed();
    assert!(result.is_none(), "no delivery, expected None, got {result:?}");
    assert!(
        elapsed >= Duration::from_millis(40),
        "should wait at least close to 50 ms, only waited {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_millis(200),
        "should not wait much longer than 50 ms, waited {elapsed:?}"
    );
}
