//! The interlock: three u64 words in a sealed memfd, and the operations on them.

use std::fmt;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crate::clock::monotonic_now_nanos;
use crate::error::{AllocationStep, Condition, Result};

/// The reserved name of the daemon-owned clock interlock. The daemon refuses to create
/// it; the SDK attaches it on connect.
pub const CLOCK_NAME: &str = "clock";

/// Size of one interlock in bytes: three u64 words.
pub const INTERLOCK_SIZE: usize = 24;
/// TTL a freshly created interlock starts with, in nanoseconds (100 ms).
pub const CREATION_TTL_NANOS: u64 = 100_000_000;
/// The terminal marker. Any word at SENTINEL means the interlock is terminated.
pub const SENTINEL: u64 = u64::MAX;

/// F_SEAL_FUTURE_WRITE: prevents new writable mappings of the memfd. Existing writable
/// mappings (created before this seal) continue to work. Not yet in the libc crate.
const F_SEAL_FUTURE_WRITE: libc::c_int = 0x0010;

// -- Layout --

/// The interlock memory layout. `repr(C)`, 24 bytes, word offsets 0, 8, 16.
#[repr(C)]
pub struct Interlock {
    /// Word 0. Futex-waitable. Monotonic, arbitrary increment.
    pub open_count: AtomicU64,
    /// Word 1. Futex-waitable. Monotonic, arbitrary increment.
    pub closed_count: AtomicU64,
    /// Word 2. CLOCK_MONOTONIC nanoseconds. Future means alive, past means reapable.
    pub expiration_ns: AtomicU64,
}

const _: () = assert!(std::mem::size_of::<Interlock>() == INTERLOCK_SIZE);

// -- Handle --

struct InterlockRegion {
    fd: OwnedFd,
    words: NonNull<Interlock>,
}

impl Drop for InterlockRegion {
    fn drop(&mut self) {
        let ret = unsafe { libc::munmap(self.words.as_ptr().cast(), INTERLOCK_SIZE) };
        if ret != 0 {
            let errno = last_errno();
            eprintln!(
                "abacus: InterlockRegion munmap failed: ptr={:p}, errno={errno}",
                self.words
            );
        }
    }
}

// All access to the mapped words goes through atomics; the mapping itself is shared memory
// designed for concurrent access from several processes.
unsafe impl Send for InterlockRegion {}
unsafe impl Sync for InterlockRegion {}

impl fmt::Debug for InterlockRegion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InterlockRegion")
            .field("fd", &self.fd.as_raw_fd())
            .finish()
    }
}

/// A reference-counted mapping of one interlock. Cloning shares the mapping; the last clone
/// unmaps and closes the fd.
#[derive(Clone, Debug)]
pub struct InterlockHandle(Arc<InterlockRegion>);

impl InterlockHandle {
    pub(crate) fn from_raw(fd: OwnedFd, words: NonNull<Interlock>) -> Self {
        Self(Arc::new(InterlockRegion { fd, words }))
    }

    /// The three words.
    pub fn words(&self) -> &Interlock {
        unsafe { self.0.words.as_ref() }
    }

    /// The memfd backing this mapping.
    pub fn as_raw_fd(&self) -> std::os::fd::RawFd {
        self.0.fd.as_raw_fd()
    }
}

// -- Create / Open --

/// Allocate a new interlock: a sealed memfd of `INTERLOCK_SIZE` bytes, mapped shared, with all
/// counters zero and expiration set to now plus `CREATION_TTL_NANOS`.
///
/// The memfd is sealed against shrink and grow before it is mapped, so no fd holder can ever
/// make a mapping fault (SIGBUS) by resizing it.
pub fn interlock_create() -> Result<InterlockHandle> {
    let raw_fd = memfd_create()?;
    let fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };
    ftruncate(&fd)?;
    seal(&fd)?;
    let handle = mmap_interlock(fd, libc::PROT_READ | libc::PROT_WRITE)?;
    let deadline = monotonic_now_nanos() + CREATION_TTL_NANOS;
    handle
        .words()
        .expiration_ns
        .store(deadline, Ordering::Release);
    Ok(handle)
}

