//! The keepalive: one thread per client that refreshes every registered interlock's
//! expiration on its own cadence, stamps ProcessClocks with the clock, and owns
//! process liveness (the Abacus clock check and the ProcessClock reap check).

use std::io::Write;
use std::os::unix::thread::JoinHandleExt;
#[cfg(test)]
use std::sync::atomic::AtomicI32;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::thread;
use std::time::Duration;

use abacus_core::clock::{expiration_alive, monotonic_now_nanos, ms_to_nanos, NANOS_PER_MS};
use abacus_core::interlock::{
    interlock_extend, InterlockHandle, ReadOnlyInterlockHandle, SENTINEL,
};

use crate::client::SdkError;
use crate::types::{default_touch_ttl_ms, KeepalivePriority, Liveness, DEFAULT_TOUCH_INTERVAL_MS};

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
    abacus_clock: ReadOnlyInterlockHandle,
    interval_ns: u64,
    ttl_ns: u64,
    next_due_ns: u64,
}

struct State {
    entries: Vec<Entry>,
    next_id: u64,
    thread_running: bool,
    process: Option<Process>,
    /// True after a keepalive panic: no new registrations accepted.
    failed: bool,
    /// The scheduling the caller chose for the thread; applied at spawn and by
    /// `set_priority`.
    priority: KeepalivePriority,
    /// The running thread, for `pthread_setschedparam`. Some only while
    /// `thread_running`; every exit path of `run` clears both under this lock before
    /// the thread returns, so a stored value always names a live thread.
    thread: Option<libc::pthread_t>,
}

