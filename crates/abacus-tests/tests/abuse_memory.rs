//! L4: abuse through the shared memory itself: registry storms, memfd shrinking, hostile
//! mappings, and garbage in interlock and clock words. The pass condition is always the
//! same: the daemon is alive, a well-behaved client in a fresh process can create a timer
//! and wait on it, and afterwards the daemon's idle CPU is normal. Ignored by default; run
//! with `-- --ignored`.
//!
//! Anything that can shrink a memfd runs entirely in role children: a shrunk memfd SIGBUSes
//! every process that maps it, and this test process must survive to report.

#![allow(non_snake_case)]

use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::{Duration, Instant};

use abacus_client::{AbacusClient, Interlock, WaitState, WatchedWord};
use abacus_core::error::{AllocationStep, Condition};
use abacus_core::interlock::{interlock_map, SENTINEL};
use abacus_tests::{
    abacus_binary, attach_words, attach_words_readonly, describe_exit, interlock_words, role_args,
    role_command, run_child, serialized, wait_child, wait_for, ProcessDaemon, RawClient,
    RawResponse, Rng, TIER_WAIT_CRON,
};

const STORM_CLIENTS: usize = 100;
const GARBAGE_ROUNDS: usize = 64;
const SEED_GARBAGE: u64 = 0x5EED_0A1B_C0DE_0003;
/// How long the scribbler races WaitCron creates.
const SCRIBBLE_RACE: Duration = Duration::from_millis(300);
/// Names the holder role creates and the attacker role shrinks, plus the clock.
const HOLDER_NAMES: [&str; 5] = ["h-il", "h-timer", "h-counter", "h-cron", "h-barrier"];

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
            "[{stage}] {e}; daemon alive={alive}; cpu over last 1 s={ticks} ticks{}; last stderr lines:\n{}",
            health_hint(alive, ticks),
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
#[ignore = "role: process entry point for the abuse health checks in abuse_memory"]
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

/// Role: `args = [socket, hold_ms]`. Create one interlock of every tier under HOLDER_NAMES
/// and hold them alive for hold_ms with their touch threads running. Not a test; spawned by
/// `abuse__attach_and_ftruncate_every_interlock`.
#[test]
#[ignore = "role: process entry point for abuse__attach_and_ftruncate_every_interlock"]
fn role__holder() {
    let Some(args) = role_args("role__holder") else {
        return;
    };
    let mut client = AbacusClient::connect(Path::new(&args[0])).expect("connect");
    let hold_ms: u64 = args[1].parse().expect("hold_ms");
    let _il = client.create_interlock("h-il").expect("create h-il");
    let _timer = client.create_wait_timer("h-timer").expect("create h-timer");
    let _counter = client
        .create_wait_counter("h-counter", "h-il", WatchedWord::OpenCount)
        .expect("create h-counter");
    let _cron = client
        .create_wait_cron("h-cron", 10)
        .expect("create h-cron");
    let _barrier = client
        .create_wait_barrier(
            "h-barrier",
            vec![("h-il".to_string(), WatchedWord::ClosedCount, 1)],
        )
        .expect("create h-barrier");
    // The hold is the stimulus: touch threads keep touching shrunk mappings.
    thread::sleep(Duration::from_millis(hold_ms));
}

/// Role: `args = [socket, comma-separated names]`. Attach to each name over the raw wire,
/// never map it, and `ftruncate(fd, 0)`. Prints one `TRUNCATE <name> ret=<r> errno=<e>` line
/// per name. Not a test; spawned by `abuse__attach_and_ftruncate_every_interlock`.
#[test]
#[ignore = "role: process entry point for abuse__attach_and_ftruncate_every_interlock"]
fn role__truncate_attacker() {
    let Some(args) = role_args("role__truncate_attacker") else {
        return;
    };
    let sock = Path::new(&args[0]);
    // Leading newline: libtest under --nocapture leaves the cursor after "test ... ".
    println!();
    for name in args[1].split(',') {
        let attached = RawClient::connect(sock)
            .map_err(|e| format!("connect: {e}"))
            .and_then(|mut raw| raw.attach(name));
        let mut fds = match attached {
            Ok((RawResponse::Attached { .. }, fds)) if fds.len() == 1 => fds,
            Ok((resp, fds)) => {
                println!(
                    "TRUNCATE {name} attach-unexpected {resp:?} fds={}",
                    fds.len()
                );
                continue;
            }
            Err(e) => {
                // A daemon already dead from an earlier shrink shows up here.
                println!("TRUNCATE {name} attach-failed {e}");
                continue;
            }
        };
        let fd = fds.remove(0);
        // SAFETY: ftruncate on an fd we own; no mapping of it exists in this process.
        let ret = unsafe { libc::ftruncate(fd.as_raw_fd(), 0) };
        let errno = if ret == 0 {
            0
        } else {
            std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
        };
        println!("TRUNCATE {name} ret={ret} errno={errno}");
    }
}

