<purpose>
# tests/
L0 unit tests for `abacus-daemon` registry and Unix-domain socket transport. They exercise public daemon components directly, without the daemon event loop.
</purpose>

<dependencies>
## Dependencies
Imports sibling daemon modules `crate::registry` and `crate::transport`; workspace crates `abacus-core` and `abacus-wire`; external `libc`; Rust standard Unix socket, fd, synchronization, timing, and filesystem APIs.
</dependencies>

<consumed-by>
## Consumed By
- Test harness consumers are not visible in supplied material.
</consumed-by>

<data-flow>
## Data Flow
- Registry tests construct `Registry` instances, create and mutate shared interlocks, then evaluate lifecycle, ownership, dependency, wait-tier, and capacity outcomes.
- Transport tests connect raw Unix clients to `Server`, exchange encoded wire frames and SCM_RIGHTS descriptors, then assert protocol and connection error classification.
- Helpers generate process-unique temporary socket paths and raise the process file-descriptor soft limit for high-cardinality tests.
</data-flow>

<known-hazards>
## Known Hazards
- MEDIUM: High-cardinality registry tests modify the process-wide `RLIMIT_NOFILE` soft limit and do not restore it.
- MEDIUM: Performance ceilings and wall-clock timeout assertions can be flaky under heavily contended CI hosts.
</known-hazards>

<files>
## Files
| File | Purpose |
|------|---------|
| `mod.rs` | Test module root and shared temporary-path, errno, and file-descriptor-limit helpers. |
| `registry.rs` | Direct registry tests for creation validation, tier evaluation, lifecycle reaping, ownership, dependencies, limits, and performance. |
| `transport.rs` | UDS server and connection tests for framing, SCM_RIGHTS passing, socket-path handling, nonblocking I/O, and failures. |
</files>