<purpose>
# tests/
L0 unit-test modules for `abacus-core` clock, error vocabulary, and shared-memory interlock contracts.
</purpose>

<dependencies>
## Dependencies
Imports sibling `clock`, `error`, and `interlock` modules, plus `libc` and Rust standard-library concurrency, timing, fd, and atomic APIs.
</dependencies>

<consumed-by>
## Consumed By
Consumer discovery pending downstream static import scan.
</consumed-by>

<data-flow>
## Data Flow
- Test inputs: core API calls, atomic shared-memory state, OS futex and memfd behavior, optional `ABACUS_TEST_SEED`.
- Test outputs: assertions over return values, layouts, error displays, timing, wake behavior, and sealing.
- Contention tests derive reproducible randomized TTL workloads from a seeded xorshift generator.
</data-flow>

<known-hazards>
## Known Hazards
- HIGH: Futex tests require Linux futex semantics, operate on the low 32 bits of `u64`, and assume little-endian memory layout.
- MEDIUM: Wake and timeout tests depend on scheduler timing, retry stimuli to reduce lost-wake races, and can be flaky under severe system load.
- MEDIUM: Memfd sealing tests temporarily attempt size changes on a live mapping and must restore size before accessing it if sealing is absent.
</known-hazards>

<files>
## Files
| File | Purpose |
|------|---------|
| `clock.rs` | Tests monotonic clock, expiration arithmetic, futex behavior, endian assumption, and duration conversion. |
| `error.rs` | Tests error enum display text, conversions, equality, and `std::error::Error` conformance. |
| `interlock.rs` | Tests interlock layout, TTL arming, termination, futex wakeups, fd sharing, mapping failures, and memfd seals. |
| `mod.rs` | Declares test modules and provides errno access plus seeded xorshift support for contention tests. |
</files>