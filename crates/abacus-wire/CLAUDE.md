<purpose>

# crates/abacus-wire/

</purpose>

<dependencies>

## Dependencies
- Sibling crate `abacus-core`: protocol condition and transport/error types.
- External crate `libc`: Unix socket control-message APIs and errno constants.
- Rust standard library Unix socket and file-descriptor ownership APIs.

</dependencies>

<consumed-by>

## Consumed By
Consumed by `abacus-client` and `abacus-daemon` (see their `Cargo.toml`).

</consumed-by>

<data-flow>

## Data Flow
- Callers construct protocol `Request` or `Response` values.
- The `src` implementation serializes values into v2 payloads, prefixes each payload with a four-byte little-endian frame length, and transports frames over Unix streams.
- File descriptors, when required, travel separately as `SCM_RIGHTS` ancillary data associated with initial frame bytes.
- Receiving callers obtain decoded request/response values plus zero or more owned descriptors, and must validate descriptor cardinality for the response type.

</data-flow>

<known-hazards>

## Known Hazards
- HIGH: Received descriptor cardinality is not enforced by the receive helper; consumers must compare received descriptors with the response's expected FD count or may accept missing or unexpected descriptors.
- MEDIUM: Request and response decoding accepts trailing bytes after a valid message; consumers requiring canonical v2 payloads must reject nonexhaustive decoding.
- MEDIUM: Raw numeric protocol values,including tier discriminants, wait-counter words, cron intervals, and barrier conditions,are not validated by the codec; daemon-side request validation is required.
- MEDIUM: Incremental frame reading can fill its bounded buffer before detecting an oversized frame prefix; callers must invoke frame extraction after fills and close protocol-faulted connections.
- LOW: FD-frame sending treats `EAGAIN` as an I/O failure rather than asynchronously retrying; callers must consider the peer unusable under that condition.

</known-hazards>

<files>

## Files
| File | Purpose |
|---|---|
| `Cargo.toml` | Declares the `abacus-wire` crate and its dependencies on `abacus-core` and `libc`. |

</files>

<notes>

## Notes
The protocol is intentionally shared rather than owned by either the daemon or SDK, preventing either endpoint from becoming the other's wire-format dependency.

Ordinary payload serialization and descriptor expectations are separate because Unix ancillary `SCM_RIGHTS` data is not part of a normal frame payload.

</notes>

<reference>

## Reference
Full contracts, units, symbol table, test inventory, and cross-boundary verification: see `README.md`.

</reference>
