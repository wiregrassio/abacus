//! L4: connection-level abuse against a real daemon process: idle connection piles, connect
//! floods, SCM_RIGHTS floods, fd exhaustion. The pass condition is always the same: the
//! daemon is alive, a well-behaved client in a fresh process can create a timer and wait on
//! it, and once the hostile connections are gone the daemon's idle CPU is normal. Ignored by
//! default; run with `-- --ignored`.

#![allow(non_snake_case)]

use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use abacus_client::{AbacusClient, WaitState};
use abacus_core::interlock::{interlock_arm, interlock_free, InterlockHandle};
use abacus_tests::{
    abacus_binary, attach_payload, describe_exit, frame, map_fd, raise_fd_limit, role_args,
    role_command, send_with_fds, serialized, wait_child, wait_for, ProcessDaemon, RawClient,
    RawResponse, ERR_ALLOCATION_FAILED, ERR_INVALID_REQUEST, TIER_INTERLOCK,
};

const IDLE_CONNECTIONS: usize = 1000;
const FLOOD_THREADS: usize = 4;
const FLOOD_DURATION: Duration = Duration::from_secs(10);
/// 10 000 connects per second for 10 s.
const FLOOD_MIN_TOTAL: u64 = 100_000;
const SCM_FRAMES: usize = 1000;
/// RLIMIT_NOFILE handed to the daemon so EMFILE arrives after a few hundred creates instead
/// of a thousand [the Jetson default is 1024].
const EXHAUST_FD_LIMIT: u64 = 256;
/// The cap requested from the daemon for the capacity test (`--max-interlocks`, a capacity flag; today
/// unknown arguments are ignored). Below EXHAUST_FD_LIMIT so the cap is hit before EMFILE.
const CREATE_CAP: usize = 200;
/// Long enough that nothing created during a test is reaped by TTL.
const HOLD_TTL_NS: u64 = 30_000_000_000;
/// Request burst size and how soon after it the clock must be advancing again (two cycles
/// plus scheduling slack).
const BURST_REQUESTS: usize = 2000;
const BURST_RECOVERY_LIMIT_MS: u64 = 5;

const HEALTH_DEADLINE: Duration = Duration::from_secs(5);
const IDLE_LIMIT_TICKS: u64 = 5;

fn bin() -> PathBuf {
    abacus_binary()
}

fn tail(lines: Vec<String>, n: usize) -> String {
    let skip = lines.len().saturating_sub(n);
    lines[skip..].join("\n")
}

/// A well-behaved client in a fresh process: connect, create a timer, wait_ms(5).
fn well_behaved_client(sock: &Path) -> Result<(), String> {
    let sock_s = sock.to_str().expect("utf8 socket path");
    let mut child = role_command("role__health_client", &[sock_s])
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| format!("spawn health client failed: {e}"))?;
    let status = wait_child(&mut child, HEALTH_DEADLINE)?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "health client ended with {}",
            describe_exit(&status)
        ))
    }
}

fn assert_daemon_serves(d: &mut ProcessDaemon, stage: &str) {
    assert!(
        d.is_alive(),
        "[{stage}] daemon died; last stderr lines:\n{}",
        tail(d.stderr_lines(), 20)
    );
    if let Err(e) = well_behaved_client(d.socket_path()) {
        let alive = d.is_alive();
        let ticks = d.cpu_ticks_over(Duration::from_secs(1));
        panic!(
            "[{stage}] {e}; daemon alive={alive}; cpu over last 1 s={ticks} ticks{}; fds={}; last stderr lines:\n{}",
            health_hint(alive, ticks),
            d.fd_count(),
            tail(d.stderr_lines(), 20)
        );
    }
}

/// What a daemon that is alive but not serving is doing, from its CPU over one second.
fn health_hint(alive: bool, ticks: u64) -> &'static str {
    if alive && ticks == 0 {
        " (alive with zero CPU: the 1 ms loop is not running, the daemon is asleep in poll)"
    } else if alive && ticks >= 90 {
        " (alive at 100 percent of a core: spinning, the partial-frame stall)"
    } else {
        ""
    }
}

