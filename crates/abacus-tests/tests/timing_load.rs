//! L3: timing under CPU load, in three shapes, plus the per-process thread budget.
//! Thresholds are named consts at the top with the value observed (debug build, daemon as
//! its own process, 2x CPU load from `yes` processes) and the value the fixed design
//! promises. Ignored by default; run with `-- --ignored` on the Jetson.
//!
//! The three load shapes:
//! - Unpinned 2x (`timing__under_2x_cpu_load_*`): `2 * nproc` spinning threads anywhere,
//!   daemon unpinned. The worst case observed under the fatal margin floor.
//! - Production profile (`timing__production_profile_*`): the daemon pinned to the isolated
//!   core (`daemon_core()`), `nproc / 2` threads pegging the low cores, clients free on
//!   all non-daemon cores. What the deployed box looks like.
//! - Stress profile (`timing__stress_profile_*`): the daemon pinned to the isolated core,
//!   `STRESS_THREADS_PER_CORE` spinning threads per non-daemon core. The isolated daemon
//!   must not care how loaded the rest of the box is.
//!
//! The test thread is pinned first, so the SDK's touch threads and the role children it
//! spawns inherit the client core set and never land on the daemon's core.
//!
//! Every wait_ms loop runs in a role child so an RTSTimeout abort is a red test with the
//! child's stderr, not a dead test binary. The load is threads in this process
//! (`abacus_tests::Load`), so a panic leaves nothing running. Every test holds
//! `serialized()`: two load shapes at once starve each other's daemon.
//!
//! Pinning is not isolation. The pinned profiles assert the deployment contract (the daemon
//! alone on its core). On a box where other processes may still run on that core (the test
//! Jetson runs containerd, docker-proxy, InfluxDB, and postgres threads there) a pinned
//! daemon waits behind whatever wakes on its core and stalls past 10 ms about once in
//! 2000 to 4000 waits, while an unpinned daemon migrates to an idle core and never does.
//! These tests stay red on such a box; they go green on an isolated core (`isolcpus` or a
//! cpuset shield) or under the fatal margin floor.

#![allow(non_snake_case)]
// Debug and release thresholds are stated as separate arms even where the values coincide:
// the release arm is filled in when release probe numbers are recorded.
#![allow(clippy::if_same_then_else)]

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use abacus_client::{AbacusClient, WaitState};
use abacus_tests::{
    abacus_binary, all_cores_except, daemon_core, describe_exit, nproc, pin_current_thread,
    proc_status_field, role_args, role_command, run_child, serialized, Load, ProcessDaemon, Stats,
};

// ---------------------------------------------------------------------------
// Thresholds. Left value: debug build. Right value: release build.
// ---------------------------------------------------------------------------

/// CPU oversubscription factor: `LOAD_OVERSUBSCRIPTION * nproc` spinning threads.
const LOAD_OVERSUBSCRIPTION: usize = 2;
const N_LOAD_WAIT5: usize = 2000;
const N_LOAD_WAIT2: usize = 500;

/// Production profile samples.
const N_PROD_WAIT5: usize = 2000;
const N_PROD_WAIT2: usize = 2000;
const N_PROD_WAIT1: usize = 500;
const N_PROD_CRON: usize = 200;
const PROD_CRON_INTERVAL_MS: u64 = 10;

/// Production profile, wait_ms(5): p99 under 7 ms and max under 15 ms, no abort. Measured
/// (Jetson Orin AGX, daemon on isolated core 4, two runs): p99 6036 to 6614 us, max 10192
/// to 12546 us. Release: same values.
const PROD_WAIT5_P99_LIMIT_US: u64 = if cfg!(debug_assertions) { 7_000 } else { 7_000 };
const PROD_WAIT5_MAX_LIMIT_US: u64 = if cfg!(debug_assertions) {
    15_000
} else {
    15_000
};

/// Production profile Overrun rate for wait_ms(5), and cron off-grid rate. Both are the
/// daemon's own one-ms rounding, not the load: Promise: under 1 percent.
const PROD_OVERRUN_LIMIT_PERMILLE: u64 = if cfg!(debug_assertions) { 10 } else { 10 };
const PROD_CRON_OFF_GRID_LIMIT_PERMILLE: u64 = if cfg!(debug_assertions) { 10 } else { 10 };

