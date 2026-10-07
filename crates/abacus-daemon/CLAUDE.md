<purpose>
# abacus-daemon/
Daemon package for the `abacus` executable and Rust coordination library. Hosts the Unix socket service, millisecond interlock loop, and process-level integration coverage.
</purpose>

<dependencies>
## Dependencies
Uses workspace crates `abacus-core` for interlock and timing primitives, `abacus-wire` for production protocol framing, and `libc` for Unix facilities. Integration tests additionally use `abacus-client` and `abacus-tests`.
</dependencies>

<consumed-by>

## Consumed By
Static import scan, directories importing `abacus-daemon`:
- `<repo root>`

(Real import edges, not inferred. A cross-service entry here is a reach into this directory's internals, a coupling to flag.)

</consumed-by>

<data-flow>
## Data Flow
- CLI configuration enters through the `abacus` binary.
- Unix socket requests enter daemon implementation in `src/`.
- Create and attach operations return wire responses and, when applicable, SCM_RIGHTS-transferred interlock descriptors.
- Integration tests in `tests/` start and exercise real daemon processes.
</data-flow>

<known-hazards>
## Known Hazards
- HIGH: The daemon loop and registry evaluation depend on absolute millisecond boundaries. Scheduling changes can skip clock updates or delay shutdown.
- HIGH: Interlock ID `0` is daemon-owned clock state and must not be allocated as an ordinary interlock.
- HIGH: Registry capacity directly affects per-tick scheduling cost. Compliance with the 1 ms budget at maximum capacity is not verified.
- HIGH: Malicious truncation of clock shared memory can SIGBUS mapped processes. Attack coverage must remain process-isolated.
- MEDIUM: Slow-reading clients are closed on socket `EAGAIN`, not guaranteed strict response delivery.
</known-hazards>

<files>
## Files
| File | Purpose |
|------|---------|
| `Cargo.toml` | Defines the daemon package, `abacus` binary target, and production and test dependencies. |
| `README.md` | Package contracts, units, test inventory, and cross-boundary verification status. |
</files>

<subdirectories>
## Subdirectories
| Directory | Purpose |
|-----------|---------|
| `src/` | Daemon library, event loop, registry, Unix socket transport, and CLI entry point. |
| `tests/` | Real-process integration coverage for lifecycle, clients, timing, and hostile protocol inputs. |
</subdirectories>

<reference>
## Reference
See `README.md` for package contracts, timing units, and verification coverage.
</reference>