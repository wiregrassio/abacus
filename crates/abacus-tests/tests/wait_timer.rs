//! L1: WaitTimer contract (SURFACE.md "WaitTimer", "WaitUntil"; CONTRACTS.md TTL rules,
//! wake outcomes), daemon as a thread.

#![allow(non_snake_case)]

use std::io::Read;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use abacus_client::{AbacusClient, SdkError, WaitState};
use abacus_tests::{
    describe_exit, on_thread, recv_within, role_args, role_command, wait_child, wait_for,
    ThreadDaemon,
};

/// SURFACE.md WaitTimer: wait_ms(W) returns Normal or Overrun after about W, and
/// completed_at is at or past the target the SDK set.
#[test]
fn wait_timer__fires_within_tolerance() {
    let d = ThreadDaemon::start("wt-fires");
    let mut client = d.client();
    let timer = client.create_wait_timer("t1").expect("create timer");
    let t0 = Instant::now();
    let r = timer.wait_ms(50);
    let elapsed = t0.elapsed();
    let r = match r {
        Ok(r) => r,
        Err(e) => panic!("wait_ms(50) failed: {e}; peek={:?}", timer.peek()),
    };
    let (target, _) = timer.peek();
    assert!(
        matches!(r.state, WaitState::Normal | WaitState::Overrun),
        "expected Normal or Overrun, got {r:?}"
    );
    assert!(
        r.completed_at >= target,
        "completed_at {} below target {target}",
        r.completed_at
    );
    assert!(
        elapsed >= Duration::from_millis(45) && elapsed < Duration::from_millis(100),
        "wait_ms(50) took {elapsed:?}"
    );
}

/// SURFACE.md WaitUntil: wait_until(t) is wait_ms(t - clock_now).
#[test]
fn wait_timer__wait_until_absolute() {
    let d = ThreadDaemon::start("wt-until");
    let mut client = d.client();
    let timer = client.create_wait_timer("t-abs").expect("create timer");
    let target = client.clock().now_ms() + 50;
    let t0 = Instant::now();
    let r = timer.wait_until(target);
    let elapsed = t0.elapsed();
    let r = match r {
        Ok(r) => r,
        Err(e) => panic!("wait_until failed: {e}; peek={:?}", timer.peek()),
    };
    assert!(
        matches!(r.state, WaitState::Normal | WaitState::Overrun),
        "expected Normal or Overrun, got {r:?}"
    );
    assert!(
        r.completed_at >= target,
        "completed_at {} below target {target}",
        r.completed_at
    );
    assert_eq!(
        timer.peek().0,
        target,
        "open_count is not the absolute target"
    );
    assert!(
        elapsed >= Duration::from_millis(40) && elapsed < Duration::from_millis(100),
        "wait_until(+50 ms) took {elapsed:?}"
    );
}

/// SURFACE.md WaitUntil: a timestamp already past is wait_ms(0) and returns the current
/// state at once, even with a target already pending. Runs in a child process so a
/// regression that aborts on `ms == 0` fails loudly rather than hanging the test binary.
#[test]
fn wait_timer__wait_until_past_returns_immediately() {
    let d = ThreadDaemon::start("wt-past");
    let sock = d.socket_path().to_str().unwrap().to_string();
    let mut child = role_command("role__wait_until_past", &[&sock])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn role");
    let status = wait_child(&mut child, Duration::from_secs(5)).expect("child did not exit");
    let mut stderr = String::new();
    if let Some(mut e) = child.stderr.take() {
        let _ = e.read_to_string(&mut stderr);
    }
    assert!(
        status.success(),
        "wait_until(past) child ended with {}; stderr:\n{stderr}",
        describe_exit(&status)
    );
    let line = stderr
        .lines()
        .find(|l| l.starts_with("past-wait:"))
        .unwrap_or_else(|| panic!("child printed no past-wait line; stderr:\n{stderr}"));
    let elapsed_us: u64 = line
        .split("elapsed_us=")
        .nth(1)
        .and_then(|s| s.split_whitespace().next())
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("no elapsed_us in {line}"));
    assert!(
        elapsed_us < 5_000,
        "past wait_until took {elapsed_us} us: {line}"
    );
}

