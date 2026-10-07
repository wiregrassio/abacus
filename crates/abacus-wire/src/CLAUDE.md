<purpose>
# src/
Wire ABI v2 implementation shared by the daemon and client SDK. Defines request and response codecs, Unix-stream framing, and SCM_RIGHTS file descriptor transport without either endpoint depending on the other.
</purpose>

<dependencies>
## Dependencies
Imports `abacus_core::error` for protocol and transport faults. Uses `libc` and the Rust standard library Unix socket and file descriptor APIs.
</dependencies>

<consumed-by>

## Consumed By
Consumed by the parent package (`crates/abacus-wire`).

</consumed-by>

<data-flow>
## Data Flow
- Client and daemon requests and responses enter as `Request` and `Response` values, or framed Unix-stream bytes.
- `codec.rs` transforms values into versioned length-prefixed frames and payloads back into values.
- `framing.rs` buffers non-blocking inbound frames or fully reads and writes blocking streams.
- `fdpass.rs` sends and receives frame prefixes with SCM_RIGHTS descriptors.
</data-flow>

<known-hazards>
## Known Hazards
MEDIUM: `decode_request` and `decode_response` accept trailing payload bytes, so callers requiring canonical wire messages must reject unconsumed data externally.
MEDIUM: Received SCM_RIGHTS descriptors are returned separately from decoded responses, callers must enforce `expected_fd_count` before accepting a message.
</known-hazards>

<files>
## Files
| File | Purpose |
|------|---------|
| `codec.rs` | ABI v2 request and response types, binary encoding, decoding, error mapping, and frame limits. |
| `fdpass.rs` | SCM_RIGHTS descriptor passing with frame-prefix send and receive helpers. |
| `framing.rs` | Length-prefixed Unix-stream buffering and blocking read/write primitives. |
| `lib.rs` | Public module declarations and crate-level wire API re-exports. |
</files>