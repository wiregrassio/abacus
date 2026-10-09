//! Operations shared by every handle type, written once: reads, sentinel-aware increments,
//! and the futex wait loop with an optional overall timeout.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use abacus_core::clock::{
    classify_futex_result, futex_wait, futex_wake, futex_word, monotonic_now_nanos, ms_to_nanos,
    FutexOutcome,
};
use abacus_core::interlock::{
    interlock_extend, InterlockHandle, ReadOnlyInterlockHandle, SENTINEL,
};

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
    // closed first: open only grows, and grows before closed, so this pair never reads closed past open.
    let closed = words.closed_count.load(Ordering::Acquire);
    let open = words.open_count.load(Ordering::Acquire);
    (open, closed)
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
    // closed first: open only grows, and grows before closed, so this pair never reads closed past open.
    let closed = words.closed_count.load(Ordering::Acquire);
    let open = words.open_count.load(Ordering::Acquire);
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
    interlock_extend(handle, ms_to_nanos(ms)).map_err(SdkError::from)
}

/// Add `h` to a counter and wake its waiters. A word at SENTINEL is never written; an addition that would reach or pass SENTINEL terminates the word. Either returns `InterlockReaped`.
pub(crate) fn increment(handle: &InterlockHandle, word: Word, h: u64) -> Result<(), SdkError> {
    let w = word_of(handle, word);
    // Never touch a terminated word, and never carry a live word onto or past SENTINEL.
    let r = w.fetch_update(Ordering::AcqRel, Ordering::Acquire, |cur| {
        (cur != SENTINEL && cur < SENTINEL - h).then(|| cur + h)
    });
    match r {
        Ok(_) => {
            futex_wake(w);
            Ok(())
        }
        Err(cur) => {
            // An increment that would reach SENTINEL terminates the word, as before.
            if cur != SENTINEL {
                w.store(SENTINEL, Ordering::Release);
            }
            futex_wake(w);
            Err(SdkError::InterlockReaped)
        }
    }
}

/// Liveness gate: returns `Err(InterlockReaped)` if any word is SENTINEL, if expiration
/// has lapsed, or (when a clock handle is provided) if the daemon clock has lapsed. Called
/// at the top of every wait loop so termination is detected before a reached target can
/// be reported as success.
pub(crate) fn check_live(
    handle: &InterlockHandle,
    clock: Option<&ReadOnlyInterlockHandle>,
    now: u64,
) -> Result<(), SdkError> {
    let words = handle.words();
    if words.open_count.load(Ordering::Acquire) == SENTINEL
        || words.closed_count.load(Ordering::Acquire) == SENTINEL
    {
        return Err(SdkError::InterlockReaped);
    }
    let exp = words.expiration_ns.load(Ordering::Acquire);
    if exp == SENTINEL || exp < now {
        return Err(SdkError::InterlockReaped);
    }
    if let Some(clk) = clock {
        let clock_exp = clk.load_expiration();
        if clock_exp == SENTINEL || clock_exp < now {
            return Err(SdkError::InterlockReaped);
        }
    }
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
/// Liveness is checked before the target comparison: a terminated interlock whose word
/// already reached the target reports `InterlockReaped`, not success.
///
/// `Ok(Some(value))` when reached, `Ok(None)` when the timeout elapsed first,
/// `Err(InterlockReaped)` when the word reads SENTINEL or either expiration has lapsed.
pub(crate) fn wait_word(
    handle: &InterlockHandle,
    word: Word,
    target: u64,
    timeout: Option<Duration>,
    clock: &ReadOnlyInterlockHandle,
) -> Result<Option<u64>, SdkError> {
    let w = word_of(handle, word);
    let deadline_ns = timeout.map(|t| monotonic_now_nanos().saturating_add(t.as_nanos() as u64));
    loop {
        let now = monotonic_now_nanos();
        check_live(handle, Some(clock), now)?;
        let current = w.load(Ordering::Acquire);
        if current == SENTINEL {
            return Err(SdkError::InterlockReaped);
        }
        if current >= target {
            return Ok(Some(current));
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
        if let FutexOutcome::Fatal(errno) =
            classify_futex_result(futex_wait(w, futex_word(current), wait_ns))
        {
            return Err(SdkError::FutexFailed { errno });
        }
    }
}

/// `wait_word` for the clock handle, which is both the wait target and the liveness
/// reference. Only reads and futex-waits; safe on a `PROT_READ` mapping.
pub(crate) fn wait_word_clock(
    clock: &ReadOnlyInterlockHandle,
    word: Word,
    target: u64,
    timeout: Option<Duration>,
) -> Result<Option<u64>, SdkError> {
    let deadline_ns = timeout.map(|t| monotonic_now_nanos().saturating_add(t.as_nanos() as u64));
    loop {
        let now = monotonic_now_nanos();
        let exp = clock.load_expiration();
        if exp == SENTINEL || exp < now {
            return Err(SdkError::InterlockReaped);
        }
        let current = match word {
            Word::Open => clock.load_open(),
            Word::Closed => clock.load_closed(),
        };
        if current == SENTINEL {
            return Err(SdkError::InterlockReaped);
        }
        if current >= target {
            return Ok(Some(current));
        }
        let mut wait_ns = DEFAULT_TIMEOUT_NANOS;
        if let Some(deadline) = deadline_ns {
            if now >= deadline {
                return Ok(None);
            }
            wait_ns = wait_ns.min(deadline - now);
        }
        let futex_result = match word {
            Word::Open => clock.futex_wait_open(futex_word(current), wait_ns),
            Word::Closed => clock.futex_wait_closed(futex_word(current), wait_ns),
        };
        if let FutexOutcome::Fatal(errno) = classify_futex_result(futex_result) {
            return Err(SdkError::FutexFailed { errno });
        }
    }
}
