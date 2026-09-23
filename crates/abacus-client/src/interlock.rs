//! Interlock handles: creator, attacher, read-only WaitCounter view, and the clock.

use std::time::Duration;

use abacus_core::interlock::{interlock_free, interlock_is_terminated, InterlockHandle, SENTINEL};

use crate::client::SdkError;
use crate::handle_ops::{self, Word};
use crate::touch::{Keepalive, TouchHandle};
use crate::types::{default_touch_ttl_ms, InterlockState, DEFAULT_TOUCH_INTERVAL_MS};

/// An interlock created by this client. Read and write on both counters and expiration.
pub struct Interlock {
    handle: InterlockHandle,
    clock: InterlockHandle,
    keepalive: Keepalive,
    touch: Option<TouchHandle>,
}

impl Interlock {
    pub(crate) fn new(
        handle: InterlockHandle,
        clock: InterlockHandle,
        keepalive: Keepalive,
    ) -> Self {
        let touch = Some(keepalive.register(
            handle.clone(),
            DEFAULT_TOUCH_INTERVAL_MS,
            default_touch_ttl_ms(DEFAULT_TOUCH_INTERVAL_MS),
        ));
        Self {
            handle,
            clock,
            keepalive,
            touch,
        }
    }

    /// The underlying shared-memory handle.
    pub fn handle(&self) -> &InterlockHandle {
        &self.handle
    }

    /// open_count += h. `Err(InterlockReaped)` if the word is at SENTINEL; the increment
    /// never erases a sentinel.
    pub fn open(&self, h: u64) -> Result<(), SdkError> {
        handle_ops::increment(&self.handle, Word::Open, h)
    }

    /// closed_count += h. `Err(InterlockReaped)` if the word is at SENTINEL.
    pub fn close(&self, h: u64) -> Result<(), SdkError> {
        handle_ops::increment(&self.handle, Word::Closed, h)
    }

    /// Extend expiration to max(current, now + ms). `Err(InterlockReaped)` if terminated.
    pub fn touch(&self, ms: u64) -> Result<(), SdkError> {
        handle_ops::touch(&self.handle, ms)
    }

    /// Read both counters: (open_count, closed_count).
    pub fn peek(&self) -> (u64, u64) {
        handle_ops::peek(&self.handle)
    }

    /// Block until open_count >= target. Blocks indefinitely while the interlock is alive;
    /// see `wait_open_for` for a bounded wait.
    pub fn wait_open(&self, target: u64) -> Result<u64, SdkError> {
        handle_ops::wait_word(&self.handle, Word::Open, target, None, &self.clock)
            .map(|v| v.unwrap_or(target))
    }

    /// Block until closed_count >= target. Blocks indefinitely while the interlock is alive.
    pub fn wait_close(&self, target: u64) -> Result<u64, SdkError> {
        handle_ops::wait_word(&self.handle, Word::Closed, target, None, &self.clock)
            .map(|v| v.unwrap_or(target))
    }

    /// Block until open_count >= target or `timeout` elapses. `Ok(None)` on timeout.
    pub fn wait_open_for(&self, target: u64, timeout: Duration) -> Result<Option<u64>, SdkError> {
        handle_ops::wait_word(&self.handle, Word::Open, target, Some(timeout), &self.clock)
    }

    /// Block until closed_count >= target or `timeout` elapses. `Ok(None)` on timeout.
    pub fn wait_close_for(&self, target: u64, timeout: Duration) -> Result<Option<u64>, SdkError> {
        handle_ops::wait_word(
            &self.handle,
            Word::Closed,
            target,
            Some(timeout),
            &self.clock,
        )
    }

    /// Read expiration_ns: CLOCK_MONOTONIC nanoseconds, or SENTINEL once terminated.
    pub fn expiration_ns(&self) -> u64 {
        handle_ops::expiration_ns(&self.handle)
    }

    /// open_count minus closed_count, signed.
    pub fn value(&self) -> i64 {
        handle_ops::value(&self.handle)
    }

    /// The lifecycle state against CLOCK_MONOTONIC.
    pub fn state(&self) -> InterlockState {
        handle_ops::state(&self.handle)
    }

