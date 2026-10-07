<purpose>
# common/
Shared L2 integration-test helpers for spawning and probing the `abacus` daemon without Cargo treating this module directory as a standalone test target.
</purpose>

<dependencies>
## Dependencies
Imports `abacus_client` for client and timer checks, and `abacus_tests` for role-process dispatch, raw socket clients, unique paths, and child-process utilities. Uses Rust standard filesystem, I/O, process, path, and time APIs.
</dependencies>

<consumed-by>

## Consumed By
Static import scan, directories importing `common`:
- `crates/abacus-daemon/tests`

(Real import edges, not inferred. A cross-service entry here is a reach into this directory's internals, a coupling to flag.)

</consumed-by>

<data-flow>
## Data Flow
- Test inputs: daemon arguments, socket paths, timing budgets, and raw client connections.
- Spawns the Cargo-built daemon or isolated role child processes, capturing daemon stderr to temporary files.
- Returns process, client-delivery, EOF-drain, and remaining-deadline results to calling integration tests.
</data-flow>

<known-hazards>
## Known Hazards
- HIGH: `RawDaemon` cleanup is Drop-dependent. Leaked instances can leave daemon processes or socket files behind.
- HIGH: `fresh_client_waits` must isolate SDK `wait_ms` in a role child. Calling short-budget waits in the test process can abort or hang the entire test binary.
- MEDIUM: `drained_to_eof` must be used instead of peer-close peeking when queued replies can precede connection closure.
</known-hazards>

<files>
## Files
| File | Purpose |
|------|---------|
| `mod.rs` | Daemon lifecycle, fresh-client timer, socket EOF, deadline, and role-process helpers. |
</files>