<purpose>
# abacus-wire/
Shared Abacus v2 wire-protocol crate. Packages message encoding, bounded Unix-stream framing, and SCM_RIGHTS file descriptor transfer for protocol endpoints.
</purpose>

<dependencies>
## Dependencies
Imports workspace crate `abacus-core` and external package `libc`.
</dependencies>

<consumed-by>

## Consumed By
Static import scan, directories importing `abacus-wire`:
- `crates/abacus-daemon`
- `crates/abacus-tests`

(Real import edges, not inferred. A cross-service entry here is a reach into this directory's internals, a coupling to flag.)

</consumed-by>

<data-flow>
## Data Flow
- Protocol requests and responses enter as public wire values.
- `src/` encodes values into length-prefixed payloads, frames stream traffic, and transfers ancillary file descriptors.
- Decoded values and separately owned received descriptors leave for protocol consumers.
</data-flow>

<known-hazards>
## Known Hazards
MEDIUM: Decoders accept trailing payload bytes, canonical-message consumers must enforce payload exhaustion.
MEDIUM: Received SCM_RIGHTS descriptors are separate from decoded responses, consumers must validate descriptor count against the response contract.
HIGH: FD-passing depends on Unix-domain stream sockets and `libc` control-message semantics.
</known-hazards>

<files>
## Files
| File | Purpose |
|------|---------|
| `Cargo.toml` | Defines the `abacus-wire` package and its workspace dependencies. |
| `README.md` | Documents wire-protocol contracts, transport boundaries, and verification gaps. |
</files>

<subdirectories>
## Subdirectories
| Directory | Purpose |
|-----------|---------|
| `src/` | Implements the v2 codec, framing, public API, and SCM_RIGHTS transport. |
</subdirectories>

<reference>
## Reference
See `README.md` for protocol contracts and transport details.
</reference>