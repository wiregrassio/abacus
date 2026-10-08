//! Interlock primitive: layout, create, arm, reap, free, termination, fd sharing, sealing.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use crate::clock::{futex_wait, futex_word, monotonic_now_nanos};
use crate::error::{AllocationStep, Condition};
use crate::interlock::{
    interlock_arm, interlock_create, interlock_dup_fd, interlock_extend, interlock_free,
    interlock_is_terminated, interlock_map, interlock_map_clock, interlock_read_expiration,
    interlock_reap, Interlock, InterlockHandle, CREATION_TTL_NANOS, INTERLOCK_SIZE, SENTINEL,
};

use super::{last_errno, Xorshift};

const MS: u64 = 1_000_000;

fn words(h: &InterlockHandle) -> (u64, u64, u64) {
    let w = h.words();
    (
        w.open_count.load(Ordering::Acquire),
        w.closed_count.load(Ordering::Acquire),
        w.expiration_ns.load(Ordering::Acquire),
    )
}

/// Which futex-waitable word a waiter thread blocks on.
#[derive(Clone, Copy)]
enum Word {
    Open,
    Closed,
}

/// A thread blocked in futex_wait on `which`, expecting `expected`, for up to 2 s. Reports
/// (raw return, errno, elapsed) once it returns.
fn spawn_waiter(
    h: InterlockHandle,
    which: Word,
    expected: u64,
) -> mpsc::Receiver<(libc::c_long, i32, Duration)> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let word = match which {
            Word::Open => &h.words().open_count,
            Word::Closed => &h.words().closed_count,
        };
        let t0 = Instant::now();
        let result = futex_wait(word, futex_word(expected), 2_000 * MS);
        let (ret, errno): (libc::c_long, i32) = match result {
            Ok(()) => (0, 0),
            Err(e) => (-1, e),
        };
        let _ = tx.send((ret, errno, t0.elapsed()));
    });
    rx
}

/// Repeat `stimulus` (idempotent) until both waiters have returned or 2 s pass. A wake that
/// fires before the waiter is inside futex_wait is lost, so the stimulus is re-applied
/// rather than sleeping a fixed time and hoping.
#[allow(clippy::type_complexity)]
fn drive_until_both_return(
    stimulus: impl Fn(),
    rx_a: &mpsc::Receiver<(libc::c_long, i32, Duration)>,
    rx_b: &mpsc::Receiver<(libc::c_long, i32, Duration)>,
) -> (
    Option<(libc::c_long, i32, Duration)>,
    Option<(libc::c_long, i32, Duration)>,
) {
    let deadline = Instant::now() + Duration::from_secs(2);
    let (mut a, mut b) = (None, None);
    while (a.is_none() || b.is_none()) && Instant::now() < deadline {
        stimulus();
        if a.is_none() {
            a = rx_a.try_recv().ok();
        }
        if b.is_none() {
            b = rx_b.try_recv().ok();
        }
        thread::sleep(Duration::from_millis(1));
    }
    (a, b)
}

/// CONTRACTS.md create-call init table: a fresh interlock is (0, 0, now + 100 ms).
#[test]
fn interlock__create_initializes_zero_zero_and_100ms_ttl() {
    let before = monotonic_now_nanos();
    let h = interlock_create().expect("interlock_create");
    let after = monotonic_now_nanos();
    let (open, closed, exp) = words(&h);
    assert_eq!(
        CREATION_TTL_NANOS,
        100 * MS,
        "creation TTL constant is not 100 ms"
    );
    assert_eq!(
        (open, closed),
        (0, 0),
        "fresh counters: open={open} closed={closed}"
    );
    let (lo, hi) = (before + CREATION_TTL_NANOS, after + CREATION_TTL_NANOS);
    assert!(
        exp >= lo && exp <= hi,
        "expiration {exp} outside [{lo}, {hi}] (now + 100 ms window)"
    );
    assert_eq!(interlock_read_expiration(&h), exp);
}

/// CONTRACTS.md interlock shape: three u64 words at offsets 0, 8, 16; 24 bytes; repr(C).
#[test]
fn interlock__size_is_24_and_offsets_are_0_8_16() {
    assert_eq!(std::mem::size_of::<Interlock>(), 24);
    assert_eq!(INTERLOCK_SIZE, 24);
    assert_eq!(std::mem::align_of::<Interlock>(), 8);
    assert_eq!(std::mem::offset_of!(Interlock, open_count), 0);
    assert_eq!(std::mem::offset_of!(Interlock, closed_count), 8);
    assert_eq!(std::mem::offset_of!(Interlock, expiration_ns), 16);
}

