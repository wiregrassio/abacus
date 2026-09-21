# crates/abacus-core/

`abacus-core` is the low-level Linux foundation for the Abacus RTS system. It owns the shared interlock representation used to coordinate clients and daemon processes, along with monotonic-time and futex support and the common error vocabulary.

The crate depends on Linux kernel primitives rather than providing a portable synchronization abstraction. Building it therefore requires a Linux target and a kernel that supports memfd creation, file seals, shared mappings, and futexes.

Most users should consume the higher-level SDK or daemon crates instead of manipulating interlock words directly. When reading or changing this directory, treat the interlock's 24-byte layout and sentinel conventions as a cross-process ABI.

<contracts>

## Contracts
- Interlock mappings use a fixed 24-byte layout containing two atomic counters and one atomic expiration timestamp.
- The shared layout's field order, atomic widths, offsets, sentinel value, and low-32-bit futex convention are compatibility-critical across processes.
- Normal interlocks are size-sealed before mappings are distributed, preventing another file-descriptor holder from resizing the backing memfd and invalidating mappings.
- Clock interlocks are writable by the daemon before `F_SEAL_FUTURE_WRITE` prevents future writable mappings for handout recipients.
- Callers waiting on counters must tolerate spurious wakeups, timeout, and the fact that futex comparison observes only a 32-bit portion of the `u64` word.
- Termination is represented by the sentinel in any shared word; consumers must not require all words to become sentinel simultaneously.
- Linux kernel support for required memfd, sealing, mmap, monotonic clock, and futex operations is a runtime precondition.
- The workspace must supply a compatible `libc` dependency.

</contracts>

<units-table>

## Units Table
| Symbol | Unit | Enforced By |
|---|---|---|
| Interlock expiration timestamp | nanoseconds from `CLOCK_MONOTONIC` | `src` clock/interlock implementation |
| Interlock backing allocation | bytes; fixed at 24 | shared-memory layout and memfd sizing |
| Futex comparison word | low 32 bits of a shared `u64` counter | Linux futex ABI and implementation convention |

</units-table>

<test-inventory>

## Test Inventory
- Source-level tests are not shown in the supplied material.
- Shared interlock ABI compatibility: NO TEST visible.
- Sentinel-based lifecycle interpretation across consumers: NO TEST visible.
- Futex low-word behavior, wake handling, and timeout behavior: NO TEST visible.
- Memfd sealing and `F_SEAL_FUTURE_WRITE` handout behavior: NO TEST visible.
- Near-maximum timestamp overflow behavior during initial TTL creation: NO TEST visible.

</test-inventory>

<cross-boundary-verification>

## Cross-Boundary Verification
- Shared 24-byte interlock layout between mapped participants: NOT VERIFIED by tests visible in supplied material.
- memfd size sealing before distribution: NOT VERIFIED by tests visible in supplied material.
- Clock-handout `F_SEAL_FUTURE_WRITE` behavior: NOT VERIFIED by tests visible in supplied material.
- Sentinel interpretation by downstream consumers: NOT VERIFIED by tests visible in supplied material.
- Futex low-32-bit convention between writers and waiters: NOT VERIFIED by tests visible in supplied material.
- Shared error types at daemon/SDK boundaries: NOT VERIFIED by tests visible in supplied material.

</cross-boundary-verification>

<symbol-table>

## Symbol Table
### Cargo.toml
| Symbol | Kind | Purpose | Rationale |
|---|---|---|---|
| `abacus-core` | package | Defines the shared low-level Abacus RTS crate. | Isolates Linux ABI-sensitive synchronization and error primitives from higher-level consumers. |

</symbol-table>
