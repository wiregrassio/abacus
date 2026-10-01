<purpose>

# crates/

The five workspace crates: shared-memory coordination, wire transport, daemon service, client SDK, and integration-test infrastructure.

</purpose>

<dependencies>

## Dependencies
- Internal crate graph:
  - `abacus-client` depends on `abacus-core` and `abacus-wire`.
  - `abacus-wire` depends on `abacus-core`.
  - `abacus-daemon` depends on `abacus-core` and `abacus-wire`.
  - `abacus-tests` depends on all four implementation crates.
- External package: `libc` for Linux syscalls, Unix sockets, descriptor passing, futexes, signals, process control, and errno handling.
- Rust standard library facilities for atomics, shared memory ownership, Unix sockets, threads, synchronization, timing, files, and process control.
- Linux kernel facilities including memfds, file seals, memory mapping, futexes, `CLOCK_MONOTONIC`, Unix-domain sockets, and `SCM_RIGHTS`.

</dependencies>

<consumed-by>

## Consumed By
- `docs` references crate paths in its design documents.

</consumed-by>

<data-flow>

## Data Flow
- Applications use `abacus-client` to submit create or attach requests over a Unix-domain socket.
- `abacus-wire` frames and decodes requests and responses while transferring shared-memory descriptors through `SCM_RIGHTS`.
- `abacus-daemon` validates requests, manages the named interlock registry, returns duplicated descriptors, and evaluates live interlocks on a nominal 1 ms cadence.
- `abacus-core` maps descriptors into a shared 24-byte atomic layout used for counters, expiration state, process clocks, and futex synchronization.
- `abacus-tests` drives SDK, raw-protocol, daemon-process, hostile-input, timing, and resource scenarios and emits assertions, diagnostics, and probe reports.

</data-flow>

<known-hazards>

## Known Hazards
- **CRITICAL:** Correctness crosses untyped shared-memory and wire boundaries: the client, daemon, core, and raw tests must preserve the same 24-byte layout, sentinel semantics, numeric discriminants, and descriptor expectations.
- **HIGH:** Core futex waits observe only the low 32 bits of a 64-bit counter and assume little-endian layout; upper-half-only changes may not wake waiters, and big-endian targets are incompatible.
- **HIGH:** Wire receipt does not enforce descriptor cardinality; every consumer must validate the number of received descriptors against the decoded response.
- **HIGH:** Most real-process resilience, timing, soak, and hostile-environment tests are ignored, and some document unresolved behavior rather than a passing release baseline.
- **HIGH:** `WaitTimer` defaults to process abort when its fatal delivery margin is missed.
- **MEDIUM:** Shared lifecycle correctness depends on reserving `SENTINEL` globally and recognizing terminal state in any relevant word, although freeing writes only the expiration word.
- **MEDIUM:** The daemon linearly scans live registry entries every millisecond; increased capacity can overrun the scheduling budget and invalidate timing assumptions.
- **MEDIUM:** Protocol decoders accept trailing payload bytes and do not validate several raw numeric values, requiring daemon-side semantic validation.
- **MEDIUM:** Keepalive and timing behavior depend on userspace threads and host scheduling progressing before TTL or delivery deadlines.

</known-hazards>

<files>

## Files
| File | Purpose |
|------|---------|

</files>

<notes>

## Notes
- This directory's central design constraint is cross-crate ABI agreement: Rust types do not enforce the shared-memory layout, protocol tags, ancillary descriptor count, or sentinel conventions across every participant.
- Linux is part of the effective platform contract, not merely an implementation detail.
- The daemon-owned clock occupies registry ID `0`; registry changes must preserve that reservation.

</notes>

<reference>

## Reference
Full contracts, units, symbol table, test inventory, and cross-boundary verification: see `README.md`.

</reference>
