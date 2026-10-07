<purpose>
# src/
Linux shared-memory interlock primitive for `abacus-core`, providing monotonic time and futex helpers, sealed memfd lifecycle operations, and error types shared by the daemon and SDK.
</purpose>

<dependencies>
## Dependencies
Imports sibling `clock` and `error` modules. Depends on `libc` for Linux syscalls and Rust standard-library atomic, fd, synchronization, and formatting APIs.
</dependencies>

<consumed-by>

## Consumed By
Consumed by the parent package (`crates/abacus-core`).

</consumed-by>

<data-flow>
## Data Flow
- Inputs: Linux clock, futex, memfd, mmap, sealing, and fd-duplication syscalls.
- Transformations: creates or maps three-word shared-memory interlocks, manages expiration and terminal state atomically, and exposes futex wait and wake operations.
- Outputs: `InterlockHandle` mappings, duplicated memfds for SCM_RIGHTS handout, daemon-domain conditions, and transport-related error vocabulary.
</data-flow>

<known-hazards>
## Known Hazards
- HIGH: `interlock_map*` maps supplied fds without validating backing-object size, a malformed or truncated fd can fault when mapped words are accessed.
- LOW: `interlock_create` adds the creation TTL with unchecked `u64` arithmetic, unlike saturating `interlock_arm`; clock overflow can create an immediately expired deadline.
</known-hazards>

<files>
## Files
| File | Purpose |
|------|---------|
| `clock.rs` | Provides monotonic nanosecond time, duration conversion, expiration comparison, and Linux futex operations. |
| `error.rs` | Defines daemon conditions, allocation steps, protocol faults, transport errors, and startup errors. |
| `interlock.rs` | Defines the shared three-word interlock layout, sealed memfd allocation and mapping, lifecycle operations, and fd duplication. |
| `lib.rs` | Declares the Linux-only core crate and exports its public modules. |
</files>

<subdirectories>
## Subdirectories
| Directory | Purpose |
|-----------|---------|
| `tests/` | Unit-test modules for clock, error, and interlock contracts. |
</subdirectories>