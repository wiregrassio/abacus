//! WaitRace: SDK-only first-of-N over WaitCounters. Polls every 1 ms, the daemon's delivery
//! cadence. The daemon-side replacement is WaitOr in wire v2.

use std::time::Duration;

use crate::client::SdkError;
use crate::types::{classify_wake, WaitResult, WaitState};
use crate::wait_counter::WaitCounter;

/// Watches N WaitCounters and returns the first to fire.
///
/// Set each counter's target first (`wait_until` from its own thread, or by writing
/// open_count), then call `wait`. Polling costs one wake per millisecond per race.
pub struct WaitRace {
    counters: Vec<WaitCounter>,
}

impl WaitRace {
    /// A race over `counters`. Targets must already be set.
    pub fn new(counters: Vec<WaitCounter>) -> Self {
        Self { counters }
    }

    /// Poll until one counter's closed_count advances past its snapshot at entry. Returns
    /// its index and result. `Err(InterlockReaped)` as soon as any counter is terminated.
    pub fn wait(&self) -> Result<(usize, WaitResult), SdkError> {
        if self.counters.is_empty() {
            return Err(SdkError::InvalidRequest {
                message: "WaitRace over zero counters".to_string(),
            });
        }
        let initial: Vec<u64> = self.counters.iter().map(|c| c.peek().1).collect();
        loop {
            for (i, counter) in self.counters.iter().enumerate() {
                if counter.is_reaped() {
                    return Err(SdkError::InterlockReaped);
                }
                let (open, closed) = counter.peek();
                if closed > initial[i] {
                    let state = classify_wake(open, closed).unwrap_or(WaitState::Timeout);
                    return Ok((
                        i,
                        WaitResult {
                            completed_at: closed,
                            state,
                        },
                    ));
                }
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
