<purpose>

# src/

Shared wire-protocol implementation for Abacus v1: codec, length-prefixed Unix-stream framing, and SCM_RIGHTS file-descriptor passing.

</purpose>

<dependencies>

## Dependencies
- Sibling crate `abacus-core`: `Condition`, `ProtocolFault`, `TransportError`, and `IoOperation` error types.
- External crate `libc`: Unix socket control-message APIs and errno constants.
- Rust standard library: Unix-domain streams and file-descriptor ownership types.

</dependencies>

<consumed-by>

## Consumed By
- `abacus-client` and `abacus-daemon` import this crate for request/response encoding and descriptor passing.

</consumed-by>

<data-flow>

## Data Flow
- Client or daemon constructs a `Request` or `Response` value.
- `codec` serializes it into a v1 payload and a four-byte little-endian frame-length prefix.
- `framing` transports ordinary frame bytes over `UnixStream`, either incrementally through `FrameReader` or synchronously through `read_exact` / `write_all`.
- `fdpass` sends or receives the first frame bytes using `sendmsg` / `recvmsg` and associates up to one `SCM_RIGHTS` descriptor with the frame prefix.
- Received payload bytes are decoded back into `Request` or `Response`; received descriptors are returned as `OwnedFd` values for the caller to validate against the response type.

</data-flow>

<known-hazards>

## Known Hazards
MEDIUM: `decode_request` and `decode_response` accept trailing payload bytes after a valid message; consumers that require canonical v1 encodings must independently reject nonexhaustive decoding.  
HIGH: Descriptor cardinality is not enforced by `recv_prefix_with_fds`; callers must compare received descriptors against `expected_fd_count(response)` or risk accepting an FD-less success response or an unexpected descriptor.  
MEDIUM: Tier discriminants, WaitCounter word values, cron intervals, and barrier-condition semantics are encoded as raw numeric values and are not validated by the codec; daemon-side request validation is required.  
MEDIUM: `FrameReader` is intentionally bounded to one maximum frame and reports a full incomplete buffer as progress until `next_frame` detects the oversized prefix; callers must invoke `next_frame` after fills and terminate protocol-faulted connections.  
LOW: `send_frame_with_fds` treats `EAGAIN` as an I/O failure rather than retrying asynchronously; callers must treat this as a dead or unusable peer as documented.

</known-hazards>

<files>

## Files
| File | Purpose |
|---|---|
| `codec.rs` | Defines wire-v1 request/response types, binary encoding and decoding, error-code mapping, frame-size limits, and expected response FD counts. |
| `fdpass.rs` | Sends frames with SCM_RIGHTS descriptors and receives frame prefixes plus owned descriptors over Unix streams. |
| `framing.rs` | Implements bounded incremental frame reception and blocking exact-read/full-write transport helpers. |
| `lib.rs` | Exposes the wire crate's public modules and re-exports its protocol and transport API. |

</files>

<notes>

## Notes
The protocol deliberately keeps wire data independent of either daemon or SDK implementation: both sides depend on this crate rather than on each other.

Response FD expectations are represented separately from response serialization because Unix SCM_RIGHTS ancillary data cannot be expressed inside the ordinary frame payload.

</notes>

<reference>

## Reference

</reference>