    /// True once the interlock is terminated (reaped, freed, or name claimed), whether seen
    /// by the keepalive or read from the words now.
    pub fn is_reaped(&self) -> bool {
        interlock_is_terminated(&self.handle) || self.touch.as_ref().is_some_and(|t| t.is_reaped())
    }

    /// Replace the keepalive registration: arm `ttl_ms` every `interval_ms`.
    pub fn start_touch_thread(&mut self, interval_ms: u64, ttl_ms: u64) -> &TouchHandle {
        self.touch.take();
        self.touch = Some(
            self.keepalive
                .register(self.handle.clone(), interval_ms, ttl_ms),
        );
        self.touch.as_ref().expect("just set")
    }

    /// Stop the keepalive without terminating. The TTL lapses unless touched manually.
    pub fn stop_touch_thread(&mut self) {
        self.touch.take();
    }

    /// Terminate: stop the keepalive and stamp SENTINEL on expiration. The daemon cleans up
    /// on its next cycle.
    pub fn free(&mut self) {
        self.stop_touch_thread();
        interlock_free(&self.handle);
    }
}

/// An interlock attached by name. Read and write on both counters; expiration read-only.
pub struct AttachedInterlock {
    handle: InterlockHandle,
    clock: InterlockHandle,
}

impl AttachedInterlock {
    pub(crate) fn new(handle: InterlockHandle, clock: InterlockHandle) -> Self {
        Self { handle, clock }
    }

    /// The underlying shared-memory handle.
    pub fn handle(&self) -> &InterlockHandle {
        &self.handle
    }

    /// open_count += h. Sentinel-aware; see `Interlock::open`.
    pub fn open(&self, h: u64) -> Result<(), SdkError> {
        handle_ops::increment(&self.handle, Word::Open, h)
    }

    /// closed_count += h. Sentinel-aware; see `Interlock::close`.
    pub fn close(&self, h: u64) -> Result<(), SdkError> {
        handle_ops::increment(&self.handle, Word::Closed, h)
    }

    /// Read both counters: (open_count, closed_count).
    pub fn peek(&self) -> (u64, u64) {
        handle_ops::peek(&self.handle)
    }

    /// Block until open_count >= target. Blocks indefinitely while the interlock is alive.
    pub fn wait_open(&self, target: u64) -> Result<u64, SdkError> {
        handle_ops::wait_word(&self.handle, Word::Open, target, None, &self.clock)
            .map(|v| v.unwrap_or(target))
    }

    /// Block until closed_count >= target. Blocks indefinitely while the interlock is alive.
    pub fn wait_close(&self, target: u64) -> Result<u64, SdkError> {
        handle_ops::wait_word(&self.handle, Word::Closed, target, None, &self.clock)
            .map(|v| v.unwrap_or(target))
    }

    /// Bounded `wait_open`. `Ok(None)` on timeout.
    pub fn wait_open_for(&self, target: u64, timeout: Duration) -> Result<Option<u64>, SdkError> {
        handle_ops::wait_word(&self.handle, Word::Open, target, Some(timeout), &self.clock)
    }

    /// Bounded `wait_close`. `Ok(None)` on timeout.
    pub fn wait_close_for(&self, target: u64, timeout: Duration) -> Result<Option<u64>, SdkError> {
        handle_ops::wait_word(
            &self.handle,
            Word::Closed,
            target,
            Some(timeout),
            &self.clock,
        )
    }

    /// Read expiration_ns: CLOCK_MONOTONIC nanoseconds, or SENTINEL once terminated.
    pub fn expiration_ns(&self) -> u64 {
        handle_ops::expiration_ns(&self.handle)
    }

    /// open_count minus closed_count, signed.
    pub fn value(&self) -> i64 {
        handle_ops::value(&self.handle)
    }

    /// The lifecycle state against CLOCK_MONOTONIC.
    pub fn state(&self) -> InterlockState {
        handle_ops::state(&self.handle)
    }

    /// True once any word reads SENTINEL.
    pub fn is_reaped(&self) -> bool {
        interlock_is_terminated(&self.handle)
    }