/// The daemon's fd count once it stops changing: the startup connect probe's connection is
/// still in its table for a cycle or two after `ProcessDaemon::start` returns.
fn settled_fd_count(d: &ProcessDaemon) -> usize {
    let mut last = d.fd_count();
    let mut stable_for = 0u32;
    let _ = wait_for(Duration::from_secs(2), Duration::from_millis(10), || {
        let now = d.fd_count();
        stable_for = if now == last { stable_for + 1 } else { 0 };
        last = now;
        stable_for >= 5
    });
    last
}

/// Idle means: quiet (no CPU over a 200 ms window) within IDLE_SETTLE, then under
/// IDLE_LIMIT_TICKS over the following second.
const IDLE_SETTLE: Duration = Duration::from_secs(3);

fn assert_daemon_idle(d: &mut ProcessDaemon, stage: &str) {
    let quiet = wait_for(IDLE_SETTLE, Duration::from_millis(1), || {
        d.cpu_ticks_over(Duration::from_millis(200)) == 0
    });
    let ticks = d.cpu_ticks_over(Duration::from_secs(1));
    assert!(
        quiet.is_ok() && ticks < IDLE_LIMIT_TICKS,
        "[{stage}] daemon did not go idle: quiet within {IDLE_SETTLE:?}={}, then {ticks} ticks over 1 s (limit {IDLE_LIMIT_TICKS}); alive={}; last stderr lines:\n{}",
        quiet.is_ok(),
        d.is_alive(),
        tail(d.stderr_lines(), 20)
    );
}

fn assert_daemon_healthy(d: &mut ProcessDaemon, stage: &str) {
    assert_daemon_serves(d, stage);
    assert_daemon_idle(d, stage);
}

/// Role: `args = [socket]`. Connect, create a timer, wait_ms(5), require Normal or Overrun.
/// Not a test; spawned by the health checks in this file.
#[test]
#[ignore = "role: process entry point for the abuse health checks in abuse_connections"]
fn role__health_client() {
    let Some(args) = role_args("role__health_client") else {
        return;
    };
    let mut client = AbacusClient::connect(Path::new(&args[0])).expect("connect");
    let timer = client.create_wait_timer("health").expect("create timer");
    let r = timer.wait_ms(5).expect("wait_ms");
    assert!(
        matches!(r.state, WaitState::Normal | WaitState::Overrun),
        "health timer state {:?}",
        r.state
    );
}

/// Create bare interlocks `<prefix>-<i>` on one raw connection, mapping and arming each for
/// HOLD_TTL_NS, until the daemon refuses one. Returns the live handles and the refusal.
fn create_until_refused(
    raw: &mut RawClient,
    prefix: &str,
    max: usize,
) -> (Vec<InterlockHandle>, Option<(u8, String)>) {
    let mut handles = Vec::new();
    for i in 0..max {
        match raw.create(&format!("{prefix}-{i}"), TIER_INTERLOCK, None, None, None) {
            Ok((RawResponse::Created { .. }, mut fds)) if fds.len() == 1 => {
                let h = map_fd(fds.remove(0));
                interlock_arm(&h, HOLD_TTL_NS)
                    .unwrap_or_else(|e| panic!("arm {prefix}-{i} failed: {e}"));
                handles.push(h);
            }
            Ok((RawResponse::Error { code, message }, _)) => {
                return (handles, Some((code, message)));
            }
            Ok((resp, fds)) => {
                panic!(
                    "create {prefix}-{i}: unexpected {resp:?} with {} fds",
                    fds.len()
                )
            }
            Err(e) => panic!(
                "create {prefix}-{i} failed: {e} ({} created so far)",
                handles.len()
            ),
        }
    }
    (handles, None)
}

