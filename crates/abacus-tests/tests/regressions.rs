//! Regression tests, in-process. Each test names the contract it guards. Process-level
//! tests live in crates/abacus-daemon/tests/.

use std::time::{Duration, Instant};

use abacus_client::{
    AbacusClient, SdkError, WaitState, WatchedWord, DEFAULT_TOUCH_INTERVAL_MS, MIN_FATAL_MARGIN_MS,
    MIN_TOUCH_TTL_MS,
};
use abacus_core::error::{IoOperation, TransportError};
use abacus_tests::{unique_name, wait_for, RawClient, ThreadDaemon};
use abacus_wire::{Request, Response, ERR_INVALID_REQUEST};

/// WaitCron fires on the grid and does not drift.
#[test]
fn cron_fires_on_grid_without_drift() {
    let d = ThreadDaemon::start("cron-grid");
    let mut client = AbacusClient::connect(d.socket_path(), &unique_name("pc"), &[]).unwrap();
    let cron = client.create_wait_cron("c10", 10).unwrap();
    assert_eq!(cron.interval_ms(), 10);

    let mut fires = Vec::with_capacity(50);
    let t0 = Instant::now();
    for _ in 0..50 {
        let r = cron.wait().unwrap();
        // An off-grid fire is reported as Overrun, never as Normal.
        if r.completed_at % 10 == 0 {
            assert_eq!(
                r.state,
                WaitState::Normal,
                "on-grid fire at {}",
                r.completed_at
            );
        } else {
            assert_eq!(
                r.state,
                WaitState::Overrun,
                "off-grid fire at {}",
                r.completed_at
            );
        }
        fires.push(r.completed_at);
    }
    let span = fires[49] - fires[0];
    assert!(
        (480..=500).contains(&span),
        "grid drifted: span {span} ms over 49 intervals"
    );
    assert!(
        t0.elapsed() < Duration::from_millis(700),
        "50 waits took {:?}",
        t0.elapsed()
    );
}

/// The margin has a floor.
#[test]
fn timer_margin_has_a_floor() {
    let d = ThreadDaemon::start("rts-timeout");
    let mut client = AbacusClient::connect(d.socket_path(), &unique_name("pc"), &[]).unwrap();
    assert_eq!(client.min_fatal_margin_ms(), MIN_FATAL_MARGIN_MS);
    let timer = client.create_wait_timer("t-err").unwrap();
    assert_eq!(timer.margin_for(5), MIN_FATAL_MARGIN_MS + 5);
    assert_eq!(timer.margin_for(50), MIN_FATAL_MARGIN_MS + 50);
    assert_eq!(timer.margin_for(200), 400);
    // A margin that cannot work is rejected up front.
    assert!(matches!(
        timer.wait_ms_with_margin(5, 5),
        Err(SdkError::InvalidRequest { .. })
    ));
}