/// Stress profile: 4 spinning threads per non-daemon core. wait_ms(5) never aborts and its
/// max stays under 30 ms (the same bound as the unpinned 2x case); wait_ms(2) never aborts;
/// the daemon's CPU over STRESS_IDLE_WINDOW stays under STRESS_IDLE_MAX_TICKS because an
/// isolated daemon's cost does not grow with client load. Release: same values.
const STRESS_THREADS_PER_CORE: usize = 4;
const N_STRESS_WAIT5: usize = 2000;
const N_STRESS_WAIT2: usize = 2000;
const STRESS_WAIT5_MAX_LIMIT_US: u64 = if cfg!(debug_assertions) {
    30_000
} else {
    30_000
};
const STRESS_IDLE_WINDOW: Duration = Duration::from_secs(3);
const STRESS_IDLE_MAX_TICKS: u64 = if cfg!(debug_assertions) { 4 } else { 4 };

/// wait_ms(5) under 2x load. Measured (debug): elapsed max 22.0 ms, overrun max 17 ms, no
/// abort; it survives because the client's own deadline check is also starved past the
/// fatal margin floor of 50 ms, so it never aborts, max under 30 ms. Release: no separate
/// measurement, same value.
const LOAD_WAIT5_MAX_LIMIT_US: u64 = if cfg!(debug_assertions) {
    30_000
} else {
    30_000
};

/// Touch threads per process. One keepalive thread per client, so 20 interlocks add at most
/// 1 thread. Same in both builds.
const TOUCH_INTERLOCKS: usize = 20;
const TOUCH_THREAD_DELTA_LIMIT: u64 = 1;

fn bin() -> PathBuf {
    abacus_binary()
}

// ---------------------------------------------------------------------------
// Measuring wait_ms in a child (an RTSTimeout abort must not kill this binary)
// ---------------------------------------------------------------------------

/// The child's `REPORT ...` line. libtest under `--nocapture` prints `test name ... ` with no
/// newline before the test's own output, so the marker is searched for, not anchored.
fn report_line(text: &str) -> Option<String> {
    text.lines()
        .find_map(|l| l.find("REPORT ").map(|i| l[i..].to_string()))
}

/// `key=value` lookup on a `REPORT` line.
fn report_kv(line: &str, key: &str) -> Option<u64> {
    let prefix = format!("{key}=");
    line.split_whitespace()
        .find_map(|t| t.strip_prefix(&prefix).and_then(|v| v.parse().ok()))
}

struct Bench {
    line: String,
    p99: u64,
    max: u64,
    overrun: u64,
}

/// Run `n` x `wait_ms(ms)` on a fresh timer in a role child and parse its REPORT line.
/// `Err` carries the exit description and the child's stderr (the RTSTimeout line).
fn bench_wait_ms(sock: &Path, ms: u64, n: usize) -> Result<Bench, String> {
    let sock_s = sock.to_str().expect("utf8 socket path");
    let child = role_command(
        "role__bench_wait_ms",
        &[sock_s, &ms.to_string(), &n.to_string()],
    )
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .map_err(|e| format!("spawn bench child failed: {e}"))?;
    // Under load every iteration can take far longer than 2W; leave a generous budget.
    let deadline = Duration::from_millis((n as u64) * (2 * ms + 40) + 10_000);
    let run = run_child(child, deadline);
    if !run.status.success() {
        return Err(format!(
            "wait_ms({ms}) x {n} child ended with {}; child stderr:\n{}",
            describe_exit(&run.status),
            run.stderr.trim()
        ));
    }
    let line = report_line(&run.stdout)
        .ok_or_else(|| format!("no REPORT line in child stdout:\n{}", run.stdout))?;
    let get = |k: &str| report_kv(&line, k).ok_or_else(|| format!("missing {k} in: {line}"));
    Ok(Bench {
        p99: get("p99")?,
        max: get("max")?,
        overrun: get("overrun")?,
        line,
    })
}

// ---------------------------------------------------------------------------
// Load shapes
// ---------------------------------------------------------------------------

/// Production profile: this thread (and everything it spawns) on every core but the
/// daemon's, the daemon pinned to `daemon_core()`, `nproc / 2` threads pegging the low
/// half of the cores, clients free on all non-daemon cores.
fn production_profile(label: &str) -> (ProcessDaemon, Load, String) {
    // Core counts first: `nproc()` follows the affinity mask, so after pinning it reports
    // one fewer core.
    let cores = nproc();
    let (core, isolated) = daemon_core();
    let clients = all_cores_except(core);
    pin_current_thread(&clients);
    let d = ProcessDaemon::start_pinned(&bin(), label, core);
    let busy: Vec<usize> = (0..cores / 2).collect();
    let load = Load::cpu_on(busy.len(), &busy);
    let iso = if isolated { "isolated" } else { "NOT isolated" };
    let shape = format!(
        "production profile: daemon on core {core} ({iso}), {} load threads on cores {:?}, clients on cores {:?}",
        load.thread_count(),
        busy,
        clients
    );
    (d, load, shape)
}

