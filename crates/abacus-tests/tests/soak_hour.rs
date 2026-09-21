//! L4: the soak. One timer, one counter, one cron running continuously against a real daemon
//! process while memory and fd counts are sampled. Ignored by default; run with
//! `-- --ignored`. `ABACUS_SOAK=1` selects the full hour; without it the same test runs for
//! SHORT_SECONDS with identical assertions and says so on stdout, so the default `--ignored`
//! run still exercises the soak path.

#![allow(non_snake_case)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use abacus_client::{WaitState, WatchedWord};
use abacus_tests::{abacus_binary, serialized, ProcessDaemon, Stats};

const FULL_SECONDS: u64 = 3600;
const SHORT_SECONDS: u64 = 60;
/// Sampling cadence for VmRSS and fd count; the first sample lands after WARMUP.
const SAMPLE_EVERY: Duration = Duration::from_secs(10);
const WARMUP: Duration = Duration::from_secs(5);
/// VmRSS spread across samples, in kB. Flat within 1 MB.
const RSS_SPREAD_LIMIT_KB: u64 = 1024;
const TIMER_MS: u64 = 20;
const COUNTER_STEP: Duration = Duration::from_millis(50);
const CRON_MS: u64 = 100;
/// Fraction of the ideal fire count each loop must reach.
const MIN_FIRE_FRACTION: f64 = 0.8;

fn bin() -> PathBuf {
    abacus_binary()
}

struct TimerOutcome {
    fires: u64,
    overrun: u64,
    elapsed: Stats,
    error: Option<String>,
}

struct CounterOutcome {
    fires: u64,
    timeouts: u64,
    error: Option<String>,
}

struct CronOutcome {
    fires: u64,
    off_grid: u64,
    off_grid_reported_normal: u64,
    error: Option<String>,
}

