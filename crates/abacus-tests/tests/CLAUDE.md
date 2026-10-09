<purpose>
# tests/
Integration and resilience test suites for the Abacus client, daemon, shared-memory interlocks, wire protocol, liveness, timing, and abuse boundaries.
</purpose>

<dependencies>
## Dependencies
Imports sibling crates `abacus-client`, `abacus-core`, `abacus-daemon`, `abacus-tests`, and `abacus-wire`. Uses `libc` and Rust standard-library Unix process, socket, threading, and timing APIs.
</dependencies>

<consumed-by>

## Consumed By
Consumed by the parent package (`crates/abacus-tests`).

</consumed-by>

<data-flow>
## Data Flow
- Test cases create thread-hosted or process-hosted daemons, then drive them through SDK clients, raw Unix sockets, shared memfds, child roles, signals, and CPU load.
- Assertions observe interlock words, futex waits, daemon health, process exits, stderr diagnostics, resource counters, and timing statistics.
- Abuse and timing suites emit failure diagnostics and benchmark summaries.
</data-flow>

<known-hazards>
## Known Hazards
- HIGH: Several tests send SIGKILL or SIGSTOP, create CPU saturation, alter fd limits, or require scheduling privileges. Run ignored timing, abuse, soak, and priority suites only in suitable isolated environments.
- MEDIUM: Role tests are ignored entry points invoked by other tests through the current test binary. Renaming a role requires updating its spawners.
- MEDIUM: Hardware timing thresholds assume daemon CPU isolation. Host scheduling noise can produce legitimate failures.
</known-hazards>

<files>
## Files
| File | Purpose |
|------|---------|
| `abuse_connections.rs` | Ignored L4 daemon resilience tests for connection floods, partial recovery, SCM_RIGHTS, capacity limits, and fd exhaustion. |
| `abuse_memory.rs` | Ignored L4 shared-memory abuse tests for collisions, memfd sealing, hostile mappings, and corrupted words. |
| `abuse_wire.rs` | Ignored L4 hostile Unix-socket framing, mutation, oversized-prefix, and partial-frame tests. |
| `attached.rs` | Attached interlock and attached wait-counter behavior tests. |
| `bounded_waits.rs` | Bounded futex wait timeout, wake, and reaped-handle tests. |
| `client.rs` | SDK connection, protocol error mapping, attachment, process-clock, and keepalive configuration tests. |
| `connect_waiting.rs` | Retrying connection and dependency-wait behavior tests, including child role logging. |
| `interlock.rs` | Owning interlock lifecycle, TTL, touch thread, waiting, termination, and replacement tests. |
| `is_reaped.rs` | `is_reaped()` coverage across owning handle types and expiry paths. |
| `keepalive_priority.rs` | Ignored privileged SCHED_FIFO keepalive priority soak tests and role entry point. |
| `liveness.rs` | Process-clock dependency cascade, daemon death, replacement, and child abort tests. |
| `permissions.rs` | Runtime validation of creator, attacher, counter, and clock permission-model cells. |
| `process_clock.rs` | Process-clock uptime, fixed start time, and watcher tests. |
| `regressions.rs` | In-process regression coverage for scheduling, protocol validation, waits, ownership, and keepalive behavior. |
| `sentinel_increments.rs` | Sentinel-preserving increment and attacher-free race regression tests. |
| `soak_hour.rs` | Ignored long-running daemon resource and wait-service soak test. |
| `stop_flag.rs` | Stoppable daemon-loop, socket cleanup, connection state, and shutdown timeout tests. |
| `timeout_policy.rs` | Timer TTL margin and zero-duration wait tests. |
| `timing_liveness.rs` | Ignored hardware timing measurements for multi-level liveness cascades. |
| `timing_load.rs` | Ignored CPU-load timing benchmarks, isolation profiles, and keepalive thread-budget tests. |
| `timing_loop.rs` | Ignored idle-hardware loop, timer, cron, barrier, counter, and registry-scale timing benchmarks. |
| `touch.rs` | Default touch-thread TTL stall-survival test. |
| `wait_barrier.rs` | WaitBarrier evaluation, rearm, latency, reap, and registry-stamping tests. |
| `wait_counter.rs` | WaitCounter delivery, contention, timeout, TTL, reap, and clock-watch tests. |
| `wait_cron.rs` | WaitCron grid, rearm, overrun, and termination tests. |
| `wait_race.rs` | SDK-only multi-counter race winner and reap-error tests. |
| `wait_timer.rs` | WaitTimer duration, absolute target, monotonicity, role, and reap tests. |
| `wire_crate.rs` | Wire codec round-trip, truncation, mutation, random-input, and crate dependency-boundary tests. |
</files>