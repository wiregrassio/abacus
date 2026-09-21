<purpose>

# common/

Shared process-control, daemon-launch, socket-draining, and isolated-client helpers for `abacus-daemon` L2 integration-test binaries.

</purpose>

<dependencies>

## Dependencies
- `abacus_client::{Abacus RTSClient, WaitState}` for the isolated fresh-client timer health check.
- `abacus_tests::{describe_exit, role_args, role_command, unique_socket_path, wait_child, RawClient}` for role-process dispatch, bounded child execution, temporary paths, exit reporting, and raw socket I/O.
- Rust standard library for filesystem operations, process spawning, Unix-path handling, timing, and I/O.

</dependencies>

<consumed-by>

## Consumed By
- Imported by test binaries in `crates/abacus-daemon/tests/`.

</consumed-by>

<data-flow>

## Data Flow
- Daemon test arguments enter `RawDaemon::spawn` as string slices, plus an optional socket path for cleanup.
- `RawDaemon` starts the Cargo-built `abacus` daemon binary, redirects stderr to a unique file, and exposes the child handle and captured stderr.
- Socket paths and wait durations enter `fresh_client_waits`; it spawns a role child that connects through `Abacus RTSClient`, creates a timer, and waits for delivery.
- The role child returns an OS exit status and stderr; `fresh_client_waits` converts failure into a diagnostic string containing the interpreted exit status and captured stderr.
- A `RawClient` enters `drained_to_eof`; bytes are consumed until EOF/reset or a deadline, returning either the drained byte count or a timeout diagnostic.
- `remaining` converts an elapsed-duration budget into a nonzero remaining timeout.

</data-flow>

<known-hazards>

## Known Hazards
- HIGH: `BIN` is bound at compile time through `CARGO_BIN_EXE_abacus`; helpers cannot test a substituted daemon executable without rebuilding or bypassing this module.
- HIGH: `RawDaemon::Drop` silently ignores process kill, wait, stderr-file removal, and socket-file removal errors, so teardown failures can leave leaked processes or stale Unix sockets that contaminate later tests.
- MEDIUM: `RawDaemon::stderr` uses `unwrap_or_default`, hiding stderr-read failures and potentially discarding diagnostics needed to identify daemon failures.
- MEDIUM: `fresh_client_waits` relies on the external role-dispatch naming convention `"common::role__well_behaved_timer"`; renaming either the dispatch key or ignored test function can break health checks at runtime.
- MEDIUM: `drained_to_eof` treats any read error other than timeout/interruption as successful closure, potentially classifying unexpected socket failures as expected daemon disconnects.
- LOW: Socket paths are converted with `to_str().expect(...)`; a non-UTF-8 path panics rather than producing a test error.

</known-hazards>

<files>

## Files
| File | Purpose |
|---|---|
| `mod.rs` | Defines shared daemon lifecycle, isolated-client health-check, socket EOF, deadline, and role-entry helpers for integration tests. |

</files>

<notes>

## Notes
The fresh-client timer operation deliberately runs in a role subprocess because an SDK timeout abort or hung client creation must not terminate or stall the entire integration-test binary.

`drained_to_eof` drains queued replies before deciding whether the peer closed, avoiding false negatives caused by checking only immediate peer state.

</notes>

<reference>

## Reference

</reference>
