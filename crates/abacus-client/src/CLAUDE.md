<purpose>
# src/
Public Rust SDK for connecting to the Abacus daemon, mapping shared-memory interlocks, maintaining their TTLs, and exposing typed wait primitives.
</purpose>

<dependencies>
## Dependencies
Imports `abacus-core` for shared-memory interlocks, clocks, futexes, and transport errors; `abacus-wire` for UDS request framing, responses, and passed file descriptors; `libc` and the Rust standard library for Unix sockets, scheduling, atomics, and threads. Internal modules route client-created handles through shared operations, keepalive, types, and wait implementations.
</dependencies>

<consumed-by>

## Consumed By
Consumed by the parent package (`crates/abacus-client`).

</consumed-by>

<data-flow>
## Data Flow
- `AbacusClient` sends typed create and attach requests over a Unix domain socket to the daemon.
- Daemon responses return protocol data and shared-memory file descriptors, which the client maps into `InterlockHandle`s.
- Typed handles read and mutate shared atomic words, then wait through futexes.
- One shared `Keepalive` thread refreshes registered handle TTLs and updates the client ProcessClock.
- Wait operations return `WaitResult` or `SdkError`, including daemon death, reaping, timeout, and transport failures.
</data-flow>

<known-hazards>
## Known Hazards
- HIGH: `TimeoutPolicy::Abort` is the default, a missed WaitTimer fatal margin or lost process liveness aborts the host process.
- HIGH: Keepalive scheduling and CPU affinity inherit from the connecting thread. A non-real-time keepalive can starve behind real-time work and cause reaping.
- MEDIUM: `AttachedInterlock::free` writes the termination sentinel despite attached expiration being read-only, terminating the shared interlock for all holders.
- MEDIUM: `WaitRace` polls every millisecond and is an SDK-side stopgap pending daemon-side `WaitOr`.
</known-hazards>

<files>
## Files
| File | Purpose |
|------|---------|
| `client.rs` | Defines `AbacusClient`, SDK errors, UDS transport, daemon request handling, and typed handle creation and attachment. |
| `handle_ops.rs` | Shared atomic reads, sentinel-aware increments, lifecycle reads, TTL touches, and futex wait loops. |
| `interlock.rs` | Implements creator, attached, read-only counter, and daemon clock handle types. |
| `lib.rs` | Declares SDK modules and re-exports the public API. |
| `process_clock.rs` | Exposes the client ProcessClock liveness beacon and metadata. |
| `tests.rs` | Cross-module unit tests for pure SDK types, constants, errors, and state derivation. |
| `touch.rs` | Implements the shared keepalive thread, registrations, liveness monitoring, and thread scheduling control. |
| `types.rs` | Defines SDK policies, result and state types, defaults, and pure state classification helpers. |
| `wait_barrier.rs` | Implements daemon-evaluated multi-condition barriers. |
| `wait_counter.rs` | Implements target-based waits on another interlock word. |
| `wait_cron.rs` | Implements recurring daemon-driven grid-aligned timer waits. |
| `wait_race.rs` | Implements SDK-side first-completer polling across WaitCounters. |
| `wait_timer.rs` | Implements clock-based waits with fatal-margin timeout handling and one-waiter enforcement. |
</files>