/// CONTRACTS.md TTL rules: touch is CAS-max and never decrements. Arm 500 ms, then arm
/// 10 ms: expiration is unchanged by the shorter arm.
#[test]
fn interlock__arm_never_decrements() {
    let h = interlock_create().expect("create");
    let creation_exp = interlock_read_expiration(&h);
    let before = monotonic_now_nanos();
    interlock_arm(&h, 500 * MS).expect("arm 500 ms");
    let e1 = interlock_read_expiration(&h);
    assert!(
        e1 > creation_exp && e1 >= before + 500 * MS,
        "arm 500 ms did not extend: creation={creation_exp} after={e1} before_ns={before}"
    );
    interlock_arm(&h, 10 * MS).expect("arm 10 ms");
    let e2 = interlock_read_expiration(&h);
    assert_eq!(e2, e1, "arm 10 ms moved expiration from {e1} to {e2}");
}

#[test]
fn interlock__arm_with_a_huge_ttl_does_not_terminate() {
    let h = interlock_create().expect("create");
    assert!(interlock_arm(&h, u64::MAX).is_ok());
    assert!(
        !interlock_is_terminated(&h),
        "a huge TTL terminated the interlock"
    );
    assert_eq!(interlock_read_expiration(&h), SENTINEL - 1);
}

/// CONTRACTS.md TTL rules: interlock_arm checks expiration_ns == SENTINEL first and returns
/// InterlockReaped without writing.
#[test]
fn interlock__arm_on_sentinel_returns_reaped() {
    let h = interlock_create().expect("create");
    h.words().expiration_ns.store(SENTINEL, Ordering::Release);
    let r = interlock_arm(&h, 100 * MS);
    assert_eq!(
        r,
        Err(Condition::InterlockReaped),
        "arm on SENTINEL returned {r:?}"
    );
    let exp = interlock_read_expiration(&h);
    assert_eq!(exp, SENTINEL, "arm overwrote the sentinel with {exp}");
}

/// CONTRACTS.md TTL rules: CAS-max under contention. 8 threads arm random TTLs; the final
/// expiration is the largest deadline any thread computed, bounded by each thread's
/// before/after clock reads.
#[test]
fn interlock__arm_is_safe_under_contention() {
    let mut rng = Xorshift::from_env(0xA5A5_0001);
    let seed = rng.seed;
    let before_create = monotonic_now_nanos();
    let h = interlock_create().expect("create");
    let after_create = monotonic_now_nanos();
    let mut lower = before_create + CREATION_TTL_NANOS;
    let mut upper = after_create + CREATION_TTL_NANOS;

    let threads: Vec<_> = (0..8)
        .map(|t| {
            let h = h.clone();
            let thread_seed = rng.next_u64() | 1;
            thread::spawn(move || {
                let mut local = Xorshift {
                    state: thread_seed,
                    seed: thread_seed,
                };
                let (mut lo, mut hi) = (0u64, 0u64);
                for i in 0..200 {
                    let ttl_ms = 1 + local.below(1000);
                    let before = monotonic_now_nanos();
                    let r = interlock_arm(&h, ttl_ms * MS);
                    let after = monotonic_now_nanos();
                    assert!(r.is_ok(), "thread {t} arm {i} (ttl {ttl_ms} ms) failed: {r:?}");
                    let read = interlock_read_expiration(&h);
                    assert!(
                        read >= before + ttl_ms * MS,
                        "thread {t} arm {i}: read {read} below own deadline lower bound {} (seed {seed})",
                        before + ttl_ms * MS
                    );
                    lo = lo.max(before + ttl_ms * MS);
                    hi = hi.max(after + ttl_ms * MS);
                }
                (lo, hi)
            })
        })
        .collect();
    for t in threads {
        let (lo, hi) = t.join().expect("arm thread panicked");
        lower = lower.max(lo);
        upper = upper.max(hi);
    }
    let final_exp = interlock_read_expiration(&h);
    assert!(
        final_exp >= lower && final_exp <= upper,
        "final expiration {final_exp} outside [{lower}, {upper}] (seed {seed})"
    );
}