struct Inner {
    state: Mutex<State>,
    wake: Condvar,
    /// Test hook: the next spawn fails as if the OS refused the thread.
    #[cfg(test)]
    fail_spawn: AtomicBool,
    /// Test hook: the thread panics at the top of its next pass.
    #[cfg(test)]
    panic_next_pass: AtomicBool,
    /// Test hook: while nonzero, every priority change fails with this errno.
    #[cfg(test)]
    fail_priority: AtomicI32,
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
                    failed: false,
                    priority: KeepalivePriority::Normal,
                    thread: None,
                }),
                wake: Condvar::new(),
                #[cfg(test)]
                fail_spawn: AtomicBool::new(false),
                #[cfg(test)]
                panic_next_pass: AtomicBool::new(false),
                #[cfg(test)]
                fail_priority: AtomicI32::new(0),
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
        abacus_clock: ReadOnlyInterlockHandle,
    ) -> Result<(), SdkError> {
        let interval_ms = DEFAULT_TOUCH_INTERVAL_MS;
        let ttl_ms = default_touch_ttl_ms(interval_ms);
        let ttl_ns = ms_to_nanos(ttl_ms);
        let interval_ns = ms_to_nanos(interval_ms);
        let now = monotonic_now_nanos();

        interlock_extend(&clock, ttl_ns).map_err(SdkError::from)?;
        if !stamp_last_seen(&clock, abacus_clock.load_open()) {
            return Err(SdkError::InterlockReaped);
        }

        let process = Process {
            clock,
            name,
            id,
            abacus_clock,
            interval_ns,
            ttl_ns,
            next_due_ns: now.saturating_add(interval_ns),
        };

        let mut st = lock(&self.inner.state);
        if st.failed {
            return Err(SdkError::KeepaliveFailed);
        }
        if !st.thread_running {
            spawn_thread(&self.inner, &mut st)?;
        }
        st.process = Some(process);
        drop(st);
        self.inner.wake.notify_all();
        Ok(())
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

    /// Set the keepalive thread's scheduling. Applied at once to a running thread and
    /// kept for any thread started later. `KeepalivePriority::Fifo(p)` outside 1 to 99
    /// is `InvalidRequest`. A kernel refusal is `KeepalivePriorityFailed { errno }` and
    /// leaves the previous priority in force; there is no silent fallback. With no
    /// thread running the priority is only stored, and the call that starts the thread
    /// reports any refusal. The thread's CPU affinity is not changed: it keeps the
    /// affinity it inherited from the thread that started it.
    pub fn set_priority(&self, priority: KeepalivePriority) -> Result<(), SdkError> {
        if let KeepalivePriority::Fifo(p) = priority {
            if !(1..=99).contains(&p) {
                return Err(SdkError::InvalidRequest {
                    message: format!("SCHED_FIFO priority {p} is outside 1 to 99"),
                });
            }
        }
        let mut st = lock(&self.inner.state);
        if let Some(thread) = st.thread {
            apply_priority(&self.inner, thread, priority)
                .map_err(|errno| SdkError::KeepalivePriorityFailed { errno })?;
        }
        st.priority = priority;
        Ok(())
    }

    /// The scheduling last set with `set_priority`; default `KeepalivePriority::Normal`.
    pub fn priority(&self) -> KeepalivePriority {
        lock(&self.inner.state).priority
    }

    /// Set the TTL the keepalive writes on the process clock, in milliseconds, raised to
    /// at least two touch intervals (80 ms at the default 40 ms interval), as `register`
    /// does. The clock is armed with it at once: a longer TTL takes effect now, a shorter
    /// one from the first touch whose `now + ttl` passes the current deadline (arming
    /// never moves a deadline back). `Err(InterlockReaped)` when no process clock is
    /// bound or the arm finds it terminated; the TTL is then unchanged.
    pub fn set_process_ttl_ms(&self, ttl_ms: u64) -> Result<(), SdkError> {
        let mut st = lock(&self.inner.state);
        let Some(process) = st.process.as_mut() else {
            return Err(SdkError::InterlockReaped);
        };
        let ttl_ns = ms_to_nanos(ttl_ms).max(process.interval_ns.saturating_mul(2));
        interlock_extend(&process.clock, ttl_ns).map_err(SdkError::from)?;
        process.ttl_ns = ttl_ns;
        Ok(())
    }

    /// The TTL the keepalive writes on the process clock, in milliseconds; `None` when
    /// no process clock is bound.
    pub fn process_ttl_ms(&self) -> Option<u64> {
        lock(&self.inner.state)
            .process
            .as_ref()
            .map(|p| p.ttl_ns / NANOS_PER_MS)
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
        interlock_extend(&handle, ttl_ns).map_err(SdkError::from)?;
        if let Some(c) = &clock {
            if !stamp_last_seen(&handle, c.words().open_count.load(Ordering::Acquire)) {
                return Err(SdkError::InterlockReaped);
            }
        }

        let reaped = Arc::new(AtomicBool::new(false));
        let mut st = lock(&self.inner.state);
        if st.failed {
            return Err(SdkError::KeepaliveFailed);
        }
        if !st.thread_running {
            spawn_thread(&self.inner, &mut st)?;
        }
        let id = st.next_id;
        st.next_id += 1;
        st.entries.push(Entry {
            id,
            handle,
            interval_ns,
            ttl_ns,
            next_due_ns: now.saturating_add(interval_ns),
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

/// Copy `clock_now` into the handle's open_count, unless it already reads SENTINEL.
/// Returns `false` when SENTINEL was found (the interlock was reaped or freed).
fn stamp_last_seen(handle: &InterlockHandle, clock_now: u64) -> bool {
    handle
        .words()
        .open_count
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |cur| {
            (cur != SENTINEL).then(|| cur.max(clock_now))
        })
        .is_ok()
}

fn spawn_thread(inner: &Arc<Inner>, st: &mut State) -> Result<(), SdkError> {
    #[cfg(test)]
    if inner.fail_spawn.load(Ordering::Acquire) {
        return Err(SdkError::KeepaliveSpawnFailed {
            message: "injected spawn failure".to_string(),
        });
    }
    let weak = Arc::downgrade(inner);
    let handle = thread::Builder::new()
        .name("abacus-keepalive".into())
        .spawn(move || run(weak))
        .map_err(|e| SdkError::KeepaliveSpawnFailed {
            message: e.to_string(),
        })?;
    let thread = handle.as_pthread_t();
    // Detached: the thread ends on its own when its owners are gone. It cannot end
    // before this returns, because it needs the state lock the caller holds.
    drop(handle);
    st.thread_running = true;
    st.thread = Some(thread);
    if st.priority != KeepalivePriority::Normal {
        if let Err(errno) = apply_priority(inner, thread, st.priority) {
            st.priority = KeepalivePriority::Normal;
            return Err(SdkError::KeepalivePriorityFailed { errno });
        }
    }
    Ok(())
}

/// Apply `priority` to `thread` with `pthread_setschedparam`; the error number on
/// refusal. Callers hold the state lock and pass `State::thread`, which is Some
/// only while the thread is alive.
#[cfg_attr(not(test), allow(unused_variables))]
fn apply_priority(
    inner: &Inner,
    thread: libc::pthread_t,
    priority: KeepalivePriority,
) -> Result<(), i32> {
    #[cfg(test)]
    {
        let errno = inner.fail_priority.load(Ordering::Acquire);
        if errno != 0 {
            return Err(errno);
        }
    }
    let (policy, value) = match priority {
        KeepalivePriority::Normal => (libc::SCHED_OTHER, 0),
        KeepalivePriority::Fifo(p) => (libc::SCHED_FIFO, i32::from(p)),
    };
    // SAFETY: a zeroed sched_param is valid; only sched_priority is read on Linux.
    let mut param: libc::sched_param = unsafe { std::mem::zeroed() };
    param.sched_priority = value;
    // SAFETY: `thread` names a live thread (see above); `param` outlives the call.
    let rc = unsafe { libc::pthread_setschedparam(thread, policy, &param) };
    if rc == 0 {
        Ok(())
    } else {
        Err(rc)
    }
}

/// On a normal exit, clears `thread_running` and `thread` so a later registration
/// starts a fresh thread. On a panic, fails closed: marks every entry reaped, drops
/// the process clock, sets `failed`, and aborts the process.
struct RunningGuard(Weak<Inner>);

impl Drop for RunningGuard {
    fn drop(&mut self) {
        let Some(inner) = self.0.upgrade() else {
            return;
        };
        let mut st = lock(&inner.state);
        st.thread_running = false;
        st.thread = None;
        if !std::thread::panicking() {
            return;
        }
        st.failed = true;
        for entry in st.entries.drain(..) {
            entry.reaped.store(true, Ordering::Release);
        }
        let process = st.process.take();
        let diag = if let Some(p) = &process {
            format!(
                "abacus: KeepaliveFailed: keepalive thread panicked, \
                 process clock \"{}\" (id {}) lost; aborting",
                p.name, p.id
            )
        } else {
            "abacus: KeepaliveFailed: keepalive thread panicked; aborting".to_string()
        };
        let _ = writeln!(std::io::stderr(), "{diag}");
        drop(st);
        std::process::abort();
    }
}

fn run(weak: Weak<Inner>) {
    let _running = RunningGuard(weak.clone());
    loop {
        let Some(inner) = weak.upgrade() else {
            return;
        };
        #[cfg(test)]
        if inner.panic_next_pass.swap(false, Ordering::AcqRel) {
            panic!("injected keepalive panic");
        }
        let mut st = lock(&inner.state);
        let now = monotonic_now_nanos();
        let mut next_due = now + IDLE_SLEEP.as_nanos() as u64;

        // Process liveness: check the Abacus clock, then touch the process clock.
        let mut death: Option<Liveness> = None;
        if let Some(proc) = st.process.as_mut() {
            let abacus_exp = proc.abacus_clock.load_expiration();
            if !expiration_alive(abacus_exp, now) {
                death = Some(Liveness::DaemonClockLapsed);
            } else if proc.next_due_ns <= now {
                if interlock_extend(&proc.clock, proc.ttl_ns).is_err()
                    || !stamp_last_seen(&proc.clock, proc.abacus_clock.load_open())
                {
                    death = Some(Liveness::ProcessClockReaped);
                } else {
                    proc.next_due_ns = now.saturating_add(proc.interval_ns);
                }
            }
            if death.is_none() {
                if let Some(p) = st.process.as_ref() {
                    next_due = next_due.min(p.next_due_ns);
                }
            }
        }

        if let Some(d) = death {
            let proc = st.process.take().unwrap();
            match d {
                Liveness::ProcessClockReaped => {
                    let _ = writeln!(
                        std::io::stderr(),
                        "abacus: ProcessClockReaped: process clock \"{}\" (id {}) was reaped; aborting",
                        proc.name, proc.id
                    );
                }
                Liveness::DaemonClockLapsed => {
                    let exp = proc.abacus_clock.load_expiration();
                    let _ = writeln!(
                        std::io::stderr(),
                        "abacus: DaemonClockLapsed: the Abacus clock expired at {exp} ns, now {now} ns; aborting"
                    );
                }
            }
            drop(st);
            std::process::abort();
        }

        let mut i = 0;
        while i < st.entries.len() {
            let entry = &mut st.entries[i];
            if entry.next_due_ns <= now {
                if interlock_extend(&entry.handle, entry.ttl_ns).is_err() {
                    entry.reaped.store(true, Ordering::Release);
                    st.entries.remove(i);
                    continue;
                }
                if let Some(clock) = &entry.clock {
                    if !stamp_last_seen(
                        &entry.handle,
                        clock.words().open_count.load(Ordering::Acquire),
                    ) {
                        entry.reaped.store(true, Ordering::Release);
                        st.entries.remove(i);
                        continue;
                    }
                }
                entry.next_due_ns = now.saturating_add(entry.interval_ns);
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
            {
                let mut st = lock(&inner.state);
                st.thread_running = false;
                st.thread = None;
            }
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
        interlock_arm, interlock_create, interlock_free, interlock_read_expiration, SENTINEL,
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
    fn process_clock_is_touched_while_healthy() {
        let k = Keepalive::new();
        let pc = interlock_create().unwrap();
        let abacus_clock = interlock_create().unwrap();
        interlock_arm(&abacus_clock, ms_to_nanos(5000)).unwrap();
        abacus_clock
            .words()
            .open_count
            .store(1000, Ordering::Release);
        k.bind_process(pc.clone(), "test".into(), 1, abacus_clock.into())
            .unwrap();
        let exp0 = interlock_read_expiration(&pc);
        thread::sleep(Duration::from_millis(3 * DEFAULT_TOUCH_INTERVAL_MS));
        assert!(k.process_bound(), "healthy process was dropped");
        assert!(
            interlock_read_expiration(&pc) > exp0,
            "expiration did not advance on a healthy process clock"
        );
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
        let pc = interlock_create().unwrap();
        let abacus_clock = interlock_create().unwrap();
        interlock_arm(&abacus_clock, ms_to_nanos(5000)).unwrap();
        abacus_clock
            .words()
            .open_count
            .store(1000, Ordering::Release);
        k.inner.fail_spawn.store(true, Ordering::Release);
        assert!(matches!(
            k.bind_process(pc.clone(), "test".into(), 1, abacus_clock.into()),
            Err(SdkError::KeepaliveSpawnFailed { .. })
        ));
        assert!(!k.process_bound());
        assert!(!k.thread_running());
    }

    #[test]
    fn set_priority_refuses_an_out_of_range_fifo_priority() {
        let k = Keepalive::new();
        for p in [0u8, 100] {
            assert!(matches!(
                k.set_priority(KeepalivePriority::Fifo(p)),
                Err(SdkError::InvalidRequest { .. })
            ));
        }
        assert_eq!(k.priority(), KeepalivePriority::Normal);
    }

    #[test]
    fn set_priority_normal_applies_to_a_running_thread() {
        let k = Keepalive::new();
        let h = interlock_create().unwrap();
        let _t = k.register(h, 40, 200).unwrap();
        k.set_priority(KeepalivePriority::Normal).unwrap();
        assert_eq!(k.priority(), KeepalivePriority::Normal);
    }

    #[test]
    fn a_refused_priority_surfaces_its_errno_and_keeps_the_stored_priority() {
        let k = Keepalive::new();
        let h = interlock_create().unwrap();
        let _t = k.register(h, 40, 200).unwrap();
        k.inner.fail_priority.store(libc::EPERM, Ordering::Release);
        assert_eq!(
            k.set_priority(KeepalivePriority::Fifo(10)),
            Err(SdkError::KeepalivePriorityFailed { errno: libc::EPERM })
        );
        assert_eq!(k.priority(), KeepalivePriority::Normal);
    }

    #[test]
    fn a_priority_stored_before_the_thread_starts_is_applied_at_spawn() {
        let k = Keepalive::new();
        k.set_priority(KeepalivePriority::Fifo(10)).unwrap();
        assert_eq!(k.priority(), KeepalivePriority::Fifo(10));
        k.inner.fail_priority.store(libc::EPERM, Ordering::Release);
        let h = interlock_create().unwrap();
        assert_eq!(
            k.register(h, 40, 200).err(),
            Some(SdkError::KeepalivePriorityFailed { errno: libc::EPERM })
        );
        assert_eq!(k.registered(), 0);
        assert_eq!(k.priority(), KeepalivePriority::Normal);
        assert!(
            k.thread_running(),
            "the thread runs at its inherited scheduling"
        );
    }

    #[test]
    fn set_process_ttl_lowers_the_ttl_of_later_touches() {
        let k = Keepalive::new();
        let pc = interlock_create().unwrap();
        let abacus_clock = interlock_create().unwrap();
        interlock_arm(&abacus_clock, ms_to_nanos(5000)).unwrap();
        abacus_clock
            .words()
            .open_count
            .store(1000, Ordering::Release);
        k.bind_process(pc.clone(), "test".into(), 1, abacus_clock.into())
            .unwrap();
        k.set_process_ttl_ms(100).unwrap();
        assert_eq!(k.process_ttl_ms(), Some(100));
        thread::sleep(Duration::from_millis(300));
        let now = monotonic_now_nanos();
        let exp = interlock_read_expiration(&pc);
        assert!(exp > now, "process clock lapsed");
        assert!(
            exp <= now + ms_to_nanos(100),
            "exp {exp} more than 100 ms past now {now}: the 200 ms TTL is still being written"
        );
    }

    #[test]
    fn set_process_ttl_floors_at_two_intervals() {
        let k = Keepalive::new();
        let pc = interlock_create().unwrap();
        let abacus_clock = interlock_create().unwrap();
        interlock_arm(&abacus_clock, ms_to_nanos(5000)).unwrap();
        abacus_clock
            .words()
            .open_count
            .store(1000, Ordering::Release);
        k.bind_process(pc.clone(), "test".into(), 1, abacus_clock.into())
            .unwrap();
        k.set_process_ttl_ms(10).unwrap();
        assert_eq!(k.process_ttl_ms(), Some(2 * DEFAULT_TOUCH_INTERVAL_MS));
    }

    #[test]
    fn set_process_ttl_raises_the_expiration_at_once() {
        let k = Keepalive::new();
        let pc = interlock_create().unwrap();
        let abacus_clock = interlock_create().unwrap();
        interlock_arm(&abacus_clock, ms_to_nanos(5000)).unwrap();
        abacus_clock
            .words()
            .open_count
            .store(1000, Ordering::Release);
        k.bind_process(pc.clone(), "test".into(), 1, abacus_clock.into())
            .unwrap();
        let before = monotonic_now_nanos();
        k.set_process_ttl_ms(1000).unwrap();
        assert!(interlock_read_expiration(&pc) >= before + ms_to_nanos(1000));
    }

    #[test]
    fn set_process_ttl_without_a_bound_process_is_interlock_reaped() {
        let k = Keepalive::new();
        assert_eq!(k.set_process_ttl_ms(100), Err(SdkError::InterlockReaped));
        assert_eq!(k.process_ttl_ms(), None);
    }

    /// A keepalive panic aborts the process. The panic runs in a child so the abort
    /// does not take down the test binary.
    #[test]
    fn keepalive_panic_aborts_the_process() {
        if std::env::var("ABACUS_CHILD_KEEPALIVE_PANIC").is_ok() {
            // Child: register, inject a panic, wait. Exits 3 only if nothing aborted.
            let k = Keepalive::new();
            let h = interlock_create().unwrap();
            let _t = k.register(h, 40, 200).unwrap();
            k.inner.panic_next_pass.store(true, Ordering::Release);
            k.inner.wake.notify_all();
            thread::sleep(Duration::from_secs(2));
            std::process::exit(3);
        }
        let exe = std::env::current_exe().unwrap();
        let output = std::process::Command::new(exe)
            .args([
                "touch::tests::keepalive_panic_aborts_the_process",
                "--exact",
                "--nocapture",
            ])
            .env("ABACUS_CHILD_KEEPALIVE_PANIC", "1")
            .output()
            .unwrap();
        use std::os::unix::process::ExitStatusExt;
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(
            output.status.signal(),
            Some(libc::SIGABRT),
            "child status {:?}, stderr: {stderr}",
            output.status
        );
        assert!(stderr.contains("KeepaliveFailed"), "stderr: {stderr}");
    }
}
