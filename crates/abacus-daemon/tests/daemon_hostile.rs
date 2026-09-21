//! L2: hostile clients against a real daemon. Every test's pass condition is the same:
//! the daemon is alive and a fresh client can create a timer and wait on it. Anything that
//! can shrink a memfd runs in a role child, and so does the check after it, because a shrunk
//! memfd SIGBUSes every process that maps it.

#![allow(non_snake_case)]

mod common;

use std::os::fd::AsRawFd;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use abacus_tests::{
    describe_exit, role_args, role_command, wait_child, wait_for, ProcessDaemon, RawClient,
    RawResponse, ERR_INVALID_REQUEST, TIER_INTERLOCK,
};
use common::{bin, drained_to_eof, fresh_client_waits, fresh_client_works};

/// A client that sends 2 of 4 length-prefix bytes and holds does not stall the daemon:
/// a second client's create and `wait_ms(5)` complete within 500 ms, and daemon CPU over 1 s
/// stays under 5 ticks. The well-behaved client is a role child, so its hung create is
/// killed at the deadline instead of hanging the test.
#[test]
fn daemon__survives_partial_frame_client() {
    let mut d = ProcessDaemon::start(bin(), "partial");
    let mut raw = RawClient::connect(d.socket_path()).expect("raw connect");
    raw.send_bytes(&[7, 0]).expect("send 2 prefix bytes");

    let served = fresh_client_waits(d.socket_path(), 5, Duration::from_millis(500));
    let ticks = d.cpu_ticks_over(Duration::from_secs(1));
    drop(raw); // the daemon recovers on EOF
    let alive = d.is_alive();

    assert!(alive, "daemon died while a partial frame was held");
    assert!(
        served.is_ok(),
        "well-behaved client not served while a partial frame was held ({}); daemon used {ticks} clock ticks in 1 s",
        served.unwrap_err()
    );
    assert!(
        ticks < 5,
        "daemon used {ticks} clock ticks in 1 s with a partial frame held"
    );
}

/// An attacher that `ftruncate`s the clock memfd gets EPERM (sealed) and the daemon
/// keeps serving. Attacker and post-check both run as role children; this process never
/// maps that daemon's clock.
#[test]
fn daemon__survives_ftruncate_from_attacher() {
    let mut d = ProcessDaemon::start(bin(), "ftruncate");
    let sock = d.socket_path().to_str().unwrap().to_string();

    let mut attacker = role_command("role__truncate_clock", &[&sock])
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn attacker");
    let attack = wait_child(&mut attacker, Duration::from_secs(3)).expect("attacker did not end");

    // Give a SIGBUS time to land before judging liveness.
    let died = wait_for(Duration::from_millis(300), Duration::from_millis(5), || {
        !d.is_alive()
    });
    let alive = died.is_err();

    let post = fresh_client_works(d.socket_path());

    assert!(
        alive,
        "daemon died after an attacher shrank the clock memfd (attacker {}, daemon stderr: {:?})",
        describe_exit(&attack),
        d.stderr_lines()
    );
    assert_eq!(
        attack.code(),
        Some(11),
        "ftruncate on the clock memfd should be refused (exit 11), attacker ended with {}",
        describe_exit(&attack)
    );
    assert!(
        post.is_ok(),
        "well-behaved client failed after the attack: {post:?}"
    );
}

/// A protocol fault closes the connection: the daemon sends InvalidRequest best-effort
/// and then EOF.
#[test]
fn daemon__closes_connection_on_protocol_fault() {
    let mut d = ProcessDaemon::start(bin(), "fault-close");
    let mut raw = RawClient::connect(d.socket_path()).expect("raw connect");
    raw.send_frame(&[1, 0x7f]).expect("send unknown-tag frame");
    let reply = raw.recv_response_with_fds();
    let eof = drained_to_eof(&mut raw, Duration::from_millis(200));
    let fresh = fresh_client_works(d.socket_path());

    assert!(d.is_alive(), "daemon died on an unknown tag");
    match &reply {
        Ok((RawResponse::Error { code, .. }, fds)) => {
            assert_eq!(*code, ERR_INVALID_REQUEST, "wrong error code");
            assert!(fds.is_empty(), "error reply carried {} fds", fds.len());
        }
        other => panic!("unknown tag did not produce an InvalidRequest reply: {other:?}"),
    }
    assert!(
        fresh.is_ok(),
        "fresh client failed after the fault: {fresh:?}"
    );
    assert!(
        eof.is_ok(),
        "connection still open after a protocol fault: {}",
        eof.unwrap_err()
    );
}

/// One understated frame followed by 200 stray bytes produces exactly one fault line on
/// stderr: the daemon closes the connection on the first fault instead of re-parsing the
/// stray bytes as further frames.
#[test]
fn daemon__stderr_has_one_line_per_fault_not_per_cycle() {
    let mut d = ProcessDaemon::start(bin(), "fault-lines");
    let mut raw = RawClient::connect(d.socket_path()).expect("raw connect");
    // Prefix says 2 bytes; payload [version, create tag] then 200 zeros the daemon must not
    // treat as further frames.
    let mut bytes = vec![2, 0, 0, 0, 1, 1];
    bytes.extend_from_slice(&[0u8; 200]);
    raw.send_bytes(&bytes).expect("send");
    // Bounded: the daemon closes the connection within a cycle after the fault.
    let _ = drained_to_eof(&mut raw, Duration::from_millis(300));
    let lines = d.fault_lines();
    assert!(d.is_alive(), "daemon died on a malformed frame");
    assert_eq!(
        lines.len(),
        1,
        "expected one fault line for one bad frame, got {}:\n{}",
        lines.len(),
        lines.join("\n")
    );
}

