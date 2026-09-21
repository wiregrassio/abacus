//! Review probe: exercises the real daemon binary from a separate process. The numbers in
//! docs/OPERATION.md come from this program; see that file for how to run it.
use std::env;
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

use abacus_client::{AbacusClient, WaitState, WatchedWord};

fn main() {
    let args: Vec<String> = env::args().collect();
    let cmd = args.get(1).map(String::as_str).unwrap_or("");
    let sock_s = args
        .get(2)
        .cloned()
        .unwrap_or_else(|| "/tmp/abacus-probe.sock".to_string());
    let sock = Path::new(&sock_s);
    let a3 = args.get(3).and_then(|s| s.parse::<u64>().ok());
    let a4 = args.get(4).and_then(|s| s.parse::<u64>().ok());
    match cmd {
        "bench" => bench(sock, a3.unwrap_or(1000) as usize, a4.unwrap_or(5)),
        "cron" => cron(sock, a3.unwrap_or(10), a4.unwrap_or(200) as usize),
        "barrier" => barrier(sock),
        "partial" => partial(sock),
        _ => eprintln!("usage: probe bench|cron|barrier|partial <sock> [n] [ms]"),
    }
}

fn pct(v: &[u64], q: f64) -> u64 {
    v[((v.len() - 1) as f64 * q) as usize]
}

fn bench(sock: &Path, n: usize, ms: u64) {
    let mut client = AbacusClient::connect(sock).expect("connect");
    let timer = client
        .create_wait_timer(&format!("bench-{}", std::process::id()))
        .expect("create timer");
    let mut elapsed = Vec::with_capacity(n);
    let mut overrun = Vec::with_capacity(n);
    let mut states = [0usize; 3];
    for _ in 0..n {
        let t0 = Instant::now();
        let r = timer.wait_ms(ms).expect("wait_ms");
        elapsed.push(t0.elapsed().as_micros() as u64);
        let (target, _) = timer.peek();
        overrun.push(r.completed_at.saturating_sub(target));
        match r.state {
            WaitState::Normal => states[0] += 1,
            WaitState::Overrun => states[1] += 1,
            WaitState::Timeout => states[2] += 1,
        }
    }
    elapsed.sort_unstable();
    overrun.sort_unstable();
    println!(
        "bench n={n} wait_ms={ms} | elapsed_us p50={} p90={} p99={} max={} | overrun_ms p50={} p99={} max={} | normal={} overrun={} timeout={}",
        pct(&elapsed, 0.5), pct(&elapsed, 0.9), pct(&elapsed, 0.99), elapsed[n - 1],
        pct(&overrun, 0.5), pct(&overrun, 0.99), overrun[n - 1],
        states[0], states[1], states[2]
    );
}

fn cron(sock: &Path, interval_ms: u64, n: usize) {
    let mut client = AbacusClient::connect(sock).expect("connect");
    let c = client
        .create_wait_cron(&format!("cron-{}", std::process::id()), interval_ms)
        .expect("create cron");
    let mut deltas = Vec::with_capacity(n);
    let mut misaligned = 0usize;
    let mut states = [0usize; 3];
    let mut last: Option<u64> = None;
    for _ in 0..n {
        let r = c.wait().expect("cron wait");
        if r.completed_at % interval_ms != 0 {
            misaligned += 1;
        }
        if let Some(l) = last {
            deltas.push(r.completed_at - l);
        }
        last = Some(r.completed_at);
        match r.state {
            WaitState::Normal => states[0] += 1,
            WaitState::Overrun => states[1] += 1,
            WaitState::Timeout => states[2] += 1,
        }
    }
    deltas.sort_unstable();
    println!(
        "cron interval={interval_ms} n={n} | delta_ms min={} p50={} p99={} max={} | fired_off_grid={} | normal={} overrun={} timeout={}",
        deltas[0], pct(&deltas, 0.5), pct(&deltas, 0.99), deltas[deltas.len() - 1],
        misaligned, states[0], states[1], states[2]
    );
}

fn barrier(sock: &Path) {
    let mut client = AbacusClient::connect(sock).expect("connect");
    let src = client.create_interlock("b-src").expect("create src");
    let b = client
        .create_wait_barrier(
            "b-all",
            vec![("b-src".to_string(), WatchedWord::ClosedCount, 3)],
        )
        .expect("create barrier");
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(20));
        src.close(3).expect("close");
        std::thread::sleep(Duration::from_millis(200));
    });
    let t0 = Instant::now();
    let r = b.wait().expect("barrier wait");
    let (open, closed) = b.peek();
    println!(
        "barrier fired after {:?} | state={:?} completed_at={} open={} closed={}",
        t0.elapsed(),
        r.state,
        r.completed_at,
        open,
        closed
    );
}

fn partial(sock: &Path) {
    let mut s = UnixStream::connect(sock).expect("connect raw");
    s.write_all(&[7, 0]).expect("write 2 bytes");
    println!("partial: sent 2 of 4 length-prefix bytes, holding connection open for 8s");
    std::thread::sleep(Duration::from_secs(8));
    println!("partial: closing");
}
