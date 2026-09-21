# Abacus backlog

Known issues and deferred work. Ordered by likely encounter order (what you'll hit first when
integrating).

## Wire protocol

### Attached response does not carry the tier
The Attached wire response carries only the id, not the tier. The SDK cannot enforce
tier-specific permissions on attach, and an attacher cannot verify whether free() via
open_count is appropriate for the tier it attached to. Documented in CONTRACTS.md permission
model. Fix requires wire ABI v2 (add a tier byte to the Attached response payload); the SDK
can then refuse `attach_wait_counter` on a bare interlock and pick the right free() path.

**When:** when a third-party consumer attaches to interlocks it did not create.

### Boolean compositions not implemented
WaitAnd, WaitOr, WaitXor, WaitNand are designed in SURFACE.md and LIFECYCLE.md but not
implemented. They require daemon-side tier discriminants (tiers 5 to 8) not present in wire
ABI v1. WaitOr also retires WaitRace's 1 ms polling.

**When:** when a consumer needs instantaneous boolean state composition (all open, any open),
or when WaitRace polling shows up in a profile.

## SDK

### Non-Rust binding (FFI)
The Rust SDK is the complete implementation; a non-Rust binding wraps it via FFI. The
prerequisite, `TimeoutPolicy::Error`, exists: a binding must select it, because an abort takes
the host with it.

**When:** when a non-Rust consumer needs Abacus coordination.

## Real-time determinism test

### Cohort cron jitter measurement
The existing timing tests measure client-side scheduling latency on non-isolated cores,
which conflates kernel scheduling noise with daemon delivery precision. The right test pins
both daemon and waiters to isolated cores (the deployment shape) and measures daemon-side
delivery jitter: nanoseconds past the whole millisecond boundary when each cron actually
fires. 1000 waiters in 10 phase-staggered cohorts, SCHED_FIFO on the daemon core, 60s run.
The metric is sub-millisecond offset from the expected grid line, not round-trip latency
through a non-isolated scheduler.

**When:** before publishing the performance contract as a specification.

## Known defects (narrow, deferred for v0)

- **Descriptor exhaustion spins the accept loop.** `try_accept` error leaves the listener in
  the pollset with POLLIN asserted; the daemon busy-loops. Requires exhausting the fd table.
- **ProcessClock keepalive can erase a SENTINEL.** Unconditional `open_count` store from the
  clock overwrites a concurrent `free()`. Requires a specific race with the 40 ms tick.
- **Stale-socket detection can unlink a live socket.** A connect failure from permissions or
  transient backlog leads to `remove_file` on a path another daemon may own.
- **Decoders accept trailing bytes.** `decode_request`/`decode_response` do not check the
  cursor consumed the full payload.
- **Keepalive thread spawn panics from a library API.** Thread exhaustion produces a panic
  from `create_interlock` where every other failure returns `SdkError`.
- **Cron interval converts lossily at the boundary.** `saturating_mul` silently clamps a very
  large interval.
- **`interlock_arm` can CAS SENTINEL into `expiration_ns`.** `saturating_add` to `u64::MAX`
  is SENTINEL. `interlock_create` uses plain addition. Reachable only near `u64::MAX`.

**When:** any of these surfaces in production or a second contributor reviews the edge cases.

## CI and deployment

### Expand CI coverage
The gate is fmt, clippy, build, and test on `ubuntu-24.04`. Missing: an aarch64 job (the
target is ARM), an MSRV job (1.85 declared, never tested), a scheduled `--ignored` run (54
ignored tests including all abuse and timing coverage), `--locked`, permissions, concurrency,
timeout, and caching.

**When:** before a second contributor or a CI-gated merge policy.

### Harden the systemd unit
The unit runs as root with no `User=`, `NoNewPrivileges`, `ProtectSystem`, `PrivateTmp`,
`RestrictAddressFamilies`, or `CapabilityBoundingSet`. `RuntimeDirectory` without
`RuntimeDirectoryPreserve=restart` deletes the directory on crash-only restarts.
`Restart=always` with `RestartSec=100ms` under the default `StartLimitBurst=5/10s` leaves the
unit dead after a fast crash loop.

**When:** before deploying outside a controlled lab environment.

## Test quality

### Test kit defects
`ThreadDaemon::restart` is broken and dead. `raise_fd_limit` changes the process-wide soft
limit permanently. `start_daemon` leaks via `mem::forget`. `RawClient::recv_response` allocates
with no `MAX_PAYLOAD` check. Role dispatch is string-keyed.

### Unsafe blocks lack SAFETY comments
Most `unsafe` blocks in product code lack `// SAFETY:` comments. The test kit is annotated;
product code largely is not. Enable `clippy::undocumented_unsafe_blocks` via `[workspace.lints]`.

**When:** before external code review or a second contributor.
