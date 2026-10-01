//! L4: hostile bytes on the socket. Each test is a hostile local client against a real
//! daemon process. The pass condition is always the same: the daemon is alive, a
//! well-behaved client in a fresh process can create a timer and wait on it, and once the
//! hostile connections are gone the daemon's idle CPU is normal. Ignored by default; run
//! with `-- --ignored`.
//!
//! The well-behaved check runs in a role child so a half-wedged daemon (wait_ms aborting on
//! DeliveryTimeout) is a red test, not a dead test binary.

#![allow(non_snake_case)]

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use abacus_client::{AbacusClient, WaitState};
use abacus_tests::{
    abacus_binary, attach_payload, create_payload, create_payload_v2, describe_exit, frame,
    role_args, role_command, serialized, unique_name, wait_child, wait_for, ProcessDaemon,
    RawClient, RawResponse, Rng, ERR_INVALID_REQUEST, TIER_INTERLOCK, TIER_WAIT_BARRIER,
    TIER_WAIT_COUNTER, TIER_WAIT_CRON, TIER_WAIT_TIMER,
};

/// Seeds; override with `ABACUS_TEST_SEED` to replay. Printed on every failure.
const SEED_RANDOM: u64 = 0x5EED_0A1B_C0DE_0001;
const SEED_MUTATE: u64 = 0x5EED_0A1B_C0DE_0002;

/// 25 connections x 400 frames = 10 000 frames per test, each connection's bytes well
/// under the socket buffer so no send blocks on the daemon's one-frame-per-cycle drain.
const CONNECTIONS: usize = 25;
const FRAMES_PER_CONNECTION: usize = 400;
const ZERO_FLOOD_CONNECTIONS: usize = 10;
const ZERO_FLOOD_FRAMES: usize = 1000;

/// Partial-frame hold. Measured: daemon at 100 percent of one core (200 ticks per 2 s)
/// and a second client's create hung past 4 s. Promise: under 1 percent of one core
/// over the hold and the concurrent client completes.
const HOLD: Duration = Duration::from_secs(60);
const HOLD_CPU_LIMIT_TICKS: u64 = 60;

/// Health check budgets: the well-behaved child must finish within HEALTH_DEADLINE; idle CPU
/// after the hostile connections are gone is under IDLE_LIMIT_TICKS over 1 s (review: 1 tick
/// per 3 s idle).
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

/// Daemon alive and serving a fresh client. Usable while hostile connections are attached.
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
#[ignore = "role: process entry point for the abuse health checks in abuse_wire"]
fn role__health_client() {
    let Some(args) = role_args("role__health_client") else {
        return;
    };
    let mut client =
        AbacusClient::connect(Path::new(&args[0]), &unique_name("role"), &[]).expect("connect");
    let timer = client.create_wait_timer("health").expect("create timer");
    let r = timer.wait_ms(5).expect("wait_ms");
    assert!(
        matches!(r.state, WaitState::Normal | WaitState::Overrun),
        "health timer state {:?}",
        r.state
    );
}

/// A healthy daemon drains one frame per connection per cycle, so `frames` frames leave a
/// connection in about `frames` ms; the write timeout is a multiple of that.
const FLOOD_WRITE_TIMEOUT: Duration = Duration::from_secs(2);

/// Open `connections` raw connections, one thread each, and send the bytes `gen` produces
/// for each frame from a per-connection rng seeded `seed ^ connection`. A connection stops
/// early when the daemon closes it (the daemon drops a faulting client) or stops draining
/// it for FLOOD_WRITE_TIMEOUT (a daemon asleep after a burst); both are recorded, not fatal,
/// because the claim is the health check that follows. Every connection must get at least
/// one frame out. Returns the still-open connections.
fn flood(
    sock: &Path,
    seed: u64,
    connections: usize,
    frames: usize,
    gen: fn(&mut Rng, usize, usize) -> Vec<u8>,
) -> Vec<RawClient> {
    let threads: Vec<_> = (0..connections)
        .map(|c| {
            let sock = sock.to_path_buf();
            thread::spawn(move || -> (RawClient, usize, usize, Option<String>) {
                let mut rng = Rng::new(seed ^ (c as u64 + 1));
                let mut raw = RawClient::connect(&sock)
                    .unwrap_or_else(|e| panic!("connection {c} failed: {e} (seed {seed})"));
                raw.set_write_timeout(Some(FLOOD_WRITE_TIMEOUT));
                let mut sent = 0usize;
                let mut bytes_out = 0usize;
                let mut cut_short = None;
                for f in 0..frames {
                    let bytes = gen(&mut rng, c, f);
                    match raw.send_bytes(&bytes) {
                        Ok(()) => {
                            sent += 1;
                            bytes_out += bytes.len();
                        }
                        Err(e)
                            if matches!(
                                e.kind(),
                                std::io::ErrorKind::WouldBlock
                                    | std::io::ErrorKind::BrokenPipe
                                    | std::io::ErrorKind::ConnectionReset
                            ) =>
                        {
                            cut_short = Some(format!("connection {c} after {sent} frames: {e}"));
                            break;
                        }
                        Err(e) => {
                            panic!("send on connection {c} frame {f} failed: {e} (seed {seed})")
                        }
                    }
                }
                (raw, sent, bytes_out, cut_short)
            })
        })
        .collect();
    let mut conns = Vec::with_capacity(connections);
    let mut total_frames = 0usize;
    let mut total_bytes = 0usize;
    let mut cut_short: Vec<String> = Vec::new();
    for (c, t) in threads.into_iter().enumerate() {
        let (raw, sent, bytes_out, cut) = t.join().expect("flood thread panicked");
        assert!(
            sent > 0,
            "connection {c} got no frame out: {} (seed {seed})",
            cut.clone().unwrap_or_default()
        );
        total_frames += sent;
        total_bytes += bytes_out;
        cut_short.extend(cut);
        conns.push(raw);
    }
    println!(
        "flood: {connections} connections, {total_frames} of {} frames, {total_bytes} bytes, seed {seed}; cut short: {}",
        connections * frames,
        if cut_short.is_empty() { "none".to_string() } else { cut_short.join("; ") }
    );
    conns
}