/// CONTRACTS.md termination: interlock_reap writes SENTINEL to all three words and wakes
/// waiters on open_count and closed_count. A waiter on each futex word returns.
#[test]
fn interlock__reap_stamps_all_three_words_and_wakes() {
    let h = interlock_create().expect("create");
    h.words().open_count.store(5, Ordering::Release);
    h.words().closed_count.store(3, Ordering::Release);
    let rx_open = spawn_waiter(h.clone(), Word::Open, 5);
    let rx_closed = spawn_waiter(h.clone(), Word::Closed, 3);
    let (a, b) = drive_until_both_return(|| interlock_reap(&h), &rx_open, &rx_closed);
    let (open, closed, exp) = words(&h);
    assert_eq!(
        (open, closed, exp),
        (SENTINEL, SENTINEL, SENTINEL),
        "reap did not stamp all words: ({open}, {closed}, {exp})"
    );
    for (name, r) in [("open_count", a), ("closed_count", b)] {
        let (ret, errno, elapsed) = r.unwrap_or_else(|| panic!("{name} waiter never returned"));
        // 0: woken. -1/EAGAIN: the stamp landed before the wait began. Never a timeout.
        assert!(
            ret == 0 || (ret == -1 && errno == libc::EAGAIN),
            "{name} waiter returned ret={ret} errno={errno} after {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "{name} waiter timed out: {elapsed:?}"
        );
    }
}

/// CONTRACTS.md termination: interlock_free writes SENTINEL to expiration_ns only and wakes
/// waiters on both counters; the counters keep their values.
#[test]
fn interlock__free_stamps_expiration_only_and_wakes() {
    let h = interlock_create().expect("create");
    h.words().open_count.store(5, Ordering::Release);
    h.words().closed_count.store(3, Ordering::Release);
    let rx_open = spawn_waiter(h.clone(), Word::Open, 5);
    let rx_closed = spawn_waiter(h.clone(), Word::Closed, 3);
    let (a, b) = drive_until_both_return(|| interlock_free(&h), &rx_open, &rx_closed);
    let (open, closed, exp) = words(&h);
    assert_eq!(
        (open, closed, exp),
        (5, 3, SENTINEL),
        "free changed the wrong words: ({open}, {closed}, {exp})"
    );
    for (name, r) in [("open_count", a), ("closed_count", b)] {
        let (ret, errno, elapsed) = r.unwrap_or_else(|| panic!("{name} waiter never returned"));
        // The counter value is unchanged, so only an explicit wake (0) can end the wait.
        assert_eq!(
            ret, 0,
            "{name} waiter returned ret={ret} errno={errno} after {elapsed:?}"
        );
    }
}

/// CONTRACTS.md termination: any single word at SENTINEL means terminated; a fresh
/// interlock is not.
#[test]
fn interlock__is_terminated_for_each_single_word_sentinel() {
    let fresh = interlock_create().expect("create");
    assert!(
        !interlock_is_terminated(&fresh),
        "fresh interlock reads terminated"
    );
    for (name, store) in [
        (
            "open_count",
            (|w: &Interlock| w.open_count.store(SENTINEL, Ordering::Release)) as fn(&Interlock),
        ),
        ("closed_count", |w| {
            w.closed_count.store(SENTINEL, Ordering::Release)
        }),
        ("expiration_ns", |w| {
            w.expiration_ns.store(SENTINEL, Ordering::Release)
        }),
    ] {
        let h = interlock_create().expect("create");
        store(h.words());
        assert!(
            interlock_is_terminated(&h),
            "SENTINEL in {name} alone not seen as terminated: {:?}",
            words(&h)
        );
    }
}

/// CONTRACTS.md UDS surface: the daemon hands out a dup of the memfd; both fds map the same
/// page. Write through one mapping, read through the other, both directions.
#[test]
fn interlock__dup_fd_maps_the_same_page() {
    let h = interlock_create().expect("create");
    let dup = interlock_dup_fd(&h).expect("dup");
    assert_ne!(
        dup.as_raw_fd(),
        h.as_raw_fd(),
        "dup returned the same fd number"
    );
    let h2 = interlock_map(dup).expect("map dup");
    h.words().open_count.store(42, Ordering::Release);
    assert_eq!(h2.words().open_count.load(Ordering::Acquire), 42);
    h2.words().closed_count.store(7, Ordering::Release);
    assert_eq!(h.words().closed_count.load(Ordering::Acquire), 7);
    let (_, _, exp_via_dup) = words(&h2);
    assert_eq!(exp_via_dup, interlock_read_expiration(&h));
}

