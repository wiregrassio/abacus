//! The keepalive: one thread per client that refreshes every registered interlock's
//! expiration on its own cadence, and stamps ProcessClocks with the clock.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::thread;
use std::time::Duration;

use abacus_core::clock::{monotonic_now_nanos, ms_to_nanos};
use abacus_core::interlock::{interlock_arm, InterlockHandle};

/// Longest the thread sleeps with nothing due, so it notices ownership changes.
const IDLE_SLEEP: Duration = Duration::from_millis(100);

struct Entry {
    id: u64,
    handle: InterlockHandle,
    interval_ns: u64,
    ttl_ns: u64,
    next_due_ns: u64,
    /// ProcessClock: stamp open_count with this clock's open_count on every touch.
    clock: Option<InterlockHandle>,
    reaped: Arc<AtomicBool>,
}

struct State {
    entries: Vec<Entry>,
    next_id: u64,
    thread_running: bool,
}

struct Inner {
    state: Mutex<State>,
    wake: Condvar,
}

/// The shared keepalive. Cloning shares the thread. The thread starts on the first
/// registration and exits when the last `Keepalive` and `TouchHandle` are gone.
#[derive(Clone)]
pub struct Keepalive {
    inner: Arc<Inner>,
}

impl Default for Keepalive {
    fn default() -> Self {
        Self::new()
    }
}

impl Keepalive {
    /// A keepalive with no thread yet.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                state: Mutex::new(State {
                    entries: Vec::new(),
                    next_id: 1,
                    thread_running: false,
                }),
                wake: Condvar::new(),
            }),
        }
    }

    /// Register `handle` to be armed with `ttl_ms` every `interval_ms`. A `ttl_ms` below
    /// `2 * interval_ms` is raised to that. The first arm happens synchronously, so the TTL
    /// is extended before this returns. Dropping the returned `TouchHandle` deregisters.
    pub fn register(&self, handle: InterlockHandle, interval_ms: u64, ttl_ms: u64) -> TouchHandle {
        self.register_inner(handle, interval_ms, ttl_ms, None)
    }

    /// Like `register`, and on every touch also store `clock.open_count` into the
    /// interlock's open_count (the ProcessClock pattern).
    pub fn register_with_clock(
        &self,
        handle: InterlockHandle,
        clock: InterlockHandle,
        interval_ms: u64,
        ttl_ms: u64,
    ) -> TouchHandle {
        self.register_inner(handle, interval_ms, ttl_ms, Some(clock))
    }

    /// True while the thread is alive.
    pub fn thread_running(&self) -> bool {
        lock(&self.inner.state).thread_running
    }

    /// Registered interlocks.
    pub fn registered(&self) -> usize {
        lock(&self.inner.state).entries.len()
    }

    fn register_inner(
        &self,
        handle: InterlockHandle,
        interval_ms: u64,
        ttl_ms: u64,
        clock: Option<InterlockHandle>,
    ) -> TouchHandle {
        // A TTL at or below the cadence would lapse between touches; floor it at two intervals.
        let ttl_ms = ttl_ms.max(interval_ms.saturating_mul(2)).max(1);
        let reaped = Arc::new(AtomicBool::new(false));
        let ttl_ns = ms_to_nanos(ttl_ms);
        let interval_ns = ms_to_nanos(interval_ms.max(1));
        let now = monotonic_now_nanos();
        if interlock_arm(&handle, ttl_ns).is_err() {
            reaped.store(true, Ordering::Release);
        }
        if let Some(c) = &clock {
            let clock_now = c.words().open_count.load(Ordering::Acquire);
            handle
                .words()
                .open_count
                .store(clock_now, Ordering::Release);
        }

        let mut st = lock(&self.inner.state);
        let id = st.next_id;
        st.next_id += 1;
        if !reaped.load(Ordering::Acquire) {
            st.entries.push(Entry {
                id,
                handle,
                interval_ns,
                ttl_ns,
                next_due_ns: now + interval_ns,
                clock,
                reaped: reaped.clone(),
            });
        }
        if !st.thread_running {
            st.thread_running = true;
            let weak = Arc::downgrade(&self.inner);
            thread::Builder::new()
                .name("abacus-keepalive".into())
                .spawn(move || run(weak))
                .expect("spawn keepalive thread");
        }
        drop(st);
        self.inner.wake.notify_all();

        TouchHandle {
            id,
            inner: self.inner.clone(),
            reaped,
        }
    }

    fn deregister(inner: &Inner, id: u64) {
        let mut st = lock(&inner.state);
        st.entries.retain(|e| e.id != id);
        drop(st);
        inner.wake.notify_all();
    }
}

