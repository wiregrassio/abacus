//! L1: connect_waiting contracts (Stage 4c). Tests that connect_waiting retries on
//! missing dependencies and absent daemon, times out with DependencyTimeout, logs once
//! per distinct reason, and returns immediately on non-retryable errors.

#![allow(non_snake_case)]

use std::path::Path;
use std::process::Stdio;
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

use abacus_client::{AbacusClient, SdkError, CONNECT_RETRY_INTERVAL};
use abacus_daemon::daemon::DaemonConfig;
use abacus_tests::{
    abacus_binary, describe_exit, role_args, role_command, run_child, spawn_stderr_reader,
    spawn_stdout_reader, unique_name, unique_socket_path, wait_child, LineRx, ProcessDaemon,
    ThreadDaemon,
};

fn bin() -> &'static Path {
    use std::sync::OnceLock;
    static BIN: OnceLock<std::path::PathBuf> = OnceLock::new();
    BIN.get_or_init(abacus_binary)
}

/// Wait for the next `abacus: waiting up to` line on a child's stderr, within 5 s, and
/// return it. The child logs one such line per distinct wait reason, the moment it first
/// fails for that reason, so the line is proof that a failed attempt happened.
fn next_wait_line(rx: &LineRx) -> String {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut seen: Vec<String> = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(remaining) {
            Ok((_, line)) if line.starts_with("abacus: waiting up to") => return line,
            Ok((_, line)) => seen.push(line),
            Err(e) => panic!(
                "no `abacus: waiting up to` line on the child's stderr within 5 s ({e}); \
                 lines seen: {seen:?}"
            ),
        }
    }
}

/// Wait up to 5 s for a stdout line containing `result: ` and return it with the instant
/// the reader read it. Panics on the deadline with the lines seen so far.
fn recv_result_line(rx: &LineRx) -> (Instant, String) {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut seen: Vec<String> = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(remaining) {
            Ok((t, line)) if line.contains("result: ") => return (t, line),
            Ok((_, line)) => seen.push(line),
            Err(e) => panic!(
                "no `result: ` line on the child's stdout within 5 s ({e}); lines seen: {seen:?}"
            ),
        }
    }
}

