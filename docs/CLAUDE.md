
# docs/

Design philosophy, mechanism, interface surface, conventions, operations, and deferred work for the Abacus interlock daemon and SDK.

## Files

| File | Purpose |
|------|---------|
| `PHILOSOPHY.md` | The project's design constitution: eight laws governing every architectural choice. Pure doctrine, no implementation names. |
| `DESIGN.md` | How and why: the primitive, five tiers, the clock, daemon evaluation, the loop, TTL, death detection, trust model. |
| `INTERFACE.md` | The frozen interface surface: wire ABI v1, SDK API, per-tier field semantics and method signatures, permissions, errors, termination. |
| `CONVENTIONS.md` | Code style, naming, test conventions (layers, quarterly run, adding tests), git conventions. |
| `OPERATION.md` | Running the service: installation, socket permissions, limits, scheduling, logging, restart recovery, measured numbers. |
| `BACKLOG.md` | Design limits, deferred implementation (wire v2, FFI), known defects, measurement caveats. |

## Contracts

- Normative contracts live in INTERFACE.md. A change there requires a coordinated daemon and SDK change.
- Measured numbers live in OPERATION.md and nowhere else.
- Design limits and deferred work live in BACKLOG.md.
- Units are stated at every boundary crossing: milliseconds at the SDK surface, nanoseconds on the wire and in shared memory.
- No em or en dashes. Shortest-complete register. Linux-only assumptions are stated, not implied.