/// Abort policy: the default policy aborts the process. Runs the waiter in a child.
#[test]
fn timer_abort_policy_aborts_the_process() {
    if std::env::var("ABACUS_CHILD_ABORT").is_ok() {
        // Child: create a timer, stop the daemon, wait. Never returns.
        let mut d = ThreadDaemon::start("abort-child");
        let mut client = AbacusClient::connect(d.socket_path(), &unique_name("pc"), &[]).unwrap();
        let timer = client.create_wait_timer("t-abort").unwrap();
        d.stop();
        let _ = timer.wait_ms(5);
        std::process::exit(3);
    }
    let exe = std::env::current_exe().unwrap();
    let output = std::process::Command::new(exe)
        .args([
            "timer_abort_policy_aborts_the_process",
            "--exact",
            "--nocapture",
        ])
        .env("ABACUS_CHILD_ABORT", "1")
        .output()
        .unwrap();
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(
        output.status.signal(),
        Some(libc::SIGABRT),
        "child status {:?}, stderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("DeliveryTimeout"), "stderr: {stderr}");
}

/// wait_ms(0) returns at once without arming.
#[test]
fn timer_wait_zero_returns_immediately() {
    let d = ThreadDaemon::start("wait-zero");
    let mut client = AbacusClient::connect(d.socket_path(), &unique_name("pc"), &[]).unwrap();
    let timer = client.create_wait_timer("t0").unwrap();
    let before = timer.peek();
    let t0 = Instant::now();
    let r = timer.wait_ms(0).unwrap();
    assert!(t0.elapsed() < Duration::from_millis(2));
    assert_eq!(r.state, WaitState::Normal);
    assert_eq!(timer.peek(), before, "wait_ms(0) arms nothing");
}

/// the daemon rejects every invalid create, with InvalidRequest at the cap.
#[test]
fn daemon_rejects_invalid_requests() {
    let config = abacus_daemon::daemon::DaemonConfig {
        max_interlocks: 3,
        ..abacus_daemon::daemon::DaemonConfig::new(abacus_tests::test_socket_path("invalid"))
    };
    let d = ThreadDaemon::start_with("invalid", config);
    let mut raw = RawClient::connect(d.socket_path()).expect("connect");

    let expect_invalid = |raw: &mut RawClient, what: &str| {
        let (resp, fds) = raw.recv_response().unwrap();
        assert!(fds.is_empty());
        match resp {
            Response::Error { code, .. } => assert_eq!(code, ERR_INVALID_REQUEST, "{what}"),
            other => panic!("{what}: expected InvalidRequest, got {other:?}"),
        }
    };
    let create = |name: &str, tier: u8, wn: Option<&str>, ww: Option<u8>, ival: Option<u64>| {
        Request::CreateInterlock {
            name: name.into(),
            tier,
            owner: None,
            dependencies: vec![],
            watched_name: wn.map(String::from),
            watched_word: ww,
            interval_ns: ival,
            conditions: None,
        }
    };

    raw.send_create("clock", 0, None, None, None).unwrap();
    expect_invalid(&mut raw, "create clock");
    raw.send_create("x", 5, None, None, None).unwrap();
    expect_invalid(&mut raw, "tier 5");
    raw.send_request(&create("w", 1, Some("clock"), Some(2), None))
        .unwrap();
    expect_invalid(&mut raw, "watched_word 2");
    raw.send_request(&create("c", 3, None, None, Some(0)))
        .unwrap();
    expect_invalid(&mut raw, "cron interval 0");
    raw.send_request(&create("c", 3, None, None, Some(500_000)))
        .unwrap();
    expect_invalid(&mut raw, "cron interval 500us");
    raw.send_create(&"n".repeat(256), 0, None, None, None)
        .unwrap();
    expect_invalid(&mut raw, "name over 255");
    raw.send_create("", 0, None, None, None).unwrap();
    expect_invalid(&mut raw, "empty name");

    // Missing watched name: InterlockNotFound.
    raw.send_request(&create("w", 1, Some("nope"), Some(0), None))
        .unwrap();
    match raw.recv_response().unwrap().0 {
        Response::Error { code, .. } => assert_eq!(code, abacus_wire::ERR_INTERLOCK_NOT_FOUND),
        other => panic!("expected InterlockNotFound, got {other:?}"),
    }

    // A tier-1 frame with no watched_name cannot be encoded by a conforming client; hand
    // build one so the daemon's decoder sees a truncated payload and closes the connection.
    // v2: version, tag, name("w"), tier(1), owner_flag(0), dep_count(0), then truncated.
    let short = vec![abacus_wire::PROTOCOL_VERSION, 0x01, 1, 0, b'w', 1, 0, 0, 0];
    let mut hostile = RawClient::connect(d.socket_path()).expect("connect");
    hostile.send_frame(&short).unwrap();
    match hostile.recv_response() {
        Ok((Response::Error { code, .. }, _)) => assert_eq!(code, ERR_INVALID_REQUEST),
        Err(TransportError::ConnectionClosed) => {}
        other => panic!("expected InvalidRequest or close, got {other:?}"),
    }
    assert!(
        hostile.is_closed_by_peer(),
        "protocol fault closes the connection"
    );

    // The cap is 3. Connect creates a ProcessClock (1 entry), leaving room for 2 more.
    let mut client = AbacusClient::connect(d.socket_path(), &unique_name("pc"), &[]).unwrap();
    let _a = client.create_interlock("a").unwrap();
    let _b = client.create_interlock("b").unwrap();
    match client.create_interlock("cc") {
        Err(SdkError::InvalidRequest { message }) => {
            assert!(message.contains("limit"), "{message}")
        }
        other => panic!("expected limit error, got {:?}", other.map(|_| ())),
    }
    let _a2 = client.create_interlock("a").unwrap();
}

/// two clients on one daemon; B watches A's interlock.
#[test]
fn multiple_clients_share_interlocks() {
    let d = ThreadDaemon::start("multi-client");
    let mut a = AbacusClient::connect(d.socket_path(), &unique_name("pc"), &[]).unwrap();
    let mut b = AbacusClient::connect(d.socket_path(), &unique_name("pc"), &[]).unwrap();
    let src = a.create_interlock("shared-src").unwrap();
    let attached = b.attach_interlock("shared-src").unwrap();
    let counter = b
        .create_wait_counter("b-watch", "shared-src", WatchedWord::OpenCount)
        .unwrap();
    src.open(4).unwrap();
    let r = counter.wait_until(4, 500).unwrap();
    assert_eq!(r.completed_at, 4);
    assert_eq!(attached.peek(), (4, 0));
    attached.close(4).unwrap();
    assert_eq!(src.value(), 0);
    assert!(a.is_connected() && b.is_connected());
}

/// an attacher's free() cannot be undone by the owner's increment.
#[test]
fn attacher_free_cannot_be_undone_by_owner_open() {
    let d = ThreadDaemon::start("sentinel-inc");
    let mut client = AbacusClient::connect(d.socket_path(), &unique_name("pc"), &[]).unwrap();
    let owner = client.create_interlock("s2").unwrap();
    let attached = client.attach_interlock("s2").unwrap();
    attached.free();
    // Before the daemon's next cycle, the owner increments.
    assert_eq!(owner.open(1), Err(SdkError::InterlockReaped));
    assert_eq!(owner.peek().0, u64::MAX, "sentinel restored");
    assert_eq!(attached.open(7), Err(SdkError::InterlockReaped));
    assert_eq!(attached.peek().0, u64::MAX);
    // A wrap from just below the sentinel is caught too.
    let big = client.create_interlock("s2-wrap").unwrap();
    big.open(u64::MAX - 1).unwrap();
    assert_eq!(big.open(5), Err(SdkError::InterlockReaped));
    assert_eq!(big.peek().0, u64::MAX);
}

/// a WaitCounter dies with its target and does not follow a recreated name.
#[test]
fn counter_dies_with_its_target() {
    let d = ThreadDaemon::start("target-dies");
    let mut client = AbacusClient::connect(d.socket_path(), &unique_name("pc"), &[]).unwrap();
    let mut x = client.create_interlock("x").unwrap();
    let counter = client
        .create_wait_counter("wx", "x", WatchedWord::ClosedCount)
        .unwrap();
    let waiter = std::thread::spawn(move || {
        let t0 = Instant::now();
        (counter.wait_until(10, 5000), t0.elapsed())
    });
    std::thread::sleep(Duration::from_millis(20));
    x.free();
    let (r, elapsed) = waiter.join().unwrap();
    assert_eq!(r, Err(SdkError::InterlockReaped));
    assert!(elapsed < Duration::from_millis(40), "took {elapsed:?}");

    // Recreate x: a counter created before the recreation would have died; a new one works.
    let x2 = client.create_interlock("x").unwrap();
    let c2 = client
        .create_wait_counter("wx", "x", WatchedWord::ClosedCount)
        .unwrap();
    x2.close(10).unwrap();
    assert_eq!(c2.wait_until(10, 500).unwrap().completed_at, 10);
    // Now recreate x again under the same name: c2 must not retarget.
    let _x3 = client.create_interlock("x").unwrap();
    wait_for(Duration::from_millis(50), Duration::from_millis(1), || {
        c2.is_reaped()
    })
    .unwrap();
    assert_eq!(c2.wait_until(20, 100), Err(SdkError::InterlockReaped));
}

/// the keepalive's TTL survives a 100 ms stall.
#[test]
fn touch_ttl_survives_100ms_stall() {
    let d = ThreadDaemon::start("ttl-stall");
    let mut client = AbacusClient::connect(d.socket_path(), &unique_name("pc"), &[]).unwrap();
    let il = client.create_interlock("stall").unwrap();
    let now = abacus_core::clock::monotonic_now_nanos();
    let exp = il.expiration_ns();
    let margin_ms = (exp - now) / 1_000_000;
    assert!(
        margin_ms >= MIN_TOUCH_TTL_MS - 5,
        "TTL after first touch: {margin_ms} ms"
    );
    assert!(margin_ms >= DEFAULT_TOUCH_INTERVAL_MS * 4);
    // Stop touching for 100 ms: still alive.
    let mut il = il;
    il.stop_touch_thread();
    std::thread::sleep(Duration::from_millis(100));
    assert!(!il.is_reaped());
    assert!(il.touch(300).is_ok());
    // Custom cadence and TTL.
    let reaped = il
        .start_touch_thread(10, 400)
        .expect("start_touch_thread")
        .is_reaped();
    assert!(!reaped);
    assert!(il.expiration_ns() >= abacus_core::clock::monotonic_now_nanos() + 390_000_000);
}

/// bounded waits time out; the plain variants block while alive.
#[test]
fn bounded_waits_time_out() {
    let d = ThreadDaemon::start("wait-for");
    let mut client = AbacusClient::connect(d.socket_path(), &unique_name("pc"), &[]).unwrap();
    let il = client.create_interlock("never").unwrap();
    let t0 = Instant::now();
    assert_eq!(
        il.wait_open_for(1, Duration::from_millis(30)).unwrap(),
        None
    );
    assert_eq!(
        il.wait_close_for(1, Duration::from_millis(30)).unwrap(),
        None
    );
    assert!(t0.elapsed() >= Duration::from_millis(60));
    assert!(t0.elapsed() < Duration::from_millis(200));
    il.open(2).unwrap();
    assert_eq!(
        il.wait_open_for(1, Duration::from_millis(30)).unwrap(),
        Some(2)
    );
    let attached = client.attach_interlock("never").unwrap();
    assert_eq!(
        attached
            .wait_close_for(1, Duration::from_millis(10))
            .unwrap(),
        None
    );
    let clock = client.clock();
    let target = clock.now_ms() + 5;
    assert!(
        clock
            .wait_open_for(target, Duration::from_millis(200))
            .unwrap()
            .unwrap()
            >= target
    );
    assert_eq!(
        clock
            .wait_close_for(u64::MAX / 2, Duration::from_millis(5))
            .unwrap(),
        None
    );
}

/// a daemon that never answers makes create fail with ETIMEDOUT.
#[test]
fn client_transport_times_out_against_a_silent_listener() {
    let path = abacus_tests::test_socket_path("silent");
    let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
    let keep = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        std::thread::sleep(Duration::from_millis(600));
        drop(stream);
    });
    let t0 = Instant::now();
    let err = AbacusClient::connect_with_timeout(&path, "pc", &[], Duration::from_millis(200))
        .err()
        .expect("must time out");
    assert!(t0.elapsed() >= Duration::from_millis(200));
    assert!(t0.elapsed() < Duration::from_millis(500));
    assert_eq!(
        err,
        SdkError::Transport(TransportError::Io {
            operation: IoOperation::RecvMsg,
            errno: libc::ETIMEDOUT
        })
    );
    keep.join().unwrap();
    let _ = std::fs::remove_file(&path);
}

