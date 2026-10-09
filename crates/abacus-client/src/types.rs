//! SDK result types, policies, defaults, and pure state derivations.

use std::time::Duration;

use abacus_core::interlock::{CLOCK_TTL_NANOS, SENTINEL};

/// Internal poll cadence for bare waits (100 ms). Each futex_wait iteration uses this as its
/// timeout, after which the loop re-checks reaped state before sleeping again. Not a
/// user-visible timeout.
pub const DEFAULT_TIMEOUT_NANOS: u64 = 100_000_000;

/// Default keepalive interval in milliseconds.
pub const DEFAULT_TOUCH_INTERVAL_MS: u64 = 40;

/// Smallest TTL the keepalive writes, in milliseconds. A consumer stalled by CFS quota
/// throttling (100 ms period) must survive a full period plus scheduling slack.
pub const MIN_TOUCH_TTL_MS: u64 = 200;

/// The TTL the keepalive writes for `interval_ms`: five intervals or `MIN_TOUCH_TTL_MS`,
/// whichever is larger.
pub const fn default_touch_ttl_ms(interval_ms: u64) -> u64 {
    let five_intervals = interval_ms.saturating_mul(5);
    if five_intervals > MIN_TOUCH_TTL_MS {
        five_intervals
    } else {
        MIN_TOUCH_TTL_MS
    }
}

/// Default keepalive TTL in milliseconds: `default_touch_ttl_ms(DEFAULT_TOUCH_INTERVAL_MS)`.
pub const DEFAULT_TOUCH_TTL_MS: u64 = 200;

/// Floor on a WaitTimer's delivery-lateness tolerance in milliseconds. `wait_ms(W)` gives
/// the daemon `W + max(W, MIN_FATAL_MARGIN_MS)` before declaring it dead. The floor is at
/// least as large as the daemon's clock TTL so the WaitTimer never gives up before the
/// keepalive's clock-lapse check does.
pub const MIN_FATAL_MARGIN_MS: u64 = 100;

const _: () = assert!(MIN_FATAL_MARGIN_MS >= CLOCK_TTL_NANOS / 1_000_000);

/// Default read and write timeout on the daemon connection.
pub const DEFAULT_TRANSPORT_TIMEOUT: Duration = Duration::from_secs(1);

/// Retry interval for `connect_waiting` when the daemon or a dependency is absent.
pub const CONNECT_RETRY_INTERVAL: Duration = Duration::from_millis(100);

const _: () = assert!(DEFAULT_TOUCH_TTL_MS == default_touch_ttl_ms(DEFAULT_TOUCH_INTERVAL_MS));

/// How a process died, as observed by the keepalive thread. Under the always-abort
/// policy the keepalive aborts on either variant; they exist for the diagnostic message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Liveness {
    /// The keepalive found this client's process clock terminated (SENTINEL after an arm or
    /// stamp attempt).
    ProcessClockReaped,
    /// The keepalive found the Abacus clock's expiration lapsed.
    DaemonClockLapsed,
}

/// Scheduling the SDK applies to a client's keepalive thread. The caller chooses; the SDK
/// never picks a real-time priority on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KeepalivePriority {
    /// The default. At start the thread keeps the scheduling it inherited from the thread
    /// that started it (the one that called `connect`); set on a running thread, it moves
    /// the thread to SCHED_OTHER.
    #[default]
    Normal,
    /// SCHED_FIFO at this priority, 1 to 99. Needs CAP_SYS_NICE or an RLIMIT_RTPRIO at or
    /// above it, and, where the kernel has RT group scheduling, real-time runtime in the
    /// thread's cgroup.
    Fifo(u8),
}

/// Outcome of a wait operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitState {
    /// Delivered at the target: closed_count == open_count for counters and timers; on a
    /// grid line for crons; fired for barriers.
    Normal,
    /// Delivered late: closed_count > open_count, or a cron fired off its grid.
    Overrun,
    /// The futex timed out and the daemon never delivered.
    Timeout,
}

/// Result returned by every wait.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WaitResult {
    /// The value the daemon delivered: the watched value for counters, the clock for timers,
    /// crons, and barriers.
    pub completed_at: u64,
    /// Normal, Overrun, or Timeout.
    pub state: WaitState,
}

/// Interlock lifecycle state, derived from value (open minus closed) and TTL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterlockState {
    /// value == 0, ttl > 0: all opened work has been closed.
    Closed,
    /// value > 0, ttl > 0: work is in progress.
    Open,
    /// value < 0: closed_count exceeded open_count.
    Overrun,
    /// Any word at SENTINEL, or ttl <= 0.
    Expired,
}

/// Derive the lifecycle state from the three words and the current clock. SENTINEL is
/// checked before the TTL comparison.
pub fn interlock_state(
    open: u64,
    closed: u64,
    expiration_ns: u64,
    clock_ns: u64,
) -> InterlockState {
    if open == SENTINEL || closed == SENTINEL || expiration_ns == SENTINEL {
        return InterlockState::Expired;
    }
    if clock_ns >= expiration_ns {
        return InterlockState::Expired;
    }
    let value = (open as i64).wrapping_sub(closed as i64);
    if value < 0 {
        InterlockState::Overrun
    } else if value > 0 {
        InterlockState::Open
    } else {
        InterlockState::Closed
    }
}

/// Which word of a watched interlock to compare against the target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchedWord {
    /// Word 0.
    OpenCount = 0,
    /// Word 1.
    ClosedCount = 1,
}

impl WatchedWord {
    /// The wire discriminant.
    pub fn to_u8(self) -> u8 {
        self as u8
    }
}

/// Classify a counter or timer's words after a wake. `None` while the daemon has not
/// delivered (open_count > closed_count).
pub fn classify_wake(open: u64, closed: u64) -> Option<WaitState> {
    if closed == open {
        Some(WaitState::Normal)
    } else if closed > open {
        Some(WaitState::Overrun)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_boundaries() {
        assert_eq!(
            interlock_state(SENTINEL, 0, 100, 1),
            InterlockState::Expired
        );
        assert_eq!(
            interlock_state(0, SENTINEL, 100, 1),
            InterlockState::Expired
        );
        assert_eq!(interlock_state(0, 0, SENTINEL, 1), InterlockState::Expired);
        assert_eq!(interlock_state(0, 0, 100, 100), InterlockState::Expired);
        assert_eq!(interlock_state(0, 0, 100, 99), InterlockState::Closed);
        assert_eq!(interlock_state(1, 0, 100, 99), InterlockState::Open);
        assert_eq!(interlock_state(0, 1, 100, 99), InterlockState::Overrun);
    }

    #[test]
    fn classify_three_cases() {
        assert_eq!(classify_wake(5, 5), Some(WaitState::Normal));
        assert_eq!(classify_wake(5, 6), Some(WaitState::Overrun));
        assert_eq!(classify_wake(6, 5), None);
    }

    #[test]
    fn ttl_floor() {
        assert_eq!(default_touch_ttl_ms(40), 200);
        assert_eq!(default_touch_ttl_ms(10), 200);
        assert_eq!(default_touch_ttl_ms(100), 500);
    }

    #[test]
    fn watched_word_discriminants() {
        assert_eq!(WatchedWord::OpenCount.to_u8(), 0);
        assert_eq!(WatchedWord::ClosedCount.to_u8(), 1);
    }
}
