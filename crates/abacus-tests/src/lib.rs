//! testkit: the kit every Abacus test above L0 is written with.
//!
//! L0 unit tests live inside the crates and do not use this kit. L1 (daemon as a thread),
//! L2 (daemon as a child process), L3 (timing and load), and L4 (abuse and soak) do.
//! Everything here is used by many tests and written once.
//!
//! Rules the kit enforces by shape:
//! - [`wait_for`] is the only way a test waits for a condition. No fixed sleeps as sync.
//! - Every daemon gets its own socket path ([`unique_socket_path`]).
//! - Hostile clients speak wire ABI v2 by hand ([`RawClient`]), independent of the SDK.
//! - Child processes are this test binary re-executed with a role ([`role_command`]).
//! - Anything that maps an interlock a hostile client can poison runs in a child process,
//!   never in the test process itself (a shrunk memfd SIGBUSes every mapper).
//!
//! Compile-fail doctests for the CONTRACTS.md permission table live in [`permissions`].

pub mod permissions;

use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use abacus_client::{AbacusClient, TimeoutPolicy};
use abacus_core::error::{ProtocolFault, TransportError};
use abacus_core::interlock::{interlock_map, interlock_map_clock, InterlockHandle};
use abacus_daemon::daemon::{daemon_run_with, DaemonConfig};
use abacus_wire::{
    decode_response, encode_request, read_exact, recv_prefix_with_fds, Request, Response,
};

// ---------------------------------------------------------------------------
// Socket paths and names
// ---------------------------------------------------------------------------

static TEST_COUNTER: AtomicU32 = AtomicU32::new(0);

/// A socket path no other test shares: `<tmp>/abacus-test-<label>-<pid>-<n>.sock`.
/// The label is capped at 40 bytes and the whole path is checked against `sun_path`
/// (108 bytes).
pub fn unique_socket_path(label: &str) -> PathBuf {
    assert!(
        label.len() <= 40,
        "socket label {label:?} is {} bytes; keep labels at or under 40",
        label.len()
    );
    let id = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let path = std::env::temp_dir().join(format!("abacus-test-{label}-{pid}-{id}.sock"));
    assert!(
        path.as_os_str().len() < 108,
        "socket path {} is {} bytes; sun_path holds 107 plus the NUL",
        path.display(),
        path.as_os_str().len()
    );
    path
}

/// An interlock name no other call in this process returns: `<prefix>-<n>`.
pub fn unique_name(prefix: &str) -> String {
    let id = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}-{id}")
}

/// Legacy alias kept for `tests/integration.rs`. New tests use [`unique_socket_path`].
pub fn test_socket_path(label: &str) -> PathBuf {
    unique_socket_path(label)
}

/// Legacy: start a thread daemon and return only its path. New tests use [`ThreadDaemon`].
pub fn start_daemon(label: &str) -> PathBuf {
    let d = ThreadDaemon::start(label);
    let path = d.socket_path().to_path_buf();
    std::mem::forget(d);
    path
}

/// Legacy: remove a socket file. Best effort.
pub fn cleanup(path: &Path) {
    let _ = std::fs::remove_file(path);
}

// ---------------------------------------------------------------------------
// Waiting
// ---------------------------------------------------------------------------

/// The only way tests wait. Polls `cond` every `poll` until it holds or `deadline` passes.
/// Returns the elapsed time on success so callers can assert latency. On failure the
/// error names the deadline; the caller appends the observed state in its panic message.
pub fn wait_for<F: FnMut() -> bool>(
    deadline: Duration,
    poll: Duration,
    mut cond: F,
) -> Result<Duration, String> {
    let start = Instant::now();
    loop {
        if cond() {
            return Ok(start.elapsed());
        }
        if start.elapsed() >= deadline {
            return Err(format!(
                "condition not met within {deadline:?} (polled every {poll:?})"
            ));
        }
        std::thread::sleep(poll);
    }
}

/// [`wait_for`] for a value: polls `probe` until it returns `Some`.
pub fn wait_for_value<T, F: FnMut() -> Option<T>>(
    deadline: Duration,
    poll: Duration,
    mut probe: F,
) -> Result<(T, Duration), String> {
    let start = Instant::now();
    loop {
        if let Some(v) = probe() {
            return Ok((v, start.elapsed()));
        }
        if start.elapsed() >= deadline {
            return Err(format!(
                "value not produced within {deadline:?} (polled every {poll:?})"
            ));
        }
        std::thread::sleep(poll);
    }
}

/// Wait until a daemon accepts connections on `path`. Connect success is the readiness
/// signal: the listener exists and the first poll cycle will accept the queued connection.
pub fn wait_for_daemon(path: &Path, deadline: Duration) -> Result<Duration, String> {
    wait_for(deadline, Duration::from_millis(1), || {
        UnixStream::connect(path).is_ok()
    })
}

/// Run `f` on its own thread and hand back the receiver of its result. Collect with
/// [`recv_within`]. A waiter that never returns leaks one thread instead of hanging the test.
pub fn on_thread<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Receiver<T> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx
}

/// The result of an [`on_thread`] call, or an error naming the deadline.
pub fn recv_within<T>(rx: &Receiver<T>, deadline: Duration) -> Result<T, String> {
    rx.recv_timeout(deadline)
        .map_err(|e| format!("no result within {deadline:?}: {e}"))
}

static SERIAL: Mutex<()> = Mutex::new(());

/// Hold this for the whole body of any test that measures time, CPU, or load. Serializes
/// both within a binary (the mutex) and across binaries (a filesystem lock), so
/// `cargo test --workspace` never runs two measurement tests at once. A poisoned mutex
/// (a previous test panicked while measuring) is taken anyway.
pub fn serialized() -> SerialGuard {
    let mutex = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let lock_path = std::env::temp_dir().join("abacus-test-serial.lock");
    let file = std::fs::File::options()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .unwrap_or_else(|e| panic!("open serial lock {}: {e}", lock_path.display()));
    loop {
        let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if ret == 0 {
            break;
        }
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::Interrupted {
            panic!("flock serial lock: {err}");
        }
    }
    SerialGuard {
        _mutex: mutex,
        _file: file,
    }
}

/// Guard returned by [`serialized`]. Dropping it releases both the in-process mutex and the
/// cross-binary file lock.
pub struct SerialGuard {
    _mutex: MutexGuard<'static, ()>,
    _file: std::fs::File,
}

// ---------------------------------------------------------------------------
// Thread daemon (L1)
// ---------------------------------------------------------------------------

/// A daemon running on a thread in this process. `stop()` or drop shuts it down and joins.
pub struct ThreadDaemon {
    path: PathBuf,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl ThreadDaemon {
    /// Start with defaults on a unique socket path.
    pub fn start(label: &str) -> Self {
        Self::start_with(label, DaemonConfig::new(unique_socket_path(label)))
    }

    /// Start with an explicit config (its socket path is used as given).
    pub fn start_with(label: &str, config: DaemonConfig) -> Self {
        let path = config.socket_path.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = stop.clone();
        let thread = std::thread::Builder::new()
            .name(format!("daemon-{label}"))
            .spawn(move || {
                if let Err(e) = daemon_run_with(&config, &stop_flag) {
                    eprintln!("test daemon failed: {e}");
                }
            })
            .expect("spawn daemon thread");
        wait_for_daemon(&path, Duration::from_secs(5))
            .unwrap_or_else(|e| panic!("thread daemon on {} did not come up: {e}", path.display()));
        Self {
            path,
            stop,
            thread: Some(thread),
        }
    }

    pub fn socket_path(&self) -> &Path {
        &self.path
    }

    /// Set the stop flag and join the thread. The socket file is removed by the daemon.
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }

