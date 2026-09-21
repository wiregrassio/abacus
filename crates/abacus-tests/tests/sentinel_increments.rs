//! Tests ensuring increments cannot resurrect or wrap through sentinel values.

#![allow(non_snake_case)]

use std::sync::atomic::Ordering;
use std::time::Duration;

use abacus_client::SdkError;
use abacus_core::interlock::SENTINEL;
use abacus_tests::{attach_words, interlock_words, wait_for, ThreadDaemon};

/// CONTRACTS.md Termination as amended: an increment from SENTINEL leaves SENTINEL in
/// place and reports InterlockReaped instead of wrapping to h - 1.
#[test]
fn interlock__fetch_add_from_sentinel_does_not_unterminate() {
    let d = ThreadDaemon::start("s2-sentinel");
    let mut client = d.client();
    let owner = client.create_interlock("s").expect("create");
    let attached = client.attach_interlock("s").expect("attach");
    let view = attach_words(d.socket_path(), "s");
    attached.free();
    assert_eq!(
        interlock_words(&view).0,
        SENTINEL,
        "attacher free() did not land"
    );
    let r = owner.open(1);
    assert!(
        matches!(r, Err(SdkError::InterlockReaped)),
        "open(1) on a SENTINEL word returned {r:?}"
    );
    assert_eq!(
        interlock_words(&view).0,
        SENTINEL,
        "open(1) erased the sentinel: words={:?}",
        interlock_words(&view)
    );
    let r = attached.open(1);
    assert!(
        matches!(r, Err(SdkError::InterlockReaped)),
        "attacher open(1) on a SENTINEL word returned {r:?}"
    );
    assert_eq!(
        interlock_words(&view).0,
        SENTINEL,
        "attacher open(1) erased the sentinel"
    );
}

/// An increment that would carry a counter past SENTINEL terminates the interlock
/// (stores SENTINEL) and reports InterlockReaped.
#[test]
fn interlock__increment_crossing_sentinel_terminates() {
    let d = ThreadDaemon::start("s2-cross");
    let mut client = d.client();
    let owner = client.create_interlock("x").expect("create");
    let view = attach_words(d.socket_path(), "x");
    view.words()
        .closed_count
        .store(SENTINEL - 1, Ordering::Release);
    let r = owner.close(2);
    assert!(
        matches!(r, Err(SdkError::InterlockReaped)),
        "close(2) from SENTINEL - 1 returned {r:?}"
    );
    assert_eq!(
        interlock_words(&view).1,
        SENTINEL,
        "crossing increment did not terminate: words={:?}",
        interlock_words(&view)
    );
}

/// An attacher's free() cannot be undone by the owner's open() racing the daemon's
/// next pass: the entry is still removed and the owner sees InterlockReaped.
#[test]
fn attached__free_cannot_be_undone_by_owner_open() {
    let d = ThreadDaemon::start("s2-undo");
    let mut client = d.client();
    let owner = client.create_interlock("u").expect("create");
    let attached = client.attach_interlock("u").expect("attach");
    attached.free();
    let r = owner.open(1);
    assert!(
        matches!(r, Err(SdkError::InterlockReaped)),
        "owner open(1) after attacher free returned {r:?}"
    );
    wait_for(
        Duration::from_millis(10),
        Duration::from_micros(200),
        || {
            matches!(
                client.attach_interlock("u"),
                Err(SdkError::InterlockNotFound { .. })
            )
        },
    )
    .unwrap_or_else(|e| {
        panic!(
            "entry survived the attacher's free: {e}; peek={:?}",
            owner.peek()
        )
    });
    let touch = owner.touch(10);
    assert!(
        matches!(touch, Err(SdkError::InterlockReaped)),
        "owner touch after the undone free returned {touch:?}"
    );
}
