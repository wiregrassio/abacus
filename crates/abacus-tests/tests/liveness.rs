//! L1: process clock liveness contracts (PHILOSOPHY.md law 3, INTERFACE.md ProcessClock
//! ownership and dependency cascade). ProcessDaemon; child processes connect with the
//! default Abort policy and are observed from the parent.

#![allow(non_snake_case)]

use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use abacus_client::{AbacusClient, SdkError, DEFAULT_TOUCH_TTL_MS};
use abacus_tests::{
    abacus_binary, abacus_reason, describe_exit, recv_abacus_line, role_args, role_command,
    spawn_stderr_reader, unique_name, wait_child, wait_for, wait_for_ready, ProcessDaemon,
};

fn bin() -> &'static Path {
    // Cache the binary path for the whole test binary.
    use std::sync::OnceLock;
    static BIN: OnceLock<std::path::PathBuf> = OnceLock::new();
    BIN.get_or_init(abacus_binary)
}

/// SIGKILL a process; every interlock it owns and every dependent process clock is reaped
/// within 1 s. The dependent process emits ProcessClockReaped within 1 s and exits by
/// SIGABRT.
#[test]
fn liveness__sigkill_reaps_owned_interlocks_and_dependents() {
    let d = ProcessDaemon::start(bin(), "sigkill-lv");
    let sock = d.socket_path().to_str().unwrap().to_string();

    // Child A: clock "A", no dependencies, creates "a.bus". Its stderr is not read.
    let mut child_a = role_command("role__liveness_node", &[&sock, "A", "", "a.bus"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn A");

    wait_for_ready(&mut child_a);

    // Child B: clock "B", depends on "A", creates "b.bus".
    let mut child_b = role_command("role__liveness_node", &[&sock, "B", "A", "b.bus"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn B");
    let stderr_b = spawn_stderr_reader(&mut child_b);

    wait_for_ready(&mut child_b);

    // Parent attaches a.bus, b.bus, and B.
    let mut client = d.client();
    let a_bus = client.attach_interlock("a.bus").expect("attach a.bus");
    let b_bus = client.attach_interlock("b.bus").expect("attach b.bus");
    let b_clock = client.attach_interlock("B").expect("attach B");

    // The owned interlocks must be alive past one keepalive TTL before the kill: alive
    // because their owners keep them, so the reaps below come from the cascade, not from
    // a lapse.
    std::thread::sleep(Duration::from_millis(DEFAULT_TOUCH_TTL_MS + 100));
    assert!(
        !a_bus.is_reaped() && !b_bus.is_reaped() && !b_clock.is_reaped(),
        "owned interlocks lapsed before the kill: a.bus={} b.bus={} B={}",
        a_bus.is_reaped(),
        b_bus.is_reaped(),
        b_clock.is_reaped()
    );

    // SIGKILL A.
    let t_kill = Instant::now();
    let kill_ret = unsafe { libc::kill(child_a.id() as i32, libc::SIGKILL) };
    assert_eq!(
        kill_ret,
        0,
        "kill(child_a, SIGKILL) failed: {}",
        std::io::Error::last_os_error()
    );
    let wait_result = wait_child(&mut child_a, Duration::from_secs(5));
    assert!(
        wait_result.is_ok(),
        "child_a did not exit after SIGKILL: {:?}",
        wait_result.err()
    );

    // All three must be reaped within 1 s of the kill: one deadline, not one budget each.
    let reap_deadline = t_kill + Duration::from_secs(1);
    for (what, il) in [("a.bus", &a_bus), ("b.bus", &b_bus), ("B clock", &b_clock)] {
        let remaining = reap_deadline.saturating_duration_since(Instant::now());
        wait_for(remaining, Duration::from_millis(1), || il.is_reaped()).unwrap_or_else(|_| {
            panic!(
                "{what} not reaped within 1 s of SIGKILL ({} ms elapsed)",
                t_kill.elapsed().as_millis()
            )
        });
    }

    // B detection: ProcessClockReaped within 1 s of the kill.
    let (_, b_line) =
        recv_abacus_line(&stderr_b, t_kill + Duration::from_secs(1)).unwrap_or_else(|e| {
            panic!(
                "B did not emit abacus: diagnostic within 1 s ({} ms elapsed): {e}",
                t_kill.elapsed().as_millis()
            )
        });
    assert_eq!(
        abacus_reason(&b_line),
        Some("ProcessClockReaped"),
        "B diagnostic is not ProcessClockReaped: {b_line}"
    );

    // B exits by SIGABRT within 5 s.
    let status_b =
        wait_child(&mut child_b, Duration::from_secs(5)).expect("B did not exit within 5 s");
    assert_eq!(
        status_b.signal(),
        Some(libc::SIGABRT),
        "B exited with {} instead of SIGABRT",
        describe_exit(&status_b)
    );
}

/// A <- B <- C chain. SIGKILL A. B and C each emit ProcessClockReaped within 1 s and exit
/// by SIGABRT.
#[test]
fn liveness__three_level_chain_aborts_every_dependent() {
    let d = ProcessDaemon::start(bin(), "chain3-lv");
    let sock = d.socket_path().to_str().unwrap().to_string();

    // Child A's stderr is not read.
    let mut child_a = role_command("role__liveness_node", &[&sock, "A", "", ""])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn A");
    wait_for_ready(&mut child_a);

    let mut child_b = role_command("role__liveness_node", &[&sock, "B", "A", ""])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn B");
    let stderr_b = spawn_stderr_reader(&mut child_b);
    wait_for_ready(&mut child_b);

    let mut child_c = role_command("role__liveness_node", &[&sock, "C", "B", ""])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn C");
    let stderr_c = spawn_stderr_reader(&mut child_c);
    wait_for_ready(&mut child_c);

    let t_kill = Instant::now();
    let kill_ret = unsafe { libc::kill(child_a.id() as i32, libc::SIGKILL) };
    assert_eq!(
        kill_ret,
        0,
        "kill(child_a, SIGKILL) failed: {}",
        std::io::Error::last_os_error()
    );
    let wait_result = wait_child(&mut child_a, Duration::from_secs(5));
    assert!(
        wait_result.is_ok(),
        "child_a did not exit after SIGKILL: {:?}",
        wait_result.err()
    );

    // B and C detection: ProcessClockReaped within 1 s.
    let detect_deadline = t_kill + Duration::from_secs(1);

    let (_, b_line) = recv_abacus_line(&stderr_b, detect_deadline).unwrap_or_else(|e| {
        panic!(
            "B did not emit abacus: diagnostic within 1 s ({} ms elapsed): {e}",
            t_kill.elapsed().as_millis()
        )
    });
    let elapsed_b = t_kill.elapsed();
    assert_eq!(
        abacus_reason(&b_line),
        Some("ProcessClockReaped"),
        "B: expected ProcessClockReaped, got: {b_line}"
    );

    let (_, c_line) = recv_abacus_line(&stderr_c, detect_deadline).unwrap_or_else(|e| {
        panic!(
            "C did not emit abacus: diagnostic within 1 s ({} ms elapsed): {e}",
            t_kill.elapsed().as_millis()
        )
    });
    let elapsed_c = t_kill.elapsed();
    assert_eq!(
        abacus_reason(&c_line),
        Some("ProcessClockReaped"),
        "C: expected ProcessClockReaped, got: {c_line}"
    );

    eprintln!(
        "chain3: B detected after {} ms, C detected after {} ms",
        elapsed_b.as_millis(),
        elapsed_c.as_millis()
    );

    // B and C exit by SIGABRT within 5 s.
    let status_b =
        wait_child(&mut child_b, Duration::from_secs(5)).expect("B did not exit within 5 s");
    assert_eq!(
        status_b.signal(),
        Some(libc::SIGABRT),
        "B exited with {} instead of SIGABRT",
        describe_exit(&status_b)
    );

    let status_c =
        wait_child(&mut child_c, Duration::from_secs(5)).expect("C did not exit within 5 s");
    assert_eq!(
        status_c.signal(),
        Some(libc::SIGABRT),
        "C exited with {} instead of SIGABRT",
        describe_exit(&status_c)
    );
}

/// A missing dependency is refused; once it exists, the same call succeeds.
#[test]
fn liveness__missing_dependency_is_refused_until_it_exists() {
    let d = ProcessDaemon::start(bin(), "dep-miss-lv");

    match d.client_with("X", &["nope"]) {
        Err(SdkError::InterlockNotFound { name }) => {
            assert_eq!(name, "nope", "wrong missing name: {name}");
        }
        Err(other) => panic!("expected InterlockNotFound, got {other}"),
        Ok(_) => panic!("connect with missing dependency succeeded"),
    }

    // Another client connects with clock "nope".
    let _nope = d.client_with("nope", &[]).expect("connect nope");

    // Now the same call succeeds.
    let _x = d
        .client_with("X", &["nope"])
        .expect("connect X with nope present");
}

/// SIGKILL the daemon; the child emits DaemonClockLapsed within 1 s and exits by SIGABRT.
#[test]
fn liveness__daemon_death_aborts_the_client() {
    let mut d = ProcessDaemon::start(bin(), "daemon-death-lv");
    let sock = d.socket_path().to_str().unwrap().to_string();

    let mut child = role_command("role__liveness_node", &[&sock, "P", "", ""])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn P");
    let stderr_rx = spawn_stderr_reader(&mut child);
    wait_for_ready(&mut child);

    // SIGKILL the daemon.
    let t_kill = Instant::now();
    d.kill(libc::SIGKILL);

    // Detection: DaemonClockLapsed within 1 s.
    let (_, line) =
        recv_abacus_line(&stderr_rx, t_kill + Duration::from_secs(1)).unwrap_or_else(|e| {
            panic!(
                "child did not emit abacus: diagnostic within 1 s ({} ms elapsed): {e}",
                t_kill.elapsed().as_millis()
            )
        });
    assert_eq!(
        abacus_reason(&line),
        Some("DaemonClockLapsed"),
        "expected DaemonClockLapsed, got: {line}"
    );

    // Process exit: SIGABRT within 5 s.
    let status =
        wait_child(&mut child, Duration::from_secs(5)).expect("child did not exit within 5 s");
    assert_eq!(
        status.signal(),
        Some(libc::SIGABRT),
        "child exited with {} instead of SIGABRT",
        describe_exit(&status)
    );
}

/// Recreating a clock name aborts the old process; ProcessClockReaped detected within 1 s.
#[test]
fn liveness__recreated_clock_aborts_the_old_process() {
    let d = ProcessDaemon::start(bin(), "recreate-lv");
    let sock = d.socket_path().to_str().unwrap().to_string();

    let mut child = role_command("role__liveness_node", &[&sock, "P", "", ""])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn P");
    let stderr_rx = spawn_stderr_reader(&mut child);
    wait_for_ready(&mut child);

    // The parent connects with the same clock name, replacing the child's.
    let t_replace = Instant::now();
    let _replacer = d.client_with("P", &[]).expect("connect as P (replacer)");

    // Detection: ProcessClockReaped within 1 s.
    let (_, line) = recv_abacus_line(&stderr_rx, t_replace + Duration::from_secs(1))
        .unwrap_or_else(|e| {
            panic!(
                "child did not emit abacus: diagnostic within 1 s ({} ms elapsed): {e}",
                t_replace.elapsed().as_millis()
            )
        });
    assert_eq!(
        abacus_reason(&line),
        Some("ProcessClockReaped"),
        "expected ProcessClockReaped, got: {line}"
    );

    // Process exit: SIGABRT within 5 s.
    let status =
        wait_child(&mut child, Duration::from_secs(5)).expect("child did not exit within 5 s");
    assert_eq!(
        status.signal(),
        Some(libc::SIGABRT),
        "child exited with {} instead of SIGABRT",
        describe_exit(&status)
    );
}

/// A stale client (whose clock was replaced) cannot create or displace.
#[test]
fn liveness__stale_client_cannot_create_or_displace() {
    let d = ProcessDaemon::start(bin(), "stale-lv");

    let mut c1 = d.client_with("P", &[]).expect("connect c1 as P");
    // c2 connects as "P", replacing c1's clock.
    let mut c2 = d.client_with("P", &[]).expect("connect c2 as P");

    let bus = c2.create_interlock("bus").expect("c2 create bus");

    // c1's create must fail with InterlockReaped (its clock was replaced).
    match c1.create_interlock("bus") {
        Err(SdkError::InterlockReaped) => {}
        Err(other) => panic!("c1 create returned {other} instead of InterlockReaped"),
        Ok(_) => panic!("c1 create succeeded after its clock was replaced"),
    }

    // c2's bus is still alive.
    assert!(!bus.is_reaped(), "c2's bus was reaped by c1's create");
}

/// A child that exits normally (drops its client): its owned interlocks lapse within 1 s.
#[test]
fn liveness__clean_exit_lapses_owned_interlocks() {
    let d = ProcessDaemon::start(bin(), "clean-lv");
    let sock = d.socket_path().to_str().unwrap().to_string();

    let mut child = role_command("role__liveness_create_and_exit", &[&sock, "x"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn child");

    // Wait for the child to create "x".
    wait_for_ready(&mut child);

    let mut client = d.client();
    let x = client.attach_interlock("x").expect("attach x");

    // Tell the child it is safe to exit now that the parent has attached "x".
    use std::io::Write;
    child
        .stdin
        .as_mut()
        .expect("child stdin not piped")
        .write_all(b"go\n")
        .expect("write go to child stdin");

    let status = wait_child(&mut child, Duration::from_secs(2)).expect("child did not exit");
    assert!(
        status.success(),
        "child exited with {}",
        describe_exit(&status)
    );

    // "x" must be reaped within 1 s.
    wait_for(Duration::from_secs(1), Duration::from_millis(1), || {
        x.is_reaped()
    })
    .unwrap_or_else(|_| panic!("x not reaped 1 s after clean exit"));
}

// ---------------------------------------------------------------------------
// Roles
// ---------------------------------------------------------------------------

/// Role: connect as clock args[1], with dependency args[2] (empty = none), create interlock
/// args[3] (empty = none), print "ready", then block forever.
#[test]
#[ignore = "role: process entry point for liveness__ tests"]
fn role__liveness_node() {
    let Some(args) = role_args("role__liveness_node") else {
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
    // Block forever; the parent kills us.
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}

/// Role: connect, create interlock args[1], print "ready", then wait for the parent's
/// handshake before exiting: reads one line from stdin (the parent writes "go\n" once it
/// has attached) and requires it to be exactly `go`, then returns. EOF, a read error, or
/// any other line fails the role.
#[test]
#[ignore = "role: process entry point for liveness__clean_exit_lapses_owned_interlocks"]
fn role__liveness_create_and_exit() {
    let Some(args) = role_args("role__liveness_create_and_exit") else {
        return;
    };
    let sock = &args[0];
    let name = &args[1];

    let mut client =
        AbacusClient::connect(Path::new(sock), &unique_name("clean"), &[]).expect("connect");
    let _il = client.create_interlock(name).expect("create");

    println!("ready");

    let mut line = String::new();
    let n = std::io::stdin()
        .read_line(&mut line)
        .expect("read the parent's handshake from stdin");
    assert_eq!(
        line.trim_end(),
        "go",
        "handshake: expected \"go\", read {n} bytes: {line:?}"
    );
}
