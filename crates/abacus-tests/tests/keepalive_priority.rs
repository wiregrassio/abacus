//! L3, privileged: a client's keepalive at SCHED_FIFO survives a SCHED_FIFO busy loop on its
//! own core with a 100 ms ProcessClock TTL, and a keepalive at normal priority does not (the
//! control that proves the soak can fail). Needs CAP_SYS_NICE or RLIMIT_RTPRIO and, where
//! the kernel has RT group scheduling, real-time runtime in the test's cgroup. Without them
//! both tests fail loudly with the kernel's error. Run with --ignored.

#![allow(non_snake_case)]

use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use abacus_client::{AbacusClient, KeepalivePriority, DEFAULT_TOUCH_INTERVAL_MS};
use abacus_tests::{
    abacus_binary, describe_exit, nproc_online, pin_current_thread, role_args, role_command,
    run_child, unique_name, ChildRun, ProcessDaemon,
};

/// The keepalive outranks the busy loop, as it must outrank inference on a shared core.
const KEEPALIVE_PRIORITY: &str = "20";
const BUSY_PRIORITY: &str = "10";
/// The ProcessClock TTL for isolated deployments.
const TTL_MS: &str = "100";
const SOAK_MS: &str = "10000";

fn bin() -> &'static Path {
    use std::sync::OnceLock;
    static BIN: OnceLock<std::path::PathBuf> = OnceLock::new();
    BIN.get_or_init(abacus_binary)
}

/// Run the soak role on the last online core, with the daemon pinned to core 0 so the busy
/// loop cannot starve it.
fn soak(label: &str, keepalive: &str) -> ChildRun {
    let cores = nproc_online();
    assert!(
        cores >= 2,
        "needs two online cores (daemon on 0, soak on the last); have {cores}"
    );
    let core = (cores - 1).to_string();
    let d = ProcessDaemon::start_pinned(bin(), label, 0);
    let sock = d.socket_path().to_str().unwrap().to_string();
    let child = role_command(
        "role__keepalive_priority_soak",
        &[&sock, &core, keepalive, BUSY_PRIORITY, TTL_MS, SOAK_MS],
    )
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .expect("spawn the soak role");
    run_child(child, Duration::from_secs(60))
}

fn describe(run: &ChildRun) -> String {
    format!(
        "{}\n--- stdout ---\n{}--- stderr ---\n{}",
        describe_exit(&run.status),
        run.stdout,
        run.stderr
    )
}

#[test]
#[ignore = "privileged: SCHED_FIFO; run with --ignored"]
fn keepalive_priority__fifo_keepalive_survives_a_fifo_busy_loop_on_its_core() {
    let run = soak("kp-fifo", KEEPALIVE_PRIORITY);
    assert!(
        run.status.code() == Some(0)
            && run.stdout.contains("survived")
            && !run.stderr.lines().any(|l| l.starts_with("abacus: ")),
        "a SCHED_FIFO keepalive did not survive the soak: {}",
        describe(&run)
    );
}

#[test]
#[ignore = "privileged: SCHED_FIFO; run with --ignored"]
fn keepalive_priority__normal_keepalive_dies_beside_a_fifo_busy_loop() {
    let run = soak("kp-normal", "normal");
    assert!(
        run.status.signal() == Some(libc::SIGABRT)
            && run.stderr.contains("abacus: ProcessClockReaped"),
        "the control did not die, so the soak cannot tell a starved keepalive from a healthy one: {}",
        describe(&run)
    );
}