/// CONTRACTS.md UDS surface: persistent connections. IDLE_CONNECTIONS idle connections are
/// accepted and held; the daemon still serves; closing them returns the fd table to its
/// baseline. The daemon's CPU with them open is printed, not asserted (poll() over a
/// thousand fds has a cost the design does not promise away).
#[test]
#[ignore = "abuse: run with --ignored"]
fn abuse__thousand_idle_connections() {
    let _serial = serialized();
    raise_fd_limit(8192);
    let mut d = ProcessDaemon::start_with(&bin(), "idle-conns", &[], Some(8192));
    let baseline = settled_fd_count(&d);
    let mut conns = Vec::with_capacity(IDLE_CONNECTIONS);
    for i in 0..IDLE_CONNECTIONS {
        conns.push(
            RawClient::connect(d.socket_path())
                .unwrap_or_else(|e| panic!("connection {i} failed: {e}")),
        );
    }
    wait_for(Duration::from_secs(5), Duration::from_millis(10), || {
        d.fd_count() >= baseline + IDLE_CONNECTIONS
    })
    .unwrap_or_else(|e| {
        panic!(
            "daemon accepted only {} of {IDLE_CONNECTIONS} connections: {e}; last stderr lines:\n{}",
            d.fd_count().saturating_sub(baseline),
            tail(d.stderr_lines(), 10)
        )
    });
    let ticks = d.cpu_ticks_over(Duration::from_secs(1));
    println!("daemon CPU with {IDLE_CONNECTIONS} idle connections: {ticks} ticks over 1 s");
    assert_daemon_serves(&mut d, "with 1000 idle connections open");
    drop(conns);
    wait_for(Duration::from_secs(5), Duration::from_millis(10), || {
        d.fd_count() <= baseline + 2
    })
    .unwrap_or_else(|e| {
        panic!(
            "daemon fd count {} did not return to baseline {baseline}: {e}",
            d.fd_count()
        )
    });
    assert_daemon_healthy(&mut d, "after 1000 idle connections closed");
}

/// CONTRACTS.md daemon contract, the 1 ms loop: a burst of BURST_REQUESTS request round
/// trips on one connection does not stall evaluation afterwards. The clock word advances
/// again within BURST_RECOVERY_LIMIT_MS of the last response. `daemon.rs` anchors its
/// evaluation boundary to real time (`anchor` plus elapsed 1 ms steps) rather than counting
/// poll wakeups, so a burst of I/O wakeups does not push the next boundary out and leave the
/// daemon sleeping with no evaluation once the burst ends.
#[test]
#[ignore = "abuse: run with --ignored"]
fn abuse__request_burst_does_not_stall_the_loop() {
    let _serial = serialized();
    let mut d = ProcessDaemon::start(&bin(), "burst");
    let clock = abacus_tests::attach_words_readonly(d.socket_path(), "clock");
    let mut raw = RawClient::connect(d.socket_path()).expect("raw connect");
    let t0 = Instant::now();
    for i in 0..BURST_REQUESTS {
        match raw.attach("clock") {
            Ok((RawResponse::Attached { .. }, _)) => {}
            Ok((resp, _)) => panic!("attach {i}: unexpected {resp:?}"),
            Err(e) => panic!("attach {i} failed: {e}"),
        }
    }
    let burst = t0.elapsed();
    let seen = clock.words().open_count.load(Ordering::Acquire);
    let gap = wait_for(Duration::from_secs(30), Duration::from_micros(200), || {
        clock.words().open_count.load(Ordering::Acquire) > seen
    });
    let gap = match gap {
        Ok(g) => g,
        Err(e) => panic!(
            "clock.open_count stayed at {seen} for 30 s after a burst of {BURST_REQUESTS} requests ({burst:?}): {e}; daemon alive={}; cpu over last 1 s={} ticks",
            d.is_alive(),
            d.cpu_ticks_over(Duration::from_secs(1))
        ),
    };
    println!(
        "burst of {BURST_REQUESTS} requests took {burst:?}; clock advanced again after {gap:?}"
    );
    assert!(
        gap < Duration::from_millis(BURST_RECOVERY_LIMIT_MS),
        "after a burst of {BURST_REQUESTS} requests ({burst:?}) the clock did not advance for {gap:?} (limit {BURST_RECOVERY_LIMIT_MS} ms): the loop stopped evaluating"
    );
    assert_daemon_healthy(&mut d, "after request burst");
}