/// A role child waits on a missing dependency with a 5 s ceiling. The dependency appears
/// only after the child has logged its first failed attempt (its `waiting up to` line), so
/// the retry path is exercised; the child then reports Ok within 2 retry intervals + 50 ms
/// slack of the dependency appearing, and exits cleanly.
#[test]
fn connect_waiting__succeeds_once_the_dependency_appears() {
    let d = ProcessDaemon::start(bin(), "cw-dep");
    let sock = d.socket_path().to_str().unwrap().to_string();
    let dep_name = unique_name("dep");

    let mut child = role_command("role__connect_waiting_log", &[&sock, &dep_name, "5000"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn waiter child");
    let err = spawn_stderr_reader(&mut child);
    let out = spawn_stdout_reader(&mut child);

    let first = next_wait_line(&err);
    assert!(
        first.contains(&format!("for dependency \"{dep_name}\"")),
        "first wait line: {first}"
    );

    let _dep = d.client_with(&dep_name, &[]).expect("create dep");
    let t_appear = Instant::now();

    let (t_ok, line) = recv_result_line(&out);
    assert!(line.contains("result: Ok"), "child result: {line}");
    let elapsed = t_ok.saturating_duration_since(t_appear);
    let bound = CONNECT_RETRY_INTERVAL * 2 + Duration::from_millis(50);
    assert!(
        elapsed <= bound,
        "connect_waiting took {:?} after the dependency appeared (bound {:?})",
        elapsed,
        bound
    );

    let status = wait_child(&mut child, Duration::from_secs(5)).expect("child exit");
    assert!(
        status.success(),
        "child exited with {}",
        describe_exit(&status)
    );
}

/// A role child waits on a socket path with no daemon, with a 5 s ceiling. A ThreadDaemon
/// starts on that path only after the child has logged its first failed attempt (its
/// `waiting up to` line), so the retry path is exercised; the child then reports Ok within
/// 2 retry intervals + 50 ms slack of the daemon appearing, and exits cleanly.
#[test]
fn connect_waiting__succeeds_once_the_daemon_appears() {
    let sock = unique_socket_path("cw-daemon");

    let mut child = role_command(
        "role__connect_waiting_log",
        &[sock.to_str().unwrap(), "", "5000"],
    )
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .expect("spawn waiter child");
    let err = spawn_stderr_reader(&mut child);
    let out = spawn_stdout_reader(&mut child);

    let first = next_wait_line(&err);
    assert!(
        first.contains("for the Abacus daemon at"),
        "first wait line: {first}"
    );

    let _d = ThreadDaemon::start_with("cw-daemon", DaemonConfig::new(sock));
    let t_appear = Instant::now();

    let (t_ok, line) = recv_result_line(&out);
    assert!(line.contains("result: Ok"), "child result: {line}");
    let elapsed = t_ok.saturating_duration_since(t_appear);
    let bound = CONNECT_RETRY_INTERVAL * 2 + Duration::from_millis(50);
    assert!(
        elapsed <= bound,
        "connect_waiting took {:?} after the daemon appeared (bound {:?})",
        elapsed,
        bound
    );

    let status = wait_child(&mut child, Duration::from_secs(5)).expect("child exit");
    assert!(
        status.success(),
        "child exited with {}",
        describe_exit(&status)
    );
}

/// 300 ms ceiling, dependency never appears; DependencyTimeout returned at no less than
/// 300 ms and no more than 300 ms + 2 retry intervals + 50 ms.
#[test]
fn connect_waiting__times_out_with_dependency_timeout() {
    let d = ProcessDaemon::start(bin(), "cw-timeout");
    let max_wait = Duration::from_millis(300);
    let t0 = Instant::now();
    let result =
        AbacusClient::connect_waiting(d.socket_path(), &unique_name("cw"), &["dep"], max_wait);
    let elapsed = t0.elapsed();

    match result {
        Err(SdkError::DependencyTimeout { missing, .. }) => {
            assert_eq!(missing, "dep", "wrong missing name: {missing}");
        }
        Err(other) => panic!("expected DependencyTimeout, got {other}"),
        Ok(_) => panic!("connect_waiting succeeded with missing dependency"),
    }

    assert!(
        elapsed >= max_wait,
        "returned too early: {:?} < {:?}",
        elapsed,
        max_wait
    );
    let upper = max_wait + CONNECT_RETRY_INTERVAL * 2 + Duration::from_millis(50);
    assert!(
        elapsed <= upper,
        "returned too late: {:?} > {:?}",
        elapsed,
        upper
    );
}

/// In a role child (stderr captured): wait 1 s for a missing dependency, then print its
/// result; exactly one `abacus: waiting up to` line on stderr across roughly ten retries.
/// The child must also exit successfully, print a DependencyTimeout naming "dep" as its
/// result, and return no sooner than its 1000 ms ceiling.
#[test]
fn connect_waiting__logs_once_per_reason() {
    let d = ProcessDaemon::start(bin(), "cw-log");
    let sock = d.socket_path().to_str().unwrap().to_string();

    let t0 = Instant::now();
    let child = role_command("role__connect_waiting_log", &[&sock, "dep", "1000"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn log child");

    let run = run_child(child, Duration::from_secs(5));
    let elapsed = t0.elapsed();
    assert!(
        run.status.success(),
        "child exited with {}:\nstdout:\n{}\nstderr:\n{}",
        describe_exit(&run.status),
        run.stdout,
        run.stderr
    );
    assert!(
        run.stdout.contains("result: Err(dependency timeout") && run.stdout.contains("\"dep\""),
        "child did not report a DependencyTimeout for \"dep\":\n{}",
        run.stdout
    );
    assert!(
        elapsed >= Duration::from_millis(1000),
        "child returned after {elapsed:?}, before its 1000 ms ceiling"
    );

    let wait_lines: Vec<&str> = run
        .stderr
        .lines()
        .filter(|l| l.starts_with("abacus: waiting up to"))
        .collect();

    assert_eq!(
        wait_lines.len(),
        1,
        "expected exactly one 'waiting up to' line, got {}:\n{}",
        wait_lines.len(),
        run.stderr
    );
}

/// A reserved clock name ("clock") returns InvalidRequest without waiting (well under one
/// retry interval).
#[test]
fn connect_waiting__other_errors_return_at_once() {
    let d = ProcessDaemon::start(bin(), "cw-fast");
    let t0 = Instant::now();
    let result =
        AbacusClient::connect_waiting(d.socket_path(), "clock", &[], Duration::from_secs(5));
    let elapsed = t0.elapsed();

    match result {
        Err(SdkError::InvalidRequest { .. }) => {}
        Err(other) => panic!("expected InvalidRequest, got {other}"),
        Ok(_) => panic!("reserved name 'clock' should fail"),
    }
    assert!(
        elapsed < CONNECT_RETRY_INTERVAL,
        "took {:?}, should be under one retry interval {:?}",
        elapsed,
        CONNECT_RETRY_INTERVAL
    );
}

/// A connect errno other than ENOENT/ECONNREFUSED is not retried. `/etc/passwd` is a
/// regular file, so a socket path under it (`/etc/passwd/abacus.sock`) fails to connect
/// with ENOTDIR regardless of uid: `connect_waiting` returns a Transport error in under
/// one retry interval instead of waiting out its 5 s ceiling.
#[test]
fn connect_waiting__non_retryable_connect_error_returns_at_once() {
    let path = Path::new("/etc/passwd/abacus.sock");
    let t0 = Instant::now();
    let result =
        AbacusClient::connect_waiting(path, &unique_name("cw"), &[], Duration::from_secs(5));
    let elapsed = t0.elapsed();

    match result {
        Err(SdkError::Transport(_)) => {}
        Err(other) => panic!("expected a Transport error, got {other}"),
        Ok(_) => panic!("connect_waiting succeeded against a socket path under a regular file"),
    }
    assert!(
        elapsed < CONNECT_RETRY_INTERVAL,
        "took {:?}, should be under one retry interval {:?}",
        elapsed,
        CONNECT_RETRY_INTERVAL
    );
}

/// The wait reason alternates as the daemon appears, disappears, and reappears: dep missing
/// (no daemon) -> dep missing (daemon up, "dep" absent) -> dep missing (no daemon again,
/// already logged) -> success (daemon up, "dep" present). Each distinct reason logs exactly
/// once: two `abacus: waiting up to` lines total, the first for the absent daemon and the
/// second for the missing dependency. Phases advance on the child's own log lines; the one
/// remaining sleep only gives the unobservable already-logged phase time to occur, and its
/// absence cannot make the test pass falsely.
#[test]
fn connect_waiting__logs_each_reason_once_when_reasons_alternate() {
    let sock = unique_socket_path("cw-alt");

    let mut child = role_command(
        "role__connect_waiting_log",
        &[sock.to_str().unwrap(), "dep", "5000"],
    )
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .expect("spawn log child");
    let err = spawn_stderr_reader(&mut child);
    let out = spawn_stdout_reader(&mut child);

    let first = next_wait_line(&err); // absent daemon
    assert!(
        first.contains("for the Abacus daemon at"),
        "first wait line: {first}"
    );
    let d1 = ThreadDaemon::start_with("cw-alt", DaemonConfig::new(sock.clone()));
    let second = next_wait_line(&err); // daemon up, "dep" missing
    assert!(
        second.contains("for dependency \"dep\""),
        "second wait line: {second}"
    );
    drop(d1);
    // The absent-daemon reason recurs but was already logged, so no line marks it.
    // Give the child at least two retries against the absent daemon.
    std::thread::sleep(CONNECT_RETRY_INTERVAL * 2);
    let d2 = ThreadDaemon::start_with("cw-alt", DaemonConfig::new(sock));
    let _dep_client = d2.client_with("dep", &[]).expect("connect as dep");
    let (_, result) = recv_result_line(&out);
    assert!(result.contains("result: Ok"), "child result: {result}");
    let status = wait_child(&mut child, Duration::from_secs(5)).expect("child exit");
    assert!(
        status.success(),
        "child exited with {}",
        describe_exit(&status)
    );

    // Collect every remaining stderr line: wait up to 1 s for the reader to reach EOF (the
    // channel disconnects), then take anything still queued.
    let mut rest: Vec<String> = Vec::new();
    let eof_deadline = Instant::now() + Duration::from_secs(1);
    loop {
        let remaining = eof_deadline.saturating_duration_since(Instant::now());
        match err.recv_timeout(remaining) {
            Ok((_, line)) => rest.push(line),
            Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => panic!(
                "stderr reader did not reach EOF within 1 s of child exit; stderr so far:\n\
                 {first}\n{second}\n{}",
                rest.join("\n")
            ),
        }
    }
    rest.extend(err.try_iter().map(|(_, line)| line));

    let further: Vec<&String> = rest
        .iter()
        .filter(|l| l.starts_with("abacus: waiting up to"))
        .collect();
    assert!(
        further.is_empty(),
        "expected exactly two 'waiting up to' lines (absent daemon, then missing dependency), \
         got {}; stderr:\n{first}\n{second}\n{}",
        2 + further.len(),
        rest.join("\n")
    );
}

// ---------------------------------------------------------------------------
// Roles
// ---------------------------------------------------------------------------

/// Role: call connect_waiting on socket args[0] with dependency args[1] (empty = no
/// dependencies) for args[2] ms, then print the result.
#[test]
#[ignore = "role: process entry point for connect_waiting__succeeds_once_the_dependency_appears, \
            connect_waiting__succeeds_once_the_daemon_appears, \
            connect_waiting__logs_once_per_reason, and \
            connect_waiting__logs_each_reason_once_when_reasons_alternate"]
fn role__connect_waiting_log() {
    let Some(args) = role_args("role__connect_waiting_log") else {
        return;
    };
    let sock = &args[0];
    let dep_name = &args[1];
    let max_wait_ms: u64 = args[2].parse().expect("max_wait_ms");

    let deps: Vec<&str> = if dep_name.is_empty() {
        vec![]
    } else {
        vec![dep_name.as_str()]
    };
    let result = AbacusClient::connect_waiting(
        Path::new(sock),
        &unique_name("log"),
        &deps,
        Duration::from_millis(max_wait_ms),
    );

    match result {
        Ok(_) => println!("result: Ok"),
        Err(e) => println!("result: Err({e})"),
    }
}
