//! Abacus RTS client SDK: connect to the daemon, create and attach interlocks, and wait on
//! them through typed handles with a shared keepalive thread and fatal-timeout handling.

#![warn(missing_docs)]

pub mod client;
pub(crate) mod handle_ops;
pub mod interlock;
pub mod process_clock;
#[cfg(test)]
mod tests;
pub mod touch;
pub mod types;
pub mod wait_barrier;
pub mod wait_counter;
pub mod wait_cron;
pub mod wait_race;
pub mod wait_timer;

pub use client::{AbacusClient, Result, SdkError};
pub use interlock::{AttachedInterlock, AttachedWaitCounter, ClockHandle, Interlock};
pub use process_clock::ProcessClock;
pub use touch::{Keepalive, TouchHandle};
pub use types::{
    default_touch_ttl_ms, InterlockState, TimeoutPolicy, WaitResult, WaitState, WatchedWord,
    DEFAULT_TOUCH_INTERVAL_MS, DEFAULT_TOUCH_TTL_MS, DEFAULT_TRANSPORT_TIMEOUT,
    MIN_FATAL_MARGIN_MS, MIN_TOUCH_TTL_MS,
};
pub use wait_barrier::WaitBarrier;
pub use wait_counter::WaitCounter;
pub use wait_cron::WaitCron;
pub use wait_race::WaitRace;
pub use wait_timer::WaitTimer;
