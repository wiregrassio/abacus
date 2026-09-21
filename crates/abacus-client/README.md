# crates/abacus-client/

This directory is the Cargo package for the Abacus RTS Rust client SDK. It provides the dependencies and package metadata needed to build the SDK implemented in `src/`.

Applications use this crate to connect to an Abacus daemon, create or attach named interlocks, and wait on counters, timers, cron schedules, barriers, races, or a daemon-backed process clock. The daemon provides shared-memory descriptors, while the client maps them and keeps registered resources alive.

Start with the public exports in `src/lib.rs`, then read `src/client.rs` for connection and creation APIs. The individual `wait_*` modules describe the behavior and policy of each wait primitive.

<contracts>

## Contracts
- The client connects using a blocking Unix-domain socket and expects daemon responses encoded according to `abacus-wire`.
- Successful create and attach operations require a valid passed shared-memory file descriptor; that descriptor is mapped into an `abacus-core` interlock handle.
- Typed handle APIs constrain intended mutation permissions in Rust, but do not make the underlying cross-process shared memory immutable.
- The daemon controls interlock allocation, naming, tier evaluation, and shared-memory descriptor delivery.
- The client refreshes TTLs only while its keepalive thread receives sufficient CPU time; no TTL-survival guarantee exists during long process stalls.
- `SENTINEL` is an implicit cross-process reserved value representing reaped state and must not be produced by ordinary counter writes.
- Timer behavior is policy-dependent; the default timeout policy may abort rather than return an ordinary failure result.
- Concurrent waiters on a shared counter or timer coordinate through monotonic CAS-max target state rather than waiter-private targets.

</contracts>

<units-table>

## Units Table
| Symbol | Unit | Enforced By |
|---|---|---|
| TTL / expiration timestamps | Monotonic-time nanoseconds | Shared-memory and daemon/client clock conventions |
| `WaitRace` polling interval | Milliseconds | SDK implementation convention |
| Timer delivery margin | Monotonic-time duration | Timer policy and wait implementation |

</units-table>

<test-inventory>

## Test Inventory
- `src/tests.rs` tests public pure type behavior, constants, lifecycle classification, wire discriminants, and error display.
- The supplied material does not identify integration tests covering a live daemon, passed descriptor mapping, shared-memory behavior, or Unix-socket transport failure handling.
- Keepalive TTL refresh under process scheduling delay: **NO TEST evidenced**.
- Fatal `WaitTimer` abort behavior: **NO TEST evidenced**.
- Cross-process contention between concurrent CAS-max waiters: **NO TEST evidenced**.
- `WaitRace` polling behavior, reaped-member failure behavior, and scheduling overhead: **NO TEST evidenced**.

</test-inventory>

<cross-boundary-verification>

## Cross-Boundary Verification
- `abacus-wire` request/response encoding and daemon-response validation: **NOT VERIFIED** by tests identified in supplied material beyond pure wire-discriminant coverage.
- Daemon-to-client shared-memory file-descriptor transfer and mapping: **NOT VERIFIED**.
- Daemon-controlled tier semantics versus `attach_wait_counter`'s assumed tier-1 mapping: **NOT VERIFIED**.
- Daemon expiration/reaping behavior versus client keepalive refresh: **NOT VERIFIED**.
- Shared `SENTINEL` convention between daemon/core/client writers and readers: **NOT VERIFIED**.
- Shared-memory futex wake/wait interoperability across processes: **NOT VERIFIED**.
- Public pure classifications, constants, wire discriminants, and error display: verified by `src/tests.rs` as summarized by the supplied child map.

</cross-boundary-verification>

<symbol-table>

## Symbol Table
### Cargo.toml
| Symbol | Kind | Purpose | Rationale |
|---|---|---|---|
| `abacus-client` | package | Defines the Abacus RTS client SDK crate. | Separates consumer-facing daemon and interlock APIs from core shared-memory and wire-protocol crates. |
| `abacus-core` | dependency | Supplies shared-memory interlock and lifecycle primitives. | Keeps low-level interlock mechanics reusable outside the SDK facade. |
| `abacus-wire` | dependency | Supplies daemon protocol and descriptor-transfer support. | Centralizes wire compatibility between client and daemon. |
| `libc` | dependency | Supplies required Unix and futex-adjacent system interfaces. | Rust standard APIs do not expose all required low-level socket and errno behavior. |

</symbol-table>