/// SURFACE.md WaitTimer, WaitCounter, WaitCron over a long run; LIFECYCLE.md reap and
/// re-arm leave nothing behind: VmRSS flat within RSS_SPREAD_LIMIT_KB, fd count flat, every
/// cron fire on the grid or reported Overrun, timers and counters keep firing with no
/// counter timeouts.
#[test]
#[ignore = "soak: run with --ignored; ABACUS_SOAK=1 for the full hour, else 60 s"]
fn soak__one_hour_timers_counters_crons_no_drift_no_leak() {
    let _serial = serialized();
    let full = std::env::var("ABACUS_SOAK")
        .map(|v| v == "1")
        .unwrap_or(false);
    let seconds = if full { FULL_SECONDS } else { SHORT_SECONDS };
    if !full {
        println!(
            "ABACUS_SOAK unset: running the {SHORT_SECONDS} s abbreviated soak with identical assertions; set ABACUS_SOAK=1 for {FULL_SECONDS} s"
        );
    }
    let duration = Duration::from_secs(seconds);

    let mut d = ProcessDaemon::start(&bin(), "soak");
    let mut client = d.client();
    let timer = client
        .create_wait_timer("soak-timer")
        .expect("create timer");
    let src = client.create_interlock("soak-src").expect("create src");
    let counter = client
        .create_wait_counter("soak-counter", "soak-src", WatchedWord::ClosedCount)
        .expect("create counter");
    let cron = client
        .create_wait_cron("soak-cron", CRON_MS)
        .expect("create cron");
    let stop = Arc::new(AtomicBool::new(false));

    let t_stop = stop.clone();
    let timer_t = thread::spawn(move || {
        let mut o = TimerOutcome {
            fires: 0,
            overrun: 0,
            elapsed: Stats::new("timer_wait_us", "us"),
            error: None,
        };
        while !t_stop.load(Ordering::Relaxed) {
            let t0 = Instant::now();
            match timer.wait_ms(TIMER_MS) {
                Ok(r) => {
                    o.fires += 1;
                    o.elapsed.push(t0.elapsed().as_micros() as u64);
                    if r.state == WaitState::Overrun {
                        o.overrun += 1;
                    }
                }
                Err(e) => {
                    o.error = Some(e.to_string());
                    break;
                }
            }
        }
        o
    });

    let s_stop = stop.clone();
    let driver_t = thread::spawn(move || {
        let mut steps = 0u64;
        while !s_stop.load(Ordering::Relaxed) {
            // The stimulus: advance the watched interlock on a fixed cadence.
            thread::sleep(COUNTER_STEP);
            src.close(1).expect("close");
            steps += 1;
        }
        steps
    });

    let c_stop = stop.clone();
    let counter_t = thread::spawn(move || {
        let mut o = CounterOutcome {
            fires: 0,
            timeouts: 0,
            error: None,
        };
        let mut target = 1u64;
        while !c_stop.load(Ordering::Relaxed) {
            match counter.wait_until(target, 500) {
                Ok(r) => {
                    if r.state == WaitState::Timeout {
                        if !c_stop.load(Ordering::Relaxed) {
                            o.timeouts += 1;
                        }
                    } else {
                        o.fires += 1;
                        target = r.completed_at.max(target) + 1;
                    }
                }
                Err(e) => {
                    o.error = Some(e.to_string());
                    break;
                }
            }
        }
        o
    });

    let k_stop = stop.clone();
    let cron_t = thread::spawn(move || {
        let mut o = CronOutcome {
            fires: 0,
            off_grid: 0,
            off_grid_reported_normal: 0,
            error: None,
        };
        while !k_stop.load(Ordering::Relaxed) {
            match cron.wait() {
                Ok(r) => {
                    o.fires += 1;
                    if r.completed_at % CRON_MS != 0 {
                        o.off_grid += 1;
                        if r.state != WaitState::Overrun {
                            o.off_grid_reported_normal += 1;
                        }
                    }
                }
                Err(e) => {
                    o.error = Some(e.to_string());
                    break;
                }
            }
        }
        o
    });

    // Sampler: this thread. Sleeps are the cadence of the stimulus, bounded by `duration`.
    let mut rss = Stats::new("vm_rss_kb", "kB");
    let mut fds = Stats::new("fd_count", "fds");
    let start = Instant::now();
    thread::sleep(WARMUP.min(duration));
    let mut next_sample = WARMUP;
    while start.elapsed() < duration {
        assert!(
            d.is_alive(),
            "daemon died {:?} into the soak; last stderr lines:\n{}",
            start.elapsed(),
            d.stderr_lines()
                .into_iter()
                .rev()
                .take(20)
                .collect::<Vec<_>>()
                .join("\n")
        );
        rss.push(d.vm_rss_kb());
        fds.push(d.fd_count() as u64);
        println!(
            "soak {:>5} s: VmRSS {} kB, fds {}",
            start.elapsed().as_secs(),
            d.vm_rss_kb(),
            d.fd_count()
        );
        next_sample += SAMPLE_EVERY;
        let remaining = duration.saturating_sub(start.elapsed());
        let until_next = next_sample.saturating_sub(start.elapsed());
        thread::sleep(until_next.min(remaining));
    }
    stop.store(true, Ordering::Relaxed);
    let timer_o = timer_t.join().expect("timer thread panicked");
    let steps = driver_t.join().expect("driver thread panicked");
    let counter_o = counter_t.join().expect("counter thread panicked");
    let cron_o = cron_t.join().expect("cron thread panicked");

    println!(
        "soak {seconds} s: {} ({} overrun) | counter fires {} timeouts {} of {steps} steps | cron fires {} off-grid {} misreported {} | {} | {}",
        timer_o.elapsed.report(),
        timer_o.overrun,
        counter_o.fires,
        counter_o.timeouts,
        cron_o.fires,
        cron_o.off_grid,
        cron_o.off_grid_reported_normal,
        rss.report(),
        fds.report()
    );

    assert!(d.is_alive(), "daemon died by the end of the soak");
    for (what, err) in [
        ("timer", &timer_o.error),
        ("counter", &counter_o.error),
        ("cron", &cron_o.error),
    ] {
        assert!(
            err.is_none(),
            "{what} loop failed during the soak: {}",
            err.as_deref().unwrap_or("")
        );
    }
    let rss_spread = rss.max().saturating_sub(rss.min());
    assert!(
        rss_spread <= RSS_SPREAD_LIMIT_KB,
        "daemon VmRSS drifted {rss_spread} kB over the soak (limit {RSS_SPREAD_LIMIT_KB}): {}",
        rss.report()
    );
    assert!(
        fds.max() == fds.min(),
        "daemon fd count was not flat over the soak: {}",
        fds.report()
    );
    assert!(
        cron_o.off_grid_reported_normal == 0,
        "{} of {} off-grid cron fires were reported Normal instead of Overrun",
        cron_o.off_grid_reported_normal,
        cron_o.off_grid
    );
    let ideal_timer = (seconds * 1000 / TIMER_MS) as f64;
    assert!(
        timer_o.fires as f64 >= ideal_timer * MIN_FIRE_FRACTION,
        "timer fired {} times, under {MIN_FIRE_FRACTION} of the ideal {ideal_timer}",
        timer_o.fires
    );
    let ideal_counter = (seconds * 1000 / COUNTER_STEP.as_millis() as u64) as f64;
    assert!(
        counter_o.fires as f64 >= ideal_counter * MIN_FIRE_FRACTION && counter_o.timeouts == 0,
        "counter fired {} times with {} timeouts (ideal {ideal_counter}, {steps} driver steps)",
        counter_o.fires,
        counter_o.timeouts
    );
    let ideal_cron = (seconds * 1000 / CRON_MS) as f64;
    assert!(
        cron_o.fires as f64 >= ideal_cron * MIN_FIRE_FRACTION,
        "cron fired {} times, under {MIN_FIRE_FRACTION} of the ideal {ideal_cron}",
        cron_o.fires
    );
}