fn random_frame(rng: &mut Rng, _c: usize, _f: usize) -> Vec<u8> {
    match rng.below(10) {
        0..=6 => {
            let len = rng.below(65) as usize;
            frame(&rng.bytes(len))
        }
        7 => {
            let len = 65 + rng.below(960) as usize;
            frame(&rng.bytes(len))
        }
        8 => {
            let len = 1 + rng.below(16) as usize;
            rng.bytes(len)
        }
        _ => {
            let mut v = rng.next_u64().to_le_bytes()[..4].to_vec();
            v.extend(rng.bytes(8));
            v
        }
    }
}

fn mutated_frame(rng: &mut Rng, c: usize, f: usize) -> Vec<u8> {
    let name = format!("m-{c}-{f}");
    let payload = match rng.below(9) {
        0 => create_payload(name.as_bytes(), TIER_INTERLOCK, None, None, None),
        1 => create_payload(
            name.as_bytes(),
            TIER_WAIT_COUNTER,
            Some((b"clock", rng.below(2) as u8)),
            None,
            None,
        ),
        2 => create_payload(name.as_bytes(), TIER_WAIT_TIMER, None, None, None),
        3 => create_payload(
            name.as_bytes(),
            TIER_WAIT_CRON,
            None,
            Some(10_000_000),
            None,
        ),
        4 => create_payload(
            name.as_bytes(),
            TIER_WAIT_BARRIER,
            None,
            None,
            Some(&[(b"clock", 0, 1), (b"clock", 1, 1)]),
        ),
        5 => attach_payload(b"clock"),
        6 => create_payload_v2(
            name.as_bytes(),
            TIER_INTERLOCK,
            Some((b"pc", 42)),
            &[],
            None,
            None,
            None,
        ),
        7 => create_payload_v2(
            name.as_bytes(),
            TIER_INTERLOCK,
            Some((b"pc", 1)),
            &[b"a", b"bb", b"ccc"],
            None,
            None,
            None,
        ),
        _ => attach_payload(name.as_bytes()),
    };
    let mut bytes = frame(&payload);
    let flips = 1 + rng.below(4) as usize;
    for _ in 0..flips {
        let pos = rng.below(bytes.len() as u64) as usize;
        let mask = 1 + rng.below(255) as u8;
        bytes[pos] ^= mask;
    }
    bytes
}

fn zero_frame(_rng: &mut Rng, _c: usize, _f: usize) -> Vec<u8> {
    vec![0, 0, 0, 0]
}

/// CONTRACTS.md wire ABI: the daemon decodes whatever arrives without panicking. 10 000
/// frames of random payload (correctly framed, oversized, unframed, and with random length
/// prefixes) from a seeded rng across CONNECTIONS connections.
#[test]
#[ignore = "abuse: run with --ignored"]
fn abuse__random_bytes_on_the_socket_10k_frames() {
    let _serial = serialized();
    let mut d = ProcessDaemon::start(&bin(), "rand-bytes");
    let seed = Rng::from_env(SEED_RANDOM).seed();
    let conns = flood(
        d.socket_path(),
        seed,
        CONNECTIONS,
        FRAMES_PER_CONNECTION,
        random_frame,
    );
    assert_daemon_serves(&mut d, "during random-byte flood");
    drop(conns);
    assert_daemon_healthy(&mut d, "after random-byte flood");
}

/// CONTRACTS.md wire ABI: valid create and attach frames with 1 to 4 random byte flips each
/// (prefix included) never crash the daemon. 10 000 frames across CONNECTIONS connections.
#[test]
#[ignore = "abuse: run with --ignored"]
fn abuse__mutated_valid_frames_10k() {
    let _serial = serialized();
    let mut d = ProcessDaemon::start(&bin(), "mutated");
    let seed = Rng::from_env(SEED_MUTATE).seed();
    let conns = flood(
        d.socket_path(),
        seed,
        CONNECTIONS,
        FRAMES_PER_CONNECTION,
        mutated_frame,
    );
    assert_daemon_serves(&mut d, "during mutated-frame flood");
    drop(conns);
    assert_daemon_healthy(&mut d, "after mutated-frame flood");
}