/// CONTRACTS.md name collision: STORM_CLIENTS clients racing to create one name leave exactly
/// one live entry; every other creator's handle reads terminated.
#[test]
#[ignore = "abuse: run with --ignored"]
fn abuse__name_collision_storm() {
    let _serial = serialized();
    let mut d = ProcessDaemon::start(&bin(), "storm");
    let sock = d.socket_path().to_path_buf();
    let threads: Vec<_> = (0..STORM_CLIENTS)
        .map(|i| {
            let sock = sock.clone();
            thread::spawn(move || -> Interlock {
                let mut c = AbacusClient::connect(&sock)
                    .unwrap_or_else(|e| panic!("client {i} connect failed: {e}"));
                c.create_interlock("storm")
                    .unwrap_or_else(|e| panic!("client {i} create failed: {e}"))
            })
        })
        .collect();
    let handles: Vec<Interlock> = threads
        .into_iter()
        .map(|t| t.join().expect("storm thread panicked"))
        .collect();
    let terminated = |h: &Interlock| h.peek() == (SENTINEL, SENTINEL);
    let count_terminated = || handles.iter().filter(|h| terminated(h)).count();
    wait_for(Duration::from_secs(1), Duration::from_millis(5), || {
        count_terminated() == STORM_CLIENTS - 1
    })
    .unwrap_or_else(|e| {
        panic!(
            "expected {} terminated handles, observed {}: {e}",
            STORM_CLIENTS - 1,
            count_terminated()
        )
    });
    let live = handles.iter().filter(|h| !terminated(h)).count();
    assert_eq!(live, 1, "{live} live handles after the storm");
    let registry_view = attach_words(&sock, "storm");
    let words = interlock_words(&registry_view);
    assert!(
        words.0 != SENTINEL && words.1 != SENTINEL && words.2 != SENTINEL,
        "the registry's 'storm' entry is terminated: {words:?}"
    );
    assert_daemon_healthy(&mut d, "after name collision storm");
    drop(handles);
}

/// A holder process creates one interlock of every tier; an attacker attaches to each
/// of them and to the clock and calls `ftruncate(fd, 0)`. Every call fails with EPERM
/// (sealed memfd), the daemon stays alive, the holder exits cleanly, and a fresh client is
/// served.
#[test]
#[ignore = "abuse: run with --ignored"]
fn abuse__attach_and_ftruncate_every_interlock() {
    let _serial = serialized();
    let mut d = ProcessDaemon::start(&bin(), "ftruncate");
    let sock = d.socket_path().to_path_buf();
    let sock_s = sock.to_str().expect("utf8 socket path");
    let mut holder = role_command("role__holder", &[sock_s, "3000"])
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn holder");
    // Wait for the holder's last create, attaching over the raw wire without mapping.
    wait_for(Duration::from_secs(3), Duration::from_millis(5), || {
        RawClient::connect(&sock)
            .ok()
            .and_then(|mut r| r.attach("h-barrier").ok())
            .map(|(resp, _)| matches!(resp, RawResponse::Attached { .. }))
            .unwrap_or(false)
    })
    .unwrap_or_else(|e| panic!("holder never created its interlocks: {e}"));

    let mut names: Vec<&str> = vec!["clock"];
    names.extend_from_slice(&HOLDER_NAMES);
    let names = names.join(",");
    let attacker = role_command("role__truncate_attacker", &[sock_s, &names])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn attacker");
    let run = run_child(attacker, Duration::from_secs(5));
    let lines: Vec<&str> = run
        .stdout
        .lines()
        .filter_map(|l| l.find("TRUNCATE ").map(|i| &l[i..]))
        .collect();

    // The daemon touches the clock every cycle; if a shrink got through it dies now.
    let died = wait_for(Duration::from_millis(300), Duration::from_millis(5), || {
        !d.is_alive()
    })
    .is_ok();
    let health = well_behaved_client(&sock);
    let holder_status = wait_child(&mut holder, Duration::from_secs(5));
    let holder_desc = holder_status
        .as_ref()
        .map(describe_exit)
        .unwrap_or_else(|e| e.clone());
    let all_eperm = lines.len() == HOLDER_NAMES.len() + 1
        && lines
            .iter()
            .all(|l| l.ends_with(&format!("errno={}", libc::EPERM)));
    assert!(
        all_eperm,
        "ftruncate on attached memfds was not refused with EPERM ({}):\n{}\nattacker {}: {}\ndaemon died={died}; health={health:?}; holder {holder_desc}",
        libc::EPERM,
        lines.join("\n"),
        describe_exit(&run.status),
        run.stderr.trim()
    );
    assert!(
        !died,
        "daemon died after an attacher shrank a memfd; last stderr lines:\n{}",
        tail(d.stderr_lines(), 20)
    );
    health.unwrap_or_else(|e| panic!("after the shrink attempt: {e}"));
    let hs = holder_status.unwrap_or_else(|e| panic!("holder: {e}"));
    assert!(
        hs.success(),
        "holder process ended with {} (SIGBUS on a shrunk mapping)",
        describe_exit(&hs)
    );
    assert_daemon_idle(&mut d, "after the shrink attempt");
}

