//! The keepalive: one thread per client that refreshes every registered interlock's
//! expiration on its own cadence, stamps ProcessClocks with the clock, and owns
//! process liveness (the Abacus clock check and the ProcessClock reap check).

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::thread;
use std::time::Duration;

use abacus_core::clock::{expiration_alive, monotonic_now_nanos, ms_to_nanos};
use abacus_core::interlock::{interlock_arm, interlock_read_expiration, InterlockHandle, SENTINEL};

use crate::client::SdkError;
use crate::types::{default_touch_ttl_ms, Liveness, TimeoutPolicy, DEFAULT_TOUCH_INTERVAL_MS};

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

struct Process {
    clock: InterlockHandle,
    name: String,
    id: u64,
    abacus_clock: InterlockHandle,
    interval_ns: u64,
    ttl_ns: u64,
    next_due_ns: u64,
}

struct State {
    entries: Vec<Entry>,
    next_id: u64,
    thread_running: bool,
    process: Option<Process>,
    policy: TimeoutPolicy,
    liveness: Liveness,
}

struct Inner {
    state: Mutex<State>,
    wake: Condvar,
    /// Test hook: the next spawn fails as if the OS refused the thread.
    #[cfg(test)]
    fail_spawn: AtomicBool,
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
                    process: None,
                    policy: TimeoutPolicy::Abort,
                    liveness: Liveness::Alive,
                }),
                wake: Condvar::new(),
                #[cfg(test)]
                fail_spawn: AtomicBool::new(false),
            }),
        }
    }

    /// Register `handle` to be armed with `ttl_ms` every `interval_ms`. An `interval_ms` of 0
    /// is treated as 1 ms. A `ttl_ms` below `2 * interval_ms` (after that clamp) is raised to
    /// it. The first arm happens synchronously, so the TTL is extended before this returns.
    /// Dropping the returned `TouchHandle` deregisters. `Err(InterlockReaped)` if the
    /// interlock is already terminated; `Err(KeepaliveSpawnFailed)` if the keepalive thread
    /// could not be started. Nothing is registered on error.
    pub fn register(
        &self,
        handle: InterlockHandle,
        interval_ms: u64,
        ttl_ms: u64,
    ) -> Result<TouchHandle, SdkError> {
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
    ) -> Result<TouchHandle, SdkError> {
        self.register_inner(handle, interval_ms, ttl_ms, Some(clock))
    }

    /// Bind the client's process clock. `Err(InterlockReaped)` if the clock was already
    /// terminated; `Err(KeepaliveSpawnFailed)` if the keepalive thread could not be started
    /// (nothing is bound).
    pub(crate) fn bind_process(
        &self,
        clock: InterlockHandle,
        name: String,
        id: u64,
        abacus_clock: InterlockHandle,
    ) -> Result<(), SdkError> {
        let interval_ms = DEFAULT_TOUCH_INTERVAL_MS;
        let ttl_ms = default_touch_ttl_ms(interval_ms);
        let ttl_ns = ms_to_nanos(ttl_ms);
        let interval_ns = ms_to_nanos(interval_ms);
        let now = monotonic_now_nanos();

        interlock_arm(&clock, ttl_ns).map_err(SdkError::from)?;
        if !stamp_last_seen(&clock, &abacus_clock) {
            return Err(SdkError::InterlockReaped);
        }

        let process = Process {
            clock,
            name,
            id,
            abacus_clock,
            interval_ns,
            ttl_ns,
            next_due_ns: now + interval_ns,
        };

        let mut st = lock(&self.inner.state);
        if !st.thread_running {
            spawn_thread(&self.inner)?;
            st.thread_running = true;
        }
        st.process = Some(process);
        drop(st);
        self.inner.wake.notify_all();
        Ok(())
    }

    /// Set the timeout policy for process liveness.
    pub(crate) fn set_policy(&self, policy: TimeoutPolicy) {
        lock(&self.inner.state).policy = policy;
    }

    /// The process liveness last observed by the keepalive thread. `Alive` until (under
    /// `TimeoutPolicy::Error`) the process clock is found reaped or the Abacus clock lapses.
    pub(crate) fn liveness(&self) -> Liveness {
        lock(&self.inner.state).liveness
    }

    #[cfg(test)]
    pub(crate) fn process_bound(&self) -> bool {
        lock(&self.inner.state).process.is_some()
    }

    /// Test hook: make the next keepalive thread spawn fail (or stop failing).
    #[cfg(test)]
    pub(crate) fn fail_next_spawn(&self, fail: bool) {
        self.inner.fail_spawn.store(fail, Ordering::Release);
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
    ) -> Result<TouchHandle, SdkError> {
        let interval_ms = interval_ms.max(1);
        // A TTL at or below the cadence would lapse between touches; floor it at two intervals.
        let ttl_ms = ttl_ms.max(interval_ms.saturating_mul(2));
        let ttl_ns = ms_to_nanos(ttl_ms);
        let interval_ns = ms_to_nanos(interval_ms);
        let now = monotonic_now_nanos();
        interlock_arm(&handle, ttl_ns).map_err(SdkError::from)?;
        if let Some(c) = &clock {
            if !stamp_last_seen(&handle, c) {
                return Err(SdkError::InterlockReaped);
            }
        }

        let reaped = Arc::new(AtomicBool::new(false));
        let mut st = lock(&self.inner.state);
        if !st.thread_running {
            spawn_thread(&self.inner)?;
            st.thread_running = true;
        }
        let id = st.next_id;
        st.next_id += 1;
        st.entries.push(Entry {
            id,
            handle,
            interval_ns,
            ttl_ns,
            next_due_ns: now + interval_ns,
            clock,
            reaped: reaped.clone(),
        });
        drop(st);
        self.inner.wake.notify_all();

        Ok(TouchHandle {
            id,
            inner: self.inner.clone(),
            reaped,
        })
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

/// Copy the clock's `open_count` into the handle's, unless the handle already reads SENTINEL.
/// Returns `false` when SENTINEL was found (the interlock was reaped or freed).
fn stamp_last_seen(handle: &InterlockHandle, clock: &InterlockHandle) -> bool {
    let clock_now = clock.words().open_count.load(Ordering::Acquire);
    handle
        .words()
        .open_count
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |cur| {
            (cur != SENTINEL).then(|| cur.max(clock_now))
        })
        .is_ok()
}