/// CONTRACTS.md wire ABI max payload 4096: a length prefix of 0xFFFFFFFF is rejected without
/// allocating. Today the daemon answers InvalidRequest and keeps the connection; it
/// closes the connection. Either is accepted; anything else is not.
#[test]
#[ignore = "abuse: run with --ignored"]
fn abuse__length_prefix_claims_4gb() {
    let _serial = serialized();
    let mut d = ProcessDaemon::start(&bin(), "prefix-4gb");
    let mut raw = RawClient::connect(d.socket_path()).expect("raw connect");
    raw.send_bytes(&[0xFF, 0xFF, 0xFF, 0xFF])
        .expect("send prefix");
    match raw.recv_response_with_fds() {
        Ok((RawResponse::Error { code, message }, fds)) => {
            assert_eq!(
                code, ERR_INVALID_REQUEST,
                "wrong error code; message={message}"
            );
            assert!(fds.is_empty(), "error response carried {} fds", fds.len());
            println!("4 GB prefix: daemon answered InvalidRequest: {message}");
        }
        Err(e) if e.contains("peer closed") => {
            println!("4 GB prefix: daemon closed the connection (protocol-fault behavior)");
        }
        Ok((resp, fds)) => panic!("4 GB prefix accepted: {resp:?} with {} fds", fds.len()),
        Err(e) => panic!("4 GB prefix: no usable response: {e}"),
    }
    let rss = d.vm_rss_kb();
    assert!(
        rss < 64 * 1024,
        "daemon VmRSS {rss} kB after a 4 GB length prefix: it allocated for it"
    );
    drop(raw);
    assert_daemon_healthy(&mut d, "after 4 GB prefix");
}

/// CONTRACTS.md wire ABI: a zero-length frame is a truncated payload, one fault each,
/// ZERO_FLOOD_FRAMES of them on each of ZERO_FLOOD_CONNECTIONS connections.
#[test]
#[ignore = "abuse: run with --ignored"]
fn abuse__zero_length_frame_flood() {
    let _serial = serialized();
    let mut d = ProcessDaemon::start(&bin(), "zero-flood");
    let conns = flood(
        d.socket_path(),
        SEED_RANDOM,
        ZERO_FLOOD_CONNECTIONS,
        ZERO_FLOOD_FRAMES,
        zero_frame,
    );
    assert_daemon_serves(&mut d, "during zero-length flood");
    drop(conns);
    assert_daemon_healthy(&mut d, "after zero-length flood");
}

/// A client that sends 2 of the 4 length-prefix bytes and holds the connection does not
/// stall the daemon: a concurrent well-behaved client completes within HEALTH_DEADLINE and
/// the daemon's CPU over the hold stays under HOLD_CPU_LIMIT_TICKS. Fails fast on the
/// concurrent check; only a green concurrent check goes on to the full HOLD.
#[test]
#[ignore = "abuse: run with --ignored"]
fn abuse__partial_frame_then_hold_60s() {
    let _serial = serialized();
    let mut d = ProcessDaemon::start(&bin(), "partial-hold");
    let mut raw = RawClient::connect(d.socket_path()).expect("raw connect");
    raw.send_bytes(&[7, 0]).expect("send 2 of 4 prefix bytes");

    // Concurrent well-behaved client, bounded by HEALTH_DEADLINE; its thread may block
    // forever on a stalled daemon, which is the finding, so it reports through a channel.
    let sock = d.socket_path().to_path_buf();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(well_behaved_client(&sock));
    });
    let ticks_before = d.cpu_ticks();
    let t0 = Instant::now();
    let concurrent = rx.recv_timeout(HEALTH_DEADLINE);
    let ticks_during = d.cpu_ticks() - ticks_before;
    match concurrent {
        Ok(Ok(())) => {}
        Ok(Err(e)) => panic!(
            "concurrent client failed while a partial frame was held: {e}; daemon CPU {ticks_during} ticks over {:?}",
            t0.elapsed()
        ),
        Err(_) => panic!(
            "concurrent client did not complete within {HEALTH_DEADLINE:?} while a partial frame was held; daemon CPU {ticks_during} ticks over that window (100 ticks = one core-second)"
        ),
    }

    // Concurrent check passed: hold for the full window and measure.
    let remaining = HOLD.saturating_sub(t0.elapsed());
    let ticks_hold = d.cpu_ticks_over(remaining) + ticks_during;
    assert!(
        ticks_hold < HOLD_CPU_LIMIT_TICKS,
        "daemon used {ticks_hold} ticks over the {HOLD:?} partial-frame hold (limit {HOLD_CPU_LIMIT_TICKS})"
    );
    assert_daemon_serves(&mut d, "at the end of the hold");
    drop(raw);
    assert_daemon_healthy(&mut d, "after the partial-frame client left");
}
