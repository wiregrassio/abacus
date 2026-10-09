//! WaitTimer: a WaitCounter on clock.open_count with a fatal margin. If the daemon does not
//! deliver within the margin, the timer prints a diagnostic and aborts the process.

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};

use abacus_core::clock::{
    classify_futex_result, futex_wait, futex_word, monotonic_now_nanos, ms_to_nanos, FutexOutcome,
};
use abacus_core::interlock::{
    interlock_extend, interlock_free, interlock_is_terminated, InterlockHandle,
    ReadOnlyInterlockHandle, SENTINEL,
};

use crate::client::SdkError;
use crate::handle_ops;
use crate::touch::{Keepalive, TouchHandle};
use crate::types::{
    classify_wake, default_touch_ttl_ms, WaitResult, WaitState, DEFAULT_TOUCH_INTERVAL_MS,
};

/// A WaitTimer. `wait_ms(W)` targets clock + W and gives the daemon
/// `W + max(W, min_fatal_margin_ms)` to deliver, so the delivery-lateness tolerance is
/// always at least `min_fatal_margin_ms`.
///
/// One waiter at a time: a wait started while another is in progress on the same timer
/// returns `InvalidRequest`. `peek`, `is_reaped`, and `completed_at` may be called from any
/// thread during a wait.
pub struct WaitTimer {
    handle: InterlockHandle,
    clock: ReadOnlyInterlockHandle,
    touch: Option<TouchHandle>,
    min_margin_ms: u64,
    /// True while a wait holds the timer's single waiter slot.
    waiting: AtomicBool,
}

