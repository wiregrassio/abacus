<purpose>

# crates/abacus-core/src/

Abacus RTS's Linux-only shared core implements sealed memfd-backed interlocks, monotonic-clock and futex primitives, and the error vocabulary used across daemon and SDK boundaries.

</purpose>

<dependencies>

## Dependencies
- Rust standard library: atomics, ownership-aware file descriptors, `Arc`, formatting, and error traits.
- External package: `libc` for Linux syscalls, `CLOCK_MONOTONIC`, futex constants, memfd/seal operations, `mmap`, and errno values.
- Sibling modules:
  - `crate::clock` supplies monotonic time and futex wake support to `interlock`.
  - `crate::error` supplies allocation and lifecycle error types to `interlock`.
- Linux kernel facilities: `memfd_create`, file seals, shared `mmap`, futexes, and `CLOCK_MONOTONIC`.

</dependencies>

<consumed-by>

## Consumed By
- `abacus-wire`, `abacus-daemon`, and `abacus-client` import these modules for interlock layout, clock, futex, and error types.

</consumed-by>

<data-flow>

## Data Flow
- Callers create an interlock as a fixed 24-byte sealed memfd, or receive one as an owned file descriptor.
- `interlock` maps that memfd as an `Interlock` containing two shared atomic counters and one shared expiration timestamp.
- Clients and daemon update/read atomic words through `InterlockHandle`; counter waiters block through `clock::futex_wait` and are awakened with `futex_wake`.
- Creation and arming derive deadlines from `CLOCK_MONOTONIC` nanoseconds; daemon or client termination writes the `SENTINEL` marker.
- Allocation, mapping, protocol-adjacent, I/O, and startup failures leave through shared `Condition`, `TransportError`, and `StartupError` types.

</data-flow>

<known-hazards>

## Known Hazards
- HIGH: Linux futexes compare only the low 32 bits of each `AtomicU64`; changes confined to a counter's upper 32 bits do not make an existing futex wait ineligible and can leave waiters asleep until a wake or timeout.
- HIGH: Futex address derivation assumes little-endian layout; on big-endian targets the kernel would inspect the high half of each `u64` instead of the logical low futex word.
- HIGH: `Interlock::words()` exposes all three shared atomics publicly, while counter ownership and type-specific write restrictions are only documented/enforced by higher SDK layers rather than this crate.
- MEDIUM: `interlock_create` and `interlock_create_clock` add the initial TTL with ordinary `u64` addition, unlike `interlock_arm`'s saturating addition; a near-`u64::MAX` monotonic timestamp can overflow or panic in debug builds.
- MEDIUM: This directory is Linux-specific at compile time and depends on kernel support for memfd seals, including `F_SEAL_FUTURE_WRITE` for the clock handout model.
- MEDIUM: `interlock_free` marks only expiration as terminal; consumers must treat termination as "any word is `SENTINEL`" and must not assume counter values are immediately overwritten.
- LOW: `futex_wake` intentionally ignores syscall failure, so invalid mappings or kernel-level wake failures are not surfaced to its caller.

</known-hazards>

<files>

## Files
| File | Purpose |
|---|---|
| `clock.rs` | Provides monotonic nanosecond time conversion, expiration checks, and Linux futex wait/wake operations over interlock words. |
| `error.rs` | Defines daemon-domain conditions, transport/protocol errors, startup errors, conversions, and display behavior. |
| `interlock.rs` | Defines the fixed shared-memory interlock layout and its creation, mapping, lifecycle, sealing, arming, and fd-duplication operations. |
| `lib.rs` | Declares the Linux-only core crate and exports its clock, error, and interlock modules. |

</files>

<notes>

## Notes
The 24-byte `Interlock` layout is an ABI shared through a memfd mapping, not merely an in-process Rust representation. Its word order, atomic widths, offsets, sentinel value, and futex low-word convention are therefore compatibility-critical.

Size seals are applied before normal interlock mapping is handed out so another fd holder cannot resize the memfd and induce SIGBUS in an existing mapping. Clock interlocks additionally use `F_SEAL_FUTURE_WRITE` after the daemon obtains its writable mapping.

</notes>

<reference>

## Reference

</reference>
