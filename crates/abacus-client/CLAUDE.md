<purpose>
# abacus-client/
Cargo package for the Abacus Rust client SDK, connecting applications to the daemon and exposing shared-memory interlock handles and wait primitives from `src/`.
</purpose>

<dependencies>
## Dependencies
`src/` imports workspace crates `abacus-core` and `abacus-wire`, plus `libc`. Package metadata inherits workspace versioning, edition, license, and Rust version.
</dependencies>

<consumed-by>

## Consumed By
Static import scan, directories importing `abacus-client`:
- `crates/abacus-daemon`
- `crates/abacus-tests`
- `docs`

(Real import edges, not inferred. A cross-service entry here is a reach into this directory's internals, a coupling to flag.)

</consumed-by>

<data-flow>
## Data Flow
- Applications call the public SDK exported from `src/`.
- The SDK exchanges Unix-domain socket requests and descriptor-bearing responses with the Abacus daemon.
- Received shared-memory descriptors become typed interlock handles; keepalive refreshes registered resource TTLs.
</data-flow>

<known-hazards>
## Known Hazards
- HIGH: The SDK aborts the host process on a missed `WaitTimer` fatal margin or lost liveness.
- HIGH: Keepalive can starve under inherited scheduling or CPU affinity, allowing registered resources to reap.
- MEDIUM: `AttachedInterlock::free` terminates the shared interlock for all holders despite attached expiration being read-only.
- MEDIUM: `WaitRace` uses millisecond SDK-side polling pending daemon-side `WaitOr`.
- MEDIUM: Daemon transport, descriptor mapping, shared-memory interoperability, reaping, and keepalive behavior lack evidenced integration coverage.
</known-hazards>

<files>
## Files
| File | Purpose |
|------|---------|
| `Cargo.toml` | Defines the `abacus-client` SDK package and its workspace dependencies. |
| `README.md` | Documents SDK contracts, units, test coverage, and cross-boundary verification gaps. |
</files>

<subdirectories>
## Subdirectories
| Directory | Purpose |
|-----------|---------|
| `src/` | Public SDK implementation, client transport, handle operations, keepalive, types, and wait primitives. |
</subdirectories>

<reference>
## Reference
See `README.md` for package contracts and verification status.
</reference>