# docs/

This directory is the entry point for understanding how Abacus RTS is designed, operated, and verified. Start with `ARCHITECTURE.md` and `CONTRACTS.md` for the normative design, then read `OPERATION.md` before deploying the daemon.

Build and install the release daemon with Cargo and the supplied systemd unit. Abacus RTS is Linux-only, uses a Unix-domain socket plus memfd/futex primitives, and requires deliberate socket permissions and scheduling choices.

Use `TESTING.md` to run the workspace suite and its ignored timing, abuse, and soak layers. Consult `BACKLOG.md` before integrating attachment-sensitive, boolean-composition, or non-Rust use cases.

<contracts>

## Contracts
- Wire ABI v1 and the three-word, 24-byte sealed memfd layout are compatibility boundaries; changing field meanings, ordering, sentinels, tags, or units requires coordinated daemon and SDK changes.
- The daemon is Linux-only and relies on Unix-domain sockets, memfd, futex, `ppoll`, and Linux process/scheduling facilities.
- The daemon creates the socket with the requested mode before accepting clients and exits with status 1 when a requested group is absent or a live daemon already occupies the path.
- Clean SIGTERM or SIGINT handling exits with status 0 and removes the socket; a stale socket left by SIGKILL is replaced at startup.
- The registry is non-durable. Consumers must recreate state after daemon restart or intentionally fail and rely on supervision.
- A create beyond `--max-interlocks` is rejected as `InvalidRequest`; interlock names must contain 1to255 bytes.
- Default timeout behavior may abort the consumer process. Embeddings and FFI users that cannot tolerate host termination must select `TimeoutPolicy::Error`.
- Timing and load tests require an otherwise idle Linux host unless the test explicitly creates its own load profile.
- Timing, CPU, and load tests must hold the shared serialization guard; fixed sleeps are forbidden as synchronization.
- Ignored tests must identify themselves with `timing:`, `abuse:`, `soak:`, or `role:` reasons.
- Randomized failures must expose a seed replayable through `ABACUS_TEST_SEED`.
- Cron interval conversion implicitly couples SDK milliseconds to wire nanoseconds, but rounding and overflow behavior are not specified.
- Tier-specific attach permissions implicitly depend on tier information absent from the v1 `Attached` response.

</contracts>

<units-table>

## Units Table
| Symbol | Unit | Enforced By |
|--------|------|-------------|
| `--socket-mode` | octal Unix permission bits | daemon socket creation |
| `--max-interlocks` | interlocks, excluding the clock | daemon registry cap |
| interlock name length | bytes | daemon request validation |
| interlock mapping size | 24 bytes | shared-memory ABI |
| daemon evaluation loop | milliseconds | implementation convention |
| `min_fatal_margin_ms` | milliseconds | SDK timeout policy |
| `timeout_ms` | milliseconds | SDK API |
| `interval_ms` | milliseconds | SDK API |
| `interval_ns` | nanoseconds | wire ABI |
| timer slack | nanoseconds | Linux `PR_SET_TIMERSLACK` |
| `LimitNOFILE` | file descriptors | systemd |
| soak duration | seconds | test configuration |
| timing percentile values | milliseconds | probe/test statistics |

</units-table>

<test-inventory>

## Test Inventory
- L0 unit tests inside crate `#[cfg(test)]` modules run by default.
- L1 in-process integration tests run the daemon as a thread and run by default.
- L2 process-level tests run the daemon as a child process and run by default.
- L3 timing and load tests are ignored by default and require a release run on an idle host.
- L4 abuse and soak tests are ignored by default; soak runs for 60 seconds unless `ABACUS_SOAK=1` requests the one-hour run.
- Process-level coverage includes partial-frame clients, memfd sealing, clean termination, socket cleanup, and fatal-margin behavior under a stopped daemon.
- Tests are required to pass with both one and twelve test threads.
- Wire-v1 tier recovery on attach: NO TEST; the protocol does not expose the required tier.
- Boolean composition tiers 5to8: NO TEST; they are not implemented.
- Non-Rust FFI behavior: NO TEST; no binding exists.
- Cron millisecond-to-nanosecond conversion, rounding, and overflow: no verification identified in the supplied material.
- Installation and systemd deployment instructions: no automated documentation test identified.
- Quarterly execution remains a manual operational process.

</test-inventory>

<cross-boundary-verification>

## Cross-Boundary Verification
- Partial wire-frame handling without daemon busy-spinning: verified by the release workspace suite and the documented `partial` probe.
- Attached-response tier and tier-specific permission coupling: NOT VERIFIED; wire ABI v1 omits the tier.
- `WaitTimer` non-attachability across SDK and daemon: NOT VERIFIED at the wire boundary.
- Memfd shrink protection across daemon and hostile clients: verified by the documented memfd-seal test.
- Fatal-margin behavior when daemon evaluation stops: verified by the documented stopped-daemon test.
- Cron SDK milliseconds to wire nanoseconds conversion: NOT VERIFIED by any test identified here.
- Monotonic-forward `WaitCounter::wait_until` TTL behavior across SDK and lifecycle contract: NOT VERIFIED by any test identified here.
- Socket mode/group deployment boundary: NOT VERIFIED by any test identified here.
- systemd clean shutdown and socket removal: described as covered by process behavior, but no specific test is identified in the supplied material.
- Daemon-restart behavior for every wait tier: NOT VERIFIED by any complete cross-tier test identified here.
- CPU affinity plus kernel isolation assumptions: exercised by pinned load profiles, which intentionally fail when the daemon core is not isolated.

</cross-boundary-verification>

<symbol-table>

## Symbol Table
### BACKLOG.md
| Symbol | Kind | Purpose | Rationale |
|--------|------|---------|-----------|
|, |, | No code symbols; this file records deferred work. | |

### OPERATION.md
| Symbol | Kind | Purpose | Rationale |
|--------|------|---------|-----------|
|, |, | No code symbols; this file provides operational guidance and measurements. | |

### TESTING.md
| Symbol | Kind | Purpose | Rationale |
|--------|------|---------|-----------|
|, |, | No code symbols; this file defines testing policy and workflow. | |

</symbol-table>
