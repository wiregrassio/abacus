//! Measurement harness against the live Abacus daemon.
//!
//! Two modes:
//!
//! **sweep** -- measures clock stamp lateness (how far past each millisecond boundary the
//! daemon writes the clock word), WaitTimer delivery stamp lateness, and the time from the
//! delivery stamp to the waiter thread running; swept over a configurable list of live
//! entries per evaluation cycle, with cores 0 to 3 either idle or saturated by memory-copy
//! threads. A spinner on an isolated core observes each millisecond boundary with sub-
//! microsecond precision; a waiter blocks on WaitTimer deliveries.
//!
//! **soak** -- a keepalive survival test on a core shaped like a Convoy inference core: a
//! capture thread (SCHED_FIFO 90) grabs the core periodically, and an inference thread
//! (SCHED_FIFO 70) spins and idles in a loop. The keepalive thread (SCHED_FIFO 80, between
//! them) must survive a configurable TTL for the configured duration.
//!
//! Core map (AGX Orin, `isolcpus=4-11`):
//!   0-3: Linux + services; saturating load threads in the `saturated` cells.
//!   4: Abacus daemon (`abacus.service`, SCHED_OTHER).
//!   5: 10 GbE IRQs.
//!   6: spinner (SCHED_FIFO 80). 7: waiter (SCHED_FIFO 80). 8: harness main + measuring
//!   keepalive. 9: loader + its keepalive. 10: unused. 11: keepalive soak.
//!   cyclictest runs on 6-11 in its own container before the harness.
//!
//! Container command shape (SYS_NICE for SCHED_FIFO):
//! ```text
//! docker run --rm --name abacus-hd-p3-<role> \
//!   --cap-add SYS_NICE --ulimit rtprio=99 --ulimit nofile=65536:65536 \
//!   -v /run/abacus:/run/abacus \
//!   -v /etc/nv_tegra_release:/etc/nv_tegra_release:ro \
//!   -v <binary>:/usr/local/bin/rtmeasure:ro \
//!   -v <outdir>:/out \
//!   ubuntu:20.04 rtmeasure <mode> --sock /run/abacus/abacus.sock --out /out <args>
//! ```
//!
//! Numbers this program produces belong in `docs/OPERATION.md` with their environment.

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use abacus_client::{AbacusClient, KeepalivePriority, SdkError, WatchedWord};
use abacus_core::clock::{monotonic_now_nanos, NANOS_PER_MS};
use abacus_tests::pin_current_thread;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const USAGE: &str = "\
usage: rtmeasure sweep --sock PATH --out DIR [--seconds 60] \
[--entries 10,30,60,100,1000,4000] [--loads idle,saturated] [--lead-us 200] [--sha SHA]
       rtmeasure soak  --sock PATH --out DIR [--seconds 600] [--core 11] \
[--ttl-ms 100] [--capture-busy-ms 3] [--capture-period-ms 30] \
[--inference-busy-ms 25] [--inference-idle-ms 1] [--sha SHA]";

const DEFAULT_SWEEP_SECONDS: u64 = 60;
const DEFAULT_ENTRIES: &str = "10,30,60,100,1000,4000";
const DEFAULT_LOADS: &str = "idle,saturated";
const DEFAULT_LEAD_US: u64 = 200;

const DEFAULT_SOAK_SECONDS: u64 = 600;
const DEFAULT_SOAK_CORE: usize = 11;
const DEFAULT_TTL_MS: u64 = 100;
const DEFAULT_CAPTURE_BUSY_MS: u64 = 3;
const DEFAULT_CAPTURE_PERIOD_MS: u64 = 30;
const DEFAULT_INFERENCE_BUSY_MS: u64 = 25;
const DEFAULT_INFERENCE_IDLE_MS: u64 = 1;

const SPINNER_CORE: usize = 6;
const WAITER_CORE: usize = 7;
const MAIN_CORE: usize = 8;
const LOADER_CORE: usize = 9;

const FIFO_PRIORITY: i32 = 80;
const LOAD_BUF_SIZE: usize = 32 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mode = args.get(1).map(String::as_str).unwrap_or("");
    match mode {
        "sweep" => {
            let cfg = parse_sweep(&args[2..]);
            run_sweep(&cfg);
        }
        "soak" => {
            let cfg = parse_soak(&args[2..]);
            run_soak(&cfg);
        }
        _ => {
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    }
}

// ---------------------------------------------------------------------------
// Argument parsing
// ---------------------------------------------------------------------------

struct SweepConfig {
    sock: PathBuf,
    out: PathBuf,
    seconds: u64,
    entries: Vec<u32>,
    loads: Vec<String>,
    lead_us: u64,
    sha: String,
}

struct SoakConfig {
    sock: PathBuf,
    out: PathBuf,
    seconds: u64,
    core: usize,
    ttl_ms: u64,
    capture_busy_ms: u64,
    capture_period_ms: u64,
    inference_busy_ms: u64,
    inference_idle_ms: u64,
    sha: String,
}

