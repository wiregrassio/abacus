<purpose>

# crates/abacus-daemon/src/

Implements the Abacus daemon process: millisecond event loop, live interlock registry, Unix-domain-socket server, and command-line entry point.

</purpose>

<dependencies>

## Dependencies
- Sibling modules: `registry`, `transport`; test-only `tests`.
- Workspace crates:
  - `abacus_core` for monotonic time, interlock memory/fd operations, expiration checks, futex wakes, and shared error types.
  - `abacus_wire` for the production request/response codec, framing, and SCM_RIGHTS fd transfer.
- External/system packages: `libc` for `ppoll`, signals, `prctl`, Unix socket operations, passwd/group lookup, and socket-related errno values.
- Standard library: atomics, Unix file descriptors and sockets, paths, collections, filesystem metadata, and timing.

</dependencies>

<consumed-by>

## Consumed By
- `crates/abacus-daemon/src/main.rs` invokes the library daemon loop to implement the `abacus` executable.
- Downstream consumers of the `abacus_daemon` library can invoke `daemon::daemon_run`, `daemon::daemon_run_with`, and public registry/transport APIs.
- Test harness consumes inline test modules and the `tests/` subdirectory.

</consumed-by>

<data-flow>

## Data Flow
- Command-line arguments enter `main.rs`, are parsed into `DaemonConfig`, and determine the Unix socket path, permissions, optional socket group, and registry limit.
- `daemon_run_with` creates a `transport::Server`, initializes a `registry::Registry` and its daemon-owned clock interlock, then waits with `ppoll` until either socket activity or the next 1 ms boundary.
- Client bytes enter through `Connection`, are frame-buffered and decoded by `abacus_wire`, then become `Request::{CreateInterlock, AttachInterlock}` values.
- Create requests are validated and installed into the registry; attach requests resolve existing entries. Successful operations return a response plus a duplicated interlock fd through SCM_RIGHTS.
- Each elapsed millisecond boundary advances the clock word, evaluates expiry and wait-tier conditions, reaps dead entries, stores completion stamps, and futex-wakes waiters.
- The daemon exits after SIGTERM/SIGINT sets `STOP`; `Server::Drop` removes its socket path.

</data-flow>

<known-hazards>

## Known Hazards
- HIGH: Socket-path stale detection treats any failed connection probe as a stale socket and removes it; a temporarily unreachable or permission-denied live daemon socket could be deleted before bind.
- MEDIUM: The daemon treats `EAGAIN` while sending a strict request/response reply as a dead connection rather than buffering the response; slow-reading clients lose their connection and request result.
- MEDIUM: Registry evaluation is a full linear scan of all live slots every millisecond; increasing `max_interlocks` directly consumes the daemon's fixed 1 ms scheduling budget.
- MEDIUM: The signal stop flag is only observed around `ppoll` iterations; shutdown does not explicitly wake a non-signal caller that sets the supplied `AtomicBool`, so clean stop latency can be up to the current poll timeout.
- LOW: The timer-slack `prctl` is process-wide and best-effort; failure merely logs, allowing millisecond evaluations to wake late.

</known-hazards>

<files>

## Files
| File | Purpose |
|---|---|
| `daemon.rs` | Runs the `ppoll`-based daemon loop, services client requests, and coordinates registry ticks. |
| `lib.rs` | Declares the daemon library's public production modules. |
| `main.rs` | Implements the `abacus` executable, CLI parsing, and SIGTERM/SIGINT shutdown handling. |
| `registry.rs` | Owns clock and named interlocks, validates creation requests, evaluates wait tiers, and reaps entries. |
| `transport.rs` | Implements Unix-domain socket creation, socket-file policy, accepted client connections, and framed response delivery. |

</files>

<notes>

## Notes
The registry deliberately binds watchers to a resolved slot and interlock id, not merely a name. Recreating a named target therefore terminates existing watchers rather than silently retargeting them.

The clock is a reserved daemon-owned interlock with id `0`. Its `closed_count` records daemon start time in milliseconds, while its `open_count` is refreshed every evaluation cycle.

</notes>

<reference>

## Reference

</reference>
