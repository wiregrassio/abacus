//! L0 unit tests for abacus-client's pure logic: state derivation, wake classification,
//! wire discriminants, constants, error display. One claim per test; the name is the claim.

#![allow(non_snake_case)]

use abacus_core::error::{ProtocolFault, TransportError};
use abacus_core::interlock::SENTINEL;

use crate::client::SdkError;
use crate::types::{
    classify_wake, interlock_state, InterlockState, WaitState, WatchedWord, DEFAULT_TIMEOUT_NANOS,
    DEFAULT_TOUCH_INTERVAL_MS,
};

/// LIFECYCLE.md evaluation order and CONTRACTS.md states table: SENTINEL in any word is
/// Expired before the ttl comparison; ttl zero or negative is Expired; then value negative,
/// zero, positive is Overrun, Closed, Open.
#[test]
fn types__interlock_state_every_boundary() {
    let now = 1_000_000_000u64;
    let alive = now + 1;
    // SENTINEL in each word wins over an otherwise live interlock.
    assert_eq!(
        interlock_state(SENTINEL, 0, alive, now),
        InterlockState::Expired,
        "SENTINEL open"
    );
    assert_eq!(
        interlock_state(0, SENTINEL, alive, now),
        InterlockState::Expired,
        "SENTINEL closed"
    );
    assert_eq!(
        interlock_state(5, 3, SENTINEL, now),
        InterlockState::Expired,
        "SENTINEL expiration"
    );
    // SENTINEL expiration reads Expired at any clock, including clock zero.
    assert_eq!(
        interlock_state(5, 3, SENTINEL, 0),
        InterlockState::Expired,
        "SENTINEL at clock 0"
    );
    // ttl boundary: equal is dead, one ns ahead is alive.
    assert_eq!(
        interlock_state(0, 0, now, now),
        InterlockState::Expired,
        "ttl == 0"
    );
    assert_eq!(
        interlock_state(0, 0, now - 1, now),
        InterlockState::Expired,
        "ttl < 0"
    );
    assert_eq!(
        interlock_state(0, 0, alive, now),
        InterlockState::Closed,
        "ttl == 1 ns"
    );
    // value sign with a live ttl.
    assert_eq!(
        interlock_state(3, 5, alive, now),
        InterlockState::Overrun,
        "value < 0"
    );
    assert_eq!(
        interlock_state(5, 5, alive, now),
        InterlockState::Closed,
        "value == 0"
    );
    assert_eq!(
        interlock_state(5, 3, alive, now),
        InterlockState::Open,
        "value > 0"
    );
    // Expired ttl beats Overrun and Open alike.
    assert_eq!(
        interlock_state(3, 5, now, now),
        InterlockState::Expired,
        "overrun but expired"
    );
    assert_eq!(
        interlock_state(5, 3, now, now),
        InterlockState::Expired,
        "open but expired"
    );
    // Large counters (below i64::MAX) still derive by signed difference.
    let big = (1u64 << 62) + 7;
    assert_eq!(
        interlock_state(big, big - 1, alive, now),
        InterlockState::Open,
        "large open"
    );
    assert_eq!(
        interlock_state(big - 1, big, alive, now),
        InterlockState::Overrun,
        "large overrun"
    );
}

/// CONTRACTS.md wake outcomes: closed == open is Normal, closed > open is Overrun,
/// open > closed is None (pending, daemon has not delivered).
#[test]
fn types__classify_wake_three_cases() {
    assert_eq!(classify_wake(10, 10), Some(WaitState::Normal));
    assert_eq!(classify_wake(10, 11), Some(WaitState::Overrun));
    assert_eq!(classify_wake(10, 9), None);
    assert_eq!(
        classify_wake(0, 0),
        Some(WaitState::Normal),
        "fresh counter pair"
    );
    assert_eq!(
        classify_wake(u64::MAX - 1, u64::MAX - 1),
        Some(WaitState::Normal)
    );
    assert_eq!(classify_wake(1, 0), None, "target set, nothing delivered");
}

/// CONTRACTS.md wire-abi: WatchedWord 0 = open_count, 1 = closed_count, on the wire and in
/// the enum discriminants.
#[test]
fn types__watched_word_discriminants_match_wire() {
    assert_eq!(WatchedWord::OpenCount.to_u8(), 0);
    assert_eq!(WatchedWord::ClosedCount.to_u8(), 1);
    assert_eq!(WatchedWord::OpenCount as u8, 0);
    assert_eq!(WatchedWord::ClosedCount as u8, 1);
    assert_ne!(WatchedWord::OpenCount, WatchedWord::ClosedCount);
}

/// SURFACE.md background touch and types.rs: the default touch interval is 40 ms and the
/// poll cadence is 100 ms.
#[test]
fn types__default_constants_match_surface() {
    assert_eq!(DEFAULT_TOUCH_INTERVAL_MS, 40);
    assert_eq!(DEFAULT_TIMEOUT_NANOS, 100_000_000);
}

/// CONTRACTS.md errors and SURFACE.md error summary: every SdkError variant displays
/// non-empty text naming its payload, and TransportError converts through From.
#[test]
fn types__sdk_error_every_variant_displays() {
    let cases: Vec<(SdkError, &str)> = vec![
        (SdkError::InterlockReaped, "reaped"),
        (
            SdkError::InterlockNotFound {
                name: "cam0".into(),
            },
            "cam0",
        ),
        (
            SdkError::AllocationFailed {
                message: "memfd".into(),
            },
            "memfd",
        ),
        (
            SdkError::Transport(TransportError::ConnectionClosed),
            "closed",
        ),
        (
            SdkError::Transport(TransportError::Protocol {
                fault: ProtocolFault::InvalidUtf8,
            }),
            "UTF-8",
        ),
        (
            SdkError::MmapFailed {
                message: "ENODEV".into(),
            },
            "ENODEV",
        ),
        (
            SdkError::InvalidRequest {
                message: "reserved".into(),
            },
            "reserved",
        ),
        (
            SdkError::UnexpectedResponse {
                message: "0x99".into(),
            },
            "0x99",
        ),
        (
            SdkError::DependencyTimeout {
                waited: std::time::Duration::from_secs(1),
                missing: "dep".into(),
            },
            "dep",
        ),
        (
            SdkError::KeepaliveSpawnFailed {
                message: "EAGAIN".into(),
            },
            "EAGAIN",
        ),
        (SdkError::KeepalivePriorityFailed { errno: 1 }, "os error 1"),
    ];
    for (err, token) in cases {
        let s = err.to_string();
        assert!(!s.is_empty(), "{err:?} displays empty");
        assert!(
            s.contains(token),
            "{err:?} displays as {s:?}, missing {token:?}"
        );
        let boxed: Box<dyn std::error::Error> = Box::new(err);
        assert!(boxed.source().is_none());
    }
}

/// WaitResult and WaitState are plain values: copyable, comparable, printable.
#[test]
fn types__wait_result_is_a_plain_value() {
    let r = crate::types::WaitResult {
        completed_at: 5,
        state: WaitState::Overrun,
    };
    let copy = r;
    assert_eq!(r, copy);
    assert!(format!("{r:?}").contains("Overrun"));
    let states = [WaitState::Normal, WaitState::Overrun, WaitState::Timeout];
    for (i, a) in states.iter().enumerate() {
        for (j, b) in states.iter().enumerate() {
            assert_eq!(a == b, i == j, "{a:?} vs {b:?}");
        }
    }
}
