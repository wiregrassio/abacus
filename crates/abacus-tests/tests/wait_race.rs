//! L1: WaitRace contract (SURFACE.md "SDK-only compositions", WaitRace), daemon as a thread.

#![allow(non_snake_case)]

use std::time::Duration;

use abacus_client::{SdkError, WaitRace, WaitState, WatchedWord};
use abacus_tests::{on_thread, recv_within, ThreadDaemon};

/// SURFACE.md WaitRace: with targets set on every counter, wait() returns the index of the
/// first counter whose closed_count advances, and its result.
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
    // Set the targets: wait_until with a 1 ms cadence returns Timeout and leaves the target.
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
    let rx = on_thread(move || race.wait());
    std::thread::sleep(Duration::from_millis(5)); // ordering only
    sources[1].close(3).expect("close");
    let (index, result) = recv_within(&rx, Duration::from_millis(300))
        .unwrap_or_else(|e| panic!("race never returned: {e}"))
        .expect("race wait");
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
    let rx = on_thread(move || race.wait());
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
