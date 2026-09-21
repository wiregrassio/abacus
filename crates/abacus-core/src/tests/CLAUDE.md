<purpose>

# crates/abacus-core/src/tests/

</purpose>

<dependencies>

## Dependencies
- Sibling crate modules: `crate::clock`, `crate::error`, and `crate::interlock`
- Rust standard library: atomics, threads, channels, timing, file-descriptor ownership, formatting, environment access
- External package: `libc` for errno constants and Linux syscalls/constants

</dependencies>

<consumed-by>

## Consumed By
- `cargo test -p abacus-core --lib` runs these tests.

</consumed-by>

<data-flow>

## Data Flow
- Test cases construct clocks, atomics, interlocks, file descriptors, error values, and concurrent waiter threads.
- Tests invoke core APIs and selected direct Linux syscalls (`pipe`, `ftruncate`, `fcntl`) to observe behavior and OS-visible state.
- Assertions validate return values, errno values, timing boundaries, shared-memory contents, futex wake behavior, memory layout, and error rendering.
- `ABACUS_TEST_SEED` may enter through the environment to make the contention test's pseudo-random TTL sequence replayable.

</data-flow>

<known-hazards>

## Known Hazards
- HIGH: Futex operations intentionally compare and wake only the low 32 bits of `u64` counters; high-word-only changes are invisible to the kernel and can leave waiters sleeping until timeout.
- HIGH: Futex address derivation assumes a little-endian target, where the first 32 bits of a `u64` are its low word; big-endian builds violate this assumption.
- MEDIUM: Tests require Linux-specific facilities and semantics, including futexes, `memfd` seals, `F_GET_SEALS`, and Linux errno values.
- MEDIUM: Timing-sensitive wake and timeout assertions can fail under extreme scheduler delay or heavily contended CI hosts despite correct implementation behavior.
- LOW: The contention test uses modulo reduction in `Xorshift::below`, which introduces distribution bias; it is sufficient for stress coverage but not statistically uniform sampling.

</known-hazards>

<files>

## Files
| File | Purpose |
|---|---|
| `clock.rs` | Tests monotonic-clock ordering, expiration boundaries, millisecond conversion, and Linux futex behavior. |
| `error.rs` | Tests error enum display strings, conversions, equality, and `std::error::Error` implementation. |
| `interlock.rs` | Tests interlock layout, lifecycle, concurrent arming, futex wakeup behavior, fd sharing, mapping failures, and memfd sealing. |
| `mod.rs` | Declares test modules and provides errno access plus seeded xorshift support for contention tests. |

</files>

<notes>

## Notes
The tests intentionally exercise implementation-visible Linux behavior rather than only public success paths, because interlock safety depends on kernel futex semantics, fixed memory layout, and sealed shared-memory backing.

</notes>

<reference>

## Reference

</reference>
