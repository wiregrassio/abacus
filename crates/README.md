# crates/

This directory holds the Rust crates that make up Abacus. The core crate defines Linux shared-memory coordination primitives, the wire crate defines the socket protocol, the daemon owns interlock registration and evaluation, and the client packages those mechanisms as an SDK.

Build the workspace with Cargo on Linux. A normal deployment runs the `abacus` daemon and links applications against `abacus-client`; the daemon and client then share mapped interlock state while using the Unix socket for setup and descriptor transfer.

Start reading with `abacus-core` and `abacus-wire` to understand the binary contracts, then follow those contracts into `abacus-daemon` and `abacus-client`. Use `abacus-tests` to find end-to-end examples, while noting that substantial timing and hostile-environment coverage is ignored by default.

<contracts>

## Contracts
- Client and daemon participants map the same fixed-size 24-byte atomic interlock representation.
- Shared interlock descriptors must refer to compatible, correctly sized memfds; malformed or truncated mappings can terminate a process with `SIGBUS`.
- `SENTINEL` is globally reserved for lifecycle termination and must never be produced as an ordinary live counter or expiration value.
- Consumers must treat an interlock as terminated when the applicable shared state contains the sentinel; they cannot assume every counter word is overwritten during reaping.
- Wire frames begin with a four-byte little-endian payload length.
- File descriptors travel as Unix `SCM_RIGHTS` ancillary data, not inside serialized payloads.
- Response consumers must validate descriptor cardinality independently of payload decoding.
- Daemon request handling must reject unsupported or invalid numeric tags, tiers, conditions, intervals, and counter values that the codec itself accepts.
- The daemon owns allocation, names, tier evaluation, registry lifetime, and the clock interlock at registry ID `0`.
- Shared deadlines use monotonic time; wall-clock adjustments must not affect expiration behavior.
- The client owns local mapped-handle lifetime, keepalive refresh, and wait policy behavior.
- Some send paths treat `EAGAIN` as a terminal I/O or client failure rather than buffering and retrying.
- Futex wake failures may be ignored, so successful state mutation does not guarantee that a wake syscall was reported as successful.
- Runtime behavior requires Linux support for memfds, required file seals, shared mappings, futexes, monotonic clocks, Unix sockets, and descriptor passing.

</contracts>

<units-table>

## Units Table
| Symbol | Unit | Enforced By |
|--------|------|-------------|
| interlock layout size | bytes (24) | shared core mapping contract |
| frame length prefix | bytes, little-endian `u32` | `abacus-wire` framing |
| monotonic deadlines and expiration timestamps | nanoseconds | `abacus-core` clock conversion |
| daemon evaluation cadence | milliseconds (nominally 1 ms) | daemon loop scheduling |
| futex comparison word | low 32 bits of a shared `u64` | Linux futex implementation |
| registry clock ID | unitless ID (`0`) | daemon registry convention |
| process-role argument separator | Unicode U+001F | `abacus-tests` convention |

</units-table>

<test-inventory>

## Test Inventory
- Shared-memory interlocks, client operations, daemon behavior, timers, cron waits, barriers, permissions, timeouts, reaping, resource recovery, and wire behavior have integration coverage in `abacus-tests`.
- Raw ABI-v1 and malformed-frame behavior is tested independently of SDK validation.
- Descriptor receipt and protocol framing have wire-level coverage.
- Dangerous malformed-memory cases use child-process isolation to contain possible `SIGBUS`.
- Daemon process, signal, socket, shared-memory, and SDK integration scenarios are represented.
- Real-process timing, soak, hostile-environment, and resource-recovery coverage: MOSTLY IGNORED BY DEFAULT.
- Known failing implementation milestones: NOT A GREEN RELEASE GATE.
- Futex behavior when only the upper 32 bits of a counter change: NO VERIFIED COVERAGE SHOWN.
- Big-endian futex compatibility: NO TEST and unsupported by the current address assumption.
- Attached wait-counter tier mismatch: NO VERIFIED COVERAGE SHOWN.
- Mandatory descriptor-cardinality validation by every consumer: NO CROSS-CONSUMER TEST SHOWN.
- Keepalive survival under long process stalls: NO VERIFIED COVERAGE SHOWN.
- Registry capacity versus the fixed 1 ms scan budget: no default, host-independent regression coverage shown.

</test-inventory>

<cross-boundary-verification>

## Cross-Boundary Verification
- Client/daemon shared 24-byte interlock layout: verified indirectly by `abacus-tests` shared-memory and SDK/daemon integration coverage; no explicit layout-compatibility test is identified in the supplied maps.
- Production request/response framing through `abacus-wire`: verified by raw wire and daemon integration coverage.
- `SCM_RIGHTS` descriptor transfer: verified by wire tests and integration coverage.
- Descriptor cardinality enforcement by all response consumers: **NOT VERIFIED**.
- Daemon validation of codec-accepted raw numeric values: partially exercised by malformed protocol tests; exhaustive validation is **NOT VERIFIED**.
- Reserved `SENTINEL` lifecycle behavior across core, daemon, and client: exercised by timeout and reaping tests; direct-write and wrapping violations are **NOT VERIFIED**.
- Registry clock reservation at ID `0`: **NOT VERIFIED**.
- Client attachment tier matching daemon registry type: **NOT VERIFIED**.
- One-millisecond daemon cadence under production registry load: timing coverage exists but is largely ignored and host-sensitive; not verified by the default suite.
- Keepalive progress before TTL expiry during process stalls: **NOT VERIFIED**.
- Low-32-bit futex comparison against 64-bit counter updates: **NOT VERIFIED**.
- Linux file-seal and malformed-memfd behavior: hostile mapping coverage exists, but dangerous scenarios require child isolation and much hostile-environment coverage is ignored.
- `docs` use of `abacus-tests` internals: **NOT VERIFIED** by a documented compatibility test.

</cross-boundary-verification>

<symbol-table>

## Symbol Table
No files were supplied for this directory, so there are no file-level symbols to list.

</symbol-table>
