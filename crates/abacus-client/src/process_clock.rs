//! ProcessClock: a read-only view of the client's liveness beacon.
//!
//! The keepalive thread owns the touch cycle and the liveness checks; this type exposes
//! the handle's counters (uptime, last seen, start time) and reaped state.

use std::sync::atomic::{AtomicU64, Ordering};

use abacus_core::interlock::{interlock_is_terminated, InterlockHandle, SENTINEL};

use crate::client::SdkError;
use crate::handle_ops;

/// A read-only view of the client's ProcessClock. The keepalive stamps open_count with the
/// daemon clock on every touch; closed_count holds the start time. Other processes attach
/// and read uptime. If the process dies, touches stop, the daemon reaps, and watchers get
/// InterlockReaped.
pub struct ProcessClock {
    handle: InterlockHandle,
    name: String,
    id: u64,
    dependencies: Vec<String>,
}

impl ProcessClock {
    pub(crate) fn new(
        handle: InterlockHandle,
        clock: &InterlockHandle,
        name: String,
        id: u64,
        dependencies: Vec<String>,
    ) -> Result<Self, SdkError> {
        let start_time = clock.words().open_count.load(Ordering::Acquire);
        if !stamp_word(&handle.words().closed_count, start_time) {
            return Err(SdkError::InterlockReaped);
        }
        if !stamp_word(&handle.words().open_count, start_time) {
            return Err(SdkError::InterlockReaped);
        }
        Ok(Self {
            handle,
            name,
            id,
            dependencies,
        })
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

    /// The ProcessClock name passed to connect.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The dependency names passed to connect, in order. Names as given: the daemon
    /// resolved each to whatever held that name at create time, and this clock dies with
    /// those entries.
    pub fn dependencies(&self) -> &[String] {
        &self.dependencies
    }

    /// The registry id assigned at creation.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// True once the interlock is terminated.
    pub fn is_reaped(&self) -> bool {
        interlock_is_terminated(&self.handle)
    }
}

/// Store `value` into `word` unless it already holds SENTINEL (the interlock was reaped or
/// freed out from under the stamp). Same shape as `touch.rs::stamp_last_seen`.
fn stamp_word(word: &AtomicU64, value: u64) -> bool {
    word.fetch_update(Ordering::AcqRel, Ordering::Acquire, |cur| {
        (cur != SENTINEL).then_some(value)
    })
    .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use abacus_core::interlock::interlock_create;

    #[test]
    fn new_refuses_to_overwrite_a_reaped_open_count() {
        let handle = interlock_create().unwrap();
        let clock = interlock_create().unwrap();
        handle.words().open_count.store(SENTINEL, Ordering::Release);
        match ProcessClock::new(handle.clone(), &clock, "test".into(), 1, Vec::new()) {
            Err(err) => assert_eq!(err, SdkError::InterlockReaped),
            Ok(_) => panic!("ProcessClock::new must refuse a SENTINEL open_count"),
        }
        assert_eq!(
            handle.words().open_count.load(Ordering::Acquire),
            SENTINEL,
            "open_count must not be overwritten once it reads SENTINEL"
        );
    }
}
