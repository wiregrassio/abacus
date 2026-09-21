# crates/abacus-wire/

`abacus-wire` is the shared protocol boundary for Abacus RTS components communicating over Unix-domain streams. It contains the v1 message encoding, bounded length-prefixed framing, and support for passing file descriptors through Unix socket ancillary data.

Use this crate when implementing either side of the protocol. Construct requests or responses using its public protocol API, serialize and frame them for transport, and decode received frames back into protocol values. Responses that can carry file descriptors require separate validation of the received descriptor count.

When reading the implementation, start with the public exports in `src/lib.rs`, then follow the codec for message layout, framing for normal stream transport, and FD-passing support for `SCM_RIGHTS` behavior.

<contracts>

## Contracts
- Frames use a four-byte little-endian payload-length prefix.
- Normal protocol data is encoded independently from Unix ancillary descriptor transfer.
- Receive operations may return owned file descriptors without enforcing whether their count matches the decoded response; consumers must apply the response FD-count contract.
- Decoders accept valid messages with trailing bytes; canonical-message consumers must perform an additional exhaustion check.
- The incremental reader is bounded to one maximum frame and requires callers to poll/extract frames after input fills.
- FD-passing transport depends on Unix-domain stream sockets and `libc` control-message semantics.
- Numeric fields whose semantic validity is protocol-specific are passed through the codec without complete semantic validation.

</contracts>

<units-table>

## Units Table
| Symbol | Unit | Enforced By |
|---|---|---|
| Frame length prefix | bytes of payload | Four-byte little-endian v1 framing contract |
| Frame-length prefix width | bytes | Protocol contract |
| SCM_RIGHTS descriptor count | file descriptors | Caller validation against expected response count |
| Incremental reader capacity | one maximum frame | Framing implementation contract |

</units-table>

<test-inventory>

## Test Inventory
- Source-level tests are not included in the supplied material.
- Binary codec round trips and malformed-input handling: test status not visible.
- Frame boundary, oversized-frame, and partial-read behavior: test status not visible.
- `SCM_RIGHTS` send/receive behavior, ownership transfer, and malformed ancillary-data handling: test status not visible.
- Response-type-to-FD-count validation: caller-owned coupling; test status not visible.
- Canonical decoding rejection of trailing bytes: not provided by the codec; consumer-side test status not visible.

</test-inventory>

<cross-boundary-verification>

## Cross-Boundary Verification
- Codec payload layout <-> framing length-prefix transport: NOT VERIFIED by tests in supplied material.
- Response type <-> expected descriptor count: NOT VERIFIED by tests in supplied material.
- FD-passing helper <-> Unix `SCM_RIGHTS` kernel behavior: NOT VERIFIED by tests in supplied material.
- Decoder trailing-byte acceptance <-> consumer canonicality enforcement: NOT VERIFIED by tests in supplied material.
- Raw numeric field encoding <-> daemon semantic validation: NOT VERIFIED by tests in supplied material.

</cross-boundary-verification>

<symbol-table>

## Symbol Table
### Cargo.toml
| Symbol | Kind | Purpose | Rationale |
|---|---|---|---|
| `abacus-wire` | crate | Packages the shared Abacus RTS v1 wire-protocol implementation. | Keeps daemon and SDK protocol behavior in a common dependency rather than coupling either implementation directly to the other. |

</symbol-table>
