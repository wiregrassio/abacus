# crates/abacus-daemon/

This directory contains the Abacus daemon package. It builds both the `abacus` command-line program and a Rust library containing the coordination loop, interlock registry, and Unix-domain-socket transport.

The daemon is the process that accepts interlock create and attach requests, shares interlock memory through file descriptors, and evaluates timers and wait conditions on millisecond boundaries. Its production protocol implementation comes from `abacus-wire`, while core interlock and timing operations come from `abacus-core`.

Build or run it through Cargo using the `abacus` binary target. Read `src/` for daemon implementation details and `tests/` for real-process integration coverage, including lifecycle, client, timing, and malformed-protocol cases.

<contracts>

## Contracts
- The package exposes an `abacus` binary whose entry point is `src/main.rs`.
- The daemon library provides the event loop and public registry/transport APIs consumed by the binary and potential downstream Rust callers.
- Production socket traffic uses the external `abacus-wire` framing and codec.
- Successful create or attach operations return both a protocol response and an interlock fd transferred using SCM_RIGHTS.
- The daemon evaluates live interlock state against millisecond timing boundaries; registry capacity therefore has a direct scheduling-cost implication.
- Interlock ID `0` is reserved for the daemon-owned clock and must not be allocated as an ordinary named interlock.
- Integration tests require Unix process, socket, signal, fd, and shared-memory facilities; they are not portable to non-Unix environments.
- Test-only dependencies `abacus-client` and `abacus-tests` are required when compiling this package's integration tests.

</contracts>

<units-table>

## Units Table
| Symbol | Unit | Enforced By |
|---|---|---|
| daemon coordination cadence | ms | daemon loop design and registry evaluation scheduling |
| clock interlock `closed_count` | ms since daemon start | reserved clock-interlock convention |
| clock interlock `open_count` refresh interval | evaluation cycle | daemon registry loop convention |

</units-table>

<test-inventory>

## Test Inventory
- Process lifecycle, CLI arguments, socket creation/cleanup, restart behavior, signal shutdown, cadence, CPU use, and multi-process shared interlocks: covered by `tests/daemon_process.rs`.
- SDK client behavior across process boundaries, owner death, disconnect survival, timer scale, and zero-duration waits: covered by `tests/daemon_clients.rs`.
- Malformed/partial/oversized protocol frames, unread clients, connection churn, and memfd truncation attacks: covered by `tests/daemon_hostile.rs`.
- Exact end-to-end verification of every daemon-library public API: NO COMPLETE INVENTORY AVAILABLE from this directory-level material.
- Reliable timing behavior under scheduler contention: NO deterministic test guarantee; existing timing tests are host-sensitive.

</test-inventory>

<cross-boundary-verification>

## Cross-Boundary Verification
- CLI configuration to daemon process startup: verified by process integration coverage in `tests/daemon_process.rs`.
- SDK client to real daemon socket protocol: verified by `tests/daemon_clients.rs`.
- Raw malformed wire input to daemon fault containment: verified by `tests/daemon_hostile.rs`.
- SCM_RIGHTS interlock-fd transfer and shared-memory behavior: verified by process-level sealing and shared-interlock coverage described in `tests/daemon_process.rs`.
- Daemon signal handling and socket cleanup: verified by `tests/daemon_process.rs`.
- Registry linear-scan execution remaining within the 1 ms budget at configured maximum capacity: NOT VERIFIED.
- Correct delivery of strict responses to slow-reading clients under socket backpressure: NOT VERIFIED; current behavior closes on `EAGAIN`.
- Stale-socket removal preserving inaccessible but live daemon sockets: NOT VERIFIED.

</cross-boundary-verification>

<symbol-table>

## Symbol Table
### Cargo.toml
| Symbol | Kind | Purpose | Rationale |
|---|---|---|---|
| `abacus-daemon` | package | Declares the daemon crate package. | Keeps daemon packaging distinct from core interlock and wire-protocol crates. |
| `abacus` | binary target | Builds the daemon command-line executable from `src/main.rs`. | Provides the operational process separately from library consumers. |

</symbol-table>
