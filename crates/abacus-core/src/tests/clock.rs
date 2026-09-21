//! Monotonic clock, expiration arithmetic, futex helpers, endianness assumption.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

use crate::clock::{
    expiration_alive, futex_wait, futex_wake, futex_word, monotonic_now_nanos, ms_to_nanos,
};

const MS: u64 = 1_000_000;

/// abacus-core CLAUDE.md contracts: monotonic_now_nanos never goes backward.
#[test]
fn clock__monotonic_now_is_monotonic_across_10k_reads() {
    let mut prev = monotonic_now_nanos();
    assert!(prev > 0, "monotonic clock read zero");
    for i in 0..10_000 {
        let now = monotonic_now_nanos();
        assert!(now >= prev, "read {i}: {now} < previous {prev}");
        prev = now;
    }
}

/// LIFECYCLE.md derived properties: ttl = expiration - clock; zero or negative is expired.
/// Equal is dead, one nanosecond later is alive.
#[test]
fn clock__expiration_alive_boundary() {
    assert!(!expiration_alive(100, 100), "equal expiration reads alive");
    assert!(
        expiration_alive(101, 100),
        "expiration one ns ahead reads dead"
    );
    assert!(!expiration_alive(99, 100), "past expiration reads alive");
    assert!(!expiration_alive(0, 0), "zero/zero reads alive");
}

/// SURFACE.md wait_open/wait_close: a waiter blocked in futex_wait returns when a wake is
/// delivered on the word, without the value changing. The wake is repeated until the waiter
/// is observed inside the wait, so a wake lost before entry cannot stall the test.
#[test]
fn clock__futex_wait_returns_on_wake() {
    let word = Arc::new(AtomicU64::new(0));
    let (tx, rx) = mpsc::channel();
    let w = word.clone();
    thread::spawn(move || {
        let t0 = Instant::now();
        let ret = futex_wait(&w, 0, 2_000 * MS);
        let _ = tx.send((ret, t0.elapsed()));
    });
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut got = None;
    while got.is_none() && Instant::now() < deadline {
        futex_wake(&word);
        got = rx.try_recv().ok();
        thread::sleep(Duration::from_millis(1));
    }
    let (ret, elapsed) = got.expect("waiter never returned within 2 s despite wakes");
    assert!(
        ret.is_ok(),
        "expected Ok (wake), got {ret:?} after {elapsed:?}"
    );
    assert!(elapsed < Duration::from_secs(1), "wake took {elapsed:?}");
}

/// abacus-core CLAUDE.md contracts: a timed futex_wait with no wake returns
/// Err(ETIMEDOUT) after the timeout elapses.
#[test]
fn clock__futex_wait_returns_etimedout() {
    let word = AtomicU64::new(0);
    let t0 = Instant::now();
    let ret = futex_wait(&word, 0, 5 * MS);
    let elapsed = t0.elapsed();
    assert_eq!(ret, Err(libc::ETIMEDOUT), "futex_wait returned {ret:?}");
    assert!(
        elapsed >= Duration::from_millis(5),
        "returned early: {elapsed:?}"
    );
}

/// abacus-core CLAUDE.md contracts: if the word no longer matches the expected value the
/// kernel returns EAGAIN at once instead of sleeping.
#[test]
fn clock__futex_wait_returns_eagain_when_value_already_changed() {
    let word = AtomicU64::new(1);
    let t0 = Instant::now();
    let ret = futex_wait(&word, 0, 500 * MS);
    let elapsed = t0.elapsed();
    assert_eq!(ret, Err(libc::EAGAIN), "futex_wait returned {ret:?}");
    assert!(
        elapsed < Duration::from_millis(100),
        "EAGAIN path slept {elapsed:?}"
    );
}

/// abacus-core CLAUDE.md contracts: futex_wait/futex_wake operate on the low 32 bits of a
/// u64 word (Linux kernel constraint, the documented known limitation). A change confined
/// to the high word is invisible to the kernel's compare: the wait sleeps to timeout. A
/// change in the low word returns EAGAIN at once.
#[test]
fn clock__futex_operates_on_low_32_bits() {
    assert_eq!(
        futex_word(1 << 32),
        0,
        "futex_word did not drop the high word"
    );
    assert_eq!(futex_word((1 << 32) | 7), 7);

    let word = AtomicU64::new(1 << 32);
    let t0 = Instant::now();
    let ret = futex_wait(&word, futex_word(0), 20 * MS);
    let elapsed = t0.elapsed();
    assert!(
        ret == Err(libc::ETIMEDOUT) && elapsed >= Duration::from_millis(20),
        "high-word-only change: ret={ret:?} elapsed={elapsed:?}; expected a full \
         20 ms sleep to ETIMEDOUT (the kernel compares only the low 32 bits)"
    );

    word.store(1, Ordering::Release);
    let ret = futex_wait(&word, futex_word(0), 500 * MS);
    assert!(
        ret == Err(libc::EAGAIN),
        "low-word change: ret={ret:?}; expected EAGAIN"
    );
}

/// futex_addr casts the u64 pointer to u32 and points at the low word only on little-endian
/// targets. A compile-time assert guards this beside futex_addr; this test states the
/// assumption at runtime so a big-endian build is visibly wrong.
#[test]
// The constant is the claim: the assertion documents the target assumption in the test log.
#[allow(clippy::assertions_on_constants)]
fn clock__little_endian_assumed() {
    assert!(
        cfg!(target_endian = "little"),
        "abacus-core assumes little-endian for futex low-32-bit addressing"
    );
    let v: u64 = 0x1122_3344_5566_7788;
    let first_byte = v.to_ne_bytes()[0];
    assert_eq!(first_byte, 0x88, "native byte order is not little-endian");
}

/// ms_to_nanos scales by 1e6 and saturates instead of wrapping.
#[test]
fn clock__ms_to_nanos_scales_and_saturates() {
    assert_eq!(ms_to_nanos(0), 0);
    assert_eq!(ms_to_nanos(1), MS);
    assert_eq!(ms_to_nanos(1_500), 1_500 * MS);
    assert_eq!(
        ms_to_nanos(u64::MAX),
        u64::MAX,
        "u64::MAX ms did not saturate"
    );
}

/// futex_wake with no waiter is a harmless syscall (the daemon wakes the clock every cycle
/// whether or not anyone is waiting, CONTRACTS.md daemon contract).
#[test]
fn clock__futex_wake_without_waiters_is_harmless() {
    let word = AtomicU64::new(9);
    for _ in 0..1000 {
        futex_wake(&word);
    }
    assert_eq!(word.load(Ordering::Acquire), 9, "wake changed the word");
}
