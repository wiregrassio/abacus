<purpose>

# crates/abacus-tests/src

Shared Linux-focused testkit for L0 through L4 tests: daemon lifecycle control, hostile wire clients, process isolation, load generation, measurement utilities, and API-permission compile-fail checks.

</purpose>

<dependencies>

## Dependencies
- Sibling crates:
  - `abacus_client`: SDK clients and handle types exercised by test helpers and permission doctests.
  - `abacus_core`: interlock mapping and transport error types.
  - `abacus_daemon`: in-process daemon runner and configuration.
  - `abacus_wire`: typed wire encoding, decoding, framed reads, and FD receipt.
- External/system:
  - Rust standard library Unix sockets, process spawning, file I/O, threads, atomics, channels, and timing.
  - `libc`: `flock`, signals, resource limits, CPU affinity, SCM_RIGHTS socket control messages, `/proc`-adjacent system interfaces.

</dependencies>

<consumed-by>

## Consumed By
- Downstream integration, timing, load, abuse, SDK, and daemon tests use this crate as their shared L1toL4 test infrastructure.
- `cargo test -p abacus-tests --doc` executes the permission-model compile-fail doctests.

</consumed-by>

<data-flow>

## Data Flow
- Tests provide labels, daemon binary paths, daemon options, socket paths, wire payloads, process-role names, CPU sets, and timing thresholds.
- `ThreadDaemon` starts `abacus_daemon` in a test-owned thread; `ProcessDaemon` starts the daemon binary as a child process and captures stderr.
- `RawClient` constructs or accepts ABI-v1 frames, sends them over Unix-domain sockets, receives daemon responses and SCM_RIGHTS file descriptors, and can map received interlock FDs.
- `FakeDaemon` accepts SDK connections and returns deliberately crafted protocol frames or FD counts to test SDK error handling.
- Process and `/proc` helpers expose child process state as tick counts, FD counts, thread counts, RSS KiB, stderr lines, and exit status.
- Helpers return Rust values, mapped `InterlockHandle`s, child-process output, percentile statistics, or panic with test-oriented diagnostics.

</data-flow>

<known-hazards>

## Known Hazards
- HIGH: `start_daemon` intentionally leaks its `ThreadDaemon`; callers receive only a socket path and have no supported shutdown path, leaving daemon threads alive until process exit.
- HIGH: `map_fd` maps hostile received FDs in the current process; callers must use child-process roles for untrusted interlocks because a maliciously truncated memfd can SIGBUS every mapper.
- MEDIUM: `ProcessDaemon::kill` ignores `kill(2)` failure, so tests may proceed as though a daemon was signaled when its PID is already invalid or inaccessible.
- MEDIUM: `/proc` helpers silently return zero on missing, unreadable, or malformed process data, which can make resource-accounting assertions indistinguishable from measurement failure.
- MEDIUM: CPU-affinity helpers assume supplied CPU indices are valid and permitted by the test environment; restricted containers or cpusets can make pinned tests panic.
- MEDIUM: `role_command` serializes role arguments with U+001F; arguments containing that separator cannot round-trip.
- LOW: `Stats::pct` uses rounded index selection rather than a formally specified percentile interpolation method; threshold-sensitive performance assertions may differ from other statistics tools.
- LOW: `unique_socket_path` relies on PID plus a process-local counter and does not reserve the path before binding; collisions remain possible with independently constructed identical names in unusual shared-temp-dir scenarios.

</known-hazards>

<files>

## Files
| File | Purpose |
|---|---|
| `lib.rs` | Shared daemon, socket, wire, process, measurement, load, RNG, and statistics test infrastructure. |
| `permissions.rs` | Compile-fail doctests enforcing SDK permission boundaries from the permission contract. |

</files>

<notes>

## Notes
The kit deliberately separates ordinary SDK-driven tests from hostile protocol tests: `RawClient` speaks ABI v1 directly so malformed-input coverage is independent of SDK behavior.

Process roles exist because mapping an attacker-controlled interlock can terminate the mapper with SIGBUS; dangerous mapping scenarios therefore run in a re-executed test-binary child.

</notes>

<reference>

## Reference

</reference>
