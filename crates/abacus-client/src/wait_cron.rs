//! WaitCron: a recurring grid-aligned timer. The daemon re-arms it after every fire, so the
//! client loops `wait()` with no UDS round-trip.

use std::sync::atomic::{AtomicU64, Ordering};

use abacus_core::clock::{
    classify_futex_result, futex_wait, futex_word, monotonic_now_nanos, FutexOutcome,
};
use abacus_core::interlock::{
    interlock_free, interlock_is_terminated, InterlockHandle, ReadOnlyInterlockHandle, SENTINEL,
};

use crate::client::SdkError;
use crate::handle_ops::{self, check_live};
use crate::touch::{Keepalive, TouchHandle};
use crate::types::{
    default_touch_ttl_ms, WaitResult, WaitState, DEFAULT_TIMEOUT_NANOS, DEFAULT_TOUCH_INTERVAL_MS,
};

/// A WaitCron on the `interval_ms` grid aligned to the monotonic epoch.
pub struct WaitCron {
    handle: InterlockHandle,
    clock: ReadOnlyInterlockHandle,
    touch: Option<TouchHandle>,
    interval_ms: u64,
    /// The closed_count from the last fire this caller observed (or the creation-time value).
    last_seen: AtomicU64,
}

impl WaitCron {
    pub(crate) fn new(
        handle: InterlockHandle,
        clock: ReadOnlyInterlockHandle,
        keepalive: &Keepalive,
        interval_ms: u64,
    ) -> Result<Self, SdkError> {
        let initial_closed = handle.words().closed_count.load(Ordering::Acquire);
        let touch = Some(keepalive.register(
            handle.clone(),
            DEFAULT_TOUCH_INTERVAL_MS,
            default_touch_ttl_ms(DEFAULT_TOUCH_INTERVAL_MS),
        )?);
        Ok(Self {
            handle,
            clock,
            touch,
            interval_ms,
            last_seen: AtomicU64::new(initial_closed),
        })
    }

    /// The grid interval.
    pub fn interval_ms(&self) -> u64 {
        self.interval_ms
    }

    /// Block until the next fire, or return immediately if a fire landed since the last `wait()`.
    ///
    /// `completed_at` is the clock when the daemon fired. `Normal` when the fire is on the grid
    /// and no grid lines were skipped since the previous return. `Overrun` when the daemon fired
    /// late (off grid) or when the caller was slow and at least one grid line was missed.
    pub fn wait(&self) -> Result<WaitResult, SdkError> {
        let words = self.handle.words();
        let closed_word = &words.closed_count;
        let seen = self.last_seen.load(Ordering::Acquire);
        if seen == SENTINEL {
            return Err(SdkError::InterlockReaped);
        }

        loop {
            let now = monotonic_now_nanos();
            check_live(&self.handle, Some(&self.clock), now)?;
            let closed = closed_word.load(Ordering::Acquire);
            if closed == SENTINEL {
                return Err(SdkError::InterlockReaped);
            }
            if closed > seen {
                self.last_seen.fetch_max(closed, Ordering::AcqRel);
                let state = if closed % self.interval_ms != 0 {
                    WaitState::Overrun
                } else if closed > seen + self.interval_ms {
                    WaitState::Overrun
                } else {
                    WaitState::Normal
                };
                return Ok(WaitResult {
                    completed_at: closed,
                    state,
                });
            }
            if let FutexOutcome::Fatal(errno) = classify_futex_result(futex_wait(
                closed_word,
                futex_word(closed),
                DEFAULT_TIMEOUT_NANOS,
            )) {
                return Err(SdkError::FutexFailed { errno });
            }
        }
    }

    /// The clock at the last fire (closed_count).
    pub fn completed_at(&self) -> u64 {
        handle_ops::completed_at(&self.handle)
    }

    /// Read both counters: (next grid line, last fire).
    pub fn peek(&self) -> (u64, u64) {
        handle_ops::peek(&self.handle)
    }

    /// True once the interlock is terminated.
    pub fn is_reaped(&self) -> bool {
        interlock_is_terminated(&self.handle) || self.touch.as_ref().is_some_and(|t| t.is_reaped())
    }

    /// Terminate: stop the keepalive and stamp SENTINEL on expiration.
    pub fn free(&mut self) {
        self.touch.take();
        interlock_free(&self.handle);
    }
}
