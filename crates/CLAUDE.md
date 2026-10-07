<purpose>
# crates/
Rust workspace packages for Abacus shared-memory coordination: Linux primitives, wire protocol, daemon service, client SDK, and integration test infrastructure.
</purpose>

<dependencies>
## Dependencies
Internal edges: `abacus-client` uses `abacus-core` and `abacus-wire`; `abacus-wire` uses `abacus-core`; `abacus-daemon` uses `abacus-core` and `abacus-wire`, with test dependencies on `abacus-client` and `abacus-tests`; `abacus-tests` uses all production crates. Linux-facing crates use `libc`.
</dependencies>

<consumed-by>

## Consumed By
Static import scan, directories importing `crates`:
- `<repo root>`
- `docs`

(Real import edges, not inferred. A cross-service entry here is a reach into this directory's internals, a coupling to flag.)

</consumed-by>

<data-flow>
## Data Flow
- Applications enter through `abacus-client`; daemon requests enter through the Unix socket protocol defined by `abacus-wire`.
- `abacus-daemon` allocates and evaluates interlocks backed by `abacus-core` shared-memory primitives.
- Shared descriptors leave the daemon through SCM_RIGHTS and become mapped client handles.
- `abacus-tests` drives SDK, daemon, raw protocol, lifecycle, timing, and hostile-environment scenarios.
</data-flow>

<known-hazards>
## Known Hazards
- CRITICAL: Client and daemon share a fixed 24-byte atomic ABI. Layout, sentinel, or futex-word changes require coordinated updates.
- CRITICAL: Malformed or truncated shared-memory descriptors can terminate mapped processes with `SIGBUS`.
- HIGH: Runtime behavior requires Linux memfds, seals, shared mappings, futexes, monotonic clocks, Unix sockets, and descriptor passing.
- HIGH: Descriptor cardinality is separate from wire payload decoding and is not verified across every consumer.
- HIGH: Timing, soak, hostile-environment, and resource-recovery coverage is largely ignored by default, so the default suite is not a complete release gate.
- HIGH: Daemon scheduling and client keepalive depend on timely execution; stalls can delay evaluation or reap live resources.
</known-hazards>

<files>
## Files
| File | Purpose |
|------|---------|
| `README.md` | Documents cross-crate contracts, units, test coverage, and verification gaps. |
</files>

<subdirectories>
## Subdirectories
| Directory | Purpose |
|-----------|---------|
| `abacus-client/` | Rust client SDK, shared handle lifecycle, keepalive, and wait primitives. |
| `abacus-core/` | Linux shared-memory ABI, monotonic clock, memfd, and futex primitives. |
| `abacus-daemon/` | Daemon executable, registry, evaluation loop, and Unix socket service. |
| `abacus-tests/` | Shared testkit, examples, and integration, resilience, timing, and hostile-input coverage. |
| `abacus-wire/` | Wire codec, stream framing, and SCM_RIGHTS descriptor transport. |
</subdirectories>

<notes>
## Notes
Read `abacus-core` and `abacus-wire` before following their binary contracts into `abacus-daemon` and `abacus-client`.
</notes>

<reference>
## Reference
See `README.md` for cross-crate contracts, units, and verification status.
</reference>