    /// Connect an SDK client to this daemon, panicking with the error if it fails. Timers
    /// this client creates use `TimeoutPolicy::Error`, not the SDK's `Abort` default: a
    /// missed fatal margin in the test process must not `process::abort()` the test binary
    /// (which skips every `Drop`, leaking the daemon and its socket). Role children created
    /// via `role_command` connect their own `AbacusClient` directly and keep the `Abort`
    /// default, since they are meant to abort on timeout.
    pub fn client(&self) -> AbacusClient {
        let mut client =
            AbacusClient::connect(&self.path, &unique_name("pc"), &[]).unwrap_or_else(|e| {
                panic!(
                    "connect to thread daemon {} failed: {e}",
                    self.path.display()
                )
            });
        client.set_timeout_policy(TimeoutPolicy::Error);
        client
    }

    /// Connect with an explicit clock name and dependencies. Returns the error instead of
    /// panicking. Error policy.
    pub fn client_with(
        &self,
        clock_name: &str,
        dependencies: &[&str],
    ) -> Result<AbacusClient, abacus_client::SdkError> {
        let mut client = AbacusClient::connect(&self.path, clock_name, dependencies)?;
        client.set_timeout_policy(TimeoutPolicy::Error);
        Ok(client)
    }
}

impl Drop for ThreadDaemon {
    fn drop(&mut self) {
        self.stop();
        cleanup(&self.path);
    }
}

// ---------------------------------------------------------------------------
// Process daemon (L2, L3, L4)
// ---------------------------------------------------------------------------

/// How to spawn the daemon process: extra arguments, an RLIMIT_NOFILE soft limit applied
/// before exec, and a CPU set to pin it to (the production deployment pins the daemon to
/// an isolated core; pinning here is the closest a test can get without `isolcpus`).
#[derive(Debug, Clone, Default)]
pub struct ProcessOptions {
    pub args: Vec<String>,
    pub fd_limit: Option<u64>,
    pub cpus: Option<Vec<usize>>,
}

/// The `abacus` binary as a child process on its own socket path. Kills on drop.
///
/// The binary path is `env!("CARGO_BIN_EXE_abacus")` in `crates/abacus-daemon/tests`; from
/// `crates/abacus-tests/tests` use [`abacus_binary`], which resolves it from the target dir.
pub struct ProcessDaemon {
    child: Child,
    path: PathBuf,
    bin: PathBuf,
    stderr_path: PathBuf,
    options: ProcessOptions,
    startup: Duration,
}

impl ProcessDaemon {
    pub fn start(bin: &Path, label: &str) -> Self {
        Self::start_with(bin, label, &[], None)
    }

    /// Start with extra command-line arguments and an optional RLIMIT_NOFILE soft limit
    /// applied in the child before exec (used to drive the daemon to EMFILE cheaply).
    pub fn start_with(bin: &Path, label: &str, extra_args: &[&str], fd_limit: Option<u64>) -> Self {
        Self::start_with_options(
            bin,
            label,
            ProcessOptions {
                args: extra_args.iter().map(|s| s.to_string()).collect(),
                fd_limit,
                cpus: None,
            },
        )
    }

    /// Start pinned to one CPU, the production shape (daemon on an isolated core).
    pub fn start_pinned(bin: &Path, label: &str, cpu: usize) -> Self {
        Self::start_with_options(
            bin,
            label,
            ProcessOptions {
                cpus: Some(vec![cpu]),
                ..ProcessOptions::default()
            },
        )
    }

    pub fn start_with_options(bin: &Path, label: &str, options: ProcessOptions) -> Self {
        let path = unique_socket_path(label);
        let stderr_path = path.with_extension("stderr");
        let t0 = Instant::now();
        let child = Self::spawn(bin, &path, &stderr_path, &options);
        let mut d = Self {
            child,
            path,
            bin: bin.to_path_buf(),
            stderr_path,
            options,
            startup: Duration::ZERO,
        };
        match wait_for_daemon(&d.path, Duration::from_secs(5)) {
            Ok(_) => d.startup = t0.elapsed(),
            Err(e) => {
                let alive = d.is_alive();
                let stderr = d.stderr_lines().join("\n");
                panic!(
                    "process daemon {} on {} did not come up: {e}; alive={alive}; stderr:\n{stderr}",
                    d.bin.display(),
                    d.path.display()
                );
            }
        }
        d
    }

    fn spawn(bin: &Path, path: &Path, stderr_path: &Path, options: &ProcessOptions) -> Child {
        let stderr = File::options()
            .create(true)
            .append(true)
            .open(stderr_path)
            .expect("open daemon stderr file");
        let mut cmd = Command::new(bin);
        cmd.arg(format!("--socket-path={}", path.display()))
            .args(&options.args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(stderr));
        // SAFETY: prctl(PR_SET_PDEATHSIG) is async-signal-safe and touches no Rust state.
        // Kills this daemon child if the test process dies before reaping it (a SIGABRT
        // from TimeoutPolicy::Abort in some other thread, a panic, or the test binary being
        // killed outright), so a leaked ProcessDaemon does not leak its child too.
        unsafe {
            cmd.pre_exec(set_pdeathsig_kill);
        }
        if let Some(n) = options.fd_limit {
            // SAFETY: setrlimit is async-signal-safe and touches no Rust state.
            unsafe {
                cmd.pre_exec(move || set_nofile_limit(n));
            }
        }
        if let Some(cpus) = options.cpus.clone() {
            // SAFETY: sched_setaffinity is async-signal-safe and touches no Rust state.
            unsafe {
                cmd.pre_exec(move || set_affinity(&cpus));
            }
        }
        cmd.spawn()
            .unwrap_or_else(|e| panic!("spawn {} failed: {e}", bin.display()))
    }