/// Role: connect, create a timer, let the clock advance past creation, call wait_until on a
/// past timestamp, and print the result. Not a test; spawned by
/// `wait_timer__wait_until_past_returns_immediately`.
#[test]
#[ignore = "role: process entry point for wait_timer__wait_until_past_returns_immediately"]
fn role__wait_until_past() {
    let Some(args) = role_args("role__wait_until_past") else {
        return;
    };
    let mut client = AbacusClient::connect(Path::new(&args[0])).expect("connect");
    let timer = client.create_wait_timer("past").expect("create timer");
    let created_at = client.clock().now_ms();
    wait_for(Duration::from_millis(200), Duration::from_millis(1), || {
        client.clock().now_ms() > created_at + 1
    })
    .expect("clock did not advance");
    let past = client.clock().now_ms() - 100;
    let t0 = Instant::now();
    let r = timer.wait_until(past);
    eprintln!(
        "past-wait: result={r:?} elapsed_us={}",
        t0.elapsed().as_micros()
    );
    assert!(r.is_ok(), "wait_until(past) returned {r:?}");
}

/// SURFACE.md WaitTimer: two timers on one client fire independently at their own targets.
#[test]
fn wait_timer__multiple_independent() {
    let d = ThreadDaemon::start("wt-multi");
    let mut client = d.client();
    let fast = client.create_wait_timer("fast").expect("create fast");
    let slow = client.create_wait_timer("slow").expect("create slow");
    let t0 = Instant::now();
    let r1 = fast.wait_ms(20).expect("fast wait_ms(20)");
    let elapsed_fast = t0.elapsed();
    let r2 = slow.wait_ms(80).expect("slow wait_ms(80)");
    let elapsed_slow = t0.elapsed();
    assert!(
        matches!(r1.state, WaitState::Normal | WaitState::Overrun),
        "fast {r1:?}"
    );
    assert!(
        matches!(r2.state, WaitState::Normal | WaitState::Overrun),
        "slow {r2:?}"
    );
    assert!(
        r1.completed_at >= fast.peek().0,
        "fast completed_at below target: {r1:?} {:?}",
        fast.peek()
    );
    assert!(
        r2.completed_at >= slow.peek().0,
        "slow completed_at below target: {r2:?} {:?}",
        slow.peek()
    );
    assert!(
        elapsed_fast >= Duration::from_millis(15) && elapsed_fast < Duration::from_millis(60),
        "fast took {elapsed_fast:?}"
    );
    assert!(
        elapsed_slow >= Duration::from_millis(90) && elapsed_slow < Duration::from_millis(200),
        "fast then slow took {elapsed_slow:?} (expected about 20 + 80 ms)"
    );
}

/// SURFACE.md WaitTimer and CONTRACTS.md interlock shape: sequential waits set strictly
/// increasing targets (CAS-max never goes backward) and deliver strictly increasing
/// completed_at values.
#[test]
fn wait_timer__sequential_waits_are_monotonic() {
    let d = ThreadDaemon::start("wt-seq");
    let mut client = d.client();
    let timer = client.create_wait_timer("seq").expect("create timer");
    let t0 = Instant::now();
    let mut targets = Vec::new();
    let mut completed = Vec::new();
    for i in 0..5 {
        let r = timer
            .wait_ms(20)
            .unwrap_or_else(|e| panic!("wait {i} failed: {e}; peek={:?}", timer.peek()));
        targets.push(timer.peek().0);
        completed.push(r.completed_at);
    }
    let elapsed = t0.elapsed();
    for w in targets.windows(2) {
        assert!(w[1] > w[0], "targets not strictly increasing: {targets:?}");
    }
    for w in completed.windows(2) {
        assert!(
            w[1] > w[0],
            "completed_at not strictly increasing: {completed:?}"
        );
    }
    assert!(
        elapsed >= Duration::from_millis(95),
        "five 20 ms waits took {elapsed:?}; targets={targets:?} completed={completed:?}"
    );
}

/// SURFACE.md WaitTimer: a reaped timer returns InterlockReaped and does not abort. A
/// recreate from a second client reaps it mid-wait: the loop checks both words
/// for SENTINEL on every pass and returns InterlockReaped rather than classifying
/// (SENTINEL, SENTINEL) as a delivered wait.
#[test]
fn wait_timer__reaped_returns_error_not_abort() {
    let d = ThreadDaemon::start("wt-reaped");
    let mut client = d.client();
    let timer = Arc::new(client.create_wait_timer("t").expect("create timer"));
    let t = timer.clone();
    let rx = on_thread(move || t.wait_ms(300));
    std::thread::sleep(Duration::from_millis(5)); // ordering only
    let mut client2 = d.client();
    let _replacement = client2.create_wait_timer("t").expect("recreate");
    let r = recv_within(&rx, Duration::from_millis(200)).unwrap_or_else(|e| {
        panic!(
            "waiter never returned after reap: {e}; peek={:?}",
            timer.peek()
        )
    });
    assert!(
        matches!(r, Err(SdkError::InterlockReaped)),
        "reaped timer returned {r:?}; peek={:?}",
        timer.peek()
    );
}