fn lock(m: &Mutex<State>) -> std::sync::MutexGuard<'_, State> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn run(weak: Weak<Inner>) {
    loop {
        let Some(inner) = weak.upgrade() else {
            return;
        };
        let mut st = lock(&inner.state);
        let now = monotonic_now_nanos();
        let mut next_due = now + IDLE_SLEEP.as_nanos() as u64;
        let mut i = 0;
        while i < st.entries.len() {
            let entry = &mut st.entries[i];
            if entry.next_due_ns <= now {
                if interlock_arm(&entry.handle, entry.ttl_ns).is_err() {
                    entry.reaped.store(true, Ordering::Release);
                    st.entries.remove(i);
                    continue;
                }
                if let Some(clock) = &entry.clock {
                    let clock_now = clock.words().open_count.load(Ordering::Acquire);
                    entry
                        .handle
                        .words()
                        .open_count
                        .store(clock_now, Ordering::Release);
                }
                entry.next_due_ns = now + entry.interval_ns;
            }
            next_due = next_due.min(entry.next_due_ns);
            i += 1;
        }
        let sleep = Duration::from_nanos(next_due.saturating_sub(monotonic_now_nanos()));
        // The Arc is held across the wait; the loop top re-checks ownership after it.
        let (guard, _) = inner
            .wake
            .wait_timeout(st, sleep)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        drop(guard);
        if Arc::strong_count(&inner) == 1 {
            // Only the thread's own reference is left: every owner is gone.
            lock(&inner.state).thread_running = false;
            return;
        }
    }
}

/// One interlock's registration with the keepalive. Dropping it stops the touches.
pub struct TouchHandle {
    id: u64,
    inner: Arc<Inner>,
    reaped: Arc<AtomicBool>,
}

impl TouchHandle {
    /// True once an arm found the interlock terminated. The registration is dropped then.
    pub fn is_reaped(&self) -> bool {
        self.reaped.load(Ordering::Acquire)
    }

    /// Stop touching. Idempotent; drop does the same.
    pub fn stop(&self) {
        Keepalive::deregister(&self.inner, self.id);
    }
}

impl Drop for TouchHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use abacus_core::interlock::{interlock_create, interlock_free, interlock_read_expiration};

    #[test]
    fn register_arms_immediately_with_ttl() {
        let k = Keepalive::new();
        let h = interlock_create().unwrap();
        let before = monotonic_now_nanos();
        let _t = k.register(h.clone(), 40, 200);
        let exp = interlock_read_expiration(&h);
        assert!(
            exp >= before + ms_to_nanos(200),
            "exp {exp} before {before}"
        );
        assert!(k.thread_running());
        assert_eq!(k.registered(), 1);
    }

    #[test]
    fn one_thread_serves_many_handles_and_keeps_them_alive() {
        let k = Keepalive::new();
        let handles: Vec<_> = (0..20).map(|_| interlock_create().unwrap()).collect();
        let touches: Vec<_> = handles
            .iter()
            .map(|h| k.register(h.clone(), 10, 200))
            .collect();
        thread::sleep(Duration::from_millis(60));
        let now = monotonic_now_nanos();
        for h in &handles {
            assert!(interlock_read_expiration(h) > now + ms_to_nanos(100));
        }
        assert_eq!(k.registered(), 20);
        drop(touches);
        assert_eq!(k.registered(), 0);
    }

    #[test]
    fn reaped_flag_set_and_entry_dropped() {
        let k = Keepalive::new();
        let h = interlock_create().unwrap();
        let t = k.register(h.clone(), 5, 200);
        interlock_free(&h);
        thread::sleep(Duration::from_millis(30));
        assert!(t.is_reaped());
        assert_eq!(k.registered(), 0);
    }

    #[test]
    fn thread_exits_when_all_owners_drop() {
        let k = Keepalive::new();
        let h = interlock_create().unwrap();
        let t = k.register(h.clone(), 5, 200);
        let inner = Arc::downgrade(&k.inner);
        drop(t);
        drop(k);
        thread::sleep(Duration::from_millis(250));
        assert!(
            inner.upgrade().is_none(),
            "keepalive thread should have released its Arc"
        );
    }
}