    /// The CPU set the daemon was pinned to, if any.
    pub fn cpus(&self) -> Option<&[usize]> {
        self.options.cpus.as_deref()
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    pub fn socket_path(&self) -> &Path {
        &self.path
    }

    pub fn binary(&self) -> &Path {
        &self.bin
    }

    /// Time from spawn to the first successful connect.
    pub fn startup_time(&self) -> Duration {
        self.startup
    }

    /// Send `signal` (a `libc::SIG*` constant) to the daemon.
    pub fn kill(&mut self, signal: i32) {
        let pid = self.child.id() as libc::pid_t;
        // SAFETY: plain kill(2) on a pid this guard owns.
        let rc = unsafe { libc::kill(pid, signal) };
        assert_eq!(
            rc,
            0,
            "kill({pid}, {signal}) failed: {}",
            io::Error::last_os_error()
        );
    }

    /// Wait for the daemon to exit, reaping it. Fails with the observed state on deadline.
    pub fn wait_exit(&mut self, deadline: Duration) -> Result<ExitStatus, String> {
        let start = Instant::now();
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => return Ok(status),
                Ok(None) => {}
                Err(e) => return Err(format!("try_wait failed: {e}")),
            }
            if start.elapsed() >= deadline {
                return Err(format!(
                    "daemon pid {} still running after {deadline:?}",
                    self.child.id()
                ));
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// True while the child has not exited. Reaps it if it has.
    pub fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// utime + stime in clock ticks from `/proc/<pid>/stat`.
    pub fn cpu_ticks(&self) -> u64 {
        cpu_ticks_of(self.child.id())
    }

    /// CPU ticks consumed over a sampling window.
    pub fn cpu_ticks_over(&self, window: Duration) -> u64 {
        let a = self.cpu_ticks();
        std::thread::sleep(window);
        self.cpu_ticks().saturating_sub(a)
    }

    pub fn fd_count(&self) -> usize {
        fd_count_of(self.child.id())
    }

    pub fn threads(&self) -> u64 {
        proc_status_field(self.child.id(), "Threads")
    }

    pub fn vm_rss_kb(&self) -> u64 {
        proc_status_field(self.child.id(), "VmRSS")
    }

    /// Everything the daemon has written to stderr so far, one entry per line.
    pub fn stderr_lines(&self) -> Vec<String> {
        std::fs::read_to_string(&self.stderr_path)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// Lines the daemon logged for client protocol faults.
    pub fn fault_lines(&self) -> Vec<String> {
        self.stderr_lines()
            .into_iter()
            .filter(|l| l.contains("protocol fault") || l.contains("request error"))
            .collect()
    }

    /// SIGKILL the daemon if it is still running, reap it, and start a fresh one on the same socket path.
    /// The stale socket file is left for the new daemon to replace, as a real restart would.
    pub fn restart(&mut self) {
        if self.is_alive() {
            self.kill(libc::SIGKILL);
        }
        self.wait_exit(Duration::from_secs(5))
            .unwrap_or_else(|e| panic!("restart: old daemon did not exit: {e}"));
        let t0 = Instant::now();
        self.child = Self::spawn(&self.bin, &self.path, &self.stderr_path, &self.options);
        wait_for_daemon(&self.path, Duration::from_secs(5)).unwrap_or_else(|e| {
            panic!(
                "restart: new daemon on {} did not come up: {e}",
                self.path.display()
            )
        });
        self.startup = t0.elapsed();
    }

    /// Connect an SDK client to this daemon, panicking with the error if it fails. Timers
    /// this client creates use `TimeoutPolicy::Error`, not the SDK's `Abort` default: a
    /// missed fatal margin in the test process must not `process::abort()` the test binary
    /// (which skips every `Drop`, leaking the daemon child and its socket). Role children
    /// created via `role_command` connect their own `AbacusClient` directly and keep the
    /// `Abort` default, since they are meant to abort on timeout.
    pub fn client(&self) -> AbacusClient {
        let mut client =
            AbacusClient::connect(&self.path, &unique_name("pc"), &[]).unwrap_or_else(|e| {
                panic!(
                    "connect to process daemon {} failed: {e}",
                    self.path.display()
                )
            });
        client.set_timeout_policy(TimeoutPolicy::Error);
        client
    }

    /// Connect with an explicit clock name and dependencies. Returns the error instead of
    /// panicking. Error policy.
    pub fn client_with(
        &self,
        clock_name: &str,
        dependencies: &[&str],
    ) -> Result<AbacusClient, abacus_client::SdkError> {
        let mut client = AbacusClient::connect(&self.path, clock_name, dependencies)?;
        client.set_timeout_policy(TimeoutPolicy::Error);
        Ok(client)
    }
}

impl Drop for ProcessDaemon {
    fn drop(&mut self) {
        if self.is_alive() {
            self.kill(libc::SIGKILL);
            let _ = self.child.wait();
        }
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::remove_file(&self.stderr_path);
    }
}

/// Locate the `abacus` binary from a test binary in `crates/abacus-tests/tests`: honors
/// `CARGO_BIN_EXE_abacus` when set, else `target/<profile>/abacus` next to `current_exe`.
/// Panics with the build command if neither resolves.
pub fn abacus_binary() -> PathBuf {
    if let Ok(p) = std::env::var("CARGO_BIN_EXE_abacus") {
        return PathBuf::from(p);
    }
    let exe = std::env::current_exe().expect("current_exe");
    // <target>/<profile>/deps/<test>-<hash> -> <target>/<profile>/abacus
    let candidate = exe
        .parent()
        .and_then(Path::parent)
        .map(|p| p.join("abacus"));
    match candidate {
        Some(p) if p.is_file() => p,
        other => panic!(
            "abacus binary not found at {:?}; build it first: cargo build -p abacus-daemon \
             (same profile as the tests)",
            other
        ),
    }
}

// ---------------------------------------------------------------------------
// /proc helpers and limits
// ---------------------------------------------------------------------------

/// utime + stime (clock ticks, `getconf CLK_TCK` per second) from `/proc/<pid>/stat`.
pub fn cpu_ticks_of(pid: u32) -> u64 {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
    // Fields after the last ')' start at field 3 (state); utime is field 14, stime 15.
    let rest = match stat.rfind(')') {
        Some(i) => &stat[i + 1..],
        None => return 0,
    };
    let fields: Vec<&str> = rest.split_whitespace().collect();
    let utime: u64 = fields.get(11).and_then(|s| s.parse().ok()).unwrap_or(0);
    let stime: u64 = fields.get(12).and_then(|s| s.parse().ok()).unwrap_or(0);
    utime + stime
}

/// Number of entries in `/proc/<pid>/fd`.
pub fn fd_count_of(pid: u32) -> usize {
    std::fs::read_dir(format!("/proc/{pid}/fd"))
        .map(|d| d.count())
        .unwrap_or(0)
}

/// First integer on the `<field>:` line of `/proc/<pid>/status` (e.g. `Threads`, `VmRSS`).
pub fn proc_status_field(pid: u32, field: &str) -> u64 {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap_or_default();
    status
        .lines()
        .find(|l| l.starts_with(field) && l[field.len()..].starts_with(':'))
        .and_then(|l| l[field.len() + 1..].split_whitespace().next())
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

/// Clock ticks per second for `/proc` CPU accounting.
pub fn clock_ticks_per_second() -> u64 {
    // SAFETY: sysconf has no preconditions.
    let v = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if v <= 0 {
        100
    } else {
        v as u64
    }
}

/// Cores available to the scheduler (excludes isolated cores).
pub fn nproc() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

/// Total online cores including isolated ones.
pub fn nproc_online() -> usize {
    std::fs::read_to_string("/sys/devices/system/cpu/online")
        .ok()
        .and_then(|s| {
            let s = s.trim();
            if let Some((_, end)) = s.rsplit_once('-') {
                end.parse::<usize>().ok().map(|e| e + 1)
            } else {
                s.parse::<usize>().ok().map(|n| n + 1)
            }
        })
        .unwrap_or_else(nproc)
}

/// Arm `PR_SET_PDEATHSIG(SIGKILL)` in a `pre_exec` child so it is killed if its parent (this
/// test process) dies first, instead of being reparented and leaked. Linux delivers the
/// signal when the thread that called `prctl` exits, which for `Command::spawn` is the
/// freshly forked child's own thread of control, so this holds regardless of which thread in
/// the parent process spawned it.
fn set_pdeathsig_kill() -> io::Result<()> {
    // SAFETY: prctl(PR_SET_PDEATHSIG) is async-signal-safe and touches no Rust state.
    unsafe {
        if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

fn set_affinity(cpus: &[usize]) -> io::Result<()> {
    // SAFETY: a zeroed cpu_set_t is the empty set; CPU_SET only writes inside it.
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        for &c in cpus {
            libc::CPU_SET(c, &mut set);
        }
        if libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set) != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Pin the calling thread to `cpus`. Threads spawned afterwards inherit the set, so pinning
/// a test thread before it creates SDK clients keeps their touch threads off the daemon's
/// core as well.
pub fn pin_current_thread(cpus: &[usize]) {
    set_affinity(cpus).unwrap_or_else(|e| panic!("sched_setaffinity({cpus:?}) failed: {e}"));
}

/// Every online CPU index except `cpu`: the cores left to clients and load when the daemon
/// is pinned to `cpu`. Includes isolated cores so load tests can pin threads there.
pub fn all_cores_except(cpu: usize) -> Vec<usize> {
    (0..nproc_online()).filter(|&c| c != cpu).collect()
}

/// The core the production shape pins the daemon to in tests, and whether that core is
/// actually isolated via the kernel's `isolcpus` parameter. Returns `(core, is_isolated)`.
/// Picks the first isolated core from `/sys/devices/system/cpu/isolated`, falling back to
/// the last online core when none are isolated. Callers should include the isolation state
/// in timing-failure messages so results are verifiable.
pub fn daemon_core() -> (usize, bool) {
    match std::fs::read_to_string("/sys/devices/system/cpu/isolated")
        .ok()
        .and_then(|s| {
            let s = s.trim();
            if s.is_empty() {
                return None;
            }
            s.split(',')
                .next()
                .and_then(|range| range.split('-').next())
                .and_then(|first| first.parse().ok())
        }) {
        Some(core) => (core, true),
        None => (nproc_online() - 1, false),
    }
}

fn set_nofile_limit(n: u64) -> io::Result<()> {
    let mut lim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit/setrlimit on a local struct.
    unsafe {
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) != 0 {
            return Err(io::Error::last_os_error());
        }
        lim.rlim_cur = n.min(lim.rlim_max);
        if libc::setrlimit(libc::RLIMIT_NOFILE, &lim) != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Raise this process's RLIMIT_NOFILE soft limit to `min(to, hard)`. Returns the effective
/// soft limit. Children inherit it.
pub fn raise_fd_limit(to: u64) -> u64 {
    set_nofile_limit(to).unwrap_or_else(|e| panic!("setrlimit(NOFILE) failed: {e}"));
    let mut lim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit on a local struct.
    unsafe {
        libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim);
    }
    lim.rlim_cur
}

// ---------------------------------------------------------------------------
// Process roles (child processes are this test binary re-executed)
// ---------------------------------------------------------------------------

pub const ROLE_ENV: &str = "ABACUS_TEST_ROLE";
pub const ROLE_ARGS_ENV: &str = "ABACUS_TEST_ROLE_ARGS";
const ROLE_ARG_SEP: char = '\u{1f}';

/// A [`Command`] that re-executes this test binary running only the ignored test `role`,
/// with `ROLE_ENV` set so [`role_args`] returns `Some` inside it. Roles are declared as
/// `#[test] #[ignore = "role: ..."]` functions that return immediately unless `role_args`
/// returns `Some`. The caller sets stdio and spawns.
pub fn role_command(role: &str, args: &[&str]) -> Command {
    let exe = std::env::current_exe().expect("current_exe");
    let joined: String = args
        .iter()
        .map(|s| s.to_string())
        .collect::<Vec<_>>()
        .join(&ROLE_ARG_SEP.to_string());
    let mut cmd = Command::new(exe);
    cmd.arg(role)
        .args(["--exact", "--ignored", "--nocapture", "--test-threads=1"])
        .env(ROLE_ENV, role)
        .env(ROLE_ARGS_ENV, joined)
        .stdin(Stdio::null());
    cmd
}

/// Inside a child spawned by [`role_command`] for `role`: the arguments it was given.
/// `None` when this process is not that role (the role fn then returns without acting).
pub fn role_args(role: &str) -> Option<Vec<String>> {
    if std::env::var(ROLE_ENV).ok()? != role {
        return None;
    }
    let raw = std::env::var(ROLE_ARGS_ENV).unwrap_or_default();
    if raw.is_empty() {
        return Some(Vec::new());
    }
    Some(raw.split(ROLE_ARG_SEP).map(str::to_string).collect())
}

/// Wait for a child with a deadline; kills it and fails with the observed state otherwise.
pub fn wait_child(child: &mut Child, deadline: Duration) -> Result<ExitStatus, String> {
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => {}
            Err(e) => return Err(format!("try_wait failed: {e}")),
        }
        if start.elapsed() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!(
                "child pid {} still running after {deadline:?}; killed",
                child.id()
            ));
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// What a child left behind: its exit status and everything it wrote to piped stdout and
/// stderr (empty when the caller did not pipe them).
pub struct ChildRun {
    pub status: ExitStatus,
    pub stdout: String,
    pub stderr: String,
}

/// Drain a child's piped stdout and stderr on threads while waiting for it with a deadline
/// ([`wait_child`]); on deadline the child is killed and the test panics with the reason.
/// Draining on threads matters: a child that fills a pipe blocks until someone reads it.
pub fn run_child(mut child: Child, deadline: Duration) -> ChildRun {
    let out = child.stdout.take();
    let err = child.stderr.take();
    let out_t = std::thread::spawn(move || {
        let mut s = String::new();
        if let Some(mut o) = out {
            let _ = o.read_to_string(&mut s);
        }
        s
    });
    let err_t = std::thread::spawn(move || {
        let mut s = String::new();
        if let Some(mut e) = err {
            let _ = e.read_to_string(&mut s);
        }
        s
    });
    let status = wait_child(&mut child, deadline).unwrap_or_else(|e| panic!("{e}"));
    ChildRun {
        status,
        stdout: out_t.join().unwrap_or_default(),
        stderr: err_t.join().unwrap_or_default(),
    }
}

/// Human-readable exit: `exit code 0`, `signal 6 (SIGABRT)`, ...
pub fn describe_exit(status: &ExitStatus) -> String {
    match (status.code(), status.signal()) {
        (Some(c), _) => format!("exit code {c}"),
        (None, Some(s)) => format!("signal {s} ({})", signal_name(s)),
        _ => "unknown exit".to_string(),
    }
}

pub fn signal_name(signal: i32) -> &'static str {
    match signal {
        libc::SIGABRT => "SIGABRT",
        libc::SIGKILL => "SIGKILL",
        libc::SIGTERM => "SIGTERM",
        libc::SIGSEGV => "SIGSEGV",
        libc::SIGBUS => "SIGBUS",
        libc::SIGINT => "SIGINT",
        libc::SIGPIPE => "SIGPIPE",
        _ => "other",
    }
}

// ---------------------------------------------------------------------------
// Child output pipes
// ---------------------------------------------------------------------------

/// Lines read from a child's pipe, each stamped with the instant the reader thread read it.
pub type LineRx = Receiver<(Instant, String)>;

/// Spawn a thread that reads `pipe` line by line, sending (Instant, line) over a channel.
/// A read error is sent as one final `<{label} read error: ...>` line.
fn spawn_line_reader<R: Read + Send + 'static>(pipe: R, label: &'static str) -> LineRx {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(pipe);
        // A reader that stops early closes the child's pipe and makes the child's next write fail with EPIPE.
        let mut receiver_gone = false;
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    let line = line.trim_end_matches(['\n', '\r']).to_string();
                    if !receiver_gone && tx.send((Instant::now(), line)).is_err() {
                        receiver_gone = true;
                    }
                }
                Err(e) => {
                    if !receiver_gone {
                        let _ = tx.send((Instant::now(), format!("<{label} read error: {e}>")));
                    }
                    break;
                }
            }
        }
    });
    rx
}