fn parse_kv(args: &[String]) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let mut i = 0;
    while i < args.len() {
        let key = &args[i];
        if !key.starts_with("--") {
            eprintln!("unknown argument: {key}");
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
        let key = key.strip_prefix("--").unwrap_or(key).to_string();
        if i + 1 >= args.len() {
            eprintln!("missing value for --{key}");
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
        let value = args[i + 1].clone();
        if map.insert(key.clone(), value).is_some() {
            eprintln!("duplicate argument: --{key}");
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
        i += 2;
    }
    map
}

fn require(map: &HashMap<String, String>, key: &str) -> String {
    map.get(key).cloned().unwrap_or_else(|| {
        eprintln!("missing required argument: --{key}");
        eprintln!("{USAGE}");
        std::process::exit(2);
    })
}

fn parse_sweep(args: &[String]) -> SweepConfig {
    let map = parse_kv(args);
    let sock = PathBuf::from(require(&map, "sock"));
    let out = PathBuf::from(require(&map, "out"));
    let seconds = map
        .get("seconds")
        .map(|s| parse_u64(s, "seconds"))
        .unwrap_or(DEFAULT_SWEEP_SECONDS);
    let entries_str = map
        .get("entries")
        .cloned()
        .unwrap_or_else(|| DEFAULT_ENTRIES.to_string());
    let entries: Vec<u32> = entries_str
        .split(',')
        .map(|s| parse_u32(s.trim(), "entries"))
        .collect();
    let loads_str = map
        .get("loads")
        .cloned()
        .unwrap_or_else(|| DEFAULT_LOADS.to_string());
    let loads: Vec<String> = loads_str.split(',').map(|s| s.trim().to_string()).collect();
    for l in &loads {
        if l != "idle" && l != "saturated" {
            eprintln!("unknown load: {l}");
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    }
    let lead_us = map
        .get("lead-us")
        .map(|s| parse_u64(s, "lead-us"))
        .unwrap_or(DEFAULT_LEAD_US);
    let sha = map
        .get("sha")
        .cloned()
        .unwrap_or_else(|| "unknown".to_string());

    let known = [
        "sock", "out", "seconds", "entries", "loads", "lead-us", "sha",
    ];
    for key in map.keys() {
        if !known.contains(&key.as_str()) {
            eprintln!("unknown argument: --{key}");
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    }

    SweepConfig {
        sock,
        out,
        seconds,
        entries,
        loads,
        lead_us,
        sha,
    }
}

fn parse_soak(args: &[String]) -> SoakConfig {
    let map = parse_kv(args);
    let sock = PathBuf::from(require(&map, "sock"));
    let out = PathBuf::from(require(&map, "out"));
    let seconds = map
        .get("seconds")
        .map(|s| parse_u64(s, "seconds"))
        .unwrap_or(DEFAULT_SOAK_SECONDS);
    let core = map
        .get("core")
        .map(|s| parse_usize(s, "core"))
        .unwrap_or(DEFAULT_SOAK_CORE);
    let ttl_ms = map
        .get("ttl-ms")
        .map(|s| parse_u64(s, "ttl-ms"))
        .unwrap_or(DEFAULT_TTL_MS);
    let capture_busy_ms = map
        .get("capture-busy-ms")
        .map(|s| parse_u64(s, "capture-busy-ms"))
        .unwrap_or(DEFAULT_CAPTURE_BUSY_MS);
    let capture_period_ms = map
        .get("capture-period-ms")
        .map(|s| parse_u64(s, "capture-period-ms"))
        .unwrap_or(DEFAULT_CAPTURE_PERIOD_MS);
    let inference_busy_ms = map
        .get("inference-busy-ms")
        .map(|s| parse_u64(s, "inference-busy-ms"))
        .unwrap_or(DEFAULT_INFERENCE_BUSY_MS);
    let inference_idle_ms = map
        .get("inference-idle-ms")
        .map(|s| parse_u64(s, "inference-idle-ms"))
        .unwrap_or(DEFAULT_INFERENCE_IDLE_MS);
    let sha = map
        .get("sha")
        .cloned()
        .unwrap_or_else(|| "unknown".to_string());

    let known = [
        "sock",
        "out",
        "seconds",
        "core",
        "ttl-ms",
        "capture-busy-ms",
        "capture-period-ms",
        "inference-busy-ms",
        "inference-idle-ms",
        "sha",
    ];
    for key in map.keys() {
        if !known.contains(&key.as_str()) {
            eprintln!("unknown argument: --{key}");
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    }

    SoakConfig {
        sock,
        out,
        seconds,
        core,
        ttl_ms,
        capture_busy_ms,
        capture_period_ms,
        inference_busy_ms,
        inference_idle_ms,
        sha,
    }
}

fn parse_u64(s: &str, name: &str) -> u64 {
    s.parse().unwrap_or_else(|_| {
        eprintln!("--{name}: not a valid number: {s}");
        eprintln!("{USAGE}");
        std::process::exit(2);
    })
}

fn parse_u32(s: &str, name: &str) -> u32 {
    s.parse().unwrap_or_else(|_| {
        eprintln!("--{name}: not a valid number: {s}");
        eprintln!("{USAGE}");
        std::process::exit(2);
    })
}

fn parse_usize(s: &str, name: &str) -> usize {
    s.parse().unwrap_or_else(|_| {
        eprintln!("--{name}: not a valid number: {s}");
        eprintln!("{USAGE}");
        std::process::exit(2);
    })
}

// ---------------------------------------------------------------------------
// Environment header (S3)
// ---------------------------------------------------------------------------

fn read_or_unreadable(path: &str) -> String {
    match fs::read_to_string(path) {
        Ok(s) => s.trim().to_string(),
        Err(e) => format!("unreadable: {e}"),
    }
}

fn write_env(dir: &Path, mode: &str, sha: &str, args_lines: &[String]) -> String {
    let start_ns = monotonic_now_nanos();
    let mut lines = Vec::new();
    lines.push(format!("mode: {mode}"));
    lines.push(format!("sha: {sha}"));
    lines.push(format!("start_monotonic_ns: {start_ns}"));
    lines.push(format!(
        "osrelease: {}",
        read_or_unreadable("/proc/sys/kernel/osrelease")
    ));
    lines.push(format!("cmdline: {}", read_or_unreadable("/proc/cmdline")));

    let l4t = match fs::read_to_string("/etc/nv_tegra_release") {
        Ok(s) => s.lines().next().unwrap_or("absent").to_string(),
        Err(_) => "absent".to_string(),
    };
    lines.push(format!("l4t: {l4t}"));

    lines.push(format!(
        "sched_rt_runtime_us: {}",
        read_or_unreadable("/proc/sys/kernel/sched_rt_runtime_us")
    ));
    lines.push(format!(
        "default_smp_affinity: {}",
        read_or_unreadable("/proc/irq/default_smp_affinity")
    ));

    // IRQ lines for eth0
    let interrupts = fs::read_to_string("/proc/interrupts").unwrap_or_default();
    for int_line in interrupts.lines() {
        if int_line.contains("eth0") {
            let irq = int_line.split_whitespace().next().unwrap_or("?");
            let irq = irq.trim_end_matches(':');
            let name_part = int_line
                .rsplit_once("  ")
                .map(|(_, n)| n.trim())
                .unwrap_or("eth0");
            let affinity = read_or_unreadable(&format!("/proc/irq/{irq}/smp_affinity_list"));
            let effective = read_or_unreadable(&format!("/proc/irq/{irq}/effective_affinity_list"));
            lines.push(format!(
                "irq_{irq}: {name_part} smp_affinity_list={affinity} effective_affinity_list={effective}"
            ));
        }
    }

    // Per-CPU info
    let online_cores = count_online_cores();
    for k in 0..online_cores {
        let base = format!("/sys/devices/system/cpu/cpu{k}/cpufreq");
        let idle_base = format!("/sys/devices/system/cpu/cpu{k}/cpuidle");
        let cur = read_or_unreadable(&format!("{base}/scaling_cur_freq"));
        let min = read_or_unreadable(&format!("{base}/scaling_min_freq"));
        let max = read_or_unreadable(&format!("{base}/scaling_max_freq"));
        let gov = read_or_unreadable(&format!("{base}/scaling_governor"));

        let mut idle_parts = Vec::new();
        for state_idx in 0..16 {
            let state_dir = format!("{idle_base}/state{state_idx}");
            let name_path = format!("{state_dir}/name");
            let disable_path = format!("{state_dir}/disable");
            if fs::metadata(&name_path).is_ok() {
                let name = read_or_unreadable(&name_path);
                let disable = read_or_unreadable(&disable_path);
                idle_parts.push(format!("{name}:disable={disable}"));
            } else {
                break;
            }
        }
        let idle_str = if idle_parts.is_empty() {
            "none".to_string()
        } else {
            idle_parts.join(",")
        };
        lines.push(format!(
            "cpu{k}: cur={cur} min={min} max={max} governor={gov} idle=[{idle_str}]"
        ));
    }

    for arg_line in args_lines {
        lines.push(arg_line.clone());
    }

    let text = lines.join("\n") + "\n";
    let env_path = dir.join("env.txt");
    fs::write(&env_path, &text).unwrap_or_else(|e| panic!("write {}: {e}", env_path.display()));
    print!("{text}");
    text
}

fn count_online_cores() -> usize {
    let s = fs::read_to_string("/sys/devices/system/cpu/online").unwrap_or_default();
    let s = s.trim();
    if s.is_empty() {
        return 1;
    }
    // Parse "0-11" style
    if let Some((_, end)) = s.rsplit_once('-') {
        if let Ok(e) = end.parse::<usize>() {
            return e + 1;
        }
    }
    if let Ok(n) = s.parse::<usize>() {
        return n + 1;
    }
    1
}

// ---------------------------------------------------------------------------
// Scheduling helpers
// ---------------------------------------------------------------------------

fn set_scheduling(policy: i32, priority: i32) {
    // SAFETY: sched_setscheduler on the calling thread (pid 0) with a local param.
    unsafe {
        let mut param: libc::sched_param = std::mem::zeroed();
        param.sched_priority = priority;
        let rc = libc::sched_setscheduler(0, policy, &param);
        if rc != 0 {
            let e = std::io::Error::last_os_error();
            panic!(
                "sched_setscheduler(policy={policy}, priority={priority}) failed: {e} (errno {})",
                e.raw_os_error().unwrap_or(0)
            );
        }
    }
}

fn get_scheduling() -> (i32, i32) {
    // SAFETY: sched_getscheduler and sched_getparam on the calling thread (pid 0).
    unsafe {
        let policy = libc::sched_getscheduler(0);
        assert!(
            policy >= 0,
            "sched_getscheduler: {}",
            std::io::Error::last_os_error()
        );
        let mut param: libc::sched_param = std::mem::zeroed();
        assert_eq!(
            libc::sched_getparam(0, &mut param),
            0,
            "sched_getparam: {}",
            std::io::Error::last_os_error()
        );
        (policy, param.sched_priority)
    }
}

fn clock_nanosleep_abs(target_ns: u64) {
    let ts = libc::timespec {
        tv_sec: (target_ns / 1_000_000_000) as libc::time_t,
        tv_nsec: (target_ns % 1_000_000_000) as libc::c_long,
    };
    loop {
        // SAFETY: clock_nanosleep with TIMER_ABSTIME and a local timespec.
        let rc = unsafe {
            libc::clock_nanosleep(
                libc::CLOCK_MONOTONIC,
                libc::TIMER_ABSTIME,
                &ts,
                std::ptr::null_mut(),
            )
        };
        if rc == 0 {
            return;
        }
        if rc != libc::EINTR {
            panic!("clock_nanosleep failed: errno {rc}");
        }
    }
}

fn spin_busy_ms(ms: u64) {
    let end = monotonic_now_nanos().saturating_add(ms * NANOS_PER_MS);
    while monotonic_now_nanos() < end {
        std::hint::spin_loop();
    }
}

fn policy_name(policy: i32) -> &'static str {
    match policy {
        libc::SCHED_OTHER => "SCHED_OTHER",
        libc::SCHED_FIFO => "SCHED_FIFO",
        libc::SCHED_RR => "SCHED_RR",
        libc::SCHED_BATCH => "SCHED_BATCH",
        libc::SCHED_IDLE => "SCHED_IDLE",
        _ => "UNKNOWN",
    }
}

// ---------------------------------------------------------------------------
// Percentile (nearest rank, S7)
// ---------------------------------------------------------------------------

fn percentile_ns(sorted: &[u64], q: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    // Nearest rank: ceil(q * n), 1-based index.
    let rank = (q * sorted.len() as f64).ceil() as usize;
    let idx = rank.max(1).min(sorted.len()) - 1;
    sorted[idx]
}

fn ns_to_us_3dec(ns: u64) -> String {
    let whole = ns / 1000;
    let frac = ns % 1000;
    format!("{whole}.{frac:03}")
}

fn format_stat(sorted: &[u64], label: &str) -> String {
    if sorted.is_empty() {
        return format!(
            "{label} n=0 p50_us=- p99_us=- p999_us=- max_us=- \
             over_100us=- over_500us=- over_1ms=- over_2ms=- over_5ms=- over_10ms=-"
        );
    }
    let p50 = percentile_ns(sorted, 0.50);
    let p99 = percentile_ns(sorted, 0.99);
    let p999 = percentile_ns(sorted, 0.999);
    let max = sorted[sorted.len() - 1];
    let count = |threshold_ns: u64| sorted.iter().filter(|&&v| v > threshold_ns).count();
    format!(
        "{label} n={} p50_us={} p99_us={} p999_us={} max_us={} \
         over_100us={} over_500us={} over_1ms={} over_2ms={} over_5ms={} over_10ms={}",
        sorted.len(),
        ns_to_us_3dec(p50),
        ns_to_us_3dec(p99),
        ns_to_us_3dec(p999),
        ns_to_us_3dec(max),
        count(100_000),
        count(500_000),
        count(1_000_000),
        count(2_000_000),
        count(5_000_000),
        count(10_000_000),
    )
}

// ---------------------------------------------------------------------------
// Sweep mode (S4, S5, S7)
// ---------------------------------------------------------------------------

struct CellSamples {
    clock_lateness: Vec<u64>,
    delivery_lateness: Vec<u64>,
    stamp_to_running: Vec<u64>,
    spinner_late: u64,
    clock_skips: u64,
    timeouts: u64,
    unmatched: u64,
    spinner_policy: i32,
    spinner_priority: i32,
    spinner_core: usize,
    waiter_policy: i32,
    waiter_priority: i32,
    waiter_core: usize,
}

fn run_sweep(cfg: &SweepConfig) {
    fs::create_dir_all(&cfg.out).unwrap_or_else(|e| panic!("create {}: {e}", cfg.out.display()));

    pin_current_thread(&[MAIN_CORE]);

    let pid = std::process::id();
    let start_mono = monotonic_now_nanos();
    let prefix = format!("hd-p3-{pid}-{start_mono}");

    let mut args_lines = Vec::new();
    args_lines.push(format!(
        "cores: spinner=6/FIFO/{FIFO_PRIORITY} waiter=7/FIFO/{FIFO_PRIORITY} main=8 loader=9 load=0-3"
    ));
    args_lines.push(format!("seconds: {}", cfg.seconds));
    args_lines.push(format!("entries: {:?}", cfg.entries));
    args_lines.push(format!("loads: {:?}", cfg.loads));
    args_lines.push(format!("lead_us: {}", cfg.lead_us));

    let env_text = write_env(&cfg.out, "sweep", &cfg.sha, &args_lines);

    let mut client = AbacusClient::connect_waiting(
        &cfg.sock,
        &format!("{prefix}/measure"),
        &[],
        Duration::from_secs(60),
    )
    .unwrap_or_else(|e| panic!("connect: {e}"));
    client.set_timeout_policy(abacus_client::TimeoutPolicy::Error);

    let mut csv = fs::File::create(cfg.out.join("samples.csv"))
        .unwrap_or_else(|e| panic!("create samples.csv: {e}"));
    writeln!(csv, "load,entries,metric,ms,value_ns").unwrap();

    let mut summary_lines: Vec<String> = Vec::new();

    let mut prev_timer: Option<abacus_client::WaitTimer> = None;

    for load_name in &cfg.loads {
        // Start load threads for "saturated"
        let load_stop = Arc::new(AtomicBool::new(false));
        let mut load_threads = Vec::new();
        if load_name == "saturated" {
            for cpu in 0..4usize {
                let stop = Arc::clone(&load_stop);
                let t = thread::Builder::new()
                    .name(format!("load-{cpu}"))
                    .spawn(move || {
                        pin_current_thread(&[cpu]);
                        let mut a = vec![0u8; LOAD_BUF_SIZE];
                        let mut b = vec![1u8; LOAD_BUF_SIZE];
                        while !stop.load(Ordering::Relaxed) {
                            a.copy_from_slice(&b);
                            std::hint::black_box(&a);
                            b.copy_from_slice(&a);
                            std::hint::black_box(&b);
                        }
                    })
                    .unwrap_or_else(|e| panic!("spawn load thread: {e}"));
                load_threads.push(t);
            }
        }

        // Start the loader client on a scoped thread pinned to core 9
        let loader_prefix = prefix.clone();
        let loader_sock = cfg.sock.clone();
        let loader_load_name = load_name.clone();
        let entry_list = cfg.entries.clone();

        thread::scope(|scope| {
            // Channel for loader to signal readiness per entry count
            let (loader_ready_tx, loader_ready_rx) = std::sync::mpsc::channel::<u32>();
            let (loader_done_tx, loader_done_rx) = std::sync::mpsc::channel::<()>();

            let loader_handle = scope.spawn(move || {
                pin_current_thread(&[LOADER_CORE]);
                let mut loader_client = AbacusClient::connect_waiting(
                    &loader_sock,
                    &format!("{loader_prefix}/loader-{loader_load_name}"),
                    &[],
                    Duration::from_secs(60),
                )
                .unwrap_or_else(|e| panic!("loader connect: {e}"));
                loader_client.set_timeout_policy(abacus_client::TimeoutPolicy::Error);

                let mut interlocks = Vec::new();
                let mut counters = Vec::new();

                for &target_count in &entry_list {
                    // For target_count entries: ceil(target_count/2) bare interlocks,
                    // the rest WaitCounters. Grow each pool to its target.
                    let n_il = target_count.div_ceil(2);
                    let n_wc = target_count - n_il;

                    // Grow interlocks first (counters reference them)
                    while (interlocks.len() as u32) < n_il {
                        let i = interlocks.len() as u32;
                        let il = loader_client
                            .create_interlock(&format!("{loader_prefix}/{loader_load_name}/il-{i}"))
                            .unwrap_or_else(|e| panic!("create il-{i}: {e}"));
                        interlocks.push(il);
                    }

                    // Grow WaitCounters, each watching a corresponding interlock
                    while (counters.len() as u32) < n_wc {
                        let i = counters.len() as u32;
                        let il_name = format!("{loader_prefix}/{loader_load_name}/il-{i}");
                        let wc = loader_client
                            .create_wait_counter(
                                &format!("{loader_prefix}/{loader_load_name}/wc-{i}"),
                                &il_name,
                                WatchedWord::OpenCount,
                            )
                            .unwrap_or_else(|e| panic!("create wc-{i}: {e}"));
                        // Arm and never satisfy: target 1<<40, timeout 1 ms
                        let _ = wc.wait_until(1 << 40, 1);
                        counters.push(wc);
                    }

                    loader_ready_tx.send(target_count).unwrap();
                    // Wait for the cell to finish
                    let _ = loader_done_rx.recv();
                }

                // Drop everything
                drop(counters);
                for mut il in interlocks {
                    il.free();
                }
                drop(loader_client);
                thread::sleep(Duration::from_millis(500));
            });

            // Run each entry count cell
            for &entry_count in &cfg.entries {
                let ready = loader_ready_rx.recv().unwrap();
                assert_eq!(ready, entry_count);

                // Free previous timer, create a new one
                if let Some(mut t) = prev_timer.take() {
                    t.free();
                }
                let timer = client
                    .create_wait_timer(&format!("{prefix}/{load_name}/timer-{entry_count}"))
                    .unwrap_or_else(|e| panic!("create timer: {e}"));

                // Settle 1 s
                thread::sleep(Duration::from_secs(1));

                let cell = run_cell(&client, &timer, cfg.seconds, cfg.lead_us);

                // Write CSV rows
                for &v in &cell.clock_lateness {
                    writeln!(
                        csv,
                        "{load_name},{entry_count},clock_lateness,{},{}",
                        v / NANOS_PER_MS,
                        v
                    )
                    .unwrap();
                }
                for &v in &cell.delivery_lateness {
                    writeln!(
                        csv,
                        "{load_name},{entry_count},delivery_lateness,{},{}",
                        v / NANOS_PER_MS,
                        v
                    )
                    .unwrap();
                }
                for &v in &cell.stamp_to_running {
                    writeln!(
                        csv,
                        "{load_name},{entry_count},stamp_to_running,{},{}",
                        v / NANOS_PER_MS,
                        v
                    )
                    .unwrap();
                }

                // Summary lines
                let mut cl_sorted = cell.clock_lateness.clone();
                cl_sorted.sort_unstable();
                let mut dl_sorted = cell.delivery_lateness.clone();
                dl_sorted.sort_unstable();
                let mut str_sorted = cell.stamp_to_running.clone();
                str_sorted.sort_unstable();

                summary_lines.push(format_stat(
                    &cl_sorted,
                    &format!("cell load={load_name} entries={entry_count} metric=clock_lateness"),
                ));
                summary_lines.push(format_stat(
                    &dl_sorted,
                    &format!(
                        "cell load={load_name} entries={entry_count} metric=delivery_lateness"
                    ),
                ));
                summary_lines.push(format_stat(
                    &str_sorted,
                    &format!("cell load={load_name} entries={entry_count} metric=stamp_to_running"),
                ));

                let spinner_desc = format!(
                    "{}/{}@{}",
                    policy_name(cell.spinner_policy),
                    cell.spinner_priority,
                    cell.spinner_core
                );
                let waiter_desc = format!(
                    "{}/{}@{}",
                    policy_name(cell.waiter_policy),
                    cell.waiter_priority,
                    cell.waiter_core
                );
                summary_lines.push(format!(
                    "cell load={load_name} entries={entry_count} \
                     spinner_late={} clock_skips={} timeouts={} unmatched={} \
                     spinner={spinner_desc} waiter={waiter_desc}",
                    cell.spinner_late, cell.clock_skips, cell.timeouts, cell.unmatched,
                ));

                prev_timer = Some(timer);

                loader_done_tx.send(()).unwrap();
            }

            // Signal loader to finish
            drop(loader_done_tx);
            loader_handle.join().unwrap();
        });

        // Stop load threads
        load_stop.store(true, Ordering::Relaxed);
        for t in load_threads {
            t.join().unwrap();
        }
    }

    if let Some(mut t) = prev_timer.take() {
        t.free();
    }

    // Write summary
    let mut summary_text = env_text;
    for line in &summary_lines {
        summary_text.push_str(line);
        summary_text.push('\n');
        println!("{line}");
    }
    fs::write(cfg.out.join("summary.txt"), &summary_text)
        .unwrap_or_else(|e| panic!("write summary.txt: {e}"));
}

fn run_cell(
    client: &AbacusClient,
    timer: &abacus_client::WaitTimer,
    seconds: u64,
    lead_us: u64,
) -> CellSamples {
    let stop = Arc::new(AtomicBool::new(false));
    let clock_handle = client.clock();
    let lead_ns = lead_us * 1000;

    // Shared data for spinner samples
    let spinner_clock_samples: Arc<std::sync::Mutex<Vec<(u64, u64)>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let spinner_delivery_samples: Arc<std::sync::Mutex<Vec<(u64, u64)>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let spinner_late_count = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let clock_skip_count = Arc::new(std::sync::atomic::AtomicU64::new(0));

    let spinner_sched = Arc::new(std::sync::Mutex::new((0i32, 0i32)));

    // Waiter data
    let waiter_samples: Arc<std::sync::Mutex<Vec<(u64, u64)>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let waiter_timeouts = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let waiter_sched = Arc::new(std::sync::Mutex::new((0i32, 0i32)));

    thread::scope(|scope| {
        // Spinner thread (core 6, SCHED_FIFO 80)
        let s_stop = Arc::clone(&stop);
        let s_clock = spinner_clock_samples.clone();
        let s_delivery = spinner_delivery_samples.clone();
        let s_late = spinner_late_count.clone();
        let s_skips = clock_skip_count.clone();
        let s_sched = spinner_sched.clone();

        scope.spawn(move || {
            pin_current_thread(&[SPINNER_CORE]);
            set_scheduling(libc::SCHED_FIFO, FIFO_PRIORITY);
            let (pol, pri) = get_scheduling();
            *s_sched.lock().unwrap() = (pol, pri);

            let mut now = monotonic_now_nanos();
            let mut next_boundary = ((now / NANOS_PER_MS) + 1) * NANOS_PER_MS;

            while !s_stop.load(Ordering::Relaxed) {
                // Sleep until next_boundary - lead
                let sleep_target = next_boundary.saturating_sub(lead_ns);
                if monotonic_now_nanos() < sleep_target {
                    clock_nanosleep_abs(sleep_target);
                }

                // Read "before" values
                let before_clock = clock_handle.peek().0;
                let before_closed = timer.peek().1;

                // Check if we are already late
                let next_boundary_ms = next_boundary / NANOS_PER_MS;
                if before_clock >= next_boundary_ms {
                    s_late.fetch_add(1, Ordering::Relaxed);
                    next_boundary += NANOS_PER_MS;
                    continue;
                }

                // Spin until clock changes and (timer changes or 2ms past boundary)
                let deadline = next_boundary + 2 * NANOS_PER_MS;
                let mut clock_sample: Option<(u64, u64)> = None;
                let mut delivery_sample: Option<(u64, u64)> = None;

                loop {
                    std::hint::spin_loop();
                    let cur_clock = clock_handle.peek().0;
                    let cur_closed = timer.peek().1;
                    now = monotonic_now_nanos();

                    if clock_sample.is_none() && cur_clock != before_clock {
                        let lateness = now.saturating_sub(cur_clock * NANOS_PER_MS);
                        clock_sample = Some((cur_clock, lateness));
                        if cur_clock > before_clock + 1 {
                            s_skips.fetch_add(cur_clock - before_clock - 1, Ordering::Relaxed);
                        }
                    }

                    if delivery_sample.is_none() && cur_closed != before_closed {
                        let lateness = now.saturating_sub(cur_closed * NANOS_PER_MS);
                        delivery_sample = Some((cur_closed, lateness));
                    }

                    // Exit if clock changed and (delivery changed or past deadline)
                    if clock_sample.is_some() && (delivery_sample.is_some() || now >= deadline) {
                        break;
                    }
                    if now >= deadline {
                        break;
                    }
                }

                if let Some((_val, lateness)) = clock_sample {
                    s_clock.lock().unwrap().push((_val, lateness));
                }
                if let Some((_val, lateness)) = delivery_sample {
                    s_delivery.lock().unwrap().push((_val, lateness));
                }

                next_boundary += NANOS_PER_MS;
            }
        });

        // Waiter thread (core 7, SCHED_FIFO 80)
        let w_stop = Arc::clone(&stop);
        let w_samples = waiter_samples.clone();
        let w_timeouts = waiter_timeouts.clone();
        let w_sched = waiter_sched.clone();

        scope.spawn(move || {
            pin_current_thread(&[WAITER_CORE]);
            set_scheduling(libc::SCHED_FIFO, FIFO_PRIORITY);
            let (pol, pri) = get_scheduling();
            *w_sched.lock().unwrap() = (pol, pri);

            while !w_stop.load(Ordering::Relaxed) {
                match timer.wait_ms(1) {
                    Ok(r) => {
                        let t_waiter = monotonic_now_nanos();
                        w_samples.lock().unwrap().push((r.completed_at, t_waiter));
                    }
                    Err(SdkError::DeliveryTimeout) => {
                        w_timeouts.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(e) => panic!("waiter error: {e}"),
                }
            }
        });

        // Run for the configured duration
        thread::sleep(Duration::from_secs(seconds));
        stop.store(true, Ordering::Relaxed);
    });

    // Collect results
    let spinner_clock_vec = spinner_clock_samples.lock().unwrap();
    let spinner_delivery_vec = spinner_delivery_samples.lock().unwrap();
    let waiter_vec = waiter_samples.lock().unwrap();

    let clock_lateness: Vec<u64> = spinner_clock_vec.iter().map(|&(_, l)| l).collect();
    let delivery_lateness: Vec<u64> = spinner_delivery_vec.iter().map(|&(_, l)| l).collect();

    // Join stamp_to_running: for each waiter delivery, find the spinner delivery with the
    // same completed_ms and compute t_waiter - t_spinner_delivery.
    let mut spinner_delivery_map: HashMap<u64, u64> = HashMap::new();
    for &(completed_ms, lateness_ns) in spinner_delivery_vec.iter() {
        // t_spinner = completed_ms * NANOS_PER_MS + lateness_ns
        let t_spinner = completed_ms * NANOS_PER_MS + lateness_ns;
        spinner_delivery_map.insert(completed_ms, t_spinner);
    }

    let mut stamp_to_running = Vec::new();
    let mut unmatched: u64 = 0;
    for &(completed_ms, t_waiter) in waiter_vec.iter() {
        if let Some(&t_spinner) = spinner_delivery_map.get(&completed_ms) {
            stamp_to_running.push(t_waiter.saturating_sub(t_spinner));
            spinner_delivery_map.remove(&completed_ms);
        } else {
            unmatched += 1;
        }
    }
    // Remaining unmatched spinner deliveries
    unmatched += spinner_delivery_map.len() as u64;

    let (sp, spri) = *spinner_sched.lock().unwrap();
    let (wp, wpri) = *waiter_sched.lock().unwrap();

    CellSamples {
        clock_lateness,
        delivery_lateness,
        stamp_to_running,
        spinner_late: spinner_late_count.load(Ordering::Relaxed),
        clock_skips: clock_skip_count.load(Ordering::Relaxed),
        timeouts: waiter_timeouts.load(Ordering::Relaxed),
        unmatched,
        spinner_policy: sp,
        spinner_priority: spri,
        spinner_core: SPINNER_CORE,
        waiter_policy: wp,
        waiter_priority: wpri,
        waiter_core: WAITER_CORE,
    }
}

// ---------------------------------------------------------------------------
// Soak mode (S6)
// ---------------------------------------------------------------------------

fn run_soak(cfg: &SoakConfig) {
    fs::create_dir_all(&cfg.out).unwrap_or_else(|e| panic!("create {}: {e}", cfg.out.display()));

    // Pin to the soak core first so the keepalive inherits it
    pin_current_thread(&[cfg.core]);

    let pid = std::process::id();
    let start_mono = monotonic_now_nanos();
    let prefix = format!("hd-p3-{pid}-{start_mono}");

    let mut client = AbacusClient::connect_waiting(
        &cfg.sock,
        &format!("{prefix}/soak"),
        &[],
        Duration::from_secs(60),
    )
    .unwrap_or_else(|e| panic!("connect: {e}"));
    // Default TimeoutPolicy::Abort is correct for soak

    client
        .set_process_clock_ttl_ms(cfg.ttl_ms)
        .unwrap_or_else(|e| panic!("set ttl: {e}"));
    client
        .set_keepalive_priority(KeepalivePriority::Fifo(80))
        .unwrap_or_else(|e| panic!("set keepalive priority: {e}"));
    let il = client
        .create_interlock(&format!("{prefix}/soak-il"))
        .unwrap_or_else(|e| panic!("create soak-il: {e}"));

    // Now re-pin main to core 8 so we do not starve the keepalive
    pin_current_thread(&[MAIN_CORE]);

    let mut args_lines = Vec::new();
    args_lines.push(format!("cores: soak={} main=8", cfg.core));
    args_lines.push(format!("seconds: {}", cfg.seconds));
    args_lines.push(format!("core: {}", cfg.core));
    args_lines.push(format!("ttl_ms: {}", cfg.ttl_ms));
    args_lines.push(format!("capture_busy_ms: {}", cfg.capture_busy_ms));
    args_lines.push(format!("capture_period_ms: {}", cfg.capture_period_ms));
    args_lines.push(format!("inference_busy_ms: {}", cfg.inference_busy_ms));
    args_lines.push(format!("inference_idle_ms: {}", cfg.inference_idle_ms));

    write_env(&cfg.out, "soak", &cfg.sha, &args_lines);

    let stop = Arc::new(AtomicBool::new(false));

    thread::scope(|scope| {
        // Capture thread (SCHED_FIFO 90, pinned to soak core)
        let c_stop = Arc::clone(&stop);
        let capture_core = cfg.core;
        let capture_period_ms = cfg.capture_period_ms;
        let capture_busy_ms = cfg.capture_busy_ms;

        scope.spawn(move || {
            pin_current_thread(&[capture_core]);
            set_scheduling(libc::SCHED_FIFO, 90);
            let mut next = monotonic_now_nanos() + capture_period_ms * NANOS_PER_MS;
            while !c_stop.load(Ordering::Relaxed) {
                clock_nanosleep_abs(next);
                spin_busy_ms(capture_busy_ms);
                next += capture_period_ms * NANOS_PER_MS;
            }
        });

        // Inference thread (SCHED_FIFO 70, pinned to soak core)
        let i_stop = Arc::clone(&stop);
        let inference_core = cfg.core;
        let inference_busy_ms = cfg.inference_busy_ms;
        let inference_idle_ms = cfg.inference_idle_ms;

        scope.spawn(move || {
            pin_current_thread(&[inference_core]);
            set_scheduling(libc::SCHED_FIFO, 70);
            while !i_stop.load(Ordering::Relaxed) {
                spin_busy_ms(inference_busy_ms);
                if inference_idle_ms > 0 {
                    let target = monotonic_now_nanos() + inference_idle_ms * NANOS_PER_MS;
                    clock_nanosleep_abs(target);
                }
            }
        });

        // Progress and duration tracking on main thread
        let start = std::time::Instant::now();
        let deadline = Duration::from_secs(cfg.seconds);
        let mut last_progress = 0u64;
        loop {
            thread::sleep(Duration::from_secs(1));
            let elapsed = start.elapsed();
            if elapsed >= deadline {
                break;
            }
            let elapsed_secs = elapsed.as_secs();
            if elapsed_secs >= last_progress + 60 {
                last_progress = elapsed_secs;
                println!(
                    "soak progress: {} s, process_clock reaped={}, interlock reaped={}",
                    elapsed_secs,
                    client.process_clock().is_reaped(),
                    il.is_reaped(),
                );
            }
        }

        stop.store(true, Ordering::Relaxed);
    });

    // Re-check after three keepalive intervals
    let touch_interval = abacus_client::DEFAULT_TOUCH_INTERVAL_MS;
    thread::sleep(Duration::from_millis(3 * touch_interval));

    // Read the keepalive thread's scheduling by tid
    let (ka_policy, ka_priority, ka_cpus) = read_keepalive_sched();

    let survived = !client.process_clock().is_reaped() && !il.is_reaped();

    let mut soak_text = String::new();
    soak_text.push_str(&format!("ttl_ms: {}\n", cfg.ttl_ms));
    soak_text.push_str(&format!(
        "capture: busy_ms={} period_ms={}\n",
        cfg.capture_busy_ms, cfg.capture_period_ms
    ));
    soak_text.push_str(&format!(
        "inference: busy_ms={} idle_ms={}\n",
        cfg.inference_busy_ms, cfg.inference_idle_ms
    ));
    soak_text.push_str(&format!(
        "keepalive: policy={} priority={} cpus={:?}\n",
        policy_name(ka_policy),
        ka_priority,
        ka_cpus
    ));

    if survived {
        let msg = format!("soak survived {} s\n", cfg.seconds);
        soak_text.push_str(&msg);
        print!("{soak_text}");
    } else {
        let msg = format!(
            "soak FAILED: process_clock reaped={}, interlock reaped={}\n",
            client.process_clock().is_reaped(),
            il.is_reaped()
        );
        soak_text.push_str(&msg);
        print!("{soak_text}");
        fs::write(cfg.out.join("soak.txt"), &soak_text)
            .unwrap_or_else(|e| panic!("write soak.txt: {e}"));
        std::process::exit(1);
    }

    fs::write(cfg.out.join("soak.txt"), &soak_text)
        .unwrap_or_else(|e| panic!("write soak.txt: {e}"));
}

/// Read the keepalive thread's scheduling and affinity by scanning /proc/self/task for the
/// thread named "abacus-keepaliv" (the kernel truncates to 15 chars).
fn read_keepalive_sched() -> (i32, i32, Vec<usize>) {
    let mut tid: Option<libc::pid_t> = None;
    if let Ok(entries) = fs::read_dir("/proc/self/task") {
        for entry in entries {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };
            let comm_path = entry.path().join("comm");
            if let Ok(comm) = fs::read_to_string(&comm_path) {
                if comm.trim() == "abacus-keepaliv" {
                    if let Some(t) = entry.file_name().to_str() {
                        tid = t.parse().ok();
                        break;
                    }
                }
            }
        }
    }

    let tid_val =
        tid.expect("could not find keepalive thread (abacus-keepaliv in /proc/self/task)");

    // SAFETY: sched_getscheduler/sched_getparam/sched_getaffinity read from a local.
    unsafe {
        let policy = libc::sched_getscheduler(tid_val);
        let mut param: libc::sched_param = std::mem::zeroed();
        libc::sched_getparam(tid_val, &mut param);
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::sched_getaffinity(tid_val, std::mem::size_of::<libc::cpu_set_t>(), &mut set);
        let cpus: Vec<usize> = (0..count_online_cores())
            .filter(|&c| libc::CPU_ISSET(c, &set))
            .collect();
        (
            if policy >= 0 { policy } else { 0 },
            param.sched_priority,
            cpus,
        )
    }
}