/// Role: pin to core args[1], connect, set the ProcessClock TTL to args[4] ms and the
/// keepalive priority to args[2] ("normal" or a SCHED_FIFO priority), create an owned
/// interlock, check the keepalive thread's scheduling and affinity, then spin at SCHED_FIFO
/// priority args[3] on the same core for args[5] ms. Prints "survived" if the process clock
/// and the interlock are alive afterwards. Default Abort policy: a starved keepalive aborts.
#[test]
#[ignore = "role: process entry point for keepalive_priority__ tests"]
fn role__keepalive_priority_soak() {
    let Some(args) = role_args("role__keepalive_priority_soak") else {
        return;
    };
    let sock = &args[0];
    let core: usize = args[1].parse().expect("core");
    let keepalive = match args[2].as_str() {
        "normal" => KeepalivePriority::Normal,
        p => KeepalivePriority::Fifo(p.parse().expect("keepalive priority")),
    };
    let busy: i32 = args[3].parse().expect("busy priority");
    let ttl_ms: u64 = args[4].parse().expect("ttl");
    let soak = Duration::from_millis(args[5].parse().expect("soak ms"));

    // Before connect: the keepalive thread inherits this thread's affinity.
    pin_current_thread(&[core]);
    let mut client =
        AbacusClient::connect(Path::new(sock), &unique_name("kp"), &[]).expect("connect");
    client
        .set_process_clock_ttl_ms(ttl_ms)
        .unwrap_or_else(|e| panic!("set ProcessClock TTL: {e}"));
    client
        .set_keepalive_priority(keepalive)
        .unwrap_or_else(|e| panic!("set keepalive priority: {e}"));
    let il = client
        .create_interlock(&unique_name("kp-il"))
        .expect("create interlock");

    let tid = keepalive_tid();
    let (policy, priority) = scheduling_of(tid);
    let cpus = affinity_of(tid);
    println!("keepalive tid={tid} policy={policy} priority={priority} cpus={cpus:?}");
    assert_eq!(
        cpus,
        vec![core],
        "the keepalive did not inherit the connecting thread's affinity"
    );
    match keepalive {
        KeepalivePriority::Fifo(p) => assert_eq!(
            (policy, priority),
            (libc::SCHED_FIFO, i32::from(p)),
            "keepalive scheduling"
        ),
        KeepalivePriority::Normal => {
            assert_eq!(policy, libc::SCHED_OTHER, "keepalive scheduling")
        }
    }

    set_own_scheduling(libc::SCHED_FIFO, busy);
    let end = Instant::now() + soak;
    while Instant::now() < end {
        std::hint::spin_loop();
    }
    set_own_scheduling(libc::SCHED_OTHER, 0);

    // Three keepalive passes to observe a death before declaring survival.
    std::thread::sleep(Duration::from_millis(3 * DEFAULT_TOUCH_INTERVAL_MS));
    assert!(
        !client.process_clock().is_reaped(),
        "process clock reaped during the soak"
    );
    assert!(!il.is_reaped(), "owned interlock reaped during the soak");
    println!("survived");
}

/// The kernel tid of this process's keepalive thread. Its name, "abacus-keepalive", is 16
/// bytes; the kernel keeps the first 15 in comm.
fn keepalive_tid() -> libc::pid_t {
    let mut found = Vec::new();
    for entry in std::fs::read_dir("/proc/self/task").expect("read /proc/self/task") {
        let entry = entry.expect("task entry");
        let comm = std::fs::read_to_string(entry.path().join("comm")).expect("read comm");
        if comm.trim_end() == "abacus-keepaliv" {
            let tid = entry.file_name().to_str().expect("tid").to_string();
            found.push(tid.parse::<libc::pid_t>().expect("tid"));
        }
    }
    assert_eq!(
        found.len(),
        1,
        "expected one keepalive thread, found {found:?}"
    );
    found[0]
}

fn scheduling_of(tid: libc::pid_t) -> (i32, i32) {
    // SAFETY: sched_getscheduler and sched_getparam read a thread's scheduling into a local.
    unsafe {
        let policy = libc::sched_getscheduler(tid);
        assert!(
            policy >= 0,
            "sched_getscheduler({tid}): {}",
            std::io::Error::last_os_error()
        );
        let mut param: libc::sched_param = std::mem::zeroed();
        assert_eq!(
            libc::sched_getparam(tid, &mut param),
            0,
            "sched_getparam({tid}): {}",
            std::io::Error::last_os_error()
        );
        (policy, param.sched_priority)
    }
}

fn affinity_of(tid: libc::pid_t) -> Vec<usize> {
    // SAFETY: a zeroed cpu_set_t is the empty set; sched_getaffinity fills it.
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        assert_eq!(
            libc::sched_getaffinity(tid, std::mem::size_of::<libc::cpu_set_t>(), &mut set),
            0,
            "sched_getaffinity({tid}): {}",
            std::io::Error::last_os_error()
        );
        (0..nproc_online())
            .filter(|&c| libc::CPU_ISSET(c, &set))
            .collect()
    }
}

/// Set the calling thread's policy and priority (pid 0 is the calling thread on Linux).
fn set_own_scheduling(policy: i32, priority: i32) {
    // SAFETY: sched_setscheduler on the calling thread with a local sched_param.
    unsafe {
        let mut param: libc::sched_param = std::mem::zeroed();
        param.sched_priority = priority;
        assert_eq!(
            libc::sched_setscheduler(0, policy, &param),
            0,
            "sched_setscheduler({policy}, {priority}): {}",
            std::io::Error::last_os_error()
        );
    }
}
