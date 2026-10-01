# crates/abacus-tests/

`abacus-tests` is the workspace's test-focused crate. It contains reusable support code for launching Abacus daemons, communicating over normal SDK and raw Unix-socket paths, measuring process behavior, and testing protocol failures that ordinary clients cannot generate.

Most routine behavioral coverage lives in `tests/` and runs as Cargo integration tests. The shared support library in `src/` is used by those tests and also contains doctests that enforce SDK permission boundaries. Run the ordinary suite with Cargo; explicitly include ignored tests only on suitable Linux hosts when exercising soak, abuse, or hardware-timing scenarios.

The `examples/` directory contains an operational probe rather than a test harness. Use it against a running daemon to inspect wait timing, cron behavior, barrier behavior, and handling of a deliberately incomplete socket frame.

<contracts>

## Contracts
- The crate provides test-only infrastructure for starting and addressing in-thread or process-backed daemons, creating SDK clients, and issuing raw ABI-v2 Unix-socket requests.
- Raw-client helpers permit deliberately malformed frames and descriptor-transfer scenarios; callers must not assume SDK-level validation applies.
- Hostile interlock mappings are unsafe in the current process because backing-memory truncation can cause `SIGBUS`; callers must isolate untrusted mappings in child-process roles.
- Process and `/proc` measurement helpers are best-effort and may return zero when data is unavailable or unparsable; consumers requiring strict accounting must distinguish measurement failure externally.
- Timing and affinity helpers require Linux facilities and valid, permitted CPU assignments; constrained containers or cpusets may cause failures.
- Ignored L3/L4 tests require explicit opt-in and suitable host conditions; they are not guaranteed to be reliable or green on arbitrary CI infrastructure.
- Permission doctests require `abacus-client`'s public type and ownership model to preserve the intended compile-time capability boundaries.
- Wire tests require `abacus-wire` ABI-v2 frame layout, numeric tags, error codes, fixed limits, and SCM_RIGHTS handling to remain coordinated with expectations in the testkit.

</contracts>

<units-table>

## Units Table
| Symbol | Unit | Enforced By |
|---|---|---|
| timing thresholds and delays | milliseconds unless an individual API specifies otherwise | test/probe call-site convention |
| process CPU accounting | kernel clock ticks | `/proc` parsing helpers |
| resident memory measurements | KiB | `/proc` parsing helpers |
| file-descriptor and thread measurements | count | `/proc` parsing helpers |
| percentile values | sample value in the source measurement's unit | `Stats` helper convention |

</units-table>

<test-inventory>

## Test Inventory
- **Shared testkit:** Supplies daemon lifecycle, raw wire I/O, fake daemon behavior, descriptor handling, hostile-FD isolation, process roles, CPU/load control, `/proc` metrics, deterministic/random helpers, and percentile statistics.
- **Permission boundaries:** `src/permissions.rs` provides compile-fail doctests; `tests/permissions.rs` provides runtime validation of permission-model rows.
- **Default L1 behavior:** Tests client setup, attached and owning interlocks, timers, counters, cron waits, barriers, races, clocks, TTL/reaping, keepalives, sentinels, and daemon stop behavior.
- **Wire behavior:** `wire_crate.rs` covers codec round trips, truncation, mutations, random input, and dependency-boundary expectations.
- **Regression and roadmap acceptance:** `regressions.rs` and roadmap-named files cover targeted race, timeout, sentinel, rearm, reaping, and bounded-wait requirements.
- **Ignored L3/L4 coverage:** Abuse, process-resource recovery, long-duration soak, and timing/load suites exist but are not run by default.
- **Gap:** Default test execution does not cover most hostile-input, descriptor-exhaustion, long-duration resource-stability, or hardware-latency behavior.
- **Gap:** Host-dependent timing guarantees cannot be comprehensively validated in generic CI because the required CPU isolation and scheduler conditions are external to the crate.

</test-inventory>

<cross-boundary-verification>

## Cross-Boundary Verification
- `abacus-client` SDK behavior against daemon operations: verified by default client, interlock, wait, clock, and permission integration tests.
- `abacus-daemon` lifecycle and socket cleanup behavior: verified by daemon-stop, timeout-policy, regression, and process-daemon tests; some real-process scenarios are ignored.
- `abacus-wire` ABI-v2 encoding/decoding and malformed-frame handling: verified by `tests/wire_crate.rs` and ignored abuse-wire coverage.
- SCM_RIGHTS descriptor receipt and hostile FD behavior: partially verified by raw-client/fake-daemon support and ignored abuse suites; exhaustive hostile descriptor-flood behavior is NOT VERIFIED by the default test run.
- Shared-memory interlock behavior and reaping: verified by default attached, interlock, wait-counter, wait-barrier, process-clock, and reaping tests.
- Unsafe hostile-mapping isolation through process roles: verified by process-role-oriented abuse tests where enabled; NOT VERIFIED by the default test run if those ignored cases are excluded.
- Linux `/proc` resource metrics used for soak and abuse assertions: partially verified through consuming tests; correctness under missing, restricted, or malformed `/proc` data is NOT VERIFIED.
- Hardware timing and affinity assumptions: exercised only by ignored timing suites; NOT VERIFIED in ordinary CI.
- Partial socket-frame daemon handling: probed by `examples/probe.rs`; NOT VERIFIED as a standard integration-test contract from the supplied material.

</cross-boundary-verification>

<symbol-table>

## Symbol Table
### Cargo.toml
| Symbol | Kind | Purpose | Rationale |
|---|---|---|---|
| `abacus-tests` | package | Declares the workspace integration-test and testkit crate. | Kept as a dedicated crate so cross-crate integration tests can share Linux-specific support code without placing it in production libraries. |

</symbol-table>
