<purpose>
# crates/abacus-core/
Low-level Linux foundation crate for Abacus. It packages the cross-process interlock ABI, monotonic clock and futex support, and shared error vocabulary behind a Linux-specific core dependency.
</purpose>

<dependencies>
## Dependencies
Depends on workspace `libc` for Linux kernel interfaces. Its implementation resides in `src/`.
</dependencies>

<consumed-by>

## Consumed By
Static import scan, directories importing `abacus-core`:
- `crates/abacus-client`
- `crates/abacus-daemon`
- `docs`

(Real import edges, not inferred. A cross-service entry here is a reach into this directory's internals, a coupling to flag.)

</consumed-by>

<data-flow>
## Data Flow
- Inputs: Linux memfd, sealing, shared mapping, monotonic-clock, and futex facilities.
- Outputs: shared interlock mappings and descriptors, timing operations, and common errors for higher-level processes.
</data-flow>

<known-hazards>
## Known Hazards
- CRITICAL: The 24-byte interlock layout, atomic field offsets, sentinel conventions, and low-32-bit futex behavior are cross-process ABI contracts.
- HIGH: Linux kernel support for memfd, seals, shared mappings, monotonic clocks, and futexes is required at runtime.
- HIGH: Visible material shows no verification of shared ABI compatibility, seal handoff behavior, sentinel interpretation, or futex wake and timeout behavior.
</known-hazards>

<files>
## Files
| File | Purpose |
|------|---------|
| `Cargo.toml` | Defines the Linux-specific `abacus-core` package and its `libc` dependency. |
| `README.md` | Details interlock ABI contracts, Linux requirements, units, and verification gaps. |
</files>

<subdirectories>
## Subdirectories
| Directory | Purpose |
|-----------|---------|
| `src/` | Implements the interlock, clock and futex helpers, memfd lifecycle, errors, and tests. |
</subdirectories>

<reference>
## Reference
See `README.md` for ABI contracts, runtime preconditions, units, and test inventory.
</reference>