//! Cross-container check: two processes, normally in two containers that bind-mount the
//! daemon's socket directory, coordinate through one interlock passed to each as a memfd.
//!
//! ```sh
//! xcontainer producer <sock> <prefix> [frames] [period_ms]
//! xcontainer consumer <sock> <prefix> [frames]
//! ```
//!
//! The producer creates `<prefix>/frames` and opens it once per period (`frames` 0 runs until
//! killed). The consumer connects with the producer's clock as a dependency, watches the frame
//! count with a WaitCounter, and prints a line per delivered frame. Both connect with
//! `connect_waiting` (60 s ceiling), so a dead producer or daemon aborts them.

use std::env;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use abacus_client::{AbacusClient, SdkError, WaitState, WatchedWord};

const CONNECT_CEILING: Duration = Duration::from_secs(60);
const WAIT_TIMEOUT_MS: u64 = 1000;

fn main() {
    let args: Vec<String> = env::args().collect();
    let usage = "usage: xcontainer producer|consumer <sock> <prefix> [frames] [period_ms]";
    let role = args.get(1).map(String::as_str).expect(usage);
    let sock = Path::new(args.get(2).map(String::as_str).expect(usage));
    let prefix = args.get(3).map(String::as_str).expect(usage);
    let frames: u64 = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(0);
    let period_ms: u64 = args.get(5).and_then(|s| s.parse().ok()).unwrap_or(30);
    match role {
        "producer" => producer(sock, prefix, frames, period_ms),
        "consumer" => consumer(sock, prefix, frames),
        _ => panic!("{usage}"),
    }
}

fn producer(sock: &Path, prefix: &str, frames: u64, period_ms: u64) {
    let clock = format!("{prefix}/producer");
    let mut client = AbacusClient::connect_waiting(sock, &clock, &[], CONNECT_CEILING)
        .unwrap_or_else(|e| panic!("producer connect: {e}"));
    let frames_il = client
        .create_interlock(&format!("{prefix}/frames"))
        .unwrap_or_else(|e| panic!("create frames: {e}"));
    println!("producer ready: clock {clock}, pid {}", std::process::id());
    let mut n = 0u64;
    while frames == 0 || n < frames {
        thread::sleep(Duration::from_millis(period_ms));
        frames_il
            .open(1)
            .unwrap_or_else(|e| panic!("open frame {}: {e}", n + 1));
        n += 1;
    }
    println!("producer done: {n} frames");
}

fn consumer(sock: &Path, prefix: &str, frames: u64) {
    let clock = format!("{prefix}/consumer");
    let producer = format!("{prefix}/producer");
    let frames_name = format!("{prefix}/frames");
    let mut client =
        AbacusClient::connect_waiting(sock, &clock, &[producer.as_str()], CONNECT_CEILING)
            .unwrap_or_else(|e| panic!("consumer connect: {e}"));
    // The producer creates its frames interlock just after its clock; retry briefly.
    let start = Instant::now();
    let ready = loop {
        match client.create_wait_counter(
            &format!("{prefix}/ready"),
            &frames_name,
            WatchedWord::OpenCount,
        ) {
            Ok(w) => break w,
            Err(SdkError::InterlockNotFound { .. }) if start.elapsed() < Duration::from_secs(5) => {
                thread::sleep(Duration::from_millis(10));
            }
            Err(e) => panic!("create ready counter: {e}"),
        }
    };
    println!(
        "consumer ready: clock {clock}, depends on {producer}, pid {}",
        std::process::id()
    );
    let mut n = 1u64;
    while frames == 0 || n <= frames {
        let t = Instant::now();
        let r = ready
            .wait_until(n, WAIT_TIMEOUT_MS)
            .unwrap_or_else(|e| panic!("wait for frame {n}: {e}"));
        assert_ne!(r.state, WaitState::Timeout, "frame {n} timed out");
        if n <= 3 || n % 100 == 0 {
            println!(
                "frame {n}: {:?}, waited {} us",
                r.state,
                t.elapsed().as_micros()
            );
        }
        n += 1;
    }
    println!("consumer done: {} frames", n - 1);
}
