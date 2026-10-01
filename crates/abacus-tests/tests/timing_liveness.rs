//! L3: liveness cascade timing (PHILOSOPHY.md law 3, INTERFACE.md ProcessClock cascade
//! latency). Measures reap latency across a three-level dependency chain over 20 runs.
//! Detection is the arrival of the child's diagnostic line on stderr, not process exit.

#![allow(non_snake_case)]

use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use abacus_client::{AbacusClient, DEFAULT_TOUCH_INTERVAL_MS, DEFAULT_TOUCH_TTL_MS};
use abacus_tests::{
    abacus_binary, abacus_reason, recv_abacus_line, role_args, role_command, serialized,
    spawn_stderr_reader, wait_child, wait_for, wait_for_ready, ProcessDaemon, Rng, Stats,
};

fn bin() -> &'static Path {
    use std::sync::OnceLock;
    static BIN: OnceLock<std::path::PathBuf> = OnceLock::new();
    BIN.get_or_init(abacus_binary)
}

const RUNS: usize = 20;

/// 20 runs of the A <- B <- C chain with C owning "x"; per run, time from SIGKILL of A to
/// the parent observing C's clock reaped and "x" reaped. Assert max is at most the SDK's
/// DEFAULT_TOUCH_TTL_MS + 3 ms + 10 ms slack, and C's ProcessClockReaped diagnostic follows
/// within the SDK's DEFAULT_TOUCH_INTERVAL_MS + 10 ms of its clock's reap. Exit delta
/// printed as information, unasserted.
#[test]
#[ignore = "timing: run with --ignored on hardware"]
fn timing_liveness__chain_reap_is_ttl_plus_one_cycle_per_level() {
    let _serial = serialized();

    let seed_default = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64;
    let mut rng = Rng::from_env(seed_default);
    eprintln!("timing_liveness seed: {}", rng.seed());

    let mut reap_stats = Stats::new("chain-reap", "ms");
    let mut delta_stats = Stats::new("detect-delta", "ms");
    let mut exit_stats = Stats::new("exit-delta", "ms");
    let mut delay_stats = Stats::new("kill-delay", "ms");
    let mut spawn_b_stats = Stats::new("spawn-b-delay", "ms");
    let mut spawn_c_stats = Stats::new("spawn-c-delay", "ms");
    let mut kill_to_detect_stats = Stats::new("kill-to-detect", "ms");

    for run in 0..RUNS {
        let d = ProcessDaemon::start(bin(), &format!("timing-lv-{run}"));
        let sock = d.socket_path().to_str().unwrap().to_string();

        // A's and B's stderr are not read; only C's is.
        let mut child_a = role_command(
            "role__timing_liveness_node",
            &[&sock, &format!("A-{run}"), "", ""],
        )
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn A");
        wait_for_ready(&mut child_a);

        let spawn_b_delay = rng.below(DEFAULT_TOUCH_INTERVAL_MS + 1);
        std::thread::sleep(Duration::from_millis(spawn_b_delay));
        spawn_b_stats.push(spawn_b_delay);

        let mut child_b = role_command(
            "role__timing_liveness_node",
            &[&sock, &format!("B-{run}"), &format!("A-{run}"), ""],
        )
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn B");
        wait_for_ready(&mut child_b);

        let spawn_c_delay = rng.below(DEFAULT_TOUCH_INTERVAL_MS + 1);
        std::thread::sleep(Duration::from_millis(spawn_c_delay));
        spawn_c_stats.push(spawn_c_delay);

        let x_name = format!("x-{run}");
        let mut child_c = role_command(
            "role__timing_liveness_node",
            &[&sock, &format!("C-{run}"), &format!("B-{run}"), &x_name],
        )
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn C");
        let stderr_c = spawn_stderr_reader(&mut child_c);
        wait_for_ready(&mut child_c);

        let mut client = d.client();
        let c_clock = client
            .attach_interlock(&format!("C-{run}"))
            .expect("attach C clock");
        let x = client.attach_interlock(&x_name).expect("attach x");

        let delay_ms = rng.below(2 * DEFAULT_TOUCH_INTERVAL_MS + 1);
        std::thread::sleep(Duration::from_millis(delay_ms));
        delay_stats.push(delay_ms);

        assert!(
            !c_clock.is_reaped() && !x.is_reaped(),
            "run {run}: C clock or x lapsed before the kill"
        );
        let t_kill = Instant::now();
        let kill_ret = unsafe { libc::kill(child_a.id() as i32, libc::SIGKILL) };
        assert_eq!(
            kill_ret,
            0,
            "run {run}: kill(child_a, SIGKILL) failed: {}",
            std::io::Error::last_os_error()
        );
        let wait_result = wait_child(&mut child_a, Duration::from_secs(5));
        assert!(
            wait_result.is_ok(),
            "run {run}: child_a did not exit after SIGKILL: {:?}",
            wait_result.err()
        );

        let budget = Duration::from_secs(2);
        wait_for(budget, Duration::from_millis(1), || c_clock.is_reaped()).unwrap_or_else(|_| {
            panic!(
                "run {run}: C clock not reaped {} ms after SIGKILL",
                t_kill.elapsed().as_millis()
            )
        });
        // Detection is measured from C's clock reap, not from x's.
        let t_reap = Instant::now();
        wait_for(budget, Duration::from_millis(1), || x.is_reaped()).unwrap_or_else(|_| {
            panic!(
                "run {run}: x not reaped {} ms after SIGKILL",
                t_kill.elapsed().as_millis()
            )
        });
        let reap_elapsed = t_kill.elapsed();

        // C's diagnostic line: the keepalive's detection of its reaped clock.
        let (t_detect, detect_line) = recv_abacus_line(&stderr_c, t_reap + Duration::from_secs(1))
            .unwrap_or_else(|e| {
                panic!("run {run}: C did not emit abacus: diagnostic within 1 s of reap: {e}")
            });
        assert_eq!(
            abacus_reason(&detect_line),
            Some("ProcessClockReaped"),
            "run {run}: C diagnostic is not ProcessClockReaped: {detect_line}"
        );
        let detect_delta_ms = t_detect.saturating_duration_since(t_reap).as_millis() as u64;
        let kill_to_detect_ms = t_detect.saturating_duration_since(t_kill).as_millis() as u64;

        // Wait for C exit (informational timing, SIGABRT still asserted).
        let status_c = wait_child(&mut child_c, Duration::from_secs(5)).expect("C did not exit");
        let exit_delta_ms = t_reap.elapsed().as_millis() as u64;
        assert_eq!(
            status_c.signal(),
            Some(libc::SIGABRT),
            "run {run}: C exited with {:?} instead of SIGABRT",
            status_c
        );

        reap_stats.push(reap_elapsed.as_millis() as u64);
        delta_stats.push(detect_delta_ms);
        exit_stats.push(exit_delta_ms);
        kill_to_detect_stats.push(kill_to_detect_ms);

        // B must also abort: SIGABRT within 5 s.
        let status_b = wait_child(&mut child_b, Duration::from_secs(5)).expect("B did not exit");
        assert_eq!(
            status_b.signal(),
            Some(libc::SIGABRT),
            "run {run}: B exited with {:?} instead of SIGABRT",
            status_b
        );
    }

    eprintln!(
        "spawn B delay (ms): min={} median={} max={}",
        spawn_b_stats.min(),
        spawn_b_stats.p50(),
        spawn_b_stats.max()
    );
    eprintln!(
        "spawn C delay (ms): min={} median={} max={}",
        spawn_c_stats.min(),
        spawn_c_stats.p50(),
        spawn_c_stats.max()
    );
    eprintln!(
        "kill delay (ms): min={} median={} max={}",
        delay_stats.min(),
        delay_stats.p50(),
        delay_stats.max()
    );
    eprintln!(
        "chain reap (ms): min={} median={} max={}",
        reap_stats.min(),
        reap_stats.p50(),
        reap_stats.max()
    );
    eprintln!(
        "detect delta after reap (ms): min={} median={} max={}",
        delta_stats.min(),
        delta_stats.p50(),
        delta_stats.max()
    );
    eprintln!(
        "exit delta after reap (ms, informational): min={} median={} max={}",
        exit_stats.min(),
        exit_stats.p50(),
        exit_stats.max()
    );
    eprintln!(
        "kill to detect (ms): min={} median={} max={}",
        kill_to_detect_stats.min(),
        kill_to_detect_stats.p50(),
        kill_to_detect_stats.max()
    );

    let reap_ceiling = DEFAULT_TOUCH_TTL_MS + 3 + 10;
    assert!(
        reap_stats.max() <= reap_ceiling,
        "max reap time {} ms exceeds ceiling {} ms (TTL + 3 + 10 slack); {}",
        reap_stats.max(),
        reap_ceiling,
        reap_stats.report()
    );

    let detect_ceiling = DEFAULT_TOUCH_INTERVAL_MS + 10;
    assert!(
        delta_stats.max() <= detect_ceiling,
        "max detect delta {} ms exceeds ceiling {} ms (interval + 10 slack); {}",
        delta_stats.max(),
        detect_ceiling,
        delta_stats.report()
    );

    let kill_to_detect_ceiling = DEFAULT_TOUCH_TTL_MS + DEFAULT_TOUCH_INTERVAL_MS + 3 + 10;
    assert!(
        kill_to_detect_stats.max() <= kill_to_detect_ceiling,
        "max kill-to-detect time {} ms exceeds ceiling {} ms (TTL + interval + 3 + 10 slack); {}",
        kill_to_detect_stats.max(),
        kill_to_detect_ceiling,
        kill_to_detect_stats.report()
    );
}

// ---------------------------------------------------------------------------
// Roles
// ---------------------------------------------------------------------------

#[test]
#[ignore = "role: process entry point for timing_liveness__ tests"]
fn role__timing_liveness_node() {
    let Some(args) = role_args("role__timing_liveness_node") else {
        return;
    };
    let sock = &args[0];
    let clock_name = &args[1];
    let dep = &args[2];
    let interlock_name = &args[3];

    let deps: Vec<&str> = if dep.is_empty() {
        vec![]
    } else {
        dep.split(',').collect()
    };

    let mut client = AbacusClient::connect(Path::new(sock), clock_name, &deps).expect("connect");

    // Held for the life of the role: dropping the handle stops its keepalive, and the
    // interlock would lapse by TTL whatever its owner does.
    let _il = (!interlock_name.is_empty()).then(|| {
        client
            .create_interlock(interlock_name)
            .expect("create interlock")
    });

    println!("ready");
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}