/// Negative for interlock_map: a pipe fd fails validation (no seals) before reaching mmap.
#[test]
fn interlock__open_rejects_unmappable_fd() {
    let mut fds = [0i32; 2];
    // SAFETY: pipe(2) into a local array.
    let rc = unsafe { libc::pipe(fds.as_mut_ptr()) };
    assert_eq!(rc, 0, "pipe failed: errno {}", last_errno());
    // SAFETY: fds[1] is a fresh pipe fd we own; fds[0] is closed below.
    let write_end = unsafe { OwnedFd::from_raw_fd(fds[1]) };
    let _read_end = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    match interlock_map(write_end) {
        Err(Condition::AllocationFailed {
            step: AllocationStep::Validate,
            errno,
        }) => {
            assert!(errno != 0, "Validate failure on pipe carried errno 0");
        }
        other => panic!("open of a pipe fd returned {other:?}"),
    }
}

/// The memfd is sealed against shrink and grow in interlock_create, so no fd holder
/// can make every mapping fault. ftruncate returns EPERM; F_GET_SEALS includes shrink,
/// grow, seal. The size is restored immediately after any successful truncate so the
/// mapping is never touched while invalid.
#[test]
fn interlock__memfd_is_sealed_against_shrink_and_grow() {
    let h = interlock_create().expect("create");
    let fd = h.as_raw_fd();
    let size = INTERLOCK_SIZE as libc::off_t;

    // SAFETY: ftruncate on our own memfd; the mapping is not accessed until restored.
    let shrink = unsafe { libc::ftruncate(fd, 0) };
    let shrink_errno = last_errno();
    if shrink == 0 {
        // SAFETY: restore the size before anything else can touch the page.
        let restored = unsafe { libc::ftruncate(fd, size) };
        assert_eq!(
            restored,
            0,
            "could not restore memfd size after shrink: errno {}",
            last_errno()
        );
    }

    // SAFETY: as above; a successful grow is undone at once.
    let grow = unsafe { libc::ftruncate(fd, 4096) };
    let grow_errno = last_errno();
    if grow == 0 {
        // SAFETY: shrink back to the mapped 24 bytes (page 0 stays mapped).
        let restored = unsafe { libc::ftruncate(fd, size) };
        assert_eq!(
            restored,
            0,
            "could not restore memfd size after grow: errno {}",
            last_errno()
        );
    }

    // SAFETY: fcntl query on our own fd.
    let seals = unsafe { libc::fcntl(fd, libc::F_GET_SEALS) };
    let want = libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_SEAL;

    assert!(
        shrink == -1 && shrink_errno == libc::EPERM,
        "ftruncate(fd, 0) returned {shrink} errno {shrink_errno}; expected -1 EPERM (memfd unsealed)"
    );
    assert!(
        grow == -1 && grow_errno == libc::EPERM,
        "ftruncate(fd, 4096) returned {grow} errno {grow_errno}; expected -1 EPERM (memfd unsealed)"
    );
    assert!(
        seals >= 0 && (seals & want) == want,
        "F_GET_SEALS = {seals:#x}; expected shrink|grow|seal = {want:#x}"
    );
}

// -- Received-fd validation tests --

/// Helper: create a memfd with MFD_ALLOW_SEALING, optionally truncate and seal it.
fn test_memfd(size: Option<libc::off_t>, seals: Option<libc::c_int>) -> OwnedFd {
    let raw = unsafe {
        libc::memfd_create(
            c"test-interlock".as_ptr(),
            libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
        )
    };
    assert!(raw >= 0, "memfd_create failed: errno {}", last_errno());
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    if let Some(sz) = size {
        let ret = unsafe { libc::ftruncate(fd.as_raw_fd(), sz) };
        assert_eq!(ret, 0, "ftruncate failed: errno {}", last_errno());
    }
    if let Some(s) = seals {
        let ret = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_ADD_SEALS, s) };
        assert_eq!(ret, 0, "F_ADD_SEALS failed: errno {}", last_errno());
    }
    fd
}

/// A 0-byte memfd (sealed) is rejected at Validate: size mismatch.
#[test]
fn interlock__map_rejects_zero_byte_memfd() {
    let seals = libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_SEAL;
    let fd = test_memfd(None, Some(seals));
    match interlock_map(fd) {
        Err(Condition::AllocationFailed {
            step: AllocationStep::Validate,
            ..
        }) => {}
        other => panic!("expected Validate failure for 0-byte memfd, got {other:?}"),
    }
}

/// A correctly sized but unsealed memfd is rejected at Validate: missing seals.
#[test]
fn interlock__map_rejects_unsealed_memfd() {
    let fd = test_memfd(Some(INTERLOCK_SIZE as libc::off_t), None);
    match interlock_map(fd) {
        Err(Condition::AllocationFailed {
            step: AllocationStep::Validate,
            ..
        }) => {}
        other => panic!("expected Validate failure for unsealed memfd, got {other:?}"),
    }
}