/// Spawn a thread that reads a child's stdout line by line, sending (Instant, line) over a
/// channel. Start right after spawn. `wait_for_ready` takes stdout itself, so use one or the
/// other on a child.
pub fn spawn_stdout_reader(child: &mut Child) -> LineRx {
    let stdout = child.stdout.take().expect("child stdout not piped");
    spawn_line_reader(stdout, "stdout")
}

/// Spawn a thread that reads a child's stderr line by line, sending (Instant, line) over a
/// channel. Start right after spawn.
pub fn spawn_stderr_reader(child: &mut Child) -> LineRx {
    let stderr = child.stderr.take().expect("child stderr not piped");
    spawn_line_reader(stderr, "stderr")
}

/// Wait for a child to print `ready` on stdout, within 5 s. `role_command` runs the role
/// under libtest's own `--exact --nocapture`, which writes a `test <name> ... ` status
/// prefix to stdout before the role's own output, on the same line (no newline between
/// them) -- so the line the role's `println!("ready")` lands on is never `ready` alone. A
/// line counts when its last whitespace-separated word is exactly `ready`: an exact-word
/// match, not a raw substring search that would also fire on a word like `already`.
/// Panics on deadline.
pub fn wait_for_ready(child: &mut std::process::Child) {
    let rx = spawn_stdout_reader(child);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            panic!("child pid {} did not print ready within 5 s", child.id());
        }
        match rx.recv_timeout(remaining) {
            Ok((_, line)) if line.split_whitespace().last() == Some("ready") => return,
            Ok(_) => continue,
            Err(_) => panic!("child pid {} did not print ready within 5 s", child.id()),
        }
    }
}

