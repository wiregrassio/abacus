<purpose>
# docs/
Canonical Abacus design, interface, operational, philosophical, convention, and backlog documentation. It separates frozen contracts, implementation rationale, measured evidence, project law, and deferred work so each concern has one authoritative home.
</purpose>

<dependencies>
## Dependencies
References workspace Rust crates, root project guidance, deployment units, examples, and tests. Operational guidance depends on Linux, systemd, Cargo, rustfmt, Clippy, and kernel facilities including memfd, futex, Unix sockets, CPU isolation, and real-time scheduling.
</dependencies>

<consumed-by>

## Consumed By
Static import scan (`docs`): no in-repo consumers found.

</consumed-by>

<data-flow>
## Data Flow
- Source contracts, tests, deployment configuration, and measurements enter as implementation evidence.
- `PHILOSOPHY.md` defines design law, `INTERFACE.md` freezes consumer boundaries, and `DESIGN.md` records rationale and mechanisms.
- `OPERATION.md` turns deployment behavior and measurements into operator guidance.
- `CONVENTIONS.md` routes contributors to project-wide code, test, and documentation rules.
- Known defects, permanent limits, and deferred work move into `BACKLOG.md` with shipping triggers.
</data-flow>

<known-hazards>
## Known Hazards
CRITICAL: Socket peers are unauthenticated. Any process able to open the socket can replace names and terminate writable objects.
HIGH: `TimeoutPolicy::Abort` can terminate every connected default-policy client after a daemon stall exceeding the 100 ms clock TTL.
HIGH: Under `TimeoutPolicy::Error`, `WaitCounter` lacks the daemon clock and can return `Timeout` forever after daemon death.
HIGH: Except for the clock, tier write restrictions are SDK conventions over read-write mappings, not kernel-enforced permissions.
HIGH: CPU affinity without kernel-level isolation does not provide the documented timing envelope and can trigger fatal liveness failures.
HIGH: Stale-socket detection can unlink a live daemon socket after permission or transient backlog failures.
MEDIUM: Descriptor exhaustion leaves the listener readable and can busy-loop the daemon accept path.
MEDIUM: Socket mode and ownership are applied after listen; non-systemd startup exposes an umask-derived permission window.
MEDIUM: Blocking stderr writes can delay client aborts or stall the daemon loop.
MEDIUM: Request and response decoders accept trailing bytes.
MEDIUM: SDK and daemon watched-word discriminants are duplicated without a shared type.
MEDIUM: Product `unsafe` blocks largely lack `SAFETY` comments, weakening external review of syscall and pointer invariants.
LOW: Test documentation cites removed contract documents, which can misroute failure investigation.
</known-hazards>

<files>
## Files
| File | Purpose |
|------|---------|
| `BACKLOG.md` | Records permanent design limits, deferred implementation triggers, known defects, verification gaps, and measurement caveats. |
| `CONVENTIONS.md` | Defines workspace language, naming, code, test, documentation, and Git conventions. |
| `DESIGN.md` | Explains the shared-memory primitive, daemon evaluation, SDK composition, liveness, restart, and trust mechanisms. |
| `INTERFACE.md` | Specifies the frozen wire protocol, SDK API, errors, permissions, limits, timing, and termination contracts. |
| `OPERATION.md` | Covers installation, systemd deployment, permissions, scheduling, failure behavior, logs, and measured performance. |
| `PHILOSOPHY.md` | States the architectural laws governing primitives, state, failure, restart, waits, authority, ABI discipline, and testing. |
</files>

<notes>
## Notes
Contract changes belong in `INTERFACE.md`; rationale belongs in `DESIGN.md`; measured behavior belongs in `OPERATION.md`; unshipped work belongs in `BACKLOG.md`. This separation prevents measurements or plans from silently redefining the ABI.
</notes>

<reference>
## Reference
See `../README.md` for the human introduction.
</reference>