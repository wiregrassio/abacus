<purpose>

# tests/

End-to-end L2 integration tests that run the `abacus` daemon as a real process and verify lifecycle, protocol-fault containment, client behavior, timing, resource limits, and cross-process shared-memory contracts.

</purpose>

<dependencies>

## Dependencies
- `abacus_client` for SDK-level clients, interlocks, timers, timeout policies, and observed wait states.
- `abacus_tests` for daemon process management, raw protocol clients, role-process dispatch, timing/deadline helpers, socket paths, and wire-test constants.
- `abacus_wire` for protocol request encoding, response matching, version constants, and error codes.
- `libc` for Unix signals and `ftruncate`.
- Rust standard library for process control, Unix socket metadata, file descriptors, paths, atomics, threads, and timing.
- `common` child module for test-local daemon launch, isolated health checks, EOF draining, and duration-budget helpers.

</dependencies>

<consumed-by>

## Consumed By
- Cargo integration-test harnesses compile and execute these files as independent test binaries.

</consumed-by>

<data-flow>

## Data Flow
- Test cases start the Cargo-built `abacus` daemon through `ProcessDaemon` or local `RawDaemon` helpers, generally with a unique Unix-domain socket path.
- SDK clients (`AbacusClient`) exercise normal create, attach, timer, counter, connection, and restart behavior.
- Raw clients (`RawClient`) send deliberately malformed, partial, oversized, or unread protocol traffic directly over the daemon socket.
- Some tests re-execute their own integration-test binary as named ignored-test roles; parent tests pass the daemon socket and role arguments through command-line dispatch.
- Tests collect daemon exit status, socket-file state, CPU ticks, FD counts, stderr fault lines, mapped shared-memory words, and client-visible results.
- Assertions verify daemon survival, cleanup, latency bounds, protocol replies, connection closure, shared-memory sealing, and expected client timeout/reaping behavior.

</data-flow>

<known-hazards>

## Known Hazards
- HIGH: Timing assertions depend on host process startup latency and daemon cadence; a slow or heavily loaded host can cause deadline-sensitive tests to fail.
- HIGH: Timing assertions depend on host scheduler latency, Unix process startup, daemon loop cadence, and CPU-tick accounting; heavily loaded or slow debug environments can produce flaky deadline failures.
- HIGH: Role subprocesses are addressed by ignored-test names passed as strings; a rename that does not update `role_command`/`role_args` callers breaks tests only at runtime.
- MEDIUM: Tests manipulate real signals, Unix socket files, memfd file descriptors, and `/run/abacus/abacus.sock`; they are Unix-specific and can be affected by host permissions or stale external socket state.
- MEDIUM: The default-socket test branches on whether `/run/abacus` exists, so its expected outcome depends on machine configuration.
- LOW: Tests that inspect stderr assume stable diagnostic fragments such as `abacus: fatal`, `occupied by a live daemon`, and `protocol fault`.

</known-hazards>

<files>

## Files
| File | Purpose |
|---|---|
| `daemon_clients.rs` | Tests SDK client behavior across process boundaries, including owner death, disconnect survival, timer delivery scale, and zero-duration waits. |
| `daemon_hostile.rs` | Tests daemon containment and recovery against malformed frames, non-reading clients, connection churn, and memfd truncation attacks. |
| `daemon_process.rs` | Tests daemon startup, arguments, signals, restart semantics, socket lifecycle, cadence, CPU use, and multi-process shared interlocks. |

</files>

<notes>

## Notes
The suite intentionally uses subprocess roles where an SDK abort, a hung request, or shared-memory corruption could otherwise terminate or poison the integration-test parent.

`daemon_clients.rs`, `daemon_hostile.rs`, and `daemon_process.rs` describe tests as L2 coverage and rely on `common`.

</notes>

<reference>

## Reference

</reference>