/// `transport.rs` accept loop: FLOOD_THREADS threads connecting and disconnecting as fast as
/// they can for FLOOD_DURATION reach at least FLOOD_MIN_TOTAL connects, and the daemon is
/// healthy afterwards.
#[test]
#[ignore = "abuse: run with --ignored"]
fn abuse__connect_flood_10k_per_second_for_10s() {
    let _serial = serialized();
    let mut d = ProcessDaemon::start(&bin(), "connect-flood");
    let stop = Arc::new(AtomicBool::new(false));
    let total = Arc::new(AtomicU64::new(0));
    let failures = Arc::new(AtomicU64::new(0));
    let ticks_before = d.cpu_ticks();
    let t0 = Instant::now();
    let threads: Vec<_> = (0..FLOOD_THREADS)
        .map(|_| {
            let sock = d.socket_path().to_path_buf();
            let stop = stop.clone();
            let total = total.clone();
            let failures = failures.clone();
            thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    match UnixStream::connect(&sock) {
                        Ok(s) => {
                            drop(s);
                            total.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(_) => {
                            failures.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            })
        })
        .collect();
    // The stimulus: flood for FLOOD_DURATION.
    thread::sleep(FLOOD_DURATION);
    stop.store(true, Ordering::Relaxed);
    for t in threads {
        let _ = t.join();
    }
    let elapsed = t0.elapsed();
    let n = total.load(Ordering::Relaxed);
    let f = failures.load(Ordering::Relaxed);
    let ticks = d.cpu_ticks().saturating_sub(ticks_before);
    let rate = n as f64 / elapsed.as_secs_f64();
    println!(
        "connect flood: {n} connects ({f} failures) in {elapsed:?}, {rate:.0} per second; daemon CPU {ticks} ticks"
    );
    assert!(
        d.is_alive(),
        "daemon died during the connect flood after {n} connects; last stderr lines:\n{}",
        tail(d.stderr_lines(), 20)
    );
    assert!(
        n >= FLOOD_MIN_TOTAL,
        "only {n} connects in {elapsed:?} ({rate:.0} per second, {f} failures); the flood did not reach {FLOOD_MIN_TOTAL}; daemon CPU {ticks} ticks"
    );
    assert_daemon_healthy(&mut d, "after connect flood");
}

/// `transport.rs` recv path: fds a client passes with SCM_RIGHTS are never installed in the
/// daemon (it reads with no control buffer, so the kernel closes them). The daemon's fd table
/// stays at baseline during SCM_FRAMES such frames and after the client leaves.
#[test]
#[ignore = "abuse: run with --ignored"]
fn abuse__scm_rights_flood() {
    let _serial = serialized();
    raise_fd_limit(4096);
    let mut d = ProcessDaemon::start(&bin(), "scm-flood");
    let baseline = settled_fd_count(&d);
    let raw = RawClient::connect(d.socket_path()).expect("raw connect");
    // SAFETY: dup of our own stderr, closed below.
    let spare = unsafe { libc::dup(2) };
    assert!(spare >= 0, "dup(2) failed");
    let payload = frame(&attach_payload(b"clock"));
    let mut sent = 0usize;
    for i in 0..SCM_FRAMES {
        match send_with_fds(raw.stream(), &payload, &[spare]) {
            Ok(_) => sent += 1,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset
                ) =>
            {
                // The daemon drops a non-reading client once its socket buffer fills (the
                // test never reads responses). This is correct daemon behavior, not a
                // failure; we still have enough frames to verify the fd table assertion.
                break;
            }
            Err(e) => panic!("sendmsg {i} with an fd failed: {e}"),
        }
    }
    assert!(sent > 0, "could not send even one SCM_RIGHTS frame");
    // Sample the daemon's table while it drains the flood (one frame per cycle).
    let mut peak = 0usize;
    let _ = wait_for(Duration::from_millis(300), Duration::from_millis(5), || {
        peak = peak.max(d.fd_count());
        false
    });
    assert!(
        peak <= baseline + 2,
        "daemon fd table grew to {peak} during the SCM_RIGHTS flood (baseline {baseline}): passed fds are being installed"
    );
    drop(raw);
    // SAFETY: closing the fd we dup'd above.
    unsafe {
        libc::close(spare);
    }
    wait_for(Duration::from_secs(1), Duration::from_millis(5), || {
        d.fd_count() <= baseline
    })
    .unwrap_or_else(|e| {
        panic!(
            "daemon fd count {} did not return to baseline {baseline} within 1 s of the client leaving: {e}",
            d.fd_count()
        )
    });
    assert_daemon_healthy(&mut d, "after SCM_RIGHTS flood");
}