/// A client that never reads its responses is dropped once its receive buffer is full
/// (write would block): further sends fail with EPIPE. The daemon stays alive and a fresh
/// client is served.
#[test]
fn daemon__survives_client_that_never_reads() {
    let mut d = ProcessDaemon::start(bin(), "never-reads");
    let mut raw = RawClient::connect(d.socket_path()).expect("raw connect");
    raw.set_write_timeout(Some(Duration::from_secs(1)));
    let mut queued = 0usize;
    for _ in 0..600 {
        if raw.send_attach("clock").is_err() {
            break;
        }
        queued += 1;
    }
    // once the daemon drops us, our sends fail.
    let dropped = wait_for(Duration::from_secs(3), Duration::from_millis(10), || {
        raw.send_attach("clock").is_err()
    });
    let alive = d.is_alive();
    let fresh = fresh_client_works(d.socket_path());
    drop(raw);

    assert!(
        alive,
        "daemon died serving a client that never reads (queued {queued})"
    );
    assert!(
        fresh.is_ok(),
        "fresh client failed alongside a non-reading client: {fresh:?}"
    );
    assert!(
        dropped.is_ok(),
        "daemon kept a client whose responses cannot be written ({queued} requests queued): {}",
        dropped.unwrap_err()
    );
}

/// CONTRACTS.md daemon contract "Persistent connections": a thousand connect and disconnect
/// cycles leave the daemon alive, its fd table back at baseline, and a fresh client served.
/// The evaluation loop anchors its 1 ms boundary to real time, so a burst of wakeups does not
/// push the cycle ahead and freeze the clock (see
/// `daemon__loop_keeps_1ms_cadence_after_request_burst`).
#[test]
fn daemon__survives_thousand_connect_disconnect_cycles() {
    let mut d = ProcessDaemon::start(bin(), "churn");
    let baseline = d.fd_count();
    for i in 0..1000 {
        let c = RawClient::connect(d.socket_path())
            .unwrap_or_else(|e| panic!("connect {i} failed: {e}"));
        drop(c);
    }
    let settled = wait_for(Duration::from_secs(2), Duration::from_millis(5), || {
        d.fd_count() <= baseline + 1
    });
    let fds = d.fd_count();
    let fresh = fresh_client_works(d.socket_path());
    let ticks = d.cpu_ticks_over(Duration::from_millis(200));
    assert!(d.is_alive(), "daemon died under connection churn");
    assert!(
        settled.is_ok(),
        "daemon fd count {fds} did not return to baseline {baseline}"
    );
    assert!(
        fresh.is_ok(),
        "fresh client failed after churn: {fresh:?}; daemon ticks over 200 ms after: {ticks}; stderr: {:?}",
        d.stderr_lines()
    );
}

/// CONTRACTS.md wire ABI "Max payload 4096": a create whose name alone is 4096 bytes makes
/// a 4101-byte payload; the daemon rejects it with InvalidRequest (FrameTooLarge), stays
/// alive, and serves a fresh client. The daemon closes the connection on the first fault
/// instead of re-parsing the unread payload as further frames, the 1 ms loop stays anchored
/// to real time under the resulting burst (see
/// `daemon__loop_keeps_1ms_cadence_after_request_burst`), and a trailing partial prefix does
/// not spin the loop, so the fresh client's timer is stamped normally.
#[test]
fn daemon__survives_client_sending_4096_byte_name() {
    let mut d = ProcessDaemon::start(bin(), "big-name");
    let mut raw = RawClient::connect(d.socket_path()).expect("raw connect");
    let name = "n".repeat(4096);
    let reply = raw.create(&name, TIER_INTERLOCK, None, None, None);
    let fresh = fresh_client_works(d.socket_path());
    let ticks = d.cpu_ticks_over(Duration::from_millis(200));
    let faults = d.fault_lines().len();
    drop(raw);
    assert!(d.is_alive(), "daemon died on a 4096-byte name");
    match &reply {
        Ok((RawResponse::Error { code, message }, _)) => assert_eq!(
            *code, ERR_INVALID_REQUEST,
            "oversized frame answered with code {code}: {message}"
        ),
        other => panic!("oversized frame was not rejected: {other:?}"),
    }
    assert!(
        fresh.is_ok(),
        "fresh client failed after the oversized frame: {fresh:?}; daemon ticks over 200 ms: {ticks}, fault lines: {faults}"
    );
}

// ---------------------------------------------------------------------------
// Roles
// ---------------------------------------------------------------------------

/// Role: raw attach to "clock" on `args[0]`, `ftruncate(fd, 0)` the received memfd. Exit 11
/// if the kernel refused (EPERM: sealed), 10 if the shrink succeeded. Never maps the memfd.
/// Not a test; spawned by `daemon__survives_ftruncate_from_attacher`.
#[test]
#[ignore = "role: process entry point for daemon__survives_ftruncate_from_attacher"]
fn role__truncate_clock() {
    let Some(args) = role_args("role__truncate_clock") else {
        return;
    };
    let mut raw = RawClient::connect(Path::new(&args[0])).expect("raw connect");
    let (resp, fds) = raw.attach("clock").expect("attach clock");
    assert!(
        matches!(resp, RawResponse::Attached { .. }) && fds.len() == 1,
        "attach clock: {resp:?} with {} fds",
        fds.len()
    );
    // SAFETY: ftruncate on an fd this process owns.
    let rc = unsafe { libc::ftruncate(fds[0].as_raw_fd(), 0) };
    let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
    eprintln!("role__truncate_clock: ftruncate(clock fd, 0) rc={rc} errno={errno}");
    std::process::exit(if rc == 0 { 10 } else { 11 });
}
