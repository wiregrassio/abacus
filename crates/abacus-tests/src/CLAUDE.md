<purpose>
# src/
Shared Rust testkit for Abacus L1 through L4 tests: daemon lifecycle, hostile wire clients, child roles, load measurement, and permission compile-fail checks.
</purpose>

<dependencies>
## Dependencies
Imports workspace crates `abacus-client`, `abacus-core`, `abacus-daemon`, and `abacus-wire`. Uses `libc` and Linux `/proc`, Unix sockets, process, FD, and affinity APIs.
</dependencies>

<consumed-by>

## Consumed By
Consumed by the parent package (`crates/abacus-tests`).

</consumed-by>

<data-flow>
## Data Flow
- Test inputs: socket labels, daemon configurations, raw wire payloads, child-role arguments, and measurement parameters.
- Testkit starts thread or child daemons, creates SDK or raw Unix-socket clients, and returns handles, responses, process metrics, child output, and statistics.
- Raw protocol helpers encode and decode wire ABI v2 frames, including SCM_RIGHTS file descriptors.
</data-flow>

<known-hazards>
## Known Hazards
- CRITICAL: Mapping an interlock supplied by a hostile client can SIGBUS after memfd shrink. Perform such mapping in a role child, never the test process.
- HIGH: SDK clients created in the test process must use `TimeoutPolicy::Error`, not `Abort`, or test-process abort skips daemon cleanup.
- HIGH: Timing, CPU, and load tests must hold `serialized()` to prevent concurrent measurements across workspace test binaries.
- MEDIUM: `ProcessDaemon` and system metrics depend on Linux facilities including `/proc`, `flock`, `prctl`, `sched_setaffinity`, and RLIMITs.
</known-hazards>

<files>
## Files
| File | Purpose |
|------|---------|
| `lib.rs` | Shared test harness: daemon guards, polling, process roles, raw ABI v2 client, fake daemon, load, RNG, and statistics helpers. |
| `permissions.rs` | Compile-fail doctests enforcing SDK permission-table method absence. |
</files>