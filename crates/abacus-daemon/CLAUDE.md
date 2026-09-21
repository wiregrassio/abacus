<purpose>

# crates/abacus-daemon/

Builds the `abacus` daemon executable and library that run Abacus RTS's 1 ms coordination loop over a Unix-domain-socket interface.

</purpose>

<dependencies>

## Dependencies
- `abacus-core`: monotonic time, interlock memory and fd operations, expiry checks, futex wakes, and shared errors.
- `abacus-wire`: production request/response framing, protocol codec, and SCM_RIGHTS fd transfer.
- `libc`: Unix polling, signals, sockets, process controls, and system errno interfaces.
- Dev dependencies:
  - `abacus-client`: SDK-level daemon integration testing.
  - `abacus-tests`: shared daemon-process, raw-protocol, timing, and test-role infrastructure.

</dependencies>

<consumed-by>

## Consumed By
`abacus-tests` imports this crate as a dev-dependency for in-process daemon tests.

</consumed-by>

<data-flow>

## Data Flow
- CLI arguments enter the `abacus` binary and configure the socket path, socket permissions/group, and registry capacity.
- The daemon creates a Unix socket server, registry, and daemon-owned clock interlock.
- Clients send framed `abacus_wire` requests over Unix sockets; create and attach requests resolve to registry operations.
- Successful requests produce wire responses and duplicated interlock file descriptors delivered with SCM_RIGHTS.
- The daemon's `ppoll` loop processes socket activity and advances the registry at elapsed 1 ms boundaries.
- Registry evaluation updates the clock, expires and reaps interlocks, completes wait tiers, and futex-wakes waiting clients.
- SIGTERM or SIGINT stops the process; socket cleanup occurs when the server is dropped.

</data-flow>

<known-hazards>

## Known Hazards
- HIGH: Socket stale-path detection can remove a live daemon socket when a connection probe fails due to temporary reachability or permission conditions.
- MEDIUM: Strict reply sending treats `EAGAIN` as a dead client rather than buffering a response, so slow-reading clients can lose request results.
- MEDIUM: Registry evaluation linearly scans all live slots every millisecond; raising the registry limit directly risks exceeding the fixed scheduling budget.
- MEDIUM: External stop-flag changes do not explicitly wake `ppoll`, so non-signal shutdown latency can last until the current poll timeout.
- HIGH: Several process and hostile-client integration tests document currently red daemon behaviors; unresolved implementation defects make this suite fail rather than serve as a green regression baseline.
- HIGH: Timing tests depend on host scheduling, startup latency, daemon cadence, and CPU-tick accounting, making them vulnerable to slow or heavily loaded test hosts.

</known-hazards>

<files>

## Files
| File | Purpose |
|---|---|
| `Cargo.toml` | Defines the `abacus-daemon` package, its `abacus` binary target, and runtime/test dependencies. |

</files>

<notes>

## Notes
The package deliberately separates daemon implementation from its process-level test suite: unit and module behavior live under `src/`, while `tests/` launches real daemon processes and crosses Unix socket, signal, fd-transfer, and shared-memory boundaries.

The daemon-owned clock is a reserved interlock with ID `0`; consumers relying on registry state must preserve that reservation.

</notes>

<reference>

## Reference
Full contracts, units, symbol table, test inventory, and cross-boundary verification: see `README.md`.

</reference>
