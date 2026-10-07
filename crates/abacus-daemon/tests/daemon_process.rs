//! L2: daemon lifecycle as a real child process. Startup, argument forms, signals, restart,
//! the socket file, idle cost. One claim per test; the name is the claim. Child processes
//! are this binary re-executed as a role (see the testkit).

#![allow(non_snake_case)]

mod common;

use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use abacus_client::{AbacusClient, SdkError, WaitState, WatchedWord};
use abacus_tests::{
    describe_exit, role_args, role_command, unique_name, unique_socket_path, wait_child, wait_for,
    wait_for_daemon, wait_for_value, ProcessDaemon,
};
use common::{bin, fresh_client_works, RawDaemon, DEFAULT_SOCKET_PATH};

/// CONTRACTS.md daemon contract: the daemon serves create and attach as soon as it listens.
/// Threshold: connect succeeds within 500 ms of spawn. In-process timer waits in this
/// binary use 20 ms: a 40 ms stall under load would otherwise abort every test at once.
#[test]
fn daemon__starts_and_serves_within_500ms() {
    let d = ProcessDaemon::start(bin(), "starts");
    let startup = d.startup_time();
    let mut client = d.client();
    let timer = client.create_wait_timer("t").expect("create timer");
    let r = timer.wait_ms(20).expect("wait_ms");
    assert!(
        startup < Duration::from_millis(500),
        "daemon took {startup:?} to accept connections"
    );
    assert!(
        matches!(r.state, WaitState::Normal | WaitState::Overrun),
        "timer did not fire normally: {r:?}"
    );
}

/// `main.rs`: `--socket-path=<path>` and `--socket-path <path>` both select the socket; with
/// no argument the daemon uses `/run/abacus/abacus.sock`. On a host where that directory
/// is absent the daemon exits 1 with a bind failure (ENOENT); where it exists the socket
/// appears there. Both branches check the same default path. The fatal line does not name
/// the path it failed to bind.
#[test]
fn daemon__default_socket_path_and_flag_forms() {
    // Form 1: --socket-path=<path>, the form ProcessDaemon uses.
    let d = ProcessDaemon::start(bin(), "flag-eq");
    assert!(
        AbacusClient::connect(d.socket_path(), &unique_name("pc"), &[]).is_ok(),
        "--socket-path= form did not serve on {}",
        d.socket_path().display()
    );
    drop(d);

    // Form 2: --socket-path <path>.
    let path = unique_socket_path("flag-two");
    let two = RawDaemon::spawn(
        &["--socket-path", path.to_str().unwrap()],
        "flag-two",
        Some(path.clone()),
    );
    wait_for_daemon(&path, Duration::from_secs(2)).unwrap_or_else(|e| {
        panic!(
            "two-arg form did not come up on {}: {e}; stderr: {}",
            path.display(),
            two.stderr()
        )
    });
    assert!(
        AbacusClient::connect(&path, &unique_name("pc"), &[]).is_ok(),
        "two-arg form did not serve on {}",
        path.display()
    );
    drop(two);

    // Form 3: no arguments: the default path.
    let default_dir_exists = Path::new(DEFAULT_SOCKET_PATH)
        .parent()
        .map(Path::exists)
        .unwrap_or(false);
    let mut none = RawDaemon::spawn(&[], "flag-none", None);
    match wait_child(&mut none.child, Duration::from_secs(1)) {
        Ok(status) => {
            let stderr = none.stderr();
            assert_eq!(
                status.code(),
                Some(1),
                "no-arg daemon ended with {} instead of exit 1; stderr: {stderr}",
                describe_exit(&status)
            );
            assert!(
                stderr.contains("abacus: fatal"),
                "no fatal line on stderr: {stderr}"
            );
            if !default_dir_exists {
                assert!(
                    stderr.contains("Bind: errno 2"),
                    "expected ENOENT bind failure for {DEFAULT_SOCKET_PATH} (its directory is absent): {stderr}"
                );
            }
        }
        Err(_) => {
            // Still running after 1 s (wait_child killed it): it bound the default path.
            assert!(
                default_dir_exists && Path::new(DEFAULT_SOCKET_PATH).exists(),
                "no-arg daemon kept running but {DEFAULT_SOCKET_PATH} does not exist"
            );
        }
    }
}

