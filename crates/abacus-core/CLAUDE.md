<purpose>

# crates/abacus-core/

</purpose>

<dependencies>

## Dependencies
- External package: `libc` (workspace dependency), used by `src` for Linux syscalls, constants, and errno handling.
- Rust standard library, used within `src`.
- Linux kernel facilities required by `src`: `memfd_create`, file seals, shared memory mapping, futexes, and `CLOCK_MONOTONIC`.

</dependencies>

<consumed-by>

## Consumed By
Consumed by `abacus-wire`, `abacus-daemon`, and `abacus-client` (see their `Cargo.toml`).

</consumed-by>

<data-flow>

## Data Flow
- Callers create or receive owned file descriptors for fixed-size interlock memfds through `src`.
- The crate maps each memfd into a shared 24-byte atomic interlock layout.
- SDK and daemon-side callers read and update shared counters and expiration timestamps; futex waiters sleep on counter words and are woken by writers.
- Monotonic time is converted to nanosecond deadlines; lifecycle termination writes a shared sentinel value.
- Allocation, mapping, transport/protocol, and startup failures leave through the exported shared error types.

</data-flow>

<known-hazards>

## Known Hazards
- HIGH: Linux futex waits compare only the low 32 bits of an `AtomicU64`; a change exclusively in a counter's upper 32 bits may leave a waiter asleep until another wake or timeout.
- HIGH: Futex address derivation assumes little-endian `u64` layout; big-endian targets would wait on the wrong half-word.
- HIGH: The public shared-word accessor exposes all interlock atomics; ownership and type-specific write discipline are enforced only by higher layers.
- MEDIUM: Initial interlock TTL creation uses ordinary `u64` addition, which can overflow near the monotonic timestamp maximum.
- MEDIUM: The crate is Linux-specific and requires kernel support for memfd seals, including `F_SEAL_FUTURE_WRITE` for clock handouts.
- MEDIUM: Freeing an interlock marks only its expiration word as terminal; consumers must recognize termination when any word is the sentinel rather than expecting counters to be overwritten.
- LOW: Futex wake syscall failures are intentionally ignored and cannot be reported to callers.

</known-hazards>

<files>

## Files
| File | Purpose |
|---|---|
| `Cargo.toml` | Defines the `abacus-core` package and its workspace-provided `libc` dependency. |

</files>

<notes>

## Notes
The core interlock is shared-memory protocol infrastructure rather than an ordinary Rust-only data structure. Its binary layout must remain compatible between every daemon and SDK participant that maps the same memfd.

</notes>

<reference>

## Reference
Full contracts, units, symbol table, test inventory, and cross-boundary verification: see `README.md`.

</reference>