impl WaitTimer {
    pub(crate) fn new(
        handle: InterlockHandle,
        clock: ReadOnlyInterlockHandle,
        keepalive: &Keepalive,
        min_margin_ms: u64,
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
            min_margin_ms,
            waiting: AtomicBool::new(false),
        })
    }

    /// The margin `wait_ms(ms)` uses: `ms + max(ms, min_fatal_margin_ms)`.
    pub fn margin_for(&self, ms: u64) -> u64 {
        ms.saturating_add(ms.max(self.min_margin_ms))
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
    /// On `Normal` or `Overrun` delivery returns the result. On a missed margin, prints a
    /// diagnostic and aborts the process. `Err(InterlockReaped)` if the timer is terminated
    /// before delivery. `Err(InvalidRequest)` if another wait is in progress on this timer.
    pub fn wait_ms_with_margin(&self, ms: u64, margin_ms: u64) -> Result<WaitResult, SdkError> {
        let clock_now = self.clock.load_open();
        self.wait_from(clock_now, ms, margin_ms)
    }

    fn wait_from(&self, clock_now: u64, ms: u64, margin_ms: u64) -> Result<WaitResult, SdkError> {
        let words = self.handle.words();
        if clock_now == SENTINEL {
            return Err(SdkError::InterlockReaped);
        }

        if ms == 0 {
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

        let _waiter =
            WaiterGuard::acquire(&self.waiting).ok_or_else(|| SdkError::InvalidRequest {
                message: "a wait is already in progress on this WaitTimer (one timer, one waiter)"
                    .to_string(),
            })?;

        // One waiter per timer (WaiterGuard), so the target is this wait's own: set it
        // exactly, never over a SENTINEL. A stale target left by a timed-out wait is replaced.
        let target = clock_now.saturating_add(ms);
        if words
            .open_count
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |cur| {
                (cur != SENTINEL).then_some(target)
            })
            .is_err()
        {
            return Err(SdkError::InterlockReaped);
        }

        // TTL and deadline are the same number: the interlock outlives the wait.
        let margin_ns = ms_to_nanos(margin_ms);
        interlock_extend(&self.handle, margin_ns).map_err(SdkError::from)?;
        let deadline_ns = monotonic_now_nanos().saturating_add(margin_ns);

        let closed_word = &words.closed_count;
        loop {
            let now_ns = monotonic_now_nanos();
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
            if now_ns >= deadline_ns {
                return self.on_timeout(ms, margin_ms, target, open, closed);
            }
            if let FutexOutcome::Fatal(errno) = classify_futex_result(futex_wait(
                closed_word,
                futex_word(closed),
                deadline_ns - now_ns,
            )) {
                return Err(SdkError::FutexFailed { errno });
            }
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
        let _ = writeln!(
            std::io::stderr(),
            "abacus: DeliveryTimeout: daemon did not deliver within {margin_ms}ms \
             (wait={ms}ms, target={target}, open={open}, closed={closed}); aborting"
        );
        std::process::abort();
    }

    /// Wait until an absolute clock time: `wait_ms(timestamp_ms - clock_now)` from a single
    /// clock read, returning at once if the time is past.
    pub fn wait_until(&self, timestamp_ms: u64) -> Result<WaitResult, SdkError> {
        let clock_now = self.clock.load_open();
        let ms = timestamp_ms.saturating_sub(clock_now);
        self.wait_from(clock_now, ms, self.margin_for(ms))
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

/// Holds a WaitTimer's single waiter slot for the length of one wait.
struct WaiterGuard<'a>(&'a AtomicBool);

impl<'a> WaiterGuard<'a> {
    fn acquire(slot: &'a AtomicBool) -> Option<Self> {
        slot.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .ok()
            .map(|_| Self(slot))
    }
}

impl Drop for WaiterGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use abacus_core::clock::futex_wake;
    use abacus_core::interlock::interlock_create;
    use std::sync::Arc;
    use std::thread;
    use std::time::{Duration, Instant};

    /// A timer over a fake clock frozen at 1000 ms. No daemon: a test delivers a wait itself
    /// with `deliver`, since an undelivered wait aborts the process at its margin.
    fn local_timer() -> (WaitTimer, InterlockHandle, Keepalive) {
        let k = Keepalive::new();
        let h = interlock_create().unwrap();
        let clock = interlock_create().unwrap();
        clock.words().open_count.store(1000, Ordering::Release);
        let t = WaitTimer::new(h.clone(), clock.into(), &k, 100).unwrap();
        (t, h, k)
    }

    /// Spin until the timer's target is `target`, then do the daemon's part: close at it.
    fn deliver(h: &InterlockHandle, target: u64) {
        let start = Instant::now();
        while h.words().open_count.load(Ordering::Acquire) != target {
            if start.elapsed() > Duration::from_secs(1) {
                panic!("the wait never set its target {target}");
            }
            thread::sleep(Duration::from_millis(1));
        }
        h.words().closed_count.store(target, Ordering::Release);
        futex_wake(&h.words().closed_count);
    }

    #[test]
    fn a_second_concurrent_wait_is_refused() {
        let (t, h, _k) = local_timer();
        let t = Arc::new(t);
        let first = {
            let t = t.clone();
            thread::spawn(move || t.wait_ms_with_margin(100, 300))
        };
        let start = Instant::now();
        while h.words().open_count.load(Ordering::Acquire) != 1100 {
            if start.elapsed() > Duration::from_secs(1) {
                panic!("the first wait never set its target");
            }
            thread::sleep(Duration::from_millis(1));
        }
        assert!(matches!(t.wait_ms(5), Err(SdkError::InvalidRequest { .. })));
        deliver(&h, 1100);
        first.join().unwrap().unwrap();
    }

    #[test]
    fn wait_until_targets_the_timestamp() {
        let (t, h, _k) = local_timer();
        let t = Arc::new(t);
        let waiter = {
            let t = t.clone();
            thread::spawn(move || t.wait_until(1010))
        };
        deliver(&h, 1010);
        waiter.join().unwrap().unwrap();
        assert_eq!(h.words().open_count.load(Ordering::Acquire), 1010);
    }
}
