<purpose>

# crates/abacus-tests/

Integration-test crate and shared test infrastructure for validating Abacus client, daemon, wire-protocol, resilience, and timing contracts.

</purpose>

<dependencies>

## Dependencies
- Workspace crates:
  - `abacus-core`: core interlock mapping and transport error types used by the shared testkit.
  - `abacus-wire`: ABI-v2 framing, encoding/decoding, and descriptor-receipt behavior exercised by wire tests and raw clients.
  - `abacus-daemon`: in-process and process-backed daemon instances under test.
  - `abacus-client`: SDK clients, handles, waits, and permission boundaries under test.
- External:
  - `libc`: Linux process, signal, locking, affinity, resource-limit, socket-control-message, and related system interfaces.
  - Rust standard library Unix sockets, process control, threads, synchronization, timing, files, and I/O.

</dependencies>

<consumed-by>

## Consumed By
- `docs` references test file paths in its design documents.

</consumed-by>

<data-flow>

## Data Flow
- Test cases and operational probes provide daemon configuration, socket paths, SDK calls, raw ABI-v2 payloads, timing thresholds, CPU assignments, and child-process role arguments.
- The shared `src/` testkit starts daemon instances, connects SDK or raw Unix-socket clients, sends normal or malformed protocol frames, receives responses and passed file descriptors, and gathers process/resource measurements.
- Integration tests in `tests/` assert client, daemon, shared-memory interlock, timer, cron, barrier, permission, timeout, reaping, resource-recovery, and wire-format behavior.
- The `examples/` probe accepts command-line parameters, drives a live daemon, and emits benchmark/timing summaries or partial-frame behavior observations to stdout.
- Outputs are Rust test assertions, test-oriented panics/diagnostics, process status and metrics, timing statistics, and command-line probe reports.

</data-flow>

<known-hazards>

## Known Hazards
- HIGH: Most L3/L4 timing, soak, and hostile-environment coverage is `#[ignore]`; default test execution does not detect regressions in real-process resilience, resource recovery, or hardware timing.
- HIGH: The shared `start_daemon` helper intentionally leaks its `ThreadDaemon`, so callers receiving only a socket path cannot stop the daemon before process exit.
- HIGH: Mapping hostile received interlock FDs in the test process can SIGBUS it if an attacker truncates the backing memfd; such cases require child-process isolation.
- HIGH: Several ignored tests intentionally document unresolved implementation milestones; enabling them does not presently represent a green release gate.
- MEDIUM: Timing assertions rely on scheduler behavior, host CPU isolation/affinity, polling, and millisecond-scale sleeps, making them host-sensitive and potentially flaky in loaded CI.
- MEDIUM: Process-resource helpers can report zero for missing or malformed `/proc` data, conflating failed measurement with an observed zero value.
- MEDIUM: Raw wire tests and the partial-frame probe directly couple to numeric ABI tags, framing layout, descriptor behavior, and message-size limits; protocol changes require coordinated updates.
- MEDIUM: Process-role orchestration depends on libtest executable invocation and a U+001F-separated argument encoding that cannot represent arguments containing that character.
- LOW: Percentile helpers use sample-index selection rather than a formally interpolated percentile definition, so threshold comparisons may differ from external tools.

</known-hazards>

<files>

## Files
| File | Purpose |
|---|---|
| `Cargo.toml` | Defines the `abacus-tests` crate and its dependencies on Abacus workspace components plus `libc`. |

</files>

<notes>

## Notes
This crate intentionally keeps normal SDK-driven testing separate from hostile protocol testing. The raw-client infrastructure speaks ABI v2 directly, allowing malformed-input coverage without depending on SDK validation behavior.

Dangerous shared-memory mapping scenarios are run in re-executed child process roles because an intentionally malformed or truncated interlock can terminate its mapper with `SIGBUS`.

</notes>

<reference>

## Reference
Full contracts, units, symbol table, test inventory, and cross-boundary verification: see `README.md`.

</reference>