/// The first `abacus: ` line from a stderr reader, if it was read by `deadline`. A line
/// already buffered when the deadline passes is still examined, but its read timestamp
/// decides: a line read after the deadline is an error naming how late it was, never a pass.
pub fn recv_abacus_line(rx: &LineRx, deadline: Instant) -> Result<(Instant, String), String> {
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let next = if remaining.is_zero() {
            rx.try_recv().ok()
        } else {
            rx.recv_timeout(remaining).ok()
        };
        match next {
            Some((t, line)) if line.starts_with("abacus: ") => {
                if t > deadline {
                    return Err(format!(
                        "first abacus: line was read {} ms after the deadline: {line}",
                        (t - deadline).as_millis()
                    ));
                }
                return Ok((t, line));
            }
            Some(_) => continue,
            None => return Err("no abacus: line by the deadline".to_string()),
        }
    }
}

/// The reason field of an SDK diagnostic line `abacus: <Reason>: <detail>`, for an exact
/// comparison.
pub fn abacus_reason(line: &str) -> Option<&str> {
    line.strip_prefix("abacus: ")?.split(':').next()
}

// ---------------------------------------------------------------------------
// Wire ABI v2 by hand
// ---------------------------------------------------------------------------

pub const WIRE_VERSION: u8 = 2;
pub const TAG_CREATE: u8 = 0x01;
pub const TAG_ATTACH: u8 = 0x02;
pub const TAG_CREATED: u8 = 0x81;
pub const TAG_ATTACHED: u8 = 0x82;
pub const TAG_ERROR: u8 = 0x88;
pub const ERR_INTERLOCK_REAPED: u8 = 0x01;
pub const ERR_INTERLOCK_NOT_FOUND: u8 = 0x02;
pub const ERR_ALLOCATION_FAILED: u8 = 0x03;
pub const ERR_INVALID_REQUEST: u8 = 0x04;
pub const MAX_PAYLOAD: usize = 4096;
pub const TIER_INTERLOCK: u8 = 0;
pub const TIER_WAIT_COUNTER: u8 = 1;
pub const TIER_WAIT_TIMER: u8 = 2;
pub const TIER_WAIT_CRON: u8 = 3;
pub const TIER_WAIT_BARRIER: u8 = 4;

/// A decoded daemon response, parsed by hand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RawResponse {
    Created { id: u64 },
    Attached { id: u64, tier: u8 },
    Error { code: u8, message: String },
}

/// u32 LE length prefix plus payload.
pub fn frame(payload: &[u8]) -> Vec<u8> {
    let mut f = (payload.len() as u32).to_le_bytes().to_vec();
    f.extend_from_slice(payload);
    f
}

fn put_string(buf: &mut Vec<u8>, s: &[u8]) {
    buf.extend_from_slice(&(s.len() as u16).to_le_bytes());
    buf.extend_from_slice(s);
}

/// Create request payload (no length prefix). `watched` is (name, word) for tier 1;
/// `interval_ns` for tier 3; `conditions` as (name, word, threshold) for tier 4.
/// `owner` is (name, id); `dependencies` is a list of names.
pub fn create_payload(
    name: &[u8],
    tier: u8,
    watched: Option<(&[u8], u8)>,
    interval_ns: Option<u64>,
    conditions: Option<&[(&[u8], u8, u64)]>,
) -> Vec<u8> {
    create_payload_v2(name, tier, None, &[], watched, interval_ns, conditions)
}

/// Full v2 create payload with owner and dependencies.
pub fn create_payload_v2(
    name: &[u8],
    tier: u8,
    owner: Option<(&[u8], u64)>,
    dependencies: &[&[u8]],
    watched: Option<(&[u8], u8)>,
    interval_ns: Option<u64>,
    conditions: Option<&[(&[u8], u8, u64)]>,
) -> Vec<u8> {
    let mut p = vec![WIRE_VERSION, TAG_CREATE];
    put_string(&mut p, name);
    p.push(tier);
    match owner {
        Some((oname, oid)) => {
            p.push(1);
            put_string(&mut p, oname);
            p.extend_from_slice(&oid.to_le_bytes());
        }
        None => {
            p.push(0);
        }
    }
    p.extend_from_slice(&(dependencies.len() as u16).to_le_bytes());
    for dep in dependencies {
        put_string(&mut p, dep);
    }
    if let Some((wn, ww)) = watched {
        put_string(&mut p, wn);
        p.push(ww);
    }
    if let Some(i) = interval_ns {
        p.extend_from_slice(&i.to_le_bytes());
    }
    if let Some(conds) = conditions {
        p.extend_from_slice(&(conds.len() as u16).to_le_bytes());
        for (wn, ww, t) in conds {
            put_string(&mut p, wn);
            p.push(*ww);
            p.extend_from_slice(&t.to_le_bytes());
        }
    }
    p
}

/// Attach request payload (no length prefix).
pub fn attach_payload(name: &[u8]) -> Vec<u8> {
    let mut p = vec![WIRE_VERSION, TAG_ATTACH];
    put_string(&mut p, name);
    p
}

/// Parse a response payload by hand. Lenient on a missing error message (the test client
/// must not be stricter than the daemon it is probing).
pub fn parse_response(payload: &[u8]) -> Result<RawResponse, String> {
    if payload.len() < 2 {
        return Err(format!(
            "response payload too short: {} bytes",
            payload.len()
        ));
    }
    if payload[0] != WIRE_VERSION {
        return Err(format!("unexpected version byte {}", payload[0]));
    }
    let rest = &payload[2..];
    match payload[1] {
        TAG_CREATED => {
            if rest.len() < 8 {
                return Err(format!("id truncated: {} bytes", rest.len()));
            }
            let id = u64::from_le_bytes(rest[..8].try_into().unwrap());
            Ok(RawResponse::Created { id })
        }
        TAG_ATTACHED => {
            if rest.len() < 9 {
                return Err(format!("attached truncated: {} bytes", rest.len()));
            }
            let id = u64::from_le_bytes(rest[..8].try_into().unwrap());
            let tier = rest[8];
            Ok(RawResponse::Attached { id, tier })
        }
        TAG_ERROR => {
            if rest.is_empty() {
                return Err("error response without code".to_string());
            }
            let code = rest[0];
            let message = if rest.len() >= 3 {
                let len = u16::from_le_bytes([rest[1], rest[2]]) as usize;
                String::from_utf8_lossy(&rest[3..(3 + len).min(rest.len())]).into_owned()
            } else {
                String::new()
            };
            Ok(RawResponse::Error { code, message })
        }
        tag => Err(format!("unknown response tag 0x{tag:02x}")),
    }
}