    /// Terminate by writing SENTINEL to open_count. Attachers hold expiration read-only, so
    /// they terminate through a counter. Cannot be undone by any holder's increment.
    pub fn free(&self) {
        let words = self.handle.words();
        words
            .open_count
            .store(SENTINEL, std::sync::atomic::Ordering::Release);
        abacus_core::clock::futex_wake(&words.open_count);
        abacus_core::clock::futex_wake(&words.closed_count);
    }
}

/// A read-only view of a WaitCounter, obtained via attach. No open, close, touch, wait, or
/// free: the creator owns every mutation.
///
/// ```compile_fail
/// # use abacus_client::AttachedWaitCounter;
/// fn f(c: &AttachedWaitCounter) { c.open(1); }
/// ```
pub struct AttachedWaitCounter {
    handle: InterlockHandle,
}

impl AttachedWaitCounter {
    pub(crate) fn new(handle: InterlockHandle) -> Self {
        Self { handle }
    }

    /// The underlying shared-memory handle.
    pub fn handle(&self) -> &InterlockHandle {
        &self.handle
    }

    /// Read both counters: (open_count, closed_count).
    pub fn peek(&self) -> (u64, u64) {
        handle_ops::peek(&self.handle)
    }

    /// open_count minus closed_count, signed.
    pub fn value(&self) -> i64 {
        handle_ops::value(&self.handle)
    }

    /// The daemon's response: closed_count.
    pub fn completed_at(&self) -> u64 {
        handle_ops::completed_at(&self.handle)
    }

    /// True once any word reads SENTINEL.
    pub fn is_reaped(&self) -> bool {
        interlock_is_terminated(&self.handle)
    }
}

/// A read-only view of the daemon's clock interlock.
///
/// open_count = current monotonic ms (every daemon cycle), closed_count = daemon start ms
/// (set once), value = uptime ms.
pub struct ClockHandle {
    handle: InterlockHandle,
}

impl ClockHandle {
    pub(crate) fn new(handle: InterlockHandle) -> Self {
        Self { handle }
    }

    /// Read both words: (now_ms, start_time_ms).
    pub fn peek(&self) -> (u64, u64) {
        handle_ops::peek(&self.handle)
    }

    /// Uptime in milliseconds.
    pub fn value(&self) -> i64 {
        handle_ops::value(&self.handle)
    }

    /// Current monotonic time in milliseconds (open_count).
    pub fn now_ms(&self) -> u64 {
        self.handle
            .words()
            .open_count
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Daemon start time in monotonic milliseconds (closed_count).
    pub fn start_time_ms(&self) -> u64 {
        handle_ops::completed_at(&self.handle)
    }

    /// Uptime in milliseconds.
    pub fn uptime_ms(&self) -> i64 {
        self.value()
    }

    /// Block until the clock reaches `target` ms. Returns `InterlockReaped` within one clock
    /// TTL (100 ms) of the daemon stopping.
    pub fn wait_open(&self, target: u64) -> Result<u64, SdkError> {
        handle_ops::wait_word(&self.handle, Word::Open, target, None, &self.handle)
            .map(|v| v.unwrap_or(target))
    }

    /// Block until closed_count >= target.
    pub fn wait_close(&self, target: u64) -> Result<u64, SdkError> {
        handle_ops::wait_word(&self.handle, Word::Closed, target, None, &self.handle)
            .map(|v| v.unwrap_or(target))
    }

    /// Bounded `wait_open`. `Ok(None)` on timeout.
    pub fn wait_open_for(&self, target: u64, timeout: Duration) -> Result<Option<u64>, SdkError> {
        handle_ops::wait_word(
            &self.handle,
            Word::Open,
            target,
            Some(timeout),
            &self.handle,
        )
    }

    /// Bounded `wait_close`. `Ok(None)` on timeout.
    pub fn wait_close_for(&self, target: u64, timeout: Duration) -> Result<Option<u64>, SdkError> {
        handle_ops::wait_word(
            &self.handle,
            Word::Closed,
            target,
            Some(timeout),
            &self.handle,
        )
    }

    pub(crate) fn handle(&self) -> &InterlockHandle {
        &self.handle
    }
}