/// Stress profile: the daemon pinned to `daemon_core()`, STRESS_THREADS_PER_CORE spinning
/// threads per remaining core, this thread (and the clients it spawns) pinned to those same
/// cores.
fn stress_profile(label: &str) -> (ProcessDaemon, Load, String) {
    let (core, isolated) = daemon_core();
    let others = all_cores_except(core);
    pin_current_thread(&others);
    let d = ProcessDaemon::start_pinned(&bin(), label, core);
    let load = Load::cpu_on(STRESS_THREADS_PER_CORE * others.len(), &others);
    let iso = if isolated { "isolated" } else { "NOT isolated" };
    let shape = format!(
        "stress profile: daemon on core {core} ({iso}), {} load threads and the clients on cores {:?}",
        load.thread_count(),
        others
    );
    (d, load, shape)
}

/// Role: `args = [socket, ms, n]`. Runs `n` x `wait_ms(ms)` on one timer and prints one
/// `REPORT elapsed_us n=.. p50=.. p90=.. p99=.. max=.. us overrun=.. timeout=..` line.
/// Not a test; spawned by the under-load benchmarks in this file.
#[test]
#[ignore = "role: process entry point for the wait_ms benchmarks in timing_load"]
fn role__bench_wait_ms() {
    let Some(args) = role_args("role__bench_wait_ms") else {
        return;
    };
    let sock = Path::new(&args[0]);
    let ms: u64 = args[1].parse().expect("ms");
    let n: usize = args[2].parse().expect("n");
    let mut client = AbacusClient::connect(sock).expect("connect");
    let timer = client.create_wait_timer("bench").expect("create timer");
    let mut stats = Stats::new("elapsed_us", "us");
    let mut overrun = 0u64;
    let mut timeout = 0u64;
    for _ in 0..n {
        let t0 = Instant::now();
        let r = timer.wait_ms(ms).expect("wait_ms");
        stats.push(t0.elapsed().as_micros() as u64);
        match r.state {
            WaitState::Overrun => overrun += 1,
            WaitState::Timeout => timeout += 1,
            WaitState::Normal => {}
        }
    }
    println!(
        "REPORT {} overrun={overrun} timeout={timeout}",
        stats.report()
    );
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// SURFACE.md fatal timeout: under 2x CPU oversubscription a 5 ms wait never trips the
/// fatal margin and its worst case stays under LOAD_WAIT5_MAX_LIMIT_US.
#[test]
#[ignore = "timing: run with --ignored on hardware"]
fn timing__under_2x_cpu_load_wait_ms_5_never_aborts_and_max_under_30ms() {
    let _serial = serialized();
    let d = ProcessDaemon::start(&bin(), "load-wait5");
    let load = Load::cpu(LOAD_OVERSUBSCRIPTION);
    let result = bench_wait_ms(d.socket_path(), 5, N_LOAD_WAIT5);
    let threads = load.thread_count();
    drop(load);
    let b = result.unwrap_or_else(|e| panic!("under {threads} load threads: {e}"));
    assert!(
        b.max < LOAD_WAIT5_MAX_LIMIT_US,
        "wait_ms(5) under {threads} load threads: {} (limit max < {LOAD_WAIT5_MAX_LIMIT_US} us; overrun {})",
        b.line,
        b.overrun
    );
}

/// Under 2x CPU oversubscription a 2 ms wait never aborts.
#[test]
#[ignore = "timing: run with --ignored on hardware"]
fn timing__under_2x_cpu_load_wait_ms_2_never_aborts() {
    let _serial = serialized();
    let d = ProcessDaemon::start(&bin(), "load-wait2");
    let load = Load::cpu(LOAD_OVERSUBSCRIPTION);
    let result = bench_wait_ms(d.socket_path(), 2, N_LOAD_WAIT2);
    let threads = load.thread_count();
    drop(load);
    match result {
        Ok(b) => println!("wait_ms(2) under {threads} load threads: {}", b.line),
        Err(e) => panic!("under {threads} load threads: {e}"),
    }
}

// ---------------------------------------------------------------------------
// Production profile: daemon on isolated core, low cores pegged, clients on the rest
// ---------------------------------------------------------------------------

/// Production profile: wait_ms(5) meets the idle p99 (PROD_WAIT5_P99_LIMIT_US), stays under
/// PROD_WAIT5_MAX_LIMIT_US, and never aborts (exit code 0 from the role child). The daemon
/// is on its own core, so the other six cores' load must not reach it. Measured on a
/// machine that was not quiet.
#[test]
#[ignore = "timing: run with --ignored on hardware"]
fn timing__production_profile_wait_ms_5_p99_under_7ms_and_never_aborts() {
    let _serial = serialized();
    let (d, load, shape) = production_profile("prod-wait5");
    let result = bench_wait_ms(d.socket_path(), 5, N_PROD_WAIT5);
    drop(load);
    let b = result.unwrap_or_else(|e| panic!("{shape}: {e}"));
    println!("{shape}: wait_ms(5) {}", b.line);
    assert!(
        b.p99 < PROD_WAIT5_P99_LIMIT_US && b.max < PROD_WAIT5_MAX_LIMIT_US,
        "{shape}: wait_ms(5) {} (limits p99 < {PROD_WAIT5_P99_LIMIT_US} us, max < {PROD_WAIT5_MAX_LIMIT_US} us)",
        b.line
    );
}

/// Production profile: wait_ms(2) never aborts (exit code 0 over N_PROD_WAIT2 samples).
/// Percentiles are reported, not asserted.
#[test]
#[ignore = "timing: run with --ignored on hardware"]
fn timing__production_profile_wait_ms_2_never_aborts() {
    let _serial = serialized();
    let (d, load, shape) = production_profile("prod-wait2");
    let result = bench_wait_ms(d.socket_path(), 2, N_PROD_WAIT2);
    drop(load);
    match result {
        Ok(b) => println!("{shape}: wait_ms(2) {}", b.line),
        Err(e) => panic!("{shape}: {e}"),
    }
}

/// Production profile: wait_ms(1) never aborts. The 2 ms fatal margin is missed by
/// the daemon's own whole-millisecond rounding, not by the load; the daemon rounds to whole milliseconds.
#[test]
#[ignore = "timing: run with --ignored on hardware"]
fn timing__production_profile_wait_ms_1_never_aborts() {
    let _serial = serialized();
    let (d, load, shape) = production_profile("prod-wait1");
    let result = bench_wait_ms(d.socket_path(), 1, N_PROD_WAIT1);
    drop(load);
    match result {
        Ok(b) => println!("{shape}: wait_ms(1) {}", b.line),
        Err(e) => panic!("{shape}: {e}"),
    }
}

/// Production profile: a 10 ms cron fires on the grid; off-grid fires under
/// PROD_CRON_OFF_GRID_LIMIT_PERMILLE of N_PROD_CRON.
#[test]
#[ignore = "timing: run with --ignored on hardware"]
fn timing__production_profile_cron_10ms_off_grid_under_1_percent() {
    let _serial = serialized();
    let (d, load, shape) = production_profile("prod-cron");
    let mut client = d.client();
    let cron = client
        .create_wait_cron("cron10", PROD_CRON_INTERVAL_MS)
        .expect("create cron");
    let mut off_grid = 0u64;
    let mut overrun = 0u64;
    let mut deltas = Stats::new("cron_delta_ms", "ms");
    let mut last: Option<u64> = None;
    for i in 0..N_PROD_CRON {
        let r = cron
            .wait()
            .unwrap_or_else(|e| panic!("{shape}: cron wait {i} failed: {e}"));
        if r.completed_at % PROD_CRON_INTERVAL_MS != 0 {
            off_grid += 1;
        }
        if r.state == WaitState::Overrun {
            overrun += 1;
        }
        if let Some(l) = last {
            deltas.push(r.completed_at.saturating_sub(l));
        }
        last = Some(r.completed_at);
    }
    drop(load);
    let permille = off_grid * 1000 / N_PROD_CRON as u64;
    println!(
        "{shape}: cron {off_grid} of {N_PROD_CRON} off grid; {}",
        deltas.report()
    );
    assert!(
        permille < PROD_CRON_OFF_GRID_LIMIT_PERMILLE,
        "{shape}: {off_grid} of {N_PROD_CRON} cron fires off the {PROD_CRON_INTERVAL_MS} ms grid ({permille} permille, limit {PROD_CRON_OFF_GRID_LIMIT_PERMILLE}); reported Overrun {overrun}; {}",
        deltas.report()
    );
}

/// Production profile: wait_ms(5) Overrun rate under PROD_OVERRUN_LIMIT_PERMILLE of
/// N_PROD_WAIT5.
#[test]
#[ignore = "timing: run with --ignored on hardware"]
fn timing__production_profile_overrun_rate_under_1_percent() {
    let _serial = serialized();
    let (d, load, shape) = production_profile("prod-overrun");
    let result = bench_wait_ms(d.socket_path(), 5, N_PROD_WAIT5);
    drop(load);
    let b = result.unwrap_or_else(|e| panic!("{shape}: {e}"));
    let permille = b.overrun * 1000 / N_PROD_WAIT5 as u64;
    println!("{shape}: wait_ms(5) {}", b.line);
    assert!(
        permille < PROD_OVERRUN_LIMIT_PERMILLE,
        "{shape}: {} of {N_PROD_WAIT5} wait_ms(5) wakes reported Overrun ({permille} permille, limit {PROD_OVERRUN_LIMIT_PERMILLE}); {}",
        b.overrun,
        b.line
    );
}

// ---------------------------------------------------------------------------
// Stress profile: daemon on isolated core, STRESS_THREADS_PER_CORE per remaining core
// ---------------------------------------------------------------------------

/// Stress profile: wait_ms(5) never aborts and its max stays under
/// STRESS_WAIT5_MAX_LIMIT_US. The clients are starved with the load; the daemon is not.
#[test]
#[ignore = "timing: run with --ignored on hardware"]
fn timing__stress_profile_wait_ms_5_never_aborts_and_max_under_30ms() {
    let _serial = serialized();
    let (d, load, shape) = stress_profile("stress-wait5");
    let result = bench_wait_ms(d.socket_path(), 5, N_STRESS_WAIT5);
    drop(load);
    let b = result.unwrap_or_else(|e| panic!("{shape}: {e}"));
    println!("{shape}: wait_ms(5) {}", b.line);
    assert!(
        b.max < STRESS_WAIT5_MAX_LIMIT_US,
        "{shape}: wait_ms(5) {} (limit max < {STRESS_WAIT5_MAX_LIMIT_US} us; overrun {})",
        b.line,
        b.overrun
    );
}

/// Stress profile: wait_ms(2) never aborts. Expected to pass today: the daemon is isolated
/// and `wait_ms` reads the delivered state before it checks its deadline, so a client
/// scheduled late still sees Normal. An abort here is a finding.
#[test]
#[ignore = "timing: run with --ignored on hardware"]
fn timing__stress_profile_wait_ms_2_never_aborts() {
    let _serial = serialized();
    let (d, load, shape) = stress_profile("stress-wait2");
    let result = bench_wait_ms(d.socket_path(), 2, N_STRESS_WAIT2);
    drop(load);
    match result {
        Ok(b) => println!("{shape}: wait_ms(2) {}", b.line),
        Err(e) => panic!("{shape}: {e}"),
    }
}

/// Stress profile: with one idle client connected and the other cores saturated, the
/// daemon's CPU over STRESS_IDLE_WINDOW stays under STRESS_IDLE_MAX_TICKS. Isolation means
/// the daemon's cost does not grow with client load.
#[test]
#[ignore = "timing: run with --ignored on hardware"]
fn timing__stress_profile_daemon_stays_idle() {
    let _serial = serialized();
    let (d, load, shape) = stress_profile("stress-idle");
    let _client = d.client();
    let ticks = d.cpu_ticks_over(STRESS_IDLE_WINDOW);
    drop(load);
    println!("{shape}: daemon {ticks} ticks over {STRESS_IDLE_WINDOW:?}");
    assert!(
        ticks < STRESS_IDLE_MAX_TICKS,
        "{shape}: daemon used {ticks} ticks over {STRESS_IDLE_WINDOW:?} (limit {STRESS_IDLE_MAX_TICKS})"
    );
}

/// SURFACE.md background touch: keepalive is one thread per client, not one per
/// interlock. Creating TOUCH_INTERLOCKS interlocks on one client adds at most
/// TOUCH_THREAD_DELTA_LIMIT threads (`Threads:` in `/proc/self/status`).
#[test]
#[ignore = "timing: run with --ignored on hardware"]
fn timing__touch_threads_per_process() {
    let _serial = serialized();
    let d = ProcessDaemon::start(&bin(), "touch-threads");
    let mut client = d.client();
    let me = std::process::id();
    let before = proc_status_field(me, "Threads");
    let mut held = Vec::with_capacity(TOUCH_INTERLOCKS);
    for i in 0..TOUCH_INTERLOCKS {
        held.push(
            client
                .create_interlock(&format!("il-{i}"))
                .unwrap_or_else(|e| panic!("create il-{i} failed: {e}")),
        );
    }
    let after = proc_status_field(me, "Threads");
    let delta = after.saturating_sub(before);
    assert!(
        delta <= TOUCH_THREAD_DELTA_LIMIT,
        "{TOUCH_INTERLOCKS} interlocks added {delta} threads (before {before}, after {after}; limit {TOUCH_THREAD_DELTA_LIMIT})"
    );
    drop(held);
}
