//! Processing pipeline with stub work: three threads coordinating through Abacus interlocks.
//!
//! A pod is a processing pipeline: a camera captures frames (producer), inference processes
//! them (consumer), and a coordinator waits for each cycle to complete before releasing the
//! next.
//!
//! The coordination is WaitCounter-based, not barrier-based: each stage watches the previous
//! stage's cumulative count, and `wait_until(N, timeout)` naturally expresses "wait for cycle
//! N." A WaitBarrier would require per-cycle recreation or threshold bumping that the current
//! API does not support.
//!
//! Run against a live daemon:
//!
//! ```sh
//! cargo run --example pod -- /tmp/abacus-probe.sock 20
//! ```
//!
//! The second argument is the number of cycles (default 10).

use std::env;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use abacus_client::{AbacusClient, WaitState, WatchedWord};

const CAPTURE_MS: u64 = 16; // ~60 fps
const INFERENCE_MS: u64 = 8;
const WAIT_TIMEOUT_MS: u64 = 200;

fn main() {
    let args: Vec<String> = env::args().collect();
    let sock = args
        .get(1)
        .map(String::as_str)
        .unwrap_or("/tmp/abacus-probe.sock");
    let cycles: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(10);

    let sock = Path::new(sock);
    let pid = std::process::id();
    let frame_name = format!("pod-{pid}-frame");
    let result_name = format!("pod-{pid}-result");

    // -- Set up the topology --
    //
    // Two interlocks form the pipeline:
    //   frame:  producer opens after each capture, open_count = frames produced
    //   result: consumer opens after each inference, open_count = frames processed
    //
    // Three clients, one per thread. Each client is one daemon connection with its own
    // keepalive thread.

    let mut coord =
        AbacusClient::connect(sock, &format!("pod-{pid}-coord"), &[]).expect("connect coordinator");

    // Coordinator creates the shared interlocks so names exist before threads start.
    let _frame = coord.create_interlock(&frame_name).expect("create frame");
    let _result = coord.create_interlock(&result_name).expect("create result");

    // Coordinator watches the result: cycle N is done when result open_count >= N.
    let done = coord
        .create_wait_counter(
            &format!("pod-{pid}-done"),
            &result_name,
            WatchedWord::OpenCount,
        )
        .expect("create done counter");

    // -- Producer: camera capture --

    let cam_sock = sock.to_owned();
    let cam_frame = frame_name.clone();
    let cam_handle = thread::spawn(move || {
        let mut client = AbacusClient::connect(&cam_sock, &format!("pod-{pid}-cam"), &[])
            .expect("connect producer");
        let frame = client.attach_interlock(&cam_frame).expect("attach frame");

        for _ in 0..cycles {
            thread::sleep(Duration::from_millis(CAPTURE_MS));
            frame.open(1).expect("signal frame");
        }
    });

    // -- Consumer: inference --

    let inf_sock = sock.to_owned();
    let inf_frame = frame_name.clone();
    let inf_result = result_name.clone();
    let inf_handle = thread::spawn(move || {
        let mut client = AbacusClient::connect(&inf_sock, &format!("pod-{pid}-inf"), &[])
            .expect("connect consumer");

        // Watch the producer's frame count.
        let ready = client
            .create_wait_counter(
                &format!("pod-{pid}-ready"),
                &inf_frame,
                WatchedWord::OpenCount,
            )
            .expect("create ready counter");
        let result = client.attach_interlock(&inf_result).expect("attach result");

        for n in 1..=cycles as u64 {
            let r = ready
                .wait_until(n, WAIT_TIMEOUT_MS)
                .expect("wait for frame");
            assert_ne!(r.state, WaitState::Timeout, "frame {n} timed out");
            thread::sleep(Duration::from_millis(INFERENCE_MS));
            result.open(1).expect("signal result");
        }
    });

    // -- Coordinator: wait for each cycle --

    let t0 = Instant::now();
    for n in 1..=cycles as u64 {
        let r = done.wait_until(n, WAIT_TIMEOUT_MS).expect("wait for cycle");
        assert_ne!(r.state, WaitState::Timeout, "cycle {n} timed out");
    }
    let total = t0.elapsed();

    cam_handle.join().expect("producer panicked");
    inf_handle.join().expect("consumer panicked");

    let avg_ms = total.as_millis() as f64 / cycles as f64;
    let expected_ms = (CAPTURE_MS + INFERENCE_MS) as f64;
    println!(
        "{cycles} cycles in {:.0} ms | avg {avg_ms:.1} ms/cycle (floor {expected_ms:.0} ms from stubs)",
        total.as_millis()
    );
}
