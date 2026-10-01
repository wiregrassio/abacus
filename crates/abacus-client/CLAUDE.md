<purpose>

# crates/abacus-client/

The Abacus Rust client SDK: daemon-backed shared-memory interlocks and wait primitives.

</purpose>

<dependencies>

## Dependencies
- Workspace crates:
  - `abacus-core`: shared-memory interlock lifecycle operations, clocks, futex support, sentinels, and transport/error types.
  - `abacus-wire`: Unix-domain-socket protocol encoding, response validation, daemon error codes, and received file-descriptor handling.
- External packages:
  - `libc`: Unix socket peeking, errno handling, and futex timeout classification.
- Rust standard library, as used by `src/`: Unix-domain sockets, file descriptors, atomics, threads, synchronization, and timing.

</dependencies>

<consumed-by>

## Consumed By
`abacus-tests` imports this crate for integration testing. End-user applications are the primary consumers.

</consumed-by>

<data-flow>

## Data Flow
- SDK consumers create an `AbacusClient`, which connects to the Abacus daemon through a blocking Unix-domain socket.
- The client encodes create or attach requests through `abacus-wire`, receives a protocol response and shared-memory file descriptor, and maps the descriptor through `abacus-core` into an interlock handle.
- Typed SDK wrappers expose interlocks, counters, timers, cron waits, barriers, races, and process-clock functionality.
- Local operations atomically update shared lifecycle and expiration state; futex operations block until daemon-written or peer-written state changes occur.
- A keepalive worker refreshes registered interlock expiration timestamps and updates process-clock state from the daemon clock.
- Callers receive typed wait outcomes, timeout/reaped errors, or process termination when the configured fatal timer policy is triggered.

</data-flow>

<known-hazards>

## Known Hazards
- **HIGH:** `attach_interlock` maps any tier read-write, and attaching a ProcessClock is read-write: the daemon hands out the same descriptor for every tier, so read-only views are SDK convention. `attach_wait_counter` checks the daemon-reported tier before mapping and refuses anything but tier 1 (`InvalidRequest`).
- **HIGH:** `WaitTimer` defaults to `TimeoutPolicy::Abort`; missing its fatal delivery margin terminates the process rather than returning an error.
- **HIGH:** `WaitRace` performs SDK-side 1 ms polling rather than daemon-side synchronization, adding scheduling overhead proportional to raced counters and failing the entire race if any member is reaped.
- **MEDIUM:** Shared counter correctness depends on `SENTINEL` remaining globally reserved; direct or wrapping writes that use it can make a live interlock appear reaped.
- **MEDIUM:** `WaitCounter::wait_until` uses a CAS-max target, so concurrent users of one handle cannot independently lower or isolate targets; one waiter can observe another waiter's delivery. A `WaitTimer` admits one waiter at a time and refuses a concurrent second wait (`InvalidRequest`).
- **MEDIUM:** Keepalive liveness depends on its single worker thread running before TTL expiry; sufficiently long process stalls allow daemon reaping.
- **LOW:** `AbacusClient::is_connected` detects socket EOF, and reads false once a failed send or receive has poisoned the connection, but cannot establish that the daemon remains able to serve requests.

</known-hazards>

<files>

## Files
| File | Purpose |
|---|---|
| `Cargo.toml` | Declares the `abacus-client` SDK package and its workspace dependencies on core, wire, and libc support. |

</files>

<notes>

## Notes
- The package boundary is deliberately thin: protocol and shared-memory mechanics are supplied by `abacus-wire` and `abacus-core`, while `src/` composes them into a consumer-facing SDK.
- The daemon owns allocation, naming, descriptor provisioning, and tier evaluation; the client owns local mapped-handle lifetime, TTL refresh, and blocking behavior.

</notes>

<reference>

## Reference
Full contracts, units, symbol table, test inventory, and cross-boundary verification: see `README.md`.

</reference>
