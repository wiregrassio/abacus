//! L2: client processes against a real daemon. Cross-process reaping, many clients, a
//! disconnected owner, and the SDK abort path observed from a parent. Child processes are
//! this binary re-executed as a role (see the testkit).

#![allow(non_snake_case)]

mod common;

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use abacus_client::{AbacusClient, InterlockState, SdkError, WaitState};
use abacus_tests::{
    describe_exit, role_args, role_command, wait_child, wait_for, wait_for_value, ProcessDaemon,
};
use common::{bin, remaining};

/// LIFECYCLE.md ProcessClock pattern and CONTRACTS.md TTL rules: when an owning process is
/// SIGKILLed its touch thread dies with it, the TTL lapses (80 ms steady state) and the
/// daemon reaps. From the kill: the attacher's `state()` reads Expired and attach fails with
/// InterlockNotFound, both within 300 ms.
#[test]
fn daemon__client_process_sigkilled_interlock_reaped_within_300ms() {
    let d = ProcessDaemon::start(bin(), "sigkill-reap");
    let sock = d.socket_path().to_str().unwrap().to_string();
    let mut child = role_command("role__hold_interlock", &[&sock, "held", "5000"])
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn role");

    let mut client = d.client();
    let attached = wait_for_value(Duration::from_secs(3), Duration::from_millis(2), || {
        client.attach_interlock("held").ok()
    })
    .expect("child never created the interlock")
    .0;
    assert_ne!(
        attached.state(),
        InterlockState::Expired,
        "interlock expired before the kill"
    );

    child.kill().expect("SIGKILL child");
    let _ = child.wait();
    let t_kill = Instant::now();
    let budget = Duration::from_millis(300);

    let expired = wait_for(budget, Duration::from_millis(1), || {
        attached.state() == InterlockState::Expired
    });
    let (open, closed) = attached.peek();
    assert!(
        expired.is_ok(),
        "interlock still {:?} {} ms after its owner was SIGKILLed: open={open} closed={closed}",
        attached.state(),
        t_kill.elapsed().as_millis()
    );

    let not_found = wait_for(remaining(t_kill, budget), Duration::from_millis(1), || {
        matches!(
            client.attach_interlock("held"),
            Err(SdkError::InterlockNotFound { .. })
        )
    });
    assert!(
        not_found.is_ok(),
        "attach still resolves {} ms after the owner was SIGKILLed: {:?}",
        t_kill.elapsed().as_millis(),
        client.attach_interlock("held").map(|_| ())
    );
}

/// CONTRACTS.md daemon contract at scale: ten client threads, ten WaitTimers each, every
/// `wait_ms(20)` delivered (Normal or Overrun; Timeout would have aborted the process).
#[test]
fn daemon__ten_clients_hundred_interlocks_all_fire() {
    let d = ProcessDaemon::start(bin(), "ten-clients");
    let sock = d.socket_path().to_path_buf();
    let handles: Vec<_> = (0..10)
        .map(|c| {
            let sock: PathBuf = sock.clone();
            std::thread::spawn(move || -> Result<Vec<WaitState>, String> {
                let mut client =
                    AbacusClient::connect(&sock).map_err(|e| format!("client {c} connect: {e}"))?;
                let mut timers = Vec::with_capacity(10);
                for i in 0..10 {
                    timers.push(
                        client
                            .create_wait_timer(&format!("c{c}-t{i}"))
                            .map_err(|e| format!("client {c} create timer {i}: {e}"))?,
                    );
                }
                let mut states = Vec::with_capacity(10);
                for (i, t) in timers.iter().enumerate() {
                    let r = t
                        .wait_ms(20)
                        .map_err(|e| format!("client {c} timer {i} wait_ms: {e}"))?;
                    states.push(r.state);
                }
                Ok(states)
            })
        })
        .collect();
    let mut fired = 0usize;
    for h in handles {
        let states = h
            .join()
            .expect("client thread panicked")
            .unwrap_or_else(|e| panic!("{e}"));
        assert!(
            states
                .iter()
                .all(|s| matches!(s, WaitState::Normal | WaitState::Overrun)),
            "timer states {states:?}"
        );
        fired += states.len();
    }
    assert_eq!(fired, 100, "expected 100 deliveries");
}