/// is_reaped on every owning type.
#[test]
fn is_reaped_on_every_owning_type() {
    let d = ThreadDaemon::start("is-reaped");
    let mut client = AbacusClient::connect(d.socket_path(), &unique_name("pc"), &[]).unwrap();
    let mut il = client.create_interlock("r-il").unwrap();
    let mut wc = client
        .create_wait_counter("r-wc", "r-il", WatchedWord::OpenCount)
        .unwrap();
    let mut wt = client.create_wait_timer("r-wt").unwrap();
    let mut cr = client.create_wait_cron("r-cr", 10).unwrap();
    let mut br = client
        .create_wait_barrier(
            "r-br",
            vec![("r-il".to_string(), WatchedWord::OpenCount, 1)],
        )
        .unwrap();
    assert!(
        !il.is_reaped()
            && !wc.is_reaped()
            && !wt.is_reaped()
            && !cr.is_reaped()
            && !br.is_reaped()
            && !client.process_clock().is_reaped()
    );
    wt.free();
    cr.free();
    assert!(wt.is_reaped() && cr.is_reaped());
    // Free the target: the counter and barrier die on the daemon's next cycle.
    il.free();
    wait_for(Duration::from_millis(50), Duration::from_millis(1), || {
        wc.is_reaped() && br.is_reaped()
    })
    .unwrap();
    wc.free();
    br.free();
    // A name claimed by another create: the old handle learns without a failing call.
    let mut a = client.create_interlock("claimed").unwrap();
    let _b = client.create_interlock("claimed").unwrap();
    wait_for(Duration::from_millis(50), Duration::from_millis(1), || {
        a.is_reaped()
    })
    .unwrap();
    a.free();
}

