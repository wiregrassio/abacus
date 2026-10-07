<purpose>
# abacus-tests/
Workspace test-focused crate providing reusable daemon, SDK, raw Unix-socket, process-measurement, and protocol-failure test infrastructure for Abacus integration coverage.
</purpose>

<dependencies>
## Dependencies
Imports workspace crates `abacus-core`, `abacus-wire`, `abacus-daemon`, and `abacus-client`. Uses `libc` for Linux-specific facilities.
</dependencies>

<consumed-by>

## Consumed By
Static import scan, directories importing `abacus-tests`:
- `<repo root>`
- `crates`
- `crates/abacus-daemon`
- `docs`

(Real import edges, not inferred. A cross-service entry here is a reach into this directory's internals, a coupling to flag.)

</consumed-by>

<data-flow>
## Data Flow
- Integration tests and examples supply daemon configurations, SDK requests, raw ABI-v2 frames, child-role inputs, and measurement parameters.
- Shared testkit starts daemons and clients, exchanges SDK or raw Unix-socket traffic, and returns protocol responses, process metrics, child output, and timing statistics.
- Tests assert client, daemon, wire, interlock, liveness, resource, and timing behavior.
</data-flow>

<known-hazards>
## Known Hazards
- CRITICAL: Hostile interlock mappings can SIGBUS after backing-memory truncation. Isolate untrusted mappings in child-process roles.
- HIGH: Raw-client helpers bypass SDK validation and can issue malformed frames or descriptor-transfer scenarios.
- HIGH: Ignored abuse, soak, timing, and affinity tests can exhaust resources, alter process state, or require suitable Linux host privileges.
- MEDIUM: `/proc` and process measurements are best-effort and may report zero when unavailable or unparsable.
- MEDIUM: ABI-v2 wire expectations, numeric tags, limits, error codes, and SCM_RIGHTS handling must remain coordinated with `abacus-wire`.
</known-hazards>

<files>
## Files
| File | Purpose |
|------|---------|
| `Cargo.toml` | Declares the `abacus-tests` testkit and integration-test crate with workspace dependencies. |
| `README.md` | Documents testkit contracts, coverage inventory, units, and cross-boundary verification status. |
</files>

<subdirectories>
## Subdirectories
| Directory | Purpose |
|-----------|---------|
| `examples/` | Operational and integration executables for live daemon probing, coordination, and timing measurement. |
| `src/` | Shared testkit for daemon lifecycle, raw wire clients, process roles, metrics, and permission doctests. |
| `tests/` | Integration, resilience, regression, abuse, soak, and timing test suites. |
</subdirectories>

<reference>
## Reference
`README.md` contains contracts, test inventory, and verification coverage detail.
</reference>