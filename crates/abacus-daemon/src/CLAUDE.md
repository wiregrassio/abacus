<purpose>
# src/
Daemon crate implementation: Unix-domain socket server, millisecond event loop, interlock registry, and `abacus` CLI entry point.
</purpose>

<dependencies>
## Dependencies
Imports sibling `registry` and `transport` modules. Uses workspace crates `abacus-core` for shared-memory interlocks, clocks, errors, and futexes, and `abacus-wire` for request framing and responses. Uses `libc` and Rust Unix socket, fd, filesystem, path, and synchronization APIs.
</dependencies>

<consumed-by>

## Consumed By
Consumed by the parent package (`crates/abacus-daemon`).

</consumed-by>

<data-flow>
## Data Flow
- CLI flags become `DaemonConfig`, then configure UDS creation and registry capacity.
- UDS client frames enter `transport::Connection`, decode into wire requests, and become registry create or attach operations.
- Registry creates shared interlocks, evaluates expiry and wait conditions on each daemon tick, and returns responses with duplicated descriptors where needed.
- Responses leave through UDS frames, optionally carrying SCM_RIGHTS file descriptors.
</data-flow>

<known-hazards>
## Known Hazards
- HIGH: The event loop, clock semantics, and registry evaluation rely on absolute millisecond boundaries. Changing scheduling or signal restart behavior can cause clock skips or delayed shutdown.
- HIGH: Registry targets bind to slot and id, not names. Recreating an interlock intentionally reaps dependent watchers rather than retargeting them.
- MEDIUM: `Server` removes its socket path on drop. Incorrect ownership or premature drop can unlink an active daemon endpoint.
- MEDIUM: Client disconnect does not immediately reap client-created interlocks. Their TTL controls cleanup.
</known-hazards>

<files>
## Files
| File | Purpose |
|------|---------|
| `daemon.rs` | Poll-based daemon loop, request dispatch, client lifecycle, and tick scheduling. |
| `lib.rs` | Daemon crate root and public module declarations. |
| `main.rs` | `abacus` binary, CLI parsing, signal handling, and process memory locking. |
| `registry.rs` | Live interlock registry, creation validation, dependency lifecycle, and wait-tier evaluation. |
| `transport.rs` | Unix-domain socket listener, connection framing, socket permissions, and descriptor-bearing responses. |
</files>

<subdirectories>
## Subdirectories
| Directory | Purpose |
|-----------|---------|
| `tests/` | Cross-module unit tests for registry and Unix-domain socket transport behavior. |
</subdirectories>