/// `transport.rs` Server::create: a second daemon on a live socket path exits 1 with
/// SocketPathOccupied { live_daemon: true } and leaves the first daemon serving.
#[test]
fn daemon__refuses_second_instance_on_same_socket() {
    let d = ProcessDaemon::start(bin(), "second");
    let arg = format!("--socket-path={}", d.socket_path().display());
    let mut second = RawDaemon::spawn(&[&arg], "second-inst", None);
    let status = wait_child(&mut second.child, Duration::from_secs(2))
        .expect("second instance did not exit");
    let stderr = second.stderr();
    assert_eq!(
        status.code(),
        Some(1),
        "second instance ended with {}; stderr: {stderr}",
        describe_exit(&status)
    );
    assert!(
        stderr.contains("occupied by a live daemon"),
        "second instance did not report the live daemon: {stderr}"
    );
    assert!(
        d.socket_path().exists(),
        "first daemon's socket file was removed"
    );
    fresh_client_works(d.socket_path()).expect("first daemon stopped serving");
}

/// SIGTERM is a clean shutdown: exit code 0 and the socket file removed.
#[test]
fn daemon__sigterm_exits_zero_and_removes_socket() {
    let mut d = ProcessDaemon::start(bin(), "sigterm");
    let path = d.socket_path().to_path_buf();
    d.kill(libc::SIGTERM);
    let status = d
        .wait_exit(Duration::from_secs(1))
        .expect("daemon did not exit within 1 s of SIGTERM");
    let removed = wait_for(Duration::from_secs(1), Duration::from_millis(5), || {
        !path.exists()
    });
    assert_eq!(
        status.code(),
        Some(0),
        "SIGTERM should exit 0, got {}",
        describe_exit(&status)
    );
    assert!(
        removed.is_ok(),
        "socket file {} still present after SIGTERM exit",
        path.display()
    );
}

fn binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_abacus"))
}

#[test]
fn sigint_exits_zero() {
    let mut d = ProcessDaemon::start(&binary(), "sigint");
    d.kill(libc::SIGINT);
    let status = d
        .wait_exit(Duration::from_secs(2))
        .expect("exit within 2 s");
    assert_eq!(status.code(), Some(0));
}

#[test]
fn help_and_version() {
    for flag in ["--help", "--version"] {
        let out = Command::new(binary()).arg(flag).output().unwrap();
        assert_eq!(out.status.code(), Some(0), "{flag}");
        assert!(!out.stdout.is_empty(), "{flag} printed nothing");
    }
    let out = Command::new(binary()).arg("--bogus").output().unwrap();
    assert_eq!(out.status.code(), Some(2));
}

/// `transport.rs` Server::create: a stale socket file (daemon SIGKILLed) is replaced by the
/// next start, which then serves.
#[test]
fn daemon__sigkill_leaves_stale_socket_that_next_start_replaces() {
    let mut d = ProcessDaemon::start(bin(), "sigkill");
    let path = d.socket_path().to_path_buf();
    d.kill(libc::SIGKILL);
    let status = d
        .wait_exit(Duration::from_secs(1))
        .expect("daemon did not die on SIGKILL");
    assert_eq!(
        status.signal(),
        Some(libc::SIGKILL),
        "unexpected {}",
        describe_exit(&status)
    );
    assert!(
        path.exists(),
        "SIGKILL should leave the socket file {} behind",
        path.display()
    );
    d.restart();
    fresh_client_works(&path).expect("restarted daemon does not serve");
}

