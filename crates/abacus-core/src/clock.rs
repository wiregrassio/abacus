//! CLOCK_MONOTONIC reads and futex primitives over interlock words.

use std::sync::atomic::AtomicU64;

/// Nanoseconds per second.
pub const NANOS_PER_SEC: u64 = 1_000_000_000;
/// Nanoseconds per millisecond.
pub const NANOS_PER_MS: u64 = 1_000_000;

/// Current CLOCK_MONOTONIC time in nanoseconds.
///
/// Aborts the process if the clock cannot be read or does not fit a u64: every interlock
/// deadline is derived from this value, so a broken clock is unrecoverable.
pub fn monotonic_now_nanos() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let ret = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    if ret != 0 {
        eprintln!("abacus: clock_gettime failed; aborting");
        std::process::abort();
    }
    match nanos_from_timespec(&ts) {
        Some(n) => n,
        None => {
            eprintln!(
                "abacus: CLOCK_MONOTONIC returned unrepresentable timespec \
                 (tv_sec={}, tv_nsec={}); aborting",
                ts.tv_sec, ts.tv_nsec
            );
            std::process::abort();
        }
    }
}

fn nanos_from_timespec(ts: &libc::timespec) -> Option<u64> {
    let secs = u64::try_from(ts.tv_sec).ok()?;
    let nsecs = u64::try_from(ts.tv_nsec).ok()?;
    secs.checked_mul(NANOS_PER_SEC)?.checked_add(nsecs)
}

/// Convert a nanosecond duration to a `timespec`. Used for futex and ppoll timeouts.
pub fn timespec_from_nanos(nanos: u64) -> libc::timespec {
    libc::timespec {
        tv_sec: (nanos / NANOS_PER_SEC) as libc::time_t,
        tv_nsec: (nanos % NANOS_PER_SEC) as libc::c_long,
    }
}

/// True while `expiration_ns` is still in the future relative to `now`. Equal means expired.
pub fn expiration_alive(expiration_ns: u64, now: u64) -> bool {
    expiration_ns > now
}

/// Convert milliseconds to nanoseconds, saturating at `u64::MAX`.
pub fn ms_to_nanos(ms: u64) -> u64 {
    ms.saturating_mul(NANOS_PER_MS)
}

/// Wake every futex waiter blocked on `word`.
///
/// Aborts the process if the wake syscall fails: a broken futex is unrecoverable
/// because every wait path depends on it.
pub fn futex_wake(word: &AtomicU64) {
    let ptr = futex_addr(word);
    let rc = unsafe {
        libc::syscall(
            libc::SYS_futex,
            ptr,
            libc::FUTEX_WAKE,
            i32::MAX,
            std::ptr::null::<libc::timespec>(),
            std::ptr::null::<u32>(),
            0u32,
        )
    };
    if rc < 0 {
        let err = std::io::Error::last_os_error();
        eprintln!("abacus: futex wake failed: {err}; aborting");
        std::process::abort();
    }
}

/// Block until the low 32 bits of `word` differ from `expected_lo32`, or until
/// `timeout_nanos` nanoseconds elapse. A timeout of 0 returns immediately (the kernel
/// sees a zero timespec and reports `ETIMEDOUT` at once).
///
/// Returns `Ok(())` on a wake. Returns `Err(errno)` otherwise, with errno captured
/// immediately after the syscall: `ETIMEDOUT` when the timeout elapsed, `EAGAIN` when the
/// word already differed from `expected_lo32`, `EINTR` when a signal interrupted the wait.
pub fn futex_wait(word: &AtomicU64, expected_lo32: u32, timeout_nanos: u64) -> Result<(), i32> {
    let ptr = futex_addr(word);
    let ts = timespec_from_nanos(timeout_nanos);
    let ts_ptr = &ts as *const libc::timespec;
    let rc = unsafe {
        libc::syscall(
            libc::SYS_futex,
            ptr,
            libc::FUTEX_WAIT,
            expected_lo32,
            ts_ptr,
            std::ptr::null::<u32>(),
            0u32,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        // Capture errno before anything else can touch it.
        Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(0))
    }
}

/// Whether a `futex_wait` result is retriable or permanently broken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FutexOutcome {
    /// The wait returned normally (woken, timed out, word changed, or signal): loop again.
    Retry,
    /// A permanent failure: the syscall will never succeed on this word.
    Fatal(i32),
}

/// Classify a `futex_wait` result. `Ok`, `EAGAIN`, `EINTR`, and `ETIMEDOUT` are retriable;
/// every other errno (`EPERM`, `EFAULT`, `EINVAL`, `ENOSYS`, ...) is fatal.
pub fn classify_futex_result(result: Result<(), i32>) -> FutexOutcome {
    match result {
        Ok(()) => FutexOutcome::Retry,
        Err(e) if e == libc::EAGAIN || e == libc::EINTR || e == libc::ETIMEDOUT => {
            FutexOutcome::Retry
        }
        Err(e) => FutexOutcome::Fatal(e),
    }
}

/// The low 32 bits of a counter: the part a futex compares.
pub fn futex_word(value: u64) -> u32 {
    value as u32
}

// The futex syscall compares 32 bits. Casting the u64 word's address to `*const u32` addresses
// the low half only on little-endian targets; on big-endian it would address the high half and
// every wait would block on the wrong bits.
const _: () = assert!(
    cfg!(target_endian = "little"),
    "futex_addr assumes little-endian"
);

fn futex_addr(word: &AtomicU64) -> *const u32 {
    (word as *const AtomicU64).cast::<u32>()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timespec_split() {
        let ts = timespec_from_nanos(2 * NANOS_PER_SEC + 5);
        assert_eq!(ts.tv_sec, 2);
        assert_eq!(ts.tv_nsec, 5);
    }

    #[test]
    fn classify_futex_retry_and_fatal() {
        assert_eq!(classify_futex_result(Ok(())), FutexOutcome::Retry);
        assert_eq!(
            classify_futex_result(Err(libc::EAGAIN)),
            FutexOutcome::Retry
        );
        assert_eq!(
            classify_futex_result(Err(libc::EINTR)),
            FutexOutcome::Retry
        );
        assert_eq!(
            classify_futex_result(Err(libc::ETIMEDOUT)),
            FutexOutcome::Retry
        );
        assert_eq!(
            classify_futex_result(Err(libc::EPERM)),
            FutexOutcome::Fatal(libc::EPERM)
        );
        assert_eq!(
            classify_futex_result(Err(libc::EFAULT)),
            FutexOutcome::Fatal(libc::EFAULT)
        );
        assert_eq!(
            classify_futex_result(Err(libc::EINVAL)),
            FutexOutcome::Fatal(libc::EINVAL)
        );
        assert_eq!(
            classify_futex_result(Err(libc::ENOSYS)),
            FutexOutcome::Fatal(libc::ENOSYS)
        );
    }
}
