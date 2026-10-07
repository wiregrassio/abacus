<purpose>
# tests/
L2 integration tests for the `abacus` daemon as a real process, covering lifecycle, cross-process client behavior, protocol faults, resource resilience, and hostile-client isolation.
</purpose>

<dependencies>
## Dependencies
Imports sibling `common` helpers, `abacus_client` SDK types, and `abacus_tests` daemon, raw-protocol, child-role, timing, and socket utilities. Uses Rust standard library and `libc`.
</dependencies>

<consumed-by>

## Consumed By
Consumed by the parent package (`crates/abacus-daemon`).

</consumed-by>

<data-flow>
## Data Flow
- Starts Cargo-built daemon processes on unique Unix sockets.
- Connects SDK and raw-protocol clients, including re-executed test-binary role children.
- Injects lifecycle events, malformed frames, connection failures, signals, and memfd attacks.
- Asserts daemon liveness, protocol replies, socket cleanup, timing bounds, resource recovery, and child exit status.
</data-flow>

<known-hazards>
## Known Hazards
- HIGH: Role tests depend on re-executing this test binary through `abacus_tests::role_command`; renaming roles or altering test harness arguments breaks cross-process scenarios.
- HIGH: SDK wait operations can abort on delivery timeout, so short or hostile waits must remain isolated in role children.
- MEDIUM: Timing assertions depend on daemon scheduling and host load, including CPU tick, startup, TTL, and clock-cadence thresholds.
- HIGH: Clock memfd truncation can SIGBUS any process that maps it, so the attack and post-attack check must remain isolated processes.
</known-hazards>

<files>
## Files
| File | Purpose |
|------|---------|
| `daemon_clients.rs` | Cross-process client lifecycle, reaping, disconnect, concurrency, and zero-wait regression tests. |
| `daemon_hostile.rs` | Real-daemon resilience tests for malformed, stalled, non-reading, oversized, and memfd-attacking clients. |
| `daemon_process.rs` | Daemon startup, CLI, signals, restart, socket, cadence, idle-cost, and multiprocess-sharing tests. |
</files>

<subdirectories>
## Subdirectories
| Directory | Purpose |
|-----------|---------|
| `common/` | Shared daemon spawning, raw client, role-child, deadline, and liveness test helpers. |
</subdirectories>