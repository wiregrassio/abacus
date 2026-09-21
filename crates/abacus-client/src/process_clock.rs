//! ProcessClock: an interlock used as a liveness and uptime beacon.
//!
//! The keepalive refreshes expiration and stores the clock's current ms into open_count on
//! every touch. closed_count holds the start time. Other processes attach read-only and read
//! value for uptime. If the process dies, touches stop, the TTL lapses, the daemon reaps,
//! and watchers get InterlockReaped.

use std::sync::atomic::Ordering;

use abacus_core::interlock::{interlock_free, interlock_is_terminated, InterlockHandle};

use crate::handle_ops;
use crate::touch::{Keepalive, TouchHandle};
use crate::types::{default_touch_ttl_ms, DEFAULT_TOUCH_INTERVAL_MS};

/// A liveness and uptime beacon. closed_count = start time, open_count = last touch time,
/// value = uptime in milliseconds.
pub struct ProcessClock {
    handle: InterlockHandle,
    touch: Option<TouchHandle>,
}

impl ProcessClock {
    pub(crate) fn new(
        handle: InterlockHandle,
        clock: InterlockHandle,
        keepalive: &Keepalive,
    ) -> Self {
        let start_time = clock.words().open_count.load(Ordering::Acquire);
        handle
            .words()
            .closed_count
            .store(start_time, Ordering::Release);
        handle
            .words()
            .open_count
            .store(start_time, Ordering::Release);
        let touch = Some(keepalive.register_with_clock(
            handle.clone(),
            clock,
            DEFAULT_TOUCH_INTERVAL_MS,
            default_touch_ttl_ms(DEFAULT_TOUCH_INTERVAL_MS),
        ));
        Self { handle, touch }
    }

    /// Uptime in milliseconds: open_count minus closed_count.
    pub fn uptime_ms(&self) -> i64 {
        handle_ops::value(&self.handle)
    }

    /// Last touch time in monotonic milliseconds (open_count).
    pub fn last_seen_ms(&self) -> u64 {
        self.handle.words().open_count.load(Ordering::Acquire)
    }

    /// Start time in monotonic milliseconds (closed_count).
    pub fn start_time_ms(&self) -> u64 {
        handle_ops::completed_at(&self.handle)
    }

    /// True once the interlock is terminated.
    pub fn is_reaped(&self) -> bool {
        interlock_is_terminated(&self.handle) || self.touch.as_ref().is_none_or(|t| t.is_reaped())
    }

    /// Terminate: stop the keepalive and stamp SENTINEL on expiration.
    pub fn free(&mut self) {
        self.touch.take();
        interlock_free(&self.handle);
    }
}
