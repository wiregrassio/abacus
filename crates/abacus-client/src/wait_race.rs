//! WaitRace: SDK-only first-of-N over WaitCounters. Polls every 1 ms, the daemon's delivery
//! cadence.

use std::time::Duration;

use abacus_core::clock::{monotonic_now_nanos, ms_to_nanos};

use crate::client::SdkError;
use crate::types::{classify_wake, WaitResult};
use crate::wait_counter::WaitCounter;

/// Watches N WaitCounters and returns the first to fire.
///
/// Each counter's target must already be set (via `wait_until` with a zero timeout, or by
/// writing `open_count` directly) before calling `wait`. A counter that is already delivered
/// on entry (closed_count >= open_count) wins immediately.
pub struct WaitRace {
    counters: Vec<WaitCounter>,
}

impl WaitRace {
    /// A race over `counters`. Targets must already be set.
    pub fn new(counters: Vec<WaitCounter>) -> Self {
        Self { counters }
    }

    /// Poll until one counter is delivered (`closed_count >= open_count`) or `timeout_ms`
    /// elapses. Each pass checks the counters in index order, termination before delivery,
    /// and the first counter that decides the pass decides the result:
    /// `Err(InterlockReaped)` if it is terminated or its daemon clock lapsed,
    /// `Ok(Some((index, result)))` if it is delivered. A terminated counter after a
    /// delivered one is not reached. `Ok(None)` on timeout; `timeout_ms == 0` polls once.
    pub fn wait(&self, timeout_ms: u64) -> Result<Option<(usize, WaitResult)>, SdkError> {
        if self.counters.is_empty() {
            return Err(SdkError::InvalidRequest {
                message: "WaitRace over zero counters".to_string(),
            });
        }
        let deadline_ns = monotonic_now_nanos().saturating_add(ms_to_nanos(timeout_ms));
        loop {
            for (i, counter) in self.counters.iter().enumerate() {
                if counter.is_reaped() || counter.is_clock_dead() {
                    return Err(SdkError::InterlockReaped);
                }
                let (open, closed) = counter.peek();
                if let Some(state) = classify_wake(open, closed) {
                    return Ok(Some((
                        i,
                        WaitResult {
                            completed_at: closed,
                            state,
                        },
                    )));
                }
            }
            if monotonic_now_nanos() >= deadline_ns {
                return Ok(None);
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// Number of counters.
    pub fn len(&self) -> usize {
        self.counters.len()
    }

    /// True if no counters are being raced.
    pub fn is_empty(&self) -> bool {
        self.counters.is_empty()
    }

    /// The counters.
    pub fn counters(&self) -> &[WaitCounter] {
        &self.counters
    }
}
