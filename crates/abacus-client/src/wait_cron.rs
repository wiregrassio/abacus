//! WaitCron: a recurring grid-aligned timer. The daemon re-arms it after every fire, so the
//! client loops `wait()` with no UDS round-trip.

use std::sync::atomic::Ordering;

use abacus_core::clock::{futex_wait, futex_word, monotonic_now_nanos};
use abacus_core::interlock::{interlock_free, interlock_is_terminated, InterlockHandle, SENTINEL};

use crate::client::SdkError;
use crate::handle_ops;
use crate::touch::{Keepalive, TouchHandle};
use crate::types::{
    default_touch_ttl_ms, WaitResult, WaitState, DEFAULT_TIMEOUT_NANOS, DEFAULT_TOUCH_INTERVAL_MS,
};

/// A WaitCron on the `interval_ms` grid aligned to the monotonic epoch.
pub struct WaitCron {
    handle: InterlockHandle,
    clock: InterlockHandle,
    touch: Option<TouchHandle>,
    interval_ms: u64,
}

impl WaitCron {
    pub(crate) fn new(
        handle: InterlockHandle,
        clock: InterlockHandle,
        keepalive: &Keepalive,
        interval_ms: u64,
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
            interval_ms,
        })
    }

    /// The grid interval.
    pub fn interval_ms(&self) -> u64 {
        self.interval_ms
    }

    /// Block until the next grid line fires.
    ///
    /// `completed_at` is the clock when the daemon fired. `Normal` when that is on the grid,
    /// `Overrun` when the daemon fired late (off grid). After a stall the daemon fires once
    /// and re-arms to the next future line; missed lines are never replayed.
    pub fn wait(&self) -> Result<WaitResult, SdkError> {
        let words = self.handle.words();
        let closed_word = &words.closed_count;
        let initial_closed = closed_word.load(Ordering::Acquire);
        if initial_closed == SENTINEL {
            return Err(SdkError::InterlockReaped);
        }

        loop {
            let closed = closed_word.load(Ordering::Acquire);
            if closed == SENTINEL {
                return Err(SdkError::InterlockReaped);
            }
            if closed > initial_closed {
                let state = if closed % self.interval_ms == 0 {
                    WaitState::Normal
                } else {
                    WaitState::Overrun
                };
                return Ok(WaitResult {
                    completed_at: closed,
                    state,
                });
            }
            let now = monotonic_now_nanos();
            let exp = words.expiration_ns.load(Ordering::Acquire);
            if exp == SENTINEL || exp < now {
                return Err(SdkError::InterlockReaped);
            }
            // Check the daemon-owned clock's expiration. The client keepalive cannot re-arm
            // the clock (it is read-only), so after daemon death this fires within one clock TTL.
            let clock_exp = self.clock.words().expiration_ns.load(Ordering::Acquire);
            // A terminated daemon clock (SENTINEL) is dead, not alive.
            if clock_exp == SENTINEL || clock_exp < now {
                return Err(SdkError::InterlockReaped);
            }
            let _ = futex_wait(closed_word, futex_word(closed), DEFAULT_TIMEOUT_NANOS);
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