/// ARCHITECTURE.md, README: a daemon restart wakes nobody and every waiter
/// discovers it through its own futex timeout. With today's abort policy the waiting child
/// dies by SIGABRT (DeliveryTimeout) within 2x its wait; the child instead reports
/// `Err(SdkError::DeliveryTimeout)` under `TimeoutPolicy::Error` (). Then a
/// fresh client connects and creates on the new daemon.
#[test]
fn daemon__restart_wakes_nobody_and_timers_time_out() {
    let mut d = ProcessDaemon::start(bin(), "restart-timers");
    let sock = d.socket_path().to_str().unwrap().to_string();
    let mut child = role_command("role__timer_waiter", &[&sock, "waiter", "200"])
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn role");

    // Wait until the child's timer exists and has a pending target (open > closed).
    let mut observer = d.client();
    let attached = wait_for_value(Duration::from_secs(3), Duration::from_millis(2), || {
        observer.attach_interlock("waiter").ok()
    })
    .expect("child never created its timer")
    .0;
    wait_for(Duration::from_secs(2), Duration::from_millis(1), || {
        let (open, closed) = attached.peek();
        open > closed
    })
    .unwrap_or_else(|e| {
        panic!(
            "child never armed its wait: {e}; words {:?}",
            attached.peek()
        )
    });

    let t0 = Instant::now();
    d.restart();
    let status = wait_child(&mut child, Duration::from_secs(2)).expect("waiter did not end");
    let waited = t0.elapsed();
    assert_eq!(
        status.signal(),
        Some(libc::SIGABRT),
        "waiter should die by DeliveryTimeout abort after the restart, ended with {} after {waited:?}",
        describe_exit(&status)
    );
    assert!(
        waited < Duration::from_millis(1000),
        "waiter took {waited:?} to time out on a 200 ms wait (2x margin is 400 ms)"
    );

    let mut fresh = d.client();
    let timer = fresh
        .create_wait_timer("after-restart")
        .expect("create on new daemon");
    let r = timer.wait_ms(20).expect("wait on new daemon");
    assert!(
        matches!(r.state, WaitState::Normal | WaitState::Overrun),
        "state {r:?}"
    );
}

/// README: a restart comes back empty. Old connections are dead, old names are gone, and a
/// fresh client can create.
#[test]
fn daemon__restart_then_fresh_client_can_create() {
    let mut d = ProcessDaemon::start(bin(), "restart-fresh");
    let mut old = d.client();
    let _before = old
        .create_wait_timer("before")
        .expect("create before restart");
    d.restart();
    assert!(
        !old.is_connected(),
        "old connection reports connected after restart"
    );
    let mut fresh = d.client();
    let timer = fresh
        .create_wait_timer("after")
        .expect("create after restart");
    let r = timer.wait_ms(20).expect("wait after restart");
    assert!(
        matches!(r.state, WaitState::Normal | WaitState::Overrun),
        "state {r:?}"
    );
    match fresh.attach_interlock("before") {
        Err(SdkError::InterlockNotFound { .. }) => {}
        other => panic!(
            "name from before the restart still resolves: {:?}",
            other.map(|_| ())
        ),
    }
}

/// The socket is created mode 0660.
#[test]
fn daemon__socket_mode_is_0660() {
    let d = ProcessDaemon::start(bin(), "mode");
    let mode = std::fs::metadata(d.socket_path())
        .expect("stat socket")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o660, "socket mode is {mode:o}, expected 660");
}

/// LIFECYCLE.md daemon evaluation, review measurement "idle daemon CPU over 3 s: 1 tick".
/// With no clients the 1 ms loop costs under 5 clock ticks per 3 s (5% of one core at
/// CLK_TCK 100). L3 asserts a tighter number.
#[test]
fn daemon__idle_cpu_under_5_ticks_per_3s() {
    let d = ProcessDaemon::start(bin(), "idle-cpu");
    let ticks = d.cpu_ticks_over(Duration::from_secs(3));
    assert!(ticks < 5, "idle daemon used {ticks} clock ticks in 3 s");
}

