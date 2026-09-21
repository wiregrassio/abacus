//! WaitTimer: a WaitCounter on clock.open_count with a fatal margin. If the daemon does not
//! deliver within the margin it is dead, and the timer either aborts the process or returns
//! `RtsTimeout`, per the client's `TimeoutPolicy`.

use std::sync::atomic::Ordering;

use abacus_core::clock::{futex_wait, futex_word, monotonic_now_nanos, ms_to_nanos};
use abacus_core::interlock::{
    interlock_arm, interlock_free, interlock_is_terminated, InterlockHandle, SENTINEL,
};

use crate::client::SdkError;
use crate::handle_ops;
use crate::touch::{Keepalive, TouchHandle};
use crate::types::{
    classify_wake, default_touch_ttl_ms, TimeoutPolicy, WaitResult, WaitState,
    DEFAULT_TOUCH_INTERVAL_MS,
};

/// A WaitTimer. `wait_ms(W)` targets clock + W and gives the daemon
/// `max(2 * W, min_fatal_margin_ms)` to deliver.
pub struct WaitTimer {
    handle: InterlockHandle,
    clock: InterlockHandle,
    touch: Option<TouchHandle>,
    policy: TimeoutPolicy,
    min_margin_ms: u64,
}

impl WaitTimer {
    pub(crate) fn new(
        handle: InterlockHandle,
        clock: InterlockHandle,
        keepalive: &Keepalive,
        policy: TimeoutPolicy,
        min_margin_ms: u64,
    ) -> Self {
        let touch = Some(keepalive.register(
            handle.clone(),
            DEFAULT_TOUCH_INTERVAL_MS,
            default_touch_ttl_ms(DEFAULT_TOUCH_INTERVAL_MS),
        ));
        Self {
            handle,
            clock,
            touch,
            policy,
            min_margin_ms,
        }
    }

    /// The policy applied when the fatal margin elapses.
    pub fn timeout_policy(&self) -> TimeoutPolicy {
        self.policy
    }

    /// The margin `wait_ms(ms)` uses: `max(2 * ms, min_fatal_margin_ms)`.
    pub fn margin_for(&self, ms: u64) -> u64 {
        ms.saturating_mul(2).max(self.min_margin_ms)
    }

    /// Wait `ms` milliseconds from the current clock. Margin is `margin_for(ms)`.
    ///
    /// `wait_ms(0)` returns at once with the current clock and `Normal`, arming nothing.
    pub fn wait_ms(&self, ms: u64) -> Result<WaitResult, SdkError> {
        self.wait_ms_with_margin(ms, self.margin_for(ms))
    }

    /// Wait `ms` milliseconds with an explicit fatal margin. `margin_ms` must exceed `ms`;
    /// the TTL and the deadline are both set to it, so the timer cannot be reaped while
    /// legitimately waiting.
    ///
    /// On `Normal` or `Overrun` delivery returns the result. On a missed margin: with
    /// `TimeoutPolicy::Abort` prints a diagnostic and aborts the process; with
    /// `TimeoutPolicy::Error` returns `Err(RtsTimeout)`. `Err(InterlockReaped)` if the timer
    /// is terminated before delivery.
    pub fn wait_ms_with_margin(&self, ms: u64, margin_ms: u64) -> Result<WaitResult, SdkError> {
        let words = self.handle.words();
        let clock_now = self.clock.words().open_count.load(Ordering::Acquire);
        if clock_now == SENTINEL {
            return Err(SdkError::InterlockReaped);
        }

        if ms == 0 {
            if interlock_is_terminated(&self.handle) {
                return Err(SdkError::InterlockReaped);
            }
            return Ok(WaitResult {
                completed_at: clock_now,
                state: WaitState::Normal,
            });
        }
        if margin_ms <= ms {
            return Err(SdkError::InvalidRequest {
                message: format!("margin_ms ({margin_ms}) must exceed wait ({ms})"),
            });
        }

        // CAS-max on the target: concurrent waits keep the largest.
        let target = clock_now.saturating_add(ms);
        loop {
            let current = words.open_count.load(Ordering::Acquire);
            if current == SENTINEL {
                return Err(SdkError::InterlockReaped);
            }
            if target <= current {
                break;
            }
            match words.open_count.compare_exchange_weak(
                current,
                target,
                Ordering::Release,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(_) => continue,
            }
        }

        // TTL and deadline are the same number: the interlock outlives the wait.
        let margin_ns = ms_to_nanos(margin_ms);
        interlock_arm(&self.handle, margin_ns).map_err(SdkError::from)?;
        let deadline_ns = monotonic_now_nanos().saturating_add(margin_ns);

        let closed_word = &words.closed_count;
        loop {
            let closed = closed_word.load(Ordering::Acquire);
            let open = words.open_count.load(Ordering::Acquire);
            if closed == SENTINEL || open == SENTINEL {
                return Err(SdkError::InterlockReaped);
            }
            if let Some(state) = classify_wake(open, closed) {
                return Ok(WaitResult {
                    completed_at: closed,
                    state,
                });
            }
            let exp = words.expiration_ns.load(Ordering::Acquire);
            if exp == SENTINEL {
                return Err(SdkError::InterlockReaped);
            }
            let now_ns = monotonic_now_nanos();
            if now_ns >= deadline_ns {
                if exp < now_ns {
                    return Err(SdkError::InterlockReaped);
                }
                return self.on_timeout(ms, margin_ms, target, open, closed);
            }
            let _ = futex_wait(closed_word, futex_word(closed), deadline_ns - now_ns);
        }
    }

    fn on_timeout(
        &self,
        ms: u64,
        margin_ms: u64,
        target: u64,
        open: u64,
        closed: u64,
    ) -> Result<WaitResult, SdkError> {
        match self.policy {
            TimeoutPolicy::Error => Err(SdkError::RtsTimeout),
            TimeoutPolicy::Abort => {
                eprintln!(
                    "abacus: RTSTimeout: daemon did not deliver within {margin_ms}ms \
                     (wait={ms}ms, target={target}, open={open}, closed={closed}); aborting"
                );
                std::process::abort();
            }
        }
    }

    /// Wait until an absolute clock time. Sugar for `wait_ms(timestamp_ms - clock_now)`,
    /// saturating to zero if the time is past (which returns at once).
    pub fn wait_until(&self, timestamp_ms: u64) -> Result<WaitResult, SdkError> {
        let clock_now = self.clock.words().open_count.load(Ordering::Acquire);
        self.wait_ms(timestamp_ms.saturating_sub(clock_now))
    }

    /// The daemon's response: closed_count.
    pub fn completed_at(&self) -> u64 {
        handle_ops::completed_at(&self.handle)
    }

    /// Extend expiration to max(current, now + ms).
    pub fn touch(&self, ms: u64) -> Result<(), SdkError> {
        handle_ops::touch(&self.handle, ms)
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
