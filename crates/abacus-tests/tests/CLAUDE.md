<purpose>

# tests/

Integration, regression, abuse, soak, and hardware-timing test suites that exercise the Abacus RTS client/daemon contracts through in-thread and real-process daemons.

</purpose>

<dependencies>

## Dependencies
- Sibling workspace crates: `abacus-client`, `abacus-core`, `abacus-daemon`, `abacus-wire`.
- Parent test-support crate/module: `abacus_tests` helpers for daemon lifecycle, raw protocol clients, socket paths, synchronization, process roles, CPU/RSS/fd metrics, random generation, affinity, and statistics.
- External/platform facilities: Rust standard library, Unix-domain sockets, `/proc`, `libc`, memfd/mmap/futex/SCM_RIGHTS behavior, process signals, CPU affinity, and resource limits.

</dependencies>

<consumed-by>

## Consumed By
- Cargo/libtest discovers each `*.rs` file as an integration-test binary for the `abacus-tests` crate.

</consumed-by>

<data-flow>

## Data Flow
- Test code starts an in-thread `ThreadDaemon`, a process-backed `ProcessDaemon`, a fake UDS daemon, or a stoppable daemon thread.
- Tests create SDK clients, raw wire clients, shared-memory attachments, hostile socket traffic, child-process roles, CPU-load threads, and process signals.
- Client and raw-wire operations produce shared-memory interlock handles, protocol responses, wait results, daemon process metrics, and child status/output.
- Assertions compare those outputs against surface, lifecycle, timeout, permission, wire-ABI, resource-recovery, and timing contracts.
- Abuse and timing suites report diagnostic measurements such as CPU ticks, latency percentiles, RSS, descriptor count, and replay seeds.

</data-flow>

<known-hazards>

## Known Hazards
- **HIGH:** Most L3/L4 timing, soak, and abuse coverage is `#[ignore]`; regressions in hostile-input resilience, resource recovery, and real-hardware latency are not detected by the default test run.
- **HIGH:** L3/L4 timing thresholds are calibrated to a Jetson Orin AGX with an isolated core; a different host or non-isolated configuration will fail the timing assertions even with correct daemon behavior.
- **HIGH:** Hardware timing assertions depend on CPU isolation, affinity, scheduler behavior, tick accounting, and the Jetson-like deployment profile; they can fail on otherwise correct hosts with unrelated work scheduled on the daemon core.
- **MEDIUM:** Process-role tests invoke the current libtest executable by test name; renamed role functions, altered libtest invocation semantics, or output-format changes can break orchestration independently of daemon behavior.
- **MEDIUM:** Tests directly mutate and map shared memfds, including sentinel values and hostile writes; incorrect test isolation or a daemon that permits `ftruncate` can SIGBUS participating processes.
- **MEDIUM:** Numerous behavioral assertions rely on millisecond-scale sleeps and polling for ordering, so heavily loaded CI can introduce timing-related flakiness.
- **LOW:** Wire and protocol tests couple directly to numeric tags, tiers, error codes, frame layout, and fixed message-size limits; ABI changes require coordinated test updates.

</known-hazards>

<files>

## Files
| File | Purpose |
|---|---|
| `abuse_connections.rs` | Ignored L4 process-daemon tests for idle connection piles, connect floods, SCM_RIGHTS floods, creation caps, fd exhaustion, and request-burst loop recovery. |
| `abuse_memory.rs` | Ignored L4 process-daemon tests for registry collisions, memfd truncation, hostile mappings, writable-word corruption, and clock scribble resistance. |
| `abuse_wire.rs` | Ignored L4 process-daemon tests that flood malformed, random, mutated, oversized, zero-length, and partial wire frames. |
| `attached.rs` | L1 tests for attached interlock mutation/wait/free behavior and attached wait-counter visibility. |
| `bounded_waits.rs` | Focused tests for bounded interlock, attached-interlock, and clock futex waits. |
| `client.rs` | L1 tests for client connection setup, clock attachment, reserved-name handling, SDK-side rejection, and daemon error mapping. |
| `interlock.rs` | L1 tests for interlock arithmetic, state, TTL, futex wakeups, sentinels, keepalive threads, freeing, drops, and create-over-existing-name replacement. |
| `is_reaped.rs` | Tests `is_reaped()` across owning handle types, replacement, and TTL expiry. |
| `permissions.rs` | Runtime validation of the permission-model rows for creators, attachers, wait objects, and clock handles. |
| `process_clock.rs` | L1 tests for process-clock uptime, fixed start time, replacement reaping, and wait-counter observation. |
| `regressions.rs` | Consolidated in-process regression coverage for cron, barriers, races, timeout policy, invalid requests, sentinel safety, target lifetime, keepalive, and bounded waits. |
| `sentinel_increments.rs` | Focused tests ensuring increments cannot resurrect or wrap through sentinel values. |
| `soak_hour.rs` | Ignored long-running process-daemon soak checking wait liveness, cron semantics, RSS stability, and fd stability. |
| `stop_flag.rs` | Tests the stoppable daemon-loop API, socket cleanup, connection-state transition, and wait-counter timeout after daemon shutdown. |
| `timeout_policy.rs` | Tests timeout-policy behavior, timer TTL margins, zero-duration waits, daemon stalls, and daemon restart semantics. |
| `timing_load.rs` | Ignored hardware timing/load benchmarks across oversubscribed, production-affinity, and stress-affinity profiles. |
| `timing_loop.rs` | Ignored idle-hardware timing benchmarks for daemon CPU, timers, cron drift, wake latency, and large registries. |
| `touch.rs` | L1 test that the default keepalive TTL survives a 100 ms owner-process stall. |
| `wait_barrier.rs` | L1 tests for all-condition barriers, wake latency, reaping, already-reaped behavior, direct registry barrier evaluation, and SDK barrier delivery/rearming. |
| `wait_counter.rs` | L1 tests for watched-word counters, target CAS-max semantics, delivery, timeout, reaping, TTL, and clock watching. |
| `wait_cron.rs` | L1 tests for cron grid behavior, repeated waits, off-grid overrun reporting, and free behavior. |
| `wait_race.rs` | L1 tests that SDK-only wait races identify the first completed counter and that a reaped counter is an error. |
| `wait_timer.rs` | L1 tests for relative/absolute timer waits, past targets, multiple timers, monotonic sequencing, and reaping. |
| `wire_crate.rs` | Wire-crate codec round-trip, truncation, fuzz-like mutation, random-input, and dependency-boundary tests. |

</files>

<notes>

## Notes
The directory deliberately separates normal L1 behavioral coverage from L3 hardware timing and L4 hostile-environment coverage. The latter are ignored because they are expensive and host-sensitive.

File names describe the behavior under test. Their comments define the intended API/behavioral shape and make the test suite part of the implementation acceptance criteria.

</notes>

<reference>

## Reference

</reference>