/// CONTRACTS.md (now): creates past the interlock cap are refused with
/// InvalidRequest, not with an opaque AllocationFailed from EMFILE, and the daemon is not
/// wedged. The daemon is started with `--max-interlocks=CREATE_CAP` and a fd limit above it.
#[test]
#[ignore = "abuse: run with --ignored"]
fn abuse__create_flood_until_limit() {
    let _serial = serialized();
    raise_fd_limit(8192);
    let cap_flag = format!("--max-interlocks={CREATE_CAP}");
    let mut d =
        ProcessDaemon::start_with(&bin(), "create-limit", &[&cap_flag], Some(EXHAUST_FD_LIMIT));
    let mut raw = RawClient::connect(d.socket_path()).expect("raw connect");
    let (handles, refusal) = create_until_refused(&mut raw, "c", 4096);
    let n = handles.len();
    let (code, message) = refusal.unwrap_or_else(|| {
        panic!("daemon accepted {n} creates with cap {CREATE_CAP} and fd limit {EXHAUST_FD_LIMIT} without ever refusing")
    });
    // Release everything before judging, so the health check below is about the daemon.
    for h in &handles {
        interlock_free(h);
    }
    drop(handles);
    let recovered = wait_for(Duration::from_secs(1), Duration::from_millis(5), || {
        matches!(
            raw.create("after-cap", TIER_INTERLOCK, None, None, None),
            Ok((RawResponse::Created { .. }, _))
        )
    });
    let health = well_behaved_client(d.socket_path());
    assert!(
        code == ERR_INVALID_REQUEST && n == CREATE_CAP,
        "create {n} was refused with 0x{code:02x} ({message}) instead of InvalidRequest at the configured cap of {CREATE_CAP}; recovered after freeing={}; health={health:?}",
        recovered.is_ok()
    );
    recovered.unwrap_or_else(|e| panic!("daemon wedged after the cap: {e}"));
    assert_daemon_healthy(&mut d, "after create flood");
}

/// `registry.rs` and LIFECYCLE.md reap: driving the daemon to EMFILE with creates yields a
/// refusal (AllocationFailed today, InvalidRequest now), and freeing every interlock
/// lets creates succeed again within 1 s. The daemon is started with a EXHAUST_FD_LIMIT fd
/// limit.
#[test]
#[ignore = "abuse: run with --ignored"]
fn abuse__fd_exhaustion_recovers() {
    let _serial = serialized();
    raise_fd_limit(8192);
    let mut d = ProcessDaemon::start_with(&bin(), "fd-exhaust", &[], Some(EXHAUST_FD_LIMIT));
    let mut raw = RawClient::connect(d.socket_path()).expect("raw connect");
    let (handles, refusal) = create_until_refused(&mut raw, "x", 4096);
    let n = handles.len();
    let (code, message) = refusal.unwrap_or_else(|| {
        panic!(
            "daemon accepted {n} creates under a {EXHAUST_FD_LIMIT} fd limit without ever refusing"
        )
    });
    println!(
        "daemon refused create {n} with code 0x{code:02x}: {message}; fds {}",
        d.fd_count()
    );
    assert!(
        code == ERR_ALLOCATION_FAILED || code == ERR_INVALID_REQUEST,
        "unexpected refusal code 0x{code:02x}: {message}"
    );
    for h in &handles {
        interlock_free(h);
    }
    drop(handles);
    wait_for(Duration::from_secs(1), Duration::from_millis(5), || {
        matches!(
            raw.create("recovered", TIER_INTERLOCK, None, None, None),
            Ok((RawResponse::Created { .. }, _))
        )
    })
    .unwrap_or_else(|e| {
        panic!(
            "create still refused 1 s after freeing all {n} interlocks: {e}; daemon fds {}; last stderr lines:\n{}",
            d.fd_count(),
            tail(d.stderr_lines(), 10)
        )
    });
    assert_daemon_healthy(&mut d, "after fd exhaustion and recovery");
}