/// one keepalive thread per client, however many handles. Measured in a child process
/// so other tests' threads do not skew the count.
#[test]
fn one_keepalive_thread_per_client() {
    if std::env::var("ABACUS_CHILD_THREADS").is_ok() {
        let d = ThreadDaemon::start("threads-child");
        let mut client = AbacusClient::connect(d.socket_path(), &unique_name("pc"), &[]).unwrap();
        // The keepalive thread starts at connect (bind_process).
        let before = abacus_tests::proc_status_field(std::process::id(), "Threads");
        let handles: Vec<_> = (0..20)
            .map(|i| client.create_interlock(&format!("k{i}")).unwrap())
            .collect();
        let after = abacus_tests::proc_status_field(std::process::id(), "Threads");
        if after != before {
            eprintln!("threads before {before}, after {after}");
            std::process::exit(1);
        }
        if client.keepalive().registered() != 20 {
            eprintln!("registered {}", client.keepalive().registered());
            std::process::exit(2);
        }
        drop(handles);
        if client.keepalive().registered() != 0 {
            eprintln!("registered after drop {}", client.keepalive().registered());
            std::process::exit(3);
        }
        std::process::exit(0);
    }
    let exe = std::env::current_exe().unwrap();
    let output = std::process::Command::new(exe)
        .args(["one_keepalive_thread_per_client", "--exact", "--nocapture"])
        .env("ABACUS_CHILD_THREADS", "1")
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(0),
        "child stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