/// CONTRACTS.md interlock shape: a client remapping its own copy of the clock with PROT_NONE
/// changes nothing for anyone else. The daemon keeps writing and a fresh client's timer fires.
#[test]
#[ignore = "abuse: run with --ignored"]
fn abuse__attach_and_mmap_prot_none_then_daemon_still_writes() {
    let _serial = serialized();
    let mut d = ProcessDaemon::start(&bin(), "prot-none");
    let mut raw = RawClient::connect(d.socket_path()).expect("raw connect");
    let (resp, mut fds) = raw.attach("clock").expect("attach clock");
    assert!(
        matches!(resp, RawResponse::Attached { .. }) && fds.len() == 1,
        "attach clock: {resp:?} with {} fds",
        fds.len()
    );
    let fd = fds.remove(0);
    // SAFETY: a fresh PROT_NONE shared mapping of an fd we own, unmapped below.
    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            24,
            libc::PROT_NONE,
            libc::MAP_SHARED,
            fd.as_raw_fd(),
            0,
        )
    };
    assert!(
        ptr != libc::MAP_FAILED,
        "PROT_NONE mmap of the clock failed: {}",
        std::io::Error::last_os_error()
    );
    let client = d.client();
    let t1 = client.clock().now_ms();
    wait_for(Duration::from_millis(200), Duration::from_millis(1), || {
        client.clock().now_ms() > t1
    })
    .unwrap_or_else(|e| {
        panic!("clock stopped advancing past {t1} with a PROT_NONE mapping held: {e}")
    });
    assert_daemon_healthy(&mut d, "with a PROT_NONE mapping of the clock held");
    // SAFETY: unmapping exactly what we mapped.
    unsafe {
        libc::munmap(ptr, 24);
    }
}

/// LIFECYCLE.md daemon evaluation: whatever a client writes into an interlock's three words
/// (zero, SENTINEL, near-SENTINEL, high bits, seeded random) the daemon treats it as a value
/// or a termination; it never crashes and other interlocks keep being evaluated (a bystander
/// cron keeps firing).
#[test]
#[ignore = "abuse: run with --ignored"]
fn abuse__attach_and_write_garbage_to_all_words() {
    let _serial = serialized();
    let mut d = ProcessDaemon::start(&bin(), "garbage");
    let mut client = d.client();
    let victim = client.create_interlock("victim").expect("create victim");
    let _bystander = client
        .create_wait_cron("bystander", 10)
        .expect("create bystander cron");
    let bystander_view = attach_words(d.socket_path(), "bystander");
    let victim_view = attach_words(d.socket_path(), "victim");
    let mut rng = Rng::from_env(SEED_GARBAGE);
    let mut values = vec![0u64, u64::MAX, u64::MAX - 1, 1 << 63, 1 << 32, 1];
    for _ in 0..GARBAGE_ROUNDS {
        values.push(rng.next_u64());
    }
    let w = victim_view.words();
    for v in &values {
        w.open_count.store(*v, Ordering::Release);
        w.closed_count.store(rng.next_u64(), Ordering::Release);
        w.expiration_ns
            .store(*v ^ rng.next_u64(), Ordering::Release);
    }
    assert!(
        d.is_alive(),
        "daemon died after garbage writes (seed {}); last stderr lines:\n{}",
        rng.seed(),
        tail(d.stderr_lines(), 20)
    );
    let fired_before = interlock_words(&bystander_view).1;
    wait_for(Duration::from_millis(200), Duration::from_millis(1), || {
        interlock_words(&bystander_view).1 > fired_before
    })
    .unwrap_or_else(|e| {
        panic!(
            "bystander cron stopped firing after garbage writes to another interlock: {e}; bystander={:?} (seed {})",
            interlock_words(&bystander_view),
            rng.seed()
        )
    });
    println!(
        "victim words after garbage: {:?}; victim.state()={:?}; seed {}",
        interlock_words(&victim_view),
        victim.state(),
        rng.seed()
    );
    assert_daemon_healthy(&mut d, "after garbage writes");
}