fn spawn_thread(inner: &Arc<Inner>) -> Result<(), SdkError> {
    #[cfg(test)]
    if inner.fail_spawn.load(Ordering::Acquire) {
        return Err(SdkError::KeepaliveSpawnFailed {
            message: "injected spawn failure".to_string(),
        });
    }
    let weak = Arc::downgrade(inner);
    thread::Builder::new()
        .name("abacus-keepalive".into())
        .spawn(move || run(weak))
        .map(|_| ())
        .map_err(|e| SdkError::KeepaliveSpawnFailed {
            message: e.to_string(),
        })
}

fn run(weak: Weak<Inner>) {
    loop {
        let Some(inner) = weak.upgrade() else {
            return;
        };
        let mut st = lock(&inner.state);
        let now = monotonic_now_nanos();
        let mut next_due = now + IDLE_SLEEP.as_nanos() as u64;

        // Process liveness: check the Abacus clock, then touch the process clock.
        let mut death: Option<Liveness> = None;
        if let Some(proc) = st.process.as_mut() {
            let abacus_exp = interlock_read_expiration(&proc.abacus_clock);
            if !expiration_alive(abacus_exp, now) {
                death = Some(Liveness::DaemonClockLapsed);
            } else if proc.next_due_ns <= now {
                if interlock_arm(&proc.clock, proc.ttl_ns).is_err()
                    || !stamp_last_seen(&proc.clock, &proc.abacus_clock)
                {
                    death = Some(Liveness::ProcessClockReaped);
                } else {
                    proc.next_due_ns = now + proc.interval_ns;
                }
            }
            if death.is_none() {
                if let Some(p) = st.process.as_ref() {
                    next_due = next_due.min(p.next_due_ns);
                }
            }
        }

        if let Some(d) = death {
            let policy = st.policy;
            let proc = st.process.take().unwrap();
            match policy {
                TimeoutPolicy::Abort => {
                    match d {
                        Liveness::ProcessClockReaped => {
                            let _ = writeln!(
                                std::io::stderr(),
                                "abacus: ProcessClockReaped: process clock \"{}\" (id {}) was reaped; aborting",
                                proc.name, proc.id
                            );
                        }
                        Liveness::DaemonClockLapsed => {
                            let exp = interlock_read_expiration(&proc.abacus_clock);
                            let _ = writeln!(
                                std::io::stderr(),
                                "abacus: DaemonClockLapsed: the Abacus clock expired at {exp} ns, now {now} ns; aborting"
                            );
                        }
                        Liveness::Alive => unreachable!(
                            "death is only ever recorded as ProcessClockReaped or DaemonClockLapsed"
                        ),
                    }
                    drop(st);
                    std::process::abort();
                }
                TimeoutPolicy::Error => {
                    // Record the cause and drop the process clock; carry on with entries.
                    st.liveness = d;
                }
            }
        }

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
                    if !stamp_last_seen(&entry.handle, clock) {
                        entry.reaped.store(true, Ordering::Release);
                        st.entries.remove(i);
                        continue;
                    }
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
    use abacus_core::interlock::{
        interlock_create, interlock_free, interlock_read_expiration, SENTINEL,
    };

    #[test]
    fn register_arms_immediately_with_ttl() {
        let k = Keepalive::new();
        let h = interlock_create().unwrap();
        let before = monotonic_now_nanos();
        let _t = k.register(h.clone(), 40, 200).unwrap();
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
            .map(|h| k.register(h.clone(), 10, 200).unwrap())
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
        let t = k.register(h.clone(), 5, 200).unwrap();
        interlock_free(&h);
        thread::sleep(Duration::from_millis(30));
        assert!(t.is_reaped());
        assert_eq!(k.registered(), 0);
    }

    #[test]
    fn stamp_never_overwrites_sentinel() {
        let k = Keepalive::new();
        let h = interlock_create().unwrap();
        let clock = interlock_create().unwrap();
        clock.words().open_count.store(1000, Ordering::Release);
        let t = k
            .register_with_clock(h.clone(), clock.clone(), 5, 200)
            .unwrap();
        h.words().open_count.store(SENTINEL, Ordering::Release);
        thread::sleep(Duration::from_millis(30));
        assert_eq!(
            h.words().open_count.load(Ordering::Acquire),
            SENTINEL,
            "keepalive must not overwrite SENTINEL on open_count"
        );
        assert!(t.is_reaped(), "TouchHandle should report reaped");
        assert_eq!(k.registered(), 0, "entry should have been dropped");
    }

    #[test]
    fn thread_exits_when_all_owners_drop() {
        let k = Keepalive::new();
        let h = interlock_create().unwrap();
        let t = k.register(h.clone(), 5, 200).unwrap();
        let inner = Arc::downgrade(&k.inner);
        drop(t);
        drop(k);
        thread::sleep(Duration::from_millis(250));
        assert!(
            inner.upgrade().is_none(),
            "keepalive thread should have released its Arc"
        );
    }

    #[test]
    fn error_policy_drops_reaped_process_clock() {
        let k = Keepalive::new();
        k.set_policy(TimeoutPolicy::Error);
        let pc = interlock_create().unwrap();
        let abacus_clock = interlock_create().unwrap();
        interlock_arm(&abacus_clock, ms_to_nanos(5000)).unwrap();
        abacus_clock
            .words()
            .open_count
            .store(1000, Ordering::Release);
        k.bind_process(pc.clone(), "test".into(), 1, abacus_clock)
            .unwrap();
        pc.words().open_count.store(SENTINEL, Ordering::Release);
        thread::sleep(Duration::from_millis(3 * DEFAULT_TOUCH_INTERVAL_MS));
        assert!(!k.process_bound(), "process still bound after SENTINEL");
        assert_eq!(k.liveness(), Liveness::ProcessClockReaped);
    }

    #[test]
    fn error_policy_drops_on_lapsed_abacus_clock() {
        let k = Keepalive::new();
        k.set_policy(TimeoutPolicy::Error);
        let pc = interlock_create().unwrap();
        let abacus_clock = interlock_create().unwrap();
        abacus_clock
            .words()
            .open_count
            .store(1000, Ordering::Release);
        abacus_clock
            .words()
            .expiration_ns
            .store(1, Ordering::Release);
        k.bind_process(pc.clone(), "test".into(), 1, abacus_clock)
            .unwrap();
        let exp0 = interlock_read_expiration(&pc);
        thread::sleep(Duration::from_millis(3 * DEFAULT_TOUCH_INTERVAL_MS));
        assert!(
            !k.process_bound(),
            "process still bound after Abacus clock lapsed"
        );
        assert_eq!(
            interlock_read_expiration(&pc),
            exp0,
            "expiration advanced after process was dropped"
        );
        assert_eq!(k.liveness(), Liveness::DaemonClockLapsed);
    }

    #[test]
    fn process_clock_is_touched_while_healthy() {
        let k = Keepalive::new();
        k.set_policy(TimeoutPolicy::Error);
        let pc = interlock_create().unwrap();
        let abacus_clock = interlock_create().unwrap();
        interlock_arm(&abacus_clock, ms_to_nanos(5000)).unwrap();
        abacus_clock
            .words()
            .open_count
            .store(1000, Ordering::Release);
        k.bind_process(pc.clone(), "test".into(), 1, abacus_clock)
            .unwrap();
        let exp0 = interlock_read_expiration(&pc);
        thread::sleep(Duration::from_millis(3 * DEFAULT_TOUCH_INTERVAL_MS));
        assert!(k.process_bound(), "healthy process was dropped");
        assert!(
            interlock_read_expiration(&pc) > exp0,
            "expiration did not advance on a healthy process clock"
        );
        assert_eq!(k.liveness(), Liveness::Alive);
    }

    #[test]
    fn zero_interval_gets_the_documented_ttl_floor() {
        let k = Keepalive::new();
        let h = interlock_create().unwrap();
        let before = monotonic_now_nanos();
        let _t = k.register(h.clone(), 0, 0).unwrap();
        let exp = interlock_read_expiration(&h);
        assert!(
            exp >= before + ms_to_nanos(2),
            "exp {exp} before {before}: TTL below 2 x the clamped 1 ms interval"
        );
    }

    #[test]
    fn register_refuses_a_terminated_interlock() {
        let k = Keepalive::new();
        let h = interlock_create().unwrap();
        interlock_free(&h);
        assert_eq!(
            k.register(h.clone(), 40, 200).err(),
            Some(SdkError::InterlockReaped)
        );
        assert_eq!(k.registered(), 0);
    }

    #[test]
    fn failed_spawn_registers_nothing_and_leaves_no_thread_flag() {
        let k = Keepalive::new();
        let h = interlock_create().unwrap();
        k.inner.fail_spawn.store(true, Ordering::Release);
        assert!(matches!(
            k.register(h.clone(), 40, 200).err(),
            Some(SdkError::KeepaliveSpawnFailed { .. })
        ));
        assert!(!k.thread_running());
        assert_eq!(k.registered(), 0);
        k.inner.fail_spawn.store(false, Ordering::Release);
        let _t = k.register(h.clone(), 40, 200).unwrap();
        assert!(k.thread_running());
        assert_eq!(k.registered(), 1);
    }

    #[test]
    fn failed_spawn_leaves_the_process_unbound() {
        let k = Keepalive::new();
        k.set_policy(TimeoutPolicy::Error);
        let pc = interlock_create().unwrap();
        let abacus_clock = interlock_create().unwrap();
        interlock_arm(&abacus_clock, ms_to_nanos(5000)).unwrap();
        abacus_clock
            .words()
            .open_count
            .store(1000, Ordering::Release);
        k.inner.fail_spawn.store(true, Ordering::Release);
        assert!(matches!(
            k.bind_process(pc.clone(), "test".into(), 1, abacus_clock),
            Err(SdkError::KeepaliveSpawnFailed { .. })
        ));
        assert!(!k.process_bound());
        assert!(!k.thread_running());
    }
}
