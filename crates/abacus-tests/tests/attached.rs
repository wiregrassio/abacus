//! L1: AttachedInterlock and AttachedWaitCounter contract (SURFACE.md "Interlock" attach,
//! CONTRACTS.md permission model and Termination), daemon as a thread.

#![allow(non_snake_case)]

use std::sync::Arc;
use std::time::Duration;

use abacus_client::{SdkError, WaitState, WatchedWord};
use abacus_core::interlock::SENTINEL;
use abacus_tests::{attach_words, interlock_words, on_thread, recv_within, wait_for, ThreadDaemon};

/// CONTRACTS.md permission model, row "Attacher (Interlock)": open_count r/w, closed_count
/// r/w, wait_open/wait_close usable, free() writes SENTINEL to open_count and the daemon
/// finishes cleanup (SURFACE.md Termination).
#[test]
fn attached__open_close_wait_and_free_via_open_count() {
    let d = ThreadDaemon::start("at-ocwf");
    let mut client = d.client();
    let owner = Arc::new(client.create_interlock("s").expect("create"));
    let attached = Arc::new(client.attach_interlock("s").expect("attach"));
    let view = attach_words(d.socket_path(), "s");

    attached.open(2).expect("open");
    assert_eq!(
        owner.peek(),
        (2, 0),
        "attacher open(2) not visible to owner"
    );
    attached.close(2).expect("close");
    assert_eq!(
        owner.peek(),
        (2, 2),
        "attacher close(2) not visible to owner"
    );
    assert_eq!(attached.value(), 0);

    let a = attached.clone();
    let rx = on_thread(move || a.wait_open(3));
    std::thread::sleep(Duration::from_millis(5)); // ordering only
    owner.open(1).expect("open");
    let r = recv_within(&rx, Duration::from_millis(300)).unwrap_or_else(|e| {
        panic!(
            "attacher wait_open(3) never returned: {e}; peek={:?}",
            owner.peek()
        )
    });
    assert!(matches!(r, Ok(3)), "attacher wait_open(3) returned {r:?}");

    let o = owner.clone();
    let rx = on_thread(move || o.wait_close(3));
    std::thread::sleep(Duration::from_millis(5)); // ordering only
    attached.close(1).expect("close");
    let r = recv_within(&rx, Duration::from_millis(300)).unwrap_or_else(|e| {
        panic!(
            "owner wait_close(3) never returned: {e}; peek={:?}",
            owner.peek()
        )
    });
    assert!(matches!(r, Ok(3)), "owner wait_close(3) returned {r:?}");

    attached.free();
    assert_eq!(
        owner.peek().0,
        SENTINEL,
        "attacher free() did not write SENTINEL to open_count: words={:?}",
        interlock_words(&view)
    );
    wait_for(Duration::from_millis(20), Duration::from_millis(1), || {
        interlock_words(&view) == (SENTINEL, SENTINEL, SENTINEL)
    })
    .unwrap_or_else(|e| {
        panic!(
            "daemon did not finish cleanup: {e}; words={:?}",
            interlock_words(&view)
        )
    });
    let touch = owner.touch(10);
    assert!(
        matches!(touch, Err(SdkError::InterlockReaped)),
        "owner touch after attacher free returned {touch:?}"
    );
}

/// CONTRACTS.md Termination: after free() the daemon observes the SENTINEL word on its next
/// pass and removes the registry entry; attach by name then fails with InterlockNotFound.
/// Threshold: 5 ms (a handful of 1 ms cycles).
#[test]
fn attached__free_then_daemon_removes_entry_within_5ms() {
    let d = ThreadDaemon::start("at-free-5ms");
    let mut client = d.client();
    let _owner = client.create_interlock("g").expect("create");
    let attached = client.attach_interlock("g").expect("attach");
    attached.free();
    let elapsed = wait_for(Duration::from_millis(5), Duration::from_micros(200), || {
        matches!(
            client.attach_interlock("g"),
            Err(SdkError::InterlockNotFound { .. })
        )
    })
    .unwrap_or_else(|e| {
        panic!(
            "entry still attachable after free: {e}; peek={:?}",
            attached.peek()
        )
    });
    assert!(
        elapsed <= Duration::from_millis(5),
        "entry removed after {elapsed:?}"
    );
}

/// CONTRACTS.md permission model, row "Attacher (WaitCounter)": read-only, and
/// completed_at reflects the daemon's delivery to the creator's counter.
#[test]
fn attached_wait_counter__completed_at_tracks_delivery() {
    let d = ThreadDaemon::start("awc-track");
    let mut client = d.client();
    let src = client.create_interlock("src").expect("create src");
    let counter = Arc::new(
        client
            .create_wait_counter("c", "src", WatchedWord::ClosedCount)
            .expect("create counter"),
    );
    let attached = client.attach_wait_counter("c").expect("attach counter");
    assert_eq!(attached.completed_at(), 0);
    assert_eq!(attached.peek(), (0, 0));
    assert_eq!(attached.value(), 0);

    let c = counter.clone();
    let rx = on_thread(move || c.wait_until(5, 500));
    std::thread::sleep(Duration::from_millis(5)); // ordering only
    src.close(5).expect("close");
    let r = recv_within(&rx, Duration::from_millis(300))
        .unwrap_or_else(|e| panic!("counter never fired: {e}; peek={:?}", counter.peek()));
    let r = match r {
        Ok(r) => r,
        Err(e) => panic!("wait_until failed: {e}; peek={:?}", counter.peek()),
    };
    assert!(
        matches!(r.state, WaitState::Normal | WaitState::Overrun),
        "unexpected state {r:?}"
    );
    assert_eq!(r.completed_at, 5);
    assert_eq!(
        attached.completed_at(),
        r.completed_at,
        "attached view completed_at={} vs delivered {}",
        attached.completed_at(),
        r.completed_at
    );
    assert_eq!(attached.peek(), (5, 5));
}
