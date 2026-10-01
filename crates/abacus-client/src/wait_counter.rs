//! WaitCounter: watches another interlock's word. The daemon stamps closed_count with the
//! watched value when it reaches the target.

use std::sync::atomic::Ordering;

use abacus_core::clock::{futex_wait, futex_word, monotonic_now_nanos, ms_to_nanos};
use abacus_core::interlock::{
    interlock_arm, interlock_free, interlock_is_terminated, InterlockHandle, SENTINEL,
};

use crate::client::SdkError;
use crate::handle_ops;
use crate::touch::{Keepalive, TouchHandle};
use crate::types::{
    classify_wake, default_touch_ttl_ms, WaitResult, WaitState, DEFAULT_TOUCH_INTERVAL_MS,
};

/// A WaitCounter. The client writes open_count (the target) and waits on closed_count (the
/// daemon's response). A timeout is the caller's to interpret.
pub struct WaitCounter {
    handle: InterlockHandle,
    touch: Option<TouchHandle>,
}

impl WaitCounter {
    pub(crate) fn new(handle: InterlockHandle, keepalive: &Keepalive) -> Result<Self, SdkError> {
        let touch = Some(keepalive.register(
            handle.clone(),
            DEFAULT_TOUCH_INTERVAL_MS,
            default_touch_ttl_ms(DEFAULT_TOUCH_INTERVAL_MS),
        )?);
        Ok(Self { handle, touch })
    }

    /// Set open_count = target (CAS-max), arm a TTL of 2 * timeout_ms, and wait on
    /// closed_count until the daemon delivers or `timeout_ms` elapses.
    ///
    /// Returns `WaitResult { state: Timeout }` once `timeout_ms` has elapsed since the call,
    /// however many wakes arrive before delivery (the interlock is still live);
    /// `Err(InterlockReaped)` if it is terminated or its target died.
    pub fn wait_until(&self, target: u64, timeout_ms: u64) -> Result<WaitResult, SdkError> {
        let words = self.handle.words();
        let timeout_nanos = ms_to_nanos(timeout_ms);

        // CAS-max: never writes open_count backward.
        let open = &words.open_count;
        loop {
            let current = open.load(Ordering::Acquire);
            if current == SENTINEL {
                return Err(SdkError::InterlockReaped);
            }
            if target <= current {
                break;
            }
            match open.compare_exchange_weak(current, target, Ordering::Release, Ordering::Acquire)
            {
                Ok(_) => break,
                Err(_) => continue,
            }
        }

        // timeout 0 means "poll once and return", not "block forever".
        // ms_to_nanos(0) == 0, and futex_wait interprets 0 as indefinite, so we
        // check for immediate delivery and return Timeout if not yet delivered.
        if timeout_nanos == 0 {
            let closed = words.closed_count.load(Ordering::Acquire);
            let open_val = words.open_count.load(Ordering::Acquire);
            if closed == SENTINEL || open_val == SENTINEL {
                return Err(SdkError::InterlockReaped);
            }
            if words.expiration_ns.load(Ordering::Acquire) == SENTINEL {
                return Err(SdkError::InterlockReaped);
            }
            if let Some(state) = classify_wake(open_val, closed) {
                return Ok(WaitResult {
                    completed_at: closed,
                    state,
                });
            }
            return Ok(WaitResult {
                completed_at: closed,
                state: WaitState::Timeout,
            });
        }

        interlock_arm(&self.handle, timeout_nanos.saturating_mul(2)).map_err(SdkError::from)?;

        let closed_word = &words.closed_count;
        let deadline_ns = monotonic_now_nanos().saturating_add(timeout_nanos);
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
            if words.expiration_ns.load(Ordering::Acquire) == SENTINEL {
                return Err(SdkError::InterlockReaped);
            }
            let now_ns = monotonic_now_nanos();
            if now_ns >= deadline_ns {
                return Ok(WaitResult {
                    completed_at: closed,
                    state: WaitState::Timeout,
                });
            }
            // A spurious wake, EINTR, EAGAIN, or a closed_count change short of delivery waits
            // out only what is left of the caller's timeout, never a fresh full one.
            let _ = futex_wait(closed_word, futex_word(closed), deadline_ns - now_ns);
        }
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

    /// open_count minus closed_count, signed.
    pub fn value(&self) -> i64 {
        handle_ops::value(&self.handle)
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

#[cfg(test)]
mod tests {
    use super::*;
    use abacus_core::clock::futex_wake;
    use abacus_core::interlock::interlock_create;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::thread;
    use std::time::{Duration, Instant};

    #[test]
    fn wakes_short_of_delivery_do_not_extend_the_timeout() {
        let k = Keepalive::new();
        let h = interlock_create().unwrap();
        let counter = WaitCounter::new(h.clone(), &k).unwrap();
        // A peer bumps closed_count short of the target and wakes the waiter every 10 ms
        // for 1 s; with no daemon, nothing ever delivers.
        let stop = Arc::new(AtomicBool::new(false));
        let bumper = {
            let (h, stop) = (h.clone(), stop.clone());
            thread::spawn(move || {
                let t0 = Instant::now();
                while !stop.load(Ordering::Acquire) && t0.elapsed() < Duration::from_secs(1) {
                    h.words().closed_count.fetch_add(1, Ordering::AcqRel);
                    futex_wake(&h.words().closed_count);
                    thread::sleep(Duration::from_millis(10));
                }
            })
        };
        let t0 = Instant::now();
        let r = counter.wait_until(1_000_000, 100).expect("wait_until");
        let elapsed = t0.elapsed();
        stop.store(true, Ordering::Release);
        bumper.join().unwrap();
        assert_eq!(r.state, WaitState::Timeout, "{r:?}");
        assert!(
            elapsed < Duration::from_millis(400),
            "a 100 ms wait took {elapsed:?}: each wake restarted the full timeout"
        );
    }
}