/// CONTRACTS.md daemon contract "Connection death (EOF/EPIPE): remove client from table, no
/// reap". The owner's connection drops; its touch thread keeps the interlock alive past the
/// 100 ms creation TTL, and a new connection can attach.
#[test]
fn daemon__client_disconnect_does_not_reap() {
    let d = ProcessDaemon::start(bin(), "disconnect");
    let mut owner = d.client();
    let il = owner.create_interlock("kept").expect("create");
    drop(owner);

    let mut other = d.client();
    let t0 = other.clock().now_ms();
    // Let well over the creation TTL elapse, measured on the daemon's clock.
    wait_for(Duration::from_millis(600), Duration::from_millis(5), || {
        other.clock().now_ms() >= t0 + 250
    })
    .expect("daemon clock did not advance 250 ms");

    let attached = match other.attach_interlock("kept") {
        Ok(a) => a,
        Err(e) => panic!("attach after the owner's disconnect failed: {e}"),
    };
    let state = attached.state();
    assert_ne!(
        state,
        InterlockState::Expired,
        "interlock expired after owner disconnect"
    );
    assert!(il.touch(10).is_ok(), "owner handle reports reaped");
}

/// `wait_ms(0)` with a pending target returns at once with Normal (the fix).
/// The role calls `wait_ms(0)` and exits 0 on success.
#[test]
fn wait_timer__wait_ms_zero_with_pending_target_aborts() {
    let d = ProcessDaemon::start(bin(), "wait-zero");
    let sock = d.socket_path().to_str().unwrap().to_string();
    let mut child = role_command("role__wait_ms_zero", &[&sock])
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn role");
    let status = wait_child(&mut child, Duration::from_secs(3)).expect("role did not end");
    assert!(
        status.success(),
        "wait_ms(0) with a pending target ended with {} (exit 0 expected now fix)",
        describe_exit(&status)
    );
}

// ---------------------------------------------------------------------------
// Roles
// ---------------------------------------------------------------------------

/// Role: connect to `args[0]`, create interlock `args[1]`, hold it (touch thread running)
/// for `args[2]` ms. Not a test; spawned by
/// `daemon__client_process_sigkilled_interlock_reaped_within_300ms`, which SIGKILLs it.
#[test]
#[ignore = "role: process entry point for daemon__client_process_sigkilled_interlock_reaped_within_300ms"]
fn role__hold_interlock() {
    let Some(args) = role_args("role__hold_interlock") else {
        return;
    };
    let mut client = AbacusClient::connect(Path::new(&args[0])).expect("connect");
    let _il = client.create_interlock(&args[1]).expect("create");
    let hold: u64 = args[2].parse().expect("hold ms");
    // The hold is the stimulus: the parent kills this process mid-hold.
    std::thread::sleep(Duration::from_millis(hold));
}

/// Role: connect to `args[0]`, create a timer, wait for the clock to pass the creation
/// stamp, then `wait_ms(0)`. Exits 0 on success (wait_ms(0) returns at once).
/// Not a test; spawned by `wait_timer__wait_ms_zero_with_pending_target_aborts`.
#[test]
#[ignore = "role: process entry point for wait_timer__wait_ms_zero_with_pending_target_aborts"]
fn role__wait_ms_zero() {
    let Some(args) = role_args("role__wait_ms_zero") else {
        return;
    };
    let mut client = AbacusClient::connect(Path::new(&args[0])).expect("connect");
    let timer = client.create_wait_timer("zero").expect("create timer");
    let (created_at, _) = timer.peek();
    wait_for(Duration::from_secs(1), Duration::from_millis(1), || {
        client.clock().now_ms() > created_at
    })
    .expect("clock never advanced past the timer's creation stamp");
    let r = timer.wait_ms(0);
    eprintln!("role__wait_ms_zero: wait_ms(0) returned {r:?}");
    // wait_ms(0) returns at once with Normal; exit 0 confirms success.
    std::process::exit(if r.is_ok() { 0 } else { 3 });
}