/// sendmsg with SCM_RIGHTS. Returns bytes sent.
pub fn send_with_fds(stream: &UnixStream, bytes: &[u8], fds: &[RawFd]) -> io::Result<usize> {
    let fd_bytes = std::mem::size_of_val(fds);
    // SAFETY: CMSG_SPACE is a pure size computation.
    let space = unsafe { libc::CMSG_SPACE(fd_bytes as libc::c_uint) } as usize;
    let mut cmsg_buf: Vec<u64> = vec![0; space.div_ceil(8).max(1)];
    let mut iov = libc::iovec {
        iov_base: bytes.as_ptr() as *mut libc::c_void,
        iov_len: bytes.len(),
    };
    // SAFETY: zeroed msghdr is a valid starting point; every pointer set below outlives the call.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    if !fds.is_empty() {
        msg.msg_control = cmsg_buf.as_mut_ptr() as *mut libc::c_void;
        msg.msg_controllen = space;
        // SAFETY: the control buffer is CMSG_SPACE(fd_bytes) bytes, 8-byte aligned.
        unsafe {
            let c = libc::CMSG_FIRSTHDR(&msg);
            (*c).cmsg_level = libc::SOL_SOCKET;
            (*c).cmsg_type = libc::SCM_RIGHTS;
            (*c).cmsg_len = libc::CMSG_LEN(fd_bytes as libc::c_uint) as usize;
            std::ptr::copy_nonoverlapping(fds.as_ptr() as *const u8, libc::CMSG_DATA(c), fd_bytes);
        }
    }
    loop {
        // SAFETY: msg points at live buffers for the duration of the call.
        let rc = unsafe { libc::sendmsg(stream.as_raw_fd(), &msg, libc::MSG_NOSIGNAL) };
        if rc < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        return Ok(rc as usize);
    }
}

/// recvmsg with room for `max_fds` SCM_RIGHTS fds. Returns (bytes, fds, MSG_CTRUNC set).
pub fn recv_with_fds(
    stream: &UnixStream,
    buf: &mut [u8],
    max_fds: usize,
) -> io::Result<(usize, Vec<OwnedFd>, bool)> {
    let fd_bytes = max_fds * std::mem::size_of::<RawFd>();
    // SAFETY: CMSG_SPACE is a pure size computation.
    let space = unsafe { libc::CMSG_SPACE(fd_bytes as libc::c_uint) } as usize;
    let mut cmsg_buf: Vec<u64> = vec![0; space.div_ceil(8).max(1)];
    let mut iov = libc::iovec {
        iov_base: buf.as_mut_ptr() as *mut libc::c_void,
        iov_len: buf.len(),
    };
    // SAFETY: zeroed msghdr; pointers set below outlive the call.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cmsg_buf.as_mut_ptr() as *mut libc::c_void;
    msg.msg_controllen = space;
    let received = loop {
        // SAFETY: msg points at live buffers.
        let rc = unsafe { libc::recvmsg(stream.as_raw_fd(), &mut msg, libc::MSG_CMSG_CLOEXEC) };
        if rc < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        break rc as usize;
    };
    let mut fds = Vec::new();
    // SAFETY: walking the control buffer the kernel filled in.
    unsafe {
        let mut c = libc::CMSG_FIRSTHDR(&msg);
        while !c.is_null() {
            if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                let data_len = (*c).cmsg_len as usize - libc::CMSG_LEN(0) as usize;
                let n = data_len / std::mem::size_of::<RawFd>();
                let data = libc::CMSG_DATA(c) as *const RawFd;
                for i in 0..n {
                    fds.push(OwnedFd::from_raw_fd(std::ptr::read_unaligned(data.add(i))));
                }
            }
            c = libc::CMSG_NXTHDR(&msg, c);
        }
    }
    let ctrunc = msg.msg_flags & libc::MSG_CTRUNC != 0;
    Ok((received, fds, ctrunc))
}

fn read_exact_stream(stream: &mut UnixStream, buf: &mut [u8]) -> Result<(), String> {
    let mut filled = 0;
    while filled < buf.len() {
        match stream.read(&mut buf[filled..]) {
            Ok(0) => return Err(format!("peer closed after {filled} of {} bytes", buf.len())),
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                return Err(format!("timed out after {filled} of {} bytes", buf.len()))
            }
            Err(e) => return Err(format!("read failed: {e}")),
        }
    }
    Ok(())
}

/// Read one length-prefixed frame (prefix plus payload) from a stream, honoring its read
/// timeout. Returns the payload.
pub fn read_frame(stream: &mut UnixStream) -> Result<Vec<u8>, String> {
    let mut prefix = [0u8; 4];
    read_exact_stream(stream, &mut prefix)?;
    let len = u32::from_le_bytes(prefix) as usize;
    if len > MAX_PAYLOAD {
        return Err(format!("frame claims {len} bytes, over {MAX_PAYLOAD}"));
    }
    let mut payload = vec![0u8; len];
    read_exact_stream(stream, &mut payload)?;
    Ok(payload)
}

/// A `UnixStream` that speaks wire ABI v2 by hand. This is how hostile and protocol tests
/// are written, independent of the SDK. Defaults to a 2 s read timeout so a wedged daemon
/// fails the test instead of hanging it.
pub struct RawClient {
    stream: UnixStream,
}

impl RawClient {
    pub fn connect(path: &Path) -> io::Result<Self> {
        let stream = UnixStream::connect(path)?;
        stream.set_read_timeout(Some(Duration::from_secs(2)))?;
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;
        Ok(Self { stream })
    }

    pub fn set_read_timeout(&self, timeout: Option<Duration>) {
        self.stream
            .set_read_timeout(timeout)
            .expect("set_read_timeout");
    }

    /// Default 5 s: a daemon that stops draining a flood fails the test instead of hanging it.
    pub fn set_write_timeout(&self, timeout: Option<Duration>) {
        self.stream
            .set_write_timeout(timeout)
            .expect("set_write_timeout");
    }

    pub fn stream(&self) -> &UnixStream {
        &self.stream
    }

    pub fn as_raw_fd(&self) -> RawFd {
        self.stream.as_raw_fd()
    }