/// LIFECYCLE.md clock advancement: the clock memfd is world-writable by design. A client
/// scribbling on all three words must not crash the daemon, and timers in a fresh client
/// still fire because the daemon evaluates against its own monotonic read, not the word.
/// Observed side effects are printed and reported, not asserted: the daemon rewrites
/// F_SEAL_FUTURE_WRITE on the clock memfd prevents clients from mapping it writable.
/// A writable mmap attempt returns EPERM. The daemon keeps serving.
#[test]
#[ignore = "abuse: run with --ignored"]
fn abuse__attach_clock_and_write_to_it() {
    let _serial = serialized();
    let mut d = ProcessDaemon::start(&bin(), "clock-scribble");
    let mut raw = RawClient::connect(d.socket_path()).expect("raw connect");
    let (resp, fds) = raw.attach("clock").expect("attach clock");
    assert!(
        matches!(resp, RawResponse::Attached { .. }),
        "unexpected {resp:?}"
    );
    assert_eq!(fds.len(), 1);
    let r = interlock_map(fds.into_iter().next().unwrap());
    assert!(
        matches!(
            r,
            Err(Condition::AllocationFailed {
                step: AllocationStep::Mmap,
                ..
            })
        ),
        "writable mmap of clock fd should fail with EPERM: {r:?}"
    );
    let view = attach_words_readonly(d.socket_path(), "clock");
    let (open, _, _) = interlock_words(&view);
    assert!(open > 0, "clock is advancing");
    assert_daemon_serves(&mut d, "after failed clock scribble");
    assert_daemon_idle(&mut d, "after failed clock scribble");
}

/// With F_SEAL_FUTURE_WRITE on the clock, a client cannot get a writable mapping to
/// race cron creates with scribbled clock values. The checked arithmetic in the cron create
/// path guards against overflow even if the seal were absent.
#[test]
#[ignore = "abuse: run with --ignored"]
fn abuse__clock_scribble_racing_cron_create_does_not_crash_daemon() {
    let _serial = serialized();
    let mut d = ProcessDaemon::start(&bin(), "clock-race");
    let mut raw_attach = RawClient::connect(d.socket_path()).expect("raw connect");
    let (_, fds) = raw_attach.attach("clock").expect("attach clock");
    let r = interlock_map(fds.into_iter().next().unwrap());
    assert!(
        r.is_err(),
        "writable mmap of clock fd should fail (F_SEAL_FUTURE_WRITE): {r:?}"
    );
    let mut raw = RawClient::connect(d.socket_path()).expect("raw connect");
    let mut created = 0usize;
    let t0 = Instant::now();
    while t0.elapsed() < SCRIBBLE_RACE {
        match raw.create(
            &format!("cron-{created}"),
            TIER_WAIT_CRON,
            None,
            Some(10_000_000),
            None,
        ) {
            Ok((RawResponse::Created { .. }, _)) => created += 1,
            Ok((RawResponse::Error { .. }, _)) => break,
            Ok((resp, _)) => panic!("unexpected {resp:?}"),
            Err(e) => panic!("cron create failed: {e}"),
        }
    }
    assert!(
        d.is_alive(),
        "daemon died during cron flood ({created} created)"
    );
    println!("clock race (seal prevents scribble): {created} crons created, daemon alive");
    assert_daemon_healthy(&mut d, "after sealed clock race");
}