/// LIFECYCLE.md "Daemon evaluation (every 1 ms cycle)" and clock advancement: the clock word
/// advances every cycle, so consecutive updates are never more than a few ms apart, including
/// right after a burst of ordinary requests. The loop anchors cycles to real time, so a burst
/// of event-driven early poll returns does not push the next 1 ms boundary out and freeze the
/// clock. Threshold: no gap over 5 ms in 400 ms of sampling after 300 back-to-back attaches.
#[test]
fn daemon__loop_keeps_1ms_cadence_after_request_burst() {
    let d = ProcessDaemon::start(bin(), "cadence");
    let clock = abacus_tests::attach_words_readonly(d.socket_path(), "clock");
    let mut raw = abacus_tests::RawClient::connect(d.socket_path()).expect("raw connect");
    for i in 0..300 {
        let (resp, _fds) = raw
            .attach("clock")
            .unwrap_or_else(|e| panic!("attach {i}: {e}"));
        assert!(
            matches!(resp, abacus_tests::RawResponse::Attached { .. }),
            "attach {i}: {resp:?}"
        );
    }

    let sample_for = Duration::from_millis(400);
    let start = Instant::now();
    let mut last_value = clock
        .words()
        .open_count
        .load(std::sync::atomic::Ordering::Acquire);
    let mut last_change = Instant::now();
    let mut max_gap = Duration::ZERO;
    let mut max_gap_at = Duration::ZERO;
    while start.elapsed() < sample_for {
        let v = clock
            .words()
            .open_count
            .load(std::sync::atomic::Ordering::Acquire);
        if v != last_value {
            let gap = last_change.elapsed();
            if gap > max_gap {
                max_gap = gap;
                max_gap_at = start.elapsed();
            }
            last_value = v;
            last_change = Instant::now();
        }
        std::thread::sleep(Duration::from_micros(200));
    }
    let tail = last_change.elapsed();
    if tail > max_gap {
        max_gap = tail;
        max_gap_at = sample_for;
    }
    // Debug builds run the daemon loop slower; widen the threshold.
    let max_allowed = if cfg!(debug_assertions) {
        Duration::from_millis(10)
    } else {
        Duration::from_millis(5)
    };
    assert!(
        max_gap < max_allowed,
        "daemon clock froze for {max_gap:?} (ending {max_gap_at:?} into the sample) after a 300-request burst; limit {max_allowed:?}"
    );
}

/// SURFACE.md Connection: `is_connected` reports the peer closing. True while the daemon
/// runs, false within a bounded wait after it is SIGKILLed.
#[test]
fn client__is_connected_true_then_false_after_daemon_stops() {
    let mut d = ProcessDaemon::start(bin(), "is-connected");
    let client = d.client();
    assert!(
        client.is_connected(),
        "is_connected false with a live daemon"
    );
    d.kill(libc::SIGKILL);
    d.wait_exit(Duration::from_secs(1))
        .expect("daemon did not die");
    let flipped = wait_for(Duration::from_secs(1), Duration::from_millis(1), || {
        !client.is_connected()
    });
    assert!(
        flipped.is_ok(),
        "is_connected still true after the daemon died: {flipped:?}"
    );
}

/// CONTRACTS.md UDS surface: interlocks are shared by name across processes. A child
/// creates and advances an interlock; this process's WaitCounter on it fires.
#[test]
fn daemon__two_processes_share_one_interlock() {
    let d = ProcessDaemon::start(bin(), "share");
    let sock = d.socket_path().to_str().unwrap().to_string();
    let mut child = role_command("role__create_and_advance", &[&sock, "shared", "5"])
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn role");

    let mut client = d.client();
    // The child creates "shared" first; attach once it exists.
    let attached = wait_for_value(Duration::from_secs(5), Duration::from_millis(2), || {
        client.attach_interlock("shared").ok()
    })
    .expect("child never created the interlock")
    .0;
    let counter = client
        .create_wait_counter("watch-shared", "shared", WatchedWord::ClosedCount)
        .expect("create counter");
    let r = counter.wait_until(5, 200).expect("wait_until");
    let (open, closed) = attached.peek();
    assert!(
        matches!(r.state, WaitState::Normal | WaitState::Overrun),
        "counter did not fire: {r:?}; shared open={open} closed={closed}"
    );
    assert!(
        r.completed_at >= 5,
        "completed_at={} below target 5",
        r.completed_at
    );

    let status = wait_child(&mut child, Duration::from_secs(5)).expect("child exit");
    assert!(
        status.success(),
        "child role failed: {}",
        describe_exit(&status)
    );
}