    /// Write bytes as-is, no framing. EPIPE surfaces as an error, never a signal.
    pub fn send_bytes(&mut self, bytes: &[u8]) -> io::Result<()> {
        let mut written = 0;
        while written < bytes.len() {
            // SAFETY: plain send(2) on our own socket with a live buffer.
            let rc = unsafe {
                libc::send(
                    self.stream.as_raw_fd(),
                    bytes[written..].as_ptr() as *const libc::c_void,
                    bytes.len() - written,
                    libc::MSG_NOSIGNAL,
                )
            };
            if rc < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e);
            }
            written += rc as usize;
        }
        Ok(())
    }

    /// Write a length-prefixed frame around `payload`.
    pub fn send_frame(&mut self, payload: &[u8]) -> io::Result<()> {
        self.send_bytes(&frame(payload))
    }

    pub fn send_create(
        &mut self,
        name: &str,
        tier: u8,
        watched: Option<(&str, u8)>,
        interval_ns: Option<u64>,
        conditions: Option<&[(&str, u8, u64)]>,
    ) -> io::Result<()> {
        let conds: Option<Vec<(&[u8], u8, u64)>> =
            conditions.map(|c| c.iter().map(|(n, w, t)| (n.as_bytes(), *w, *t)).collect());
        let payload = create_payload_v2(
            name.as_bytes(),
            tier,
            None,
            &[],
            watched.map(|(n, w)| (n.as_bytes(), w)),
            interval_ns,
            conds.as_deref(),
        );
        self.send_frame(&payload)
    }

    pub fn send_attach(&mut self, name: &str) -> io::Result<()> {
        self.send_frame(&attach_payload(name.as_bytes()))
    }

    /// Send a create request and wait for its response.
    pub fn create(
        &mut self,
        name: &str,
        tier: u8,
        watched: Option<(&str, u8)>,
        interval_ns: Option<u64>,
        conditions: Option<&[(&str, u8, u64)]>,
    ) -> Result<(RawResponse, Vec<OwnedFd>), String> {
        self.send_create(name, tier, watched, interval_ns, conditions)
            .map_err(|e| format!("send create failed: {e}"))?;
        self.recv_response_with_fds()
    }

    /// Send an attach request and wait for its response.
    pub fn attach(&mut self, name: &str) -> Result<(RawResponse, Vec<OwnedFd>), String> {
        self.send_attach(name)
            .map_err(|e| format!("send attach failed: {e}"))?;
        self.recv_response_with_fds()
    }

    /// Receive one response frame and any fds passed with it. MSG_CTRUNC is an error.
    pub fn recv_response_with_fds(&mut self) -> Result<(RawResponse, Vec<OwnedFd>), String> {
        let mut prefix = [0u8; 4];
        let (n, fds, ctrunc) = recv_with_fds(&self.stream, &mut prefix, 4).map_err(|e| {
            if e.kind() == io::ErrorKind::WouldBlock {
                "timed out waiting for response".to_string()
            } else {
                format!("recvmsg failed: {e}")
            }
        })?;
        if n == 0 {
            return Err("peer closed before responding".to_string());
        }
        if ctrunc {
            return Err(format!(
                "control data truncated ({} fds delivered)",
                fds.len()
            ));
        }
        if n < 4 {
            read_exact_stream(&mut self.stream, &mut prefix[n..])?;
        }
        let len = u32::from_le_bytes(prefix) as usize;
        if len > MAX_PAYLOAD {
            return Err(format!(
                "response frame claims {len} bytes, over {MAX_PAYLOAD}"
            ));
        }
        let mut payload = vec![0u8; len];
        read_exact_stream(&mut self.stream, &mut payload)?;
        let response = parse_response(&payload)?;
        Ok((response, fds))
    }

    /// Read raw bytes with the stream's timeout; `Ok(0)` means the peer closed.
    pub fn recv_raw(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.stream.read(buf)
    }

    /// True once the daemon has closed its side (MSG_PEEK reads EOF).
    pub fn peer_closed(&self) -> bool {
        let mut b = [0u8; 1];
        // SAFETY: non-blocking peek on our own socket.
        let rc = unsafe {
            libc::recv(
                self.stream.as_raw_fd(),
                b.as_mut_ptr() as *mut libc::c_void,
                1,
                libc::MSG_PEEK | libc::MSG_DONTWAIT,
            )
        };
        if rc == 0 {
            return true;
        }
        if rc < 0 {
            let e = io::Error::last_os_error();
            return e.kind() != io::ErrorKind::WouldBlock;
        }
        false
    }

    /// Wait until the daemon closes the connection.
    pub fn wait_peer_closed(&self, deadline: Duration) -> Result<Duration, String> {
        wait_for(deadline, Duration::from_millis(1), || self.peer_closed())
    }

    pub fn shutdown_write(&self) -> io::Result<()> {
        self.stream.shutdown(std::net::Shutdown::Write)
    }

    /// Encode and send a typed request via `abacus_wire`. Convenience over [`send_create`]
    /// when the caller already has a `Request`.
    pub fn send_request(&mut self, req: &Request) -> io::Result<()> {
        let frame = encode_request(req).expect("encode");
        self.send_bytes(&frame)
    }

    /// Receive one typed response and its fds via `abacus_wire`. Returns the SDK's `Response`
    /// type instead of the hand-parsed `RawResponse`.
    pub fn recv_response(&mut self) -> Result<(Response, Vec<OwnedFd>), TransportError> {
        let (prefix, fds) = recv_prefix_with_fds(&mut self.stream)?;
        let len = u32::from_le_bytes(prefix) as usize;
        if len > MAX_PAYLOAD {
            return Err(TransportError::Protocol {
                fault: ProtocolFault::FrameTooLarge {
                    len,
                    max: MAX_PAYLOAD,
                },
            });
        }
        let mut payload = vec![0u8; len];
        read_exact(&mut self.stream, &mut payload)?;
        let resp = decode_response(&payload).map_err(|fault| TransportError::Protocol { fault })?;
        Ok((resp, fds))
    }

    /// True if the daemon closes this connection within 500 ms. Bounded wait, not a single peek: the daemon's close can land just after the request that caused it.
    pub fn is_closed_by_peer(&mut self) -> bool {
        self.wait_peer_closed(Duration::from_millis(500)).is_ok()
    }

    /// Mutable reference to the underlying stream.
    pub fn stream_mut(&mut self) -> &mut UnixStream {
        &mut self.stream
    }
}

/// Map a received interlock fd read-write. Panics on failure.
pub fn map_fd(fd: OwnedFd) -> InterlockHandle {
    interlock_map(fd).unwrap_or_else(|e| panic!("mmap of received fd failed: {e}"))
}

/// Map a received clock fd read-only. The clock is sealed against writable mappings.
pub fn map_fd_readonly(fd: OwnedFd) -> InterlockHandle {
    interlock_map_clock(fd)
        .unwrap_or_else(|e| panic!("mmap (read-only) of received fd failed: {e}"))
}

/// Read all three words: (open_count, closed_count, expiration_ns).
pub fn interlock_words(handle: &InterlockHandle) -> (u64, u64, u64) {
    let w = handle.words();
    (
        w.open_count.load(Ordering::Acquire),
        w.closed_count.load(Ordering::Acquire),
        w.expiration_ns.load(Ordering::Acquire),
    )
}

/// Attach to `name` through a fresh raw connection and return a read-write mapped handle.
/// This is how a test reads all three words of an SDK-owned interlock (the SDK exposes
/// only two). For the clock, use [`attach_words_readonly`].
pub fn attach_words(socket: &Path, name: &str) -> InterlockHandle {
    attach_words_with(socket, name, map_fd)
}

/// Attach to the clock (or any read-only interlock) and return a read-only mapped handle.
pub fn attach_words_readonly(socket: &Path, name: &str) -> InterlockHandle {
    attach_words_with(socket, name, map_fd_readonly)
}

fn attach_words_with(
    socket: &Path,
    name: &str,
    map: fn(OwnedFd) -> InterlockHandle,
) -> InterlockHandle {
    let mut raw = RawClient::connect(socket)
        .unwrap_or_else(|e| panic!("raw connect to {} failed: {e}", socket.display()));
    match raw.attach(name) {
        Ok((RawResponse::Attached { .. }, mut fds)) if fds.len() == 1 => map(fds.remove(0)),
        Ok((resp, fds)) => panic!("attach {name}: unexpected {resp:?} with {} fds", fds.len()),
        Err(e) => panic!("attach {name} failed: {e}"),
    }
}

// ---------------------------------------------------------------------------
// Fake daemon: a scripted responder for SDK-side tests
// ---------------------------------------------------------------------------

/// A listener that plays the daemon's side of the wire so SDK behavior on crafted replies
/// (error codes the real daemon never emits, wrong fd counts) can be tested.
pub struct FakeDaemon {
    path: PathBuf,
    listener: UnixListener,
}

