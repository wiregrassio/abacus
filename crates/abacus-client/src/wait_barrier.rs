//! WaitBarrier: wait for all of N conditions. The daemon checks every condition each cycle
//! and fires by stamping closed_count = open_count. One-shot until `rearm()`.

use std::sync::atomic::Ordering;

use abacus_core::clock::{
    classify_futex_result, futex_wait, futex_word, monotonic_now_nanos, FutexOutcome,
};
use abacus_core::interlock::{
    interlock_free, interlock_is_terminated, InterlockHandle, ReadOnlyInterlockHandle, SENTINEL,
};

use crate::client::SdkError;
use crate::handle_ops::{self, check_live, Word};
use crate::touch::{Keepalive, TouchHandle};
use crate::types::{
    default_touch_ttl_ms, WaitResult, WaitState, DEFAULT_TIMEOUT_NANOS, DEFAULT_TOUCH_INTERVAL_MS,
};

/// A WaitBarrier. Created Open (open_count = 1, closed_count = 0); fired when
/// closed_count == open_count.
pub struct WaitBarrier {
    handle: InterlockHandle,
    clock: ReadOnlyInterlockHandle,
    touch: Option<TouchHandle>,
}

impl WaitBarrier {
    pub(crate) fn new(
        handle: InterlockHandle,
        clock: ReadOnlyInterlockHandle,
        keepalive: &Keepalive,
    ) -> Result<Self, SdkError> {
        let touch = Some(keepalive.register(
            handle.clone(),
            DEFAULT_TOUCH_INTERVAL_MS,
            default_touch_ttl_ms(DEFAULT_TOUCH_INTERVAL_MS),
        )?);
        Ok(Self {
            handle,
            clock,
            touch,
        })
    }

    /// Block until the barrier has fired. Returns at once if it already has and was not
    /// re-armed. `completed_at` is the clock at return; `state` is always `Normal` (Overrun
    /// has no meaning for a barrier). `Err(InterlockReaped)` if the barrier or any watched
    /// interlock is terminated.
    pub fn wait(&self) -> Result<WaitResult, SdkError> {
        let words = self.handle.words();
        let closed_word = &words.closed_count;
        loop {
            let now = monotonic_now_nanos();
            check_live(&self.handle, Some(&self.clock), now)?;
            let closed = closed_word.load(Ordering::Acquire);
            let open = words.open_count.load(Ordering::Acquire);
            if closed == SENTINEL || open == SENTINEL {
                return Err(SdkError::InterlockReaped);
            }
            if closed >= open {
                let clock_now = self.clock.load_open();
                if clock_now == SENTINEL {
                    return Err(SdkError::InterlockReaped);
                }
                return Ok(WaitResult {
                    completed_at: clock_now,
                    state: WaitState::Normal,
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

    /// Re-arm after a fire: open_count += 1, so the daemon evaluates the conditions again
    /// and fires when they all hold. Sentinel-aware.
    pub fn rearm(&self) -> Result<(), SdkError> {
        handle_ops::increment(&self.handle, Word::Open, 1)
    }

    /// closed_count: equals open_count once fired.
    pub fn completed_at(&self) -> u64 {
        handle_ops::completed_at(&self.handle)
    }

    /// Read both counters: (open_count, closed_count).
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