/// Create the clock interlock sealed against client writes. The daemon's own writable
/// mapping is established first, then F_SEAL_FUTURE_WRITE prevents clients from mapping
/// the dup'd fd as writable. Only the clock gets this seal; other interlocks stay writable.
pub fn interlock_create_clock() -> Result<InterlockHandle> {
    let raw_fd = memfd_create()?;
    let fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };
    ftruncate(&fd)?;
    // Map FIRST so the daemon holds a writable mapping before F_SEAL_FUTURE_WRITE.
    let handle = mmap_interlock(fd, libc::PROT_READ | libc::PROT_WRITE)?;
    // Seal: size locked, future writable mappings prevented, seals locked.
    let seals = libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | F_SEAL_FUTURE_WRITE | libc::F_SEAL_SEAL;
    let ret = unsafe { libc::fcntl(handle.as_raw_fd(), libc::F_ADD_SEALS, seals) };
    if ret != 0 {
        return Err(Condition::AllocationFailed {
            step: AllocationStep::Seal,
            errno: last_errno(),
        });
    }
    let deadline = monotonic_now_nanos() + CREATION_TTL_NANOS;
    handle
        .words()
        .expiration_ns
        .store(deadline, Ordering::Release);
    Ok(handle)
}

/// Map a raw interlock received as an fd. Read-write: both sides can touch all three words.
pub fn interlock_map(fd: OwnedFd) -> Result<InterlockHandle> {
    mmap_interlock(fd, libc::PROT_READ | libc::PROT_WRITE)
}

/// Map the clock interlock. Read-only: the daemon writes, clients only read the time.
pub fn interlock_map_clock(fd: OwnedFd) -> Result<InterlockHandle> {
    mmap_interlock(fd, libc::PROT_READ)
}

/// Map a WaitCounter interlock. Read-write (the SDK enforces which words are written).
pub fn interlock_map_counter(fd: OwnedFd) -> Result<InterlockHandle> {
    mmap_interlock(fd, libc::PROT_READ | libc::PROT_WRITE)
}

/// Map a WaitTimer interlock. Read-write (the SDK enforces which words are written).
pub fn interlock_map_timer(fd: OwnedFd) -> Result<InterlockHandle> {
    mmap_interlock(fd, libc::PROT_READ | libc::PROT_WRITE)
}

/// Map a WaitCron interlock. Read-write (the SDK enforces which words are written).
pub fn interlock_map_cron(fd: OwnedFd) -> Result<InterlockHandle> {
    mmap_interlock(fd, libc::PROT_READ | libc::PROT_WRITE)
}

/// Map a WaitBarrier interlock. Read-write (the SDK enforces which words are written).
pub fn interlock_map_barrier(fd: OwnedFd) -> Result<InterlockHandle> {
    mmap_interlock(fd, libc::PROT_READ | libc::PROT_WRITE)
}

/// Extend the expiration to `max(current, now + ttl_nanos)`. CAS-max: never decrements.
/// Returns `InterlockReaped` if expiration already reads SENTINEL.
/// A TTL past the end of the clock arms to SENTINEL - 1, never to SENTINEL.
pub fn interlock_arm(handle: &InterlockHandle, ttl_nanos: u64) -> Result<()> {
    let expiration_ns = &handle.words().expiration_ns;
    // Never arm to SENTINEL: a saturated deadline would terminate the interlock.
    let deadline = monotonic_now_nanos()
        .saturating_add(ttl_nanos)
        .min(SENTINEL - 1);
    loop {
        let current = expiration_ns.load(Ordering::Acquire);
        if current == SENTINEL {
            return Err(Condition::InterlockReaped);
        }
        let target = current.max(deadline);
        if target == current {
            return Ok(());
        }
        match expiration_ns.compare_exchange_weak(
            current,
            target,
            Ordering::Release,
            Ordering::Acquire,
        ) {
            Ok(_) => return Ok(()),
            Err(_) => continue,
        }
    }
}