impl FakeDaemon {
    pub fn bind(label: &str) -> Self {
        let path = unique_socket_path(label);
        let listener = UnixListener::bind(&path)
            .unwrap_or_else(|e| panic!("fake daemon bind {} failed: {e}", path.display()));
        listener.set_nonblocking(true).expect("set_nonblocking");
        Self { path, listener }
    }

    pub fn socket_path(&self) -> &Path {
        &self.path
    }

    /// Accept one connection within `deadline`.
    pub fn accept(&self, deadline: Duration) -> Result<FakeConn, String> {
        let (stream, _) = wait_for_value(deadline, Duration::from_millis(1), || {
            self.listener.accept().ok()
        })?
        .0;
        stream
            .set_nonblocking(false)
            .expect("set_nonblocking(false)");
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("set_read_timeout");
        Ok(FakeConn { stream })
    }
}

impl Drop for FakeDaemon {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// One accepted connection on a [`FakeDaemon`].
pub struct FakeConn {
    stream: UnixStream,
}

impl FakeConn {
    /// Read one request frame's payload.
    pub fn recv_request(&mut self) -> Result<Vec<u8>, String> {
        read_frame(&mut self.stream)
    }

    pub fn send_raw(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.stream.write_all(bytes)
    }

    /// Send a framed payload with `fds` attached to the first bytes.
    pub fn send_frame_with_fds(&mut self, payload: &[u8], fds: &[RawFd]) -> io::Result<()> {
        let f = frame(payload);
        let sent = send_with_fds(&self.stream, &f, fds)?;
        if sent < f.len() {
            self.stream.write_all(&f[sent..])?;
        }
        Ok(())
    }

    pub fn send_created(&mut self, id: u64, fds: &[RawFd]) -> io::Result<()> {
        let mut p = vec![WIRE_VERSION, TAG_CREATED];
        p.extend_from_slice(&id.to_le_bytes());
        self.send_frame_with_fds(&p, fds)
    }

    pub fn send_attached(&mut self, id: u64, tier: u8, fds: &[RawFd]) -> io::Result<()> {
        let mut p = vec![WIRE_VERSION, TAG_ATTACHED];
        p.extend_from_slice(&id.to_le_bytes());
        p.push(tier);
        self.send_frame_with_fds(&p, fds)
    }

    pub fn send_error(&mut self, code: u8, message: &str) -> io::Result<()> {
        let mut p = vec![WIRE_VERSION, TAG_ERROR, code];
        put_string(&mut p, message.as_bytes());
        self.send_frame_with_fds(&p, &[])
    }
}

// ---------------------------------------------------------------------------
// CPU load
// ---------------------------------------------------------------------------

/// `oversubscription * nproc` threads spinning until dropped. Threads, not processes, so
/// nothing leaks if a test panics.
pub struct Load {
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
}

impl Load {
    /// `oversubscription * nproc` spinning threads, unpinned: the whole machine contended,
    /// daemon included. This is the shape the fatal margin floor was validated against.
    pub fn cpu(oversubscription: usize) -> Self {
        Self::threads_on(nproc() * oversubscription, None)
    }

    /// `threads` spinning threads pinned to `cpus`: load that never competes with a daemon
    /// pinned elsewhere. Production shape: about half the cores pegged, daemon isolated.
    pub fn cpu_on(threads: usize, cpus: &[usize]) -> Self {
        Self::threads_on(threads, Some(cpus.to_vec()))
    }

    fn threads_on(n: usize, cpus: Option<Vec<usize>>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let threads = (0..n)
            .map(|i| {
                let stop = stop.clone();
                let cpus = cpus.clone();
                std::thread::Builder::new()
                    .name(format!("load-{i}"))
                    .spawn(move || {
                        if let Some(cpus) = cpus {
                            pin_current_thread(&cpus);
                        }
                        let mut x: u64 = i as u64;
                        while !stop.load(Ordering::Relaxed) {
                            for _ in 0..4096 {
                                x = std::hint::black_box(
                                    x.wrapping_mul(6364136223846793005).wrapping_add(1),
                                );
                            }
                        }
                    })
                    .expect("spawn load thread")
            })
            .collect();
        Self { stop, threads }
    }

    pub fn thread_count(&self) -> usize {
        self.threads.len()
    }
}

impl Drop for Load {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

// ---------------------------------------------------------------------------
// Seeded rng
// ---------------------------------------------------------------------------

/// xorshift64 with a printable seed. `ABACUS_TEST_SEED` overrides the default so a failure
/// can be replayed. Print the seed in every failure message.
pub struct Rng {
    state: u64,
    seed: u64,
}

impl Rng {
    pub fn new(seed: u64) -> Self {
        let seed = if seed == 0 {
            0x9E37_79B9_7F4A_7C15
        } else {
            seed
        };
        Self { state: seed, seed }
    }

    /// Seed from `ABACUS_TEST_SEED` when set, else `default`.
    pub fn from_env(default: u64) -> Self {
        let seed = match std::env::var("ABACUS_TEST_SEED") {
            Ok(s) => s
                .parse()
                .unwrap_or_else(|e| panic!("ABACUS_TEST_SEED={s:?} is not a u64: {e}")),
            Err(_) => default,
        };
        Self::new(seed)
    }

    pub fn seed(&self) -> u64 {
        self.seed
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x
    }

    /// Uniform in `0..n` (n > 0).
    pub fn below(&mut self, n: u64) -> u64 {
        assert!(n > 0, "Rng::below(0)");
        self.next_u64() % n
    }

    pub fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.next_u64() as u8).collect()
    }
}

// ---------------------------------------------------------------------------
// Percentile stats
// ---------------------------------------------------------------------------

/// Collects `u64` samples and reports p50, p90, p99, max. Failure messages print all four.
#[derive(Debug, Clone)]
pub struct Stats {
    name: String,
    unit: &'static str,
    samples: Vec<u64>,
}

impl Stats {
    pub fn new(name: &str, unit: &'static str) -> Self {
        Self {
            name: name.to_string(),
            unit,
            samples: Vec::new(),
        }
    }

    pub fn push(&mut self, v: u64) {
        self.samples.push(v);
    }

    pub fn len(&self) -> usize {
        self.samples.len()
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    fn sorted(&self) -> Vec<u64> {
        let mut s = self.samples.clone();
        s.sort_unstable();
        s
    }

    /// Nearest-rank percentile, `q` in 0.0..=1.0. Zero when empty.
    pub fn pct(&self, q: f64) -> u64 {
        let s = self.sorted();
        if s.is_empty() {
            return 0;
        }
        let idx = ((s.len() - 1) as f64 * q).round() as usize;
        s[idx.min(s.len() - 1)]
    }

    pub fn p50(&self) -> u64 {
        self.pct(0.50)
    }

    pub fn p90(&self) -> u64 {
        self.pct(0.90)
    }

    pub fn p99(&self) -> u64 {
        self.pct(0.99)
    }

    pub fn max(&self) -> u64 {
        self.samples.iter().copied().max().unwrap_or(0)
    }

    pub fn min(&self) -> u64 {
        self.samples.iter().copied().min().unwrap_or(0)
    }

    /// Count of samples strictly above `threshold`.
    pub fn count_above(&self, threshold: u64) -> usize {
        self.samples.iter().filter(|&&v| v > threshold).count()
    }

    /// `name n=.. p50=.. p90=.. p99=.. max=.. <unit>`
    pub fn report(&self) -> String {
        format!(
            "{} n={} p50={} p90={} p99={} max={} {}",
            self.name,
            self.samples.len(),
            self.p50(),
            self.p90(),
            self.p99(),
            self.max(),
            self.unit
        )
    }
}
