<purpose>

# tests/

L0 unit and integration-style tests for the daemon registry, Unix-domain transport, and v1 wire codec without running the daemon event loop.

</purpose>

<dependencies>

## Dependencies
- Sibling daemon modules: `crate::registry`, `crate::transport`.
- Workspace crates: `abacus_core`, `abacus_wire`.
- External/system interfaces: `libc`, Unix sockets, SCM_RIGHTS, memfd-backed interlocks, `RLIMIT_NOFILE`.
- Standard library: filesystem, Unix file descriptors, atomics, timing, and I/O.

</dependencies>

<consumed-by>

## Consumed By
- Test harness via Rust's `#[test]` discovery; no production consumer is shown.

</consumed-by>

<data-flow>

## Data Flow
- Registry tests construct `Registry` directly, create interlocks, mutate shared atomic words, and call `evaluate_all`.
- Transport tests create a `Server` on a unique temporary Unix-socket path, connect raw `UnixStream` clients, and exchange framed messages and SCM_RIGHTS file descriptors.
- Wire tests encode requests/responses into length-prefixed frames, decode payloads, and inject truncated, corrupt, oversized, and random byte streams.
- Assertions inspect `Condition`, `ProtocolFault`, `TransportError`, interlock words, socket-path state, frame bytes, and timing measurements.

</data-flow>

<known-hazards>

## Known Hazards
- HIGH: The wire decoder is documented to accept an Error response truncated immediately after its code byte as a valid empty-message response; this violates the required strict-prefix truncation contract.
- HIGH: A blocked nonblocking transport write currently reports `EAGAIN`, while the daemon reportedly retains that client instead of treating it as dead; stalled readers can remain live indefinitely.
- HIGH: Watchers reap when their watched target is reaped or recreated; the anti-retargeting guarantee is enforced in `watched_value` via a three-word SENTINEL check.
- MEDIUM: Registry name-length and interlock-count limits are enforced in `validate_name` and `Registry::create`.
- MEDIUM: `raise_fd_limit` changes the process-wide soft `RLIMIT_NOFILE` and does not restore it, coupling later tests to execution order and host limits.
- LOW: The registry performance test uses wall-clock microsecond thresholds and can be noisy on constrained or oversubscribed CI hosts.

</known-hazards>

<files>

## Files
| File | Purpose |
|---|---|
| `mod.rs` | Declares registry and transport test modules and provides shared temporary-path, errno, file-limit, and deterministic RNG helpers. |
| `registry.rs` | Tests registry creation, validation, lifecycle reaping, wait-tier evaluation, watcher ownership, limits, and evaluation performance. |
| `transport.rs` | Tests Unix-domain server lifecycle, nonblocking connection behavior, framing, SCM_RIGHTS descriptor passing, and socket-path policies. |

</files>

<notes>

## Notes
The tests deliberately bypass the daemon loop: registry behavior is evaluated through direct calls, while transport behavior is exercised through the public `Server` and `Connection` APIs.

The random-input tests accept `ABACUS_TEST_SEED` to make a failure reproducible.

</notes>

<reference>

## Reference

</reference>