/// A sealed memfd of the wrong size is rejected at Validate.
#[test]
fn interlock__map_rejects_wrong_size_sealed_memfd() {
    let seals = libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_SEAL;
    let fd = test_memfd(Some(4096), Some(seals));
    match interlock_map(fd) {
        Err(Condition::AllocationFailed {
            step: AllocationStep::Validate,
            ..
        }) => {}
        other => panic!("expected Validate failure for wrong-size memfd, got {other:?}"),
    }
}

/// A sealed memfd smaller than 24 bytes is rejected at Validate: size mismatch.
#[test]
fn interlock__map_rejects_undersized_sealed_memfd() {
    let seals = libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_SEAL;
    let fd = test_memfd(Some(16), Some(seals));
    match interlock_map(fd) {
        Err(Condition::AllocationFailed {
            step: AllocationStep::Validate,
            ..
        }) => {}
        other => panic!("expected Validate failure for 16-byte memfd, got {other:?}"),
    }
}

/// A regular file fd is rejected at Validate: F_GET_SEALS fails on non-memfd files.
#[test]
fn interlock__map_rejects_regular_file() {
    let path = c"/dev/null";
    let raw = unsafe { libc::open(path.as_ptr(), libc::O_RDWR) };
    assert!(raw >= 0, "open /dev/null failed: errno {}", last_errno());
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    match interlock_map(fd) {
        Err(Condition::AllocationFailed {
            step: AllocationStep::Validate,
            ..
        }) => {}
        other => panic!("expected Validate failure for regular file fd, got {other:?}"),
    }
}

/// A regular interlock fd (without F_SEAL_FUTURE_WRITE) is rejected by interlock_map_clock.
#[test]
fn interlock__map_clock_rejects_non_clock_fd() {
    let h = interlock_create().expect("create");
    let dup = interlock_dup_fd(&h).expect("dup");
    match interlock_map_clock(dup) {
        Err(Condition::AllocationFailed {
            step: AllocationStep::Validate,
            ..
        }) => {}
        other => panic!(
            "expected Validate failure for non-clock fd passed to map_clock, got {other:?}"
        ),
    }
}

/// interlock_extend on a lapsed deadline returns InterlockReaped and writes SENTINEL.
#[test]
fn interlock__extend_on_lapsed_deadline_terminates_and_returns_reaped() {
    let h = interlock_create().expect("create");
    h.words()
        .expiration_ns
        .store(1, Ordering::Release);
    let r = interlock_extend(&h, 500 * MS);
    assert_eq!(
        r,
        Err(Condition::InterlockReaped),
        "extend on a lapsed deadline returned {r:?}"
    );
    assert_eq!(
        interlock_read_expiration(&h),
        SENTINEL,
        "extend did not write SENTINEL on a lapsed interlock"
    );
    assert!(
        interlock_is_terminated(&h),
        "interlock not terminated after extend on lapsed deadline"
    );
}

/// interlock_extend on SENTINEL returns InterlockReaped without modifying the word.
#[test]
fn interlock__extend_on_sentinel_returns_reaped() {
    let h = interlock_create().expect("create");
    h.words()
        .expiration_ns
        .store(SENTINEL, Ordering::Release);
    let r = interlock_extend(&h, 100 * MS);
    assert_eq!(
        r,
        Err(Condition::InterlockReaped),
        "extend on SENTINEL returned {r:?}"
    );
    assert_eq!(interlock_read_expiration(&h), SENTINEL);
}

/// interlock_extend on a live deadline extends it, same as interlock_arm.
#[test]
fn interlock__extend_on_live_deadline_extends() {
    let h = interlock_create().expect("create");
    let before = monotonic_now_nanos();
    interlock_extend(&h, 500 * MS).expect("extend");
    let exp = interlock_read_expiration(&h);
    assert!(
        exp >= before + 500 * MS,
        "extend did not advance: exp={exp} expected >= {}",
        before + 500 * MS
    );
}

/// interlock_extend never decrements.
#[test]
fn interlock__extend_never_decrements() {
    let h = interlock_create().expect("create");
    interlock_extend(&h, 500 * MS).expect("extend 500");
    let e1 = interlock_read_expiration(&h);
    interlock_extend(&h, 10 * MS).expect("extend 10");
    let e2 = interlock_read_expiration(&h);
    assert_eq!(e2, e1, "extend 10 ms moved expiration from {e1} to {e2}");
}
