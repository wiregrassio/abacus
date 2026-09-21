<purpose>

# crates/abacus-client/src/

Implements the Rust SDK: daemon connection, interlock creation/attachment, TTL maintenance, and typed blocking wait primitives.

</purpose>

<dependencies>

## Dependencies
- Sibling workspace crates:
  - `abacus_core`: shared-memory interlock handles and lifecycle operations, monotonic clock helpers, futex operations, sentinels, and transport/error types.
  - `abacus_wire`: UDS request/response encoding, frame validation, daemon error codes, and passed-file-descriptor handling.
- External/system packages:
  - Rust standard library: Unix-domain sockets, owned file descriptors, threads, atomics, synchronization, timing.
  - `libc`: socket peeking, errno values, and futex timeout classification.

</dependencies>

<consumed-by>

## Consumed By
- `abacus-tests` and end-user applications consume this SDK.

</consumed-by>

<data-flow>

## Data Flow
- `Abacus RTSClient` connects to the daemon over a blocking Unix-domain socket, sends encoded create/attach requests, receives wire responses plus one shared-memory file descriptor, and maps that descriptor into an `InterlockHandle`.
- Typed constructors wrap mapped handles as bare `Interlock`, `WaitCounter`, `WaitTimer`, `WaitCron`, `WaitBarrier`, or `ProcessClock` objects.
- Handles read and mutate atomically shared `open_count`, `closed_count`, and `expiration_ns` words, using futex waits/wakes for local blocking synchronization.
- A client-wide `Keepalive` background thread periodically arms registered interlocks with future expiration timestamps; `ProcessClock` registrations also copy the daemon clock into `open_count`.
- Wait primitives observe daemon-written completion values and return `WaitResult`, timeout/reaped errors, or,in the configured fatal timer policy,abort the process.

</data-flow>

<known-hazards>

## Known Hazards
- **HIGH:** `attach_wait_counter` receives no tier information from the daemon and maps any attached interlock as a read-only `WaitCounter`; callers must independently know that the name identifies tier 1.
- **HIGH:** `WaitTimer` defaults to `TimeoutPolicy::Abort`; a missed fatal margin terminates the entire process rather than returning an error.
- **HIGH:** `WaitRace` is SDK-side polling at 1 ms rather than daemon-side synchronization; it adds wake/scheduling overhead proportional to raced counters and treats any reaped member as failure of the entire race.
- **MEDIUM:** Shared counter semantics rely on the `SENTINEL` value remaining reserved globally; direct or wrapping writes that violate this convention can make live interlocks appear reaped.
- **MEDIUM:** `WaitCounter::wait_until` and `WaitTimer` use CAS-max targets, so concurrent users of the same handle cannot independently lower or isolate requested targets; one waiter may observe another waiter's delivery.
- **MEDIUM:** Keepalive liveness depends on its single background thread receiving CPU time before TTL expiration; long process stalls beyond the configured TTL cause daemon reaping.
- **LOW:** `Abacus RTSClient::is_connected` only distinguishes socket EOF from no immediately observable closure; it does not establish that the daemon can serve a request.

</known-hazards>

<files>

## Files
| File | Purpose |
|---|---|
| `client.rs` | Defines SDK errors, Unix-socket request/response transport, daemon connection setup, typed create/attach APIs, and descriptor mapping. |
| `handle_ops.rs` | Provides shared atomic reads, sentinel-safe counter increments, lifecycle reads, TTL touching, and futex wait loops. |
| `interlock.rs` | Implements creator, attached, read-only counter, and clock handle types. |
| `lib.rs` | Declares modules and re-exports the SDK public API. |
| `process_clock.rs` | Implements a keepalive-backed liveness and uptime beacon using daemon clock values. |
| `tests.rs` | Tests public pure type behavior, constants, lifecycle classification, wire discriminants, and error display. |
| `touch.rs` | Implements the shared keepalive registry, worker thread, and per-registration deregistration handle. |
| `types.rs` | Defines SDK policies, wait/lifecycle result types, defaults, watched-word ABI values, and pure classification functions. |
| `wait_barrier.rs` | Implements one-shot, manually rearmable all-conditions barrier waiting. |
| `wait_counter.rs` | Implements target-based waits on a watched interlock word. |
| `wait_cron.rs` | Implements recurring daemon-driven waits on a monotonic-time grid. |
| `wait_race.rs` | Implements an SDK-only first-completion race over multiple `WaitCounter`s. |
| `wait_timer.rs` | Implements clock-based waits with fatal delivery margins and configurable timeout policy. |

</files>

<notes>

## Notes
- The daemon controls interlock allocation, naming, tier evaluation, and passed-memory descriptors; the client controls local handle lifetime, TTL refresh, and futex waiting.
- Typed handles encode intended mutation permissions at the Rust API boundary, but attached shared memory remains a cross-process mutable resource.
- The clock interlock is attached automatically during `Abacus RTSClient::connect`; callers access it only through `client.clock()`.

</notes>

<reference>

## Reference

</reference>