/// Daemon-side termination: stamp SENTINEL on all three words and wake waiters on both
/// counters.
pub fn interlock_reap(handle: &InterlockHandle) {
    let words = handle.words();
    words.open_count.store(SENTINEL, Ordering::Release);
    words.closed_count.store(SENTINEL, Ordering::Release);
    words.expiration_ns.store(SENTINEL, Ordering::Release);
    crate::clock::futex_wake(&words.open_count);
    crate::clock::futex_wake(&words.closed_count);
}

/// Client-side termination: stamp SENTINEL on expiration and wake waiters on both counters.
/// The daemon finishes cleanup on its next cycle.
pub fn interlock_free(handle: &InterlockHandle) {
    let words = handle.words();
    words.expiration_ns.store(SENTINEL, Ordering::Release);
    crate::clock::futex_wake(&words.open_count);
    crate::clock::futex_wake(&words.closed_count);
}

/// True if any word reads SENTINEL.
pub fn interlock_is_terminated(handle: &InterlockHandle) -> bool {
    let words = handle.words();
    let open = words.open_count.load(Ordering::Acquire);
    let closed = words.closed_count.load(Ordering::Acquire);
    let exp = words.expiration_ns.load(Ordering::Acquire);
    open == SENTINEL || closed == SENTINEL || exp == SENTINEL
}

/// Read the expiration word.
pub fn interlock_read_expiration(handle: &InterlockHandle) -> u64 {
    handle.words().expiration_ns.load(Ordering::Acquire)
}

/// Duplicate the memfd for handout over SCM_RIGHTS. The duplicate shares the seals.
pub fn interlock_dup_fd(handle: &InterlockHandle) -> Result<OwnedFd> {
    let raw = unsafe { libc::dup(handle.as_raw_fd()) };
    if raw < 0 {
        return Err(Condition::AllocationFailed {
            step: AllocationStep::Dup,
            errno: last_errno(),
        });
    }
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

// -- Internal helpers --

fn last_errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

fn memfd_create() -> Result<i32> {
    let raw_fd = unsafe {
        libc::memfd_create(
            c"abacus-interlock".as_ptr(),
            libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
        )
    };
    if raw_fd < 0 {
        return Err(Condition::AllocationFailed {
            step: AllocationStep::MemfdCreate,
            errno: last_errno(),
        });
    }
    Ok(raw_fd)
}

fn ftruncate(fd: &OwnedFd) -> Result<()> {
    let ret = unsafe { libc::ftruncate(fd.as_raw_fd(), INTERLOCK_SIZE as libc::off_t) };
    if ret != 0 {
        return Err(Condition::AllocationFailed {
            step: AllocationStep::Ftruncate,
            errno: last_errno(),
        });
    }
    Ok(())
}

// Seal the size in both directions, then seal the seals. Shrinking a mapped memfd makes every
// mapping of it fault on the next access; sealing turns that into EPERM for the caller.
fn seal(fd: &OwnedFd) -> Result<()> {
    let seals = libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_SEAL;
    let ret = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_ADD_SEALS, seals) };
    if ret != 0 {
        return Err(Condition::AllocationFailed {
            step: AllocationStep::Seal,
            errno: last_errno(),
        });
    }
    Ok(())
}

fn mmap_interlock(fd: OwnedFd, prot: libc::c_int) -> Result<InterlockHandle> {
    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            INTERLOCK_SIZE,
            prot,
            libc::MAP_SHARED,
            fd.as_raw_fd(),
            0,
        )
    };
    if ptr == libc::MAP_FAILED {
        return Err(Condition::AllocationFailed {
            step: AllocationStep::Mmap,
            errno: last_errno(),
        });
    }
    let words = NonNull::new(ptr.cast::<Interlock>()).ok_or(Condition::AllocationFailed {
        step: AllocationStep::Mmap,
        errno: 0,
    })?;
    Ok(InterlockHandle::from_raw(fd, words))
}
