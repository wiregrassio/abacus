//! Operations shared by every handle type, written once: reads, sentinel-aware increments,
//! and the futex wait loop with an optional overall timeout.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use abacus_core::clock::{futex_wait, futex_wake, futex_word, monotonic_now_nanos, ms_to_nanos};
use abacus_core::interlock::{interlock_arm, InterlockHandle, SENTINEL};

use crate::client::SdkError;
use crate::types::{interlock_state, InterlockState, DEFAULT_TIMEOUT_NANOS};

/// One of the two futex-waitable words.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Word {
    Open,
    Closed,
}

fn word_of(handle: &InterlockHandle, word: Word) -> &AtomicU64 {
    match word {
        Word::Open => &handle.words().open_count,
        Word::Closed => &handle.words().closed_count,
    }
}

/// Read both counters: (open_count, closed_count).
pub(crate) fn peek(handle: &InterlockHandle) -> (u64, u64) {
    let words = handle.words();
    (
        words.open_count.load(Ordering::Acquire),
        words.closed_count.load(Ordering::Acquire),
    )
}

/// Signed difference open_count minus closed_count, wrapping. Counters above 2^63 are outside
/// the representable range; no defined tier reaches them.
pub(crate) fn value(handle: &InterlockHandle) -> i64 {
    let (open, closed) = peek(handle);
    (open as i64).wrapping_sub(closed as i64)
}

/// The lifecycle state, read against CLOCK_MONOTONIC.
pub(crate) fn state(handle: &InterlockHandle) -> InterlockState {
    let words = handle.words();
    let open = words.open_count.load(Ordering::Acquire);
    let closed = words.closed_count.load(Ordering::Acquire);
    let expiration_ns = words.expiration_ns.load(Ordering::Acquire);
    interlock_state(open, closed, expiration_ns, monotonic_now_nanos())
}

/// Read expiration_ns.
pub(crate) fn expiration_ns(handle: &InterlockHandle) -> u64 {
    handle.words().expiration_ns.load(Ordering::Acquire)
}

/// Read closed_count.
pub(crate) fn completed_at(handle: &InterlockHandle) -> u64 {
    handle.words().closed_count.load(Ordering::Acquire)
}

/// Extend expiration to max(current, now + ms).
pub(crate) fn touch(handle: &InterlockHandle, ms: u64) -> Result<(), SdkError> {
    interlock_arm(handle, ms_to_nanos(ms)).map_err(SdkError::from)
}

/// Add `h` to a counter and wake its waiters. Sentinel-aware: if the word was already at
/// SENTINEL, or the addition would carry it past SENTINEL, the sentinel is restored and
/// `InterlockReaped` is returned, so a free() by another holder cannot be undone by a
/// wrapping increment. Restoring is idempotent, so the race with a concurrent free is benign.
pub(crate) fn increment(handle: &InterlockHandle, word: Word, h: u64) -> Result<(), SdkError> {
    let w = word_of(handle, word);
    let old = w.fetch_add(h, Ordering::AcqRel);
    if old == SENTINEL || old >= SENTINEL - h {
        w.store(SENTINEL, Ordering::Release);
        futex_wake(w);
        return Err(SdkError::InterlockReaped);
    }
    futex_wake(w);
    Ok(())
}

/// Block until `word` reaches `target`. `timeout` bounds the whole wait; `None` blocks
/// indefinitely, re-checking reaped state every `DEFAULT_TIMEOUT_NANOS`.
///
/// `clock` is the daemon-owned clock interlock (registry id 0). Its expiration is checked
/// on every loop iteration: if it has lapsed, the daemon is dead and `InterlockReaped` is
/// returned. The interlock's own expiration is also checked (it catches the case where the
/// creator stopped its keepalive).
///
/// `Ok(Some(value))` when reached, `Ok(None)` when the timeout elapsed first,
/// `Err(InterlockReaped)` when the word reads SENTINEL or either expiration has lapsed.
pub(crate) fn wait_word(
    handle: &InterlockHandle,
    word: Word,
    target: u64,
    timeout: Option<Duration>,
    clock: &InterlockHandle,
) -> Result<Option<u64>, SdkError> {
    let w = word_of(handle, word);
    let deadline_ns = timeout.map(|t| monotonic_now_nanos().saturating_add(t.as_nanos() as u64));
    loop {
        let current = w.load(Ordering::Acquire);
        if current == SENTINEL {
            return Err(SdkError::InterlockReaped);
        }
        if current >= target {
            return Ok(Some(current));
        }
        let now = monotonic_now_nanos();
        let exp = handle.words().expiration_ns.load(Ordering::Acquire);
        if exp == SENTINEL || exp < now {
            return Err(SdkError::InterlockReaped);
        }
        // Check the daemon-owned clock's expiration. The client keepalive cannot re-arm
        // the clock (it is read-only), so after daemon death this fires within one clock TTL.
        let clock_exp = clock.words().expiration_ns.load(Ordering::Acquire);
        if clock_exp != SENTINEL && clock_exp < now {
            return Err(SdkError::InterlockReaped);
        }
        let mut wait_ns = DEFAULT_TIMEOUT_NANOS;
        if let Some(deadline) = deadline_ns {
            if now >= deadline {
                return Ok(None);
            }
            wait_ns = wait_ns.min(deadline - now);
        }
        // Low 32 bits only (kernel constraint). An increment that is an exact multiple of
        // 2^32 landing in the load-to-syscall window costs one poll cadence, nothing more.
        let _ = futex_wait(w, futex_word(current), wait_ns);
    }
}