/// The daemon writes `<runtime-dir>/heartbeat` once per second with the current epoch
/// timestamp. The file exists while the daemon runs and is removed on clean shutdown.
#[test]
fn daemon__heartbeat_file_exists_while_running_and_is_removed_on_stop() {
    let mut d = ProcessDaemon::start(bin(), "heartbeat");
    let heartbeat_path = d.socket_path().parent().unwrap().join("heartbeat");

    // Wait for the heartbeat file to contain a valid timestamp. The daemon writes it once
    // per second; std::fs::write creates the file before writing, so poll for content, not
    // just existence.
    let (epoch_1, _) = wait_for_value(Duration::from_secs(3), Duration::from_millis(50), || {
        let content = std::fs::read_to_string(&heartbeat_path).ok()?;
        if !content.ends_with('\n') {
            return None;
        }
        content.trim_end_matches('\n').parse::<u64>().ok()
    })
    .expect("heartbeat file never contained a valid timestamp");
    assert!(epoch_1 > 1_700_000_000, "epoch looks too small: {epoch_1}");

    // Sleep 1.5 s and verify the value has advanced.
    std::thread::sleep(Duration::from_millis(1500));
    let content_2 = std::fs::read_to_string(&heartbeat_path).expect("read heartbeat again");
    let epoch_2: u64 = content_2
        .trim_end_matches('\n')
        .parse()
        .unwrap_or_else(|e| panic!("second read is not a decimal number: {content_2:?}: {e}"));
    assert!(
        epoch_2 > epoch_1,
        "heartbeat did not advance: {epoch_1} then {epoch_2}"
    );

    // Stop the daemon and verify the file is removed.
    d.kill(libc::SIGTERM);
    let status = d
        .wait_exit(Duration::from_secs(2))
        .expect("daemon did not exit within 2 s of SIGTERM");
    assert_eq!(
        status.code(),
        Some(0),
        "SIGTERM should exit 0, got {}",
        describe_exit(&status)
    );
    let removed = wait_for(Duration::from_secs(1), Duration::from_millis(5), || {
        !heartbeat_path.exists()
    });
    assert!(
        removed.is_ok(),
        "heartbeat file {} still present after clean shutdown",
        heartbeat_path.display()
    );
}

// ---------------------------------------------------------------------------
// Roles
// ---------------------------------------------------------------------------

/// Role: connect to `args[0]`, create interlock `args[1]`, open and close it by `args[2]`,
/// then hold it alive for 500 ms so the parent can observe it. Not a test; spawned by
/// `daemon__two_processes_share_one_interlock`.
#[test]
#[ignore = "role: process entry point for daemon__two_processes_share_one_interlock"]
fn role__create_and_advance() {
    let Some(args) = role_args("role__create_and_advance") else {
        return;
    };
    let mut client =
        AbacusClient::connect(Path::new(&args[0]), &unique_name("role"), &[]).expect("connect");
    let il = client.create_interlock(&args[1]).expect("create");
    let n: u64 = args[2].parse().expect("count");
    il.open(n).expect("open");
    il.close(n).expect("close");
    // Hold (the stimulus) so the parent can attach and watch; bounded.
    std::thread::sleep(Duration::from_millis(500));
}

/// Role: connect to `args[0]`, create WaitTimer `args[1]`, `wait_ms(args[2])`. Exits 3 if the
/// wait returns Ok, 4 on Err; the parent expects neither (SIGABRT from DeliveryTimeout). Not a
/// test; spawned by `daemon__restart_wakes_nobody_and_timers_time_out`.
#[test]
#[ignore = "role: process entry point for daemon__restart_wakes_nobody_and_timers_time_out"]
fn role__timer_waiter() {
    let Some(args) = role_args("role__timer_waiter") else {
        return;
    };
    let mut client =
        AbacusClient::connect(Path::new(&args[0]), &unique_name("role"), &[]).expect("connect");
    let timer = client.create_wait_timer(&args[1]).expect("create timer");
    let ms: u64 = args[2].parse().expect("ms");
    match timer.wait_ms(ms) {
        Ok(r) => {
            eprintln!("role__timer_waiter: wait_ms({ms}) returned {r:?}");
            std::process::exit(3);
        }
        Err(e) => {
            eprintln!("role__timer_waiter: wait_ms({ms}) failed: {e}");
            std::process::exit(4);
        }
    }
}
