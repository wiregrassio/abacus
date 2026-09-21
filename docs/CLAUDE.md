
# docs/

Design contracts, operational procedure, test policy, project philosophy, and deferred work for the Abacus RTS interlock daemon and SDK.

## Files

| File | Purpose |
|------|---------|
| `PHILOSOPHY.md` | The project's design constitution: the laws that govern every architectural choice. |
| `ARCHITECTURE.md` | Why this shape: the primitive, five tiers, the clock, daemon/SDK split, TTL, death detection, the loop, trust model. |
| `CONTRACTS.md` | The frozen interface: shared-memory layout, wire ABI v1, permissions, termination, TTL rules, error contracts. |
| `LIFECYCLE.md` | The state machine: stored and derived properties, daemon evaluation order, tier composition, SDK interpretation. |
| `SURFACE.md` | The public Rust SDK: handles, waits, keepalive, compositions, errors, termination. |
| `VOCABULARY.md` | The controlled term list: every named concept in the codebase with its definition and source. |
| `CONVENTIONS.md` | Repository, code, and documentation conventions: naming, test patterns, style, deployment. |
| `OPERATION.md` | Running the service: installation, socket permissions, limits, scheduling, logging, restart recovery, measured numbers. |
| `TESTING.md` | The quarterly test procedure: five layers (L0 through L4), execution cadence, naming conventions, load profiles. |
| `KNOWN_LIMITATIONS.md` | What Abacus RTS deliberately does not do: platform, primitive, SDK, daemon, and measurement caveats. |
| `BACKLOG.md` | Deferred work requiring wire ABI changes or new bindings: tier metadata, boolean compositions, non-Rust SDK. |

## Contracts

- Normative contracts live in CONTRACTS.md. A change there requires a coordinated daemon and SDK change.
- Measured numbers live in OPERATION.md and nowhere else.
- Every term used normatively appears in VOCABULARY.md.
- Known limitations are in KNOWN_LIMITATIONS.md. Deferred wire-v2 work is in BACKLOG.md.
- Units are stated at every boundary crossing: milliseconds at the SDK surface, nanoseconds on the wire and in shared memory.
- No em or en dashes. Shortest-complete register. Linux-only assumptions are stated, not implied.
