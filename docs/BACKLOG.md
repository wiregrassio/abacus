
# Abacus backlog

Design limits, deferred work, and known defects. Design limits are permanent choices;
deferred items have a trigger for when they ship; defects are narrow bugs awaiting a fix.

All performance guarantees apply on an isolated core (`isolcpus` or cpuset shield).
Non-isolated operation is best-effort and explicitly out of contract.

## Design limits

### Platform

- **Linux only.** memfd with seals, futex, SCM_RIGHTS, signals, `/proc`. A compile-time guard refuses any other target rather than degrading.
- **Little-endian only**, for futex half-word addressing. A `const` assert fails the build instead of waiting on the wrong half of the word.

### Primitive

- **Futex waits compare the low 32 bits only.** A kernel constraint, not a design choice. A change confined to the upper half costs one poll cadence (100 ms), never a lost wake.
- **`SENTINEL` (`u64::MAX`) is globally reserved.** Termination is detectable from shared memory with no side channel, at the cost of one unusable counter value. An application that must count to `u64::MAX` cannot use a counter word for it.
- **Freeing writes one word per path.** Creator `free()` stamps `expiration_ns`; attacher `free()` stamps `open_count`. Callers detect termination through `interlock_is_terminated`, which reads all three words.

### SDK

- **`WaitTimer` aborts the process by default.** A missed fatal margin means the coordination plane is gone and there is nothing correct left to do. Hosts that cannot be aborted select `TimeoutPolicy::Error`.
- **Wait targets are CAS-max.** Waiters on one handle are not isolated from each other. One handle is one shared object, not a per-caller channel; a second waiter cannot lower the target. Use separate interlocks for independent waits.
- **`is_connected` detects EOF, not health.** It reports true for a daemon that is alive but wedged. Only a completed request proves service.

### Daemon

- **Crash-only, no state recovery, no restart notification.** A restart comes back empty; existing mappings stay valid but orphaned. Waiters discover the restart through the clock's 100 ms TTL. Recreate and resume, or crash and let the supervisor restart the consumer. This is the design, not a gap.
- **No peer authentication.** No `SO_PEERCRED` check: any process that can open the socket can create over any name and terminate any writable object. Security rests on socket ownership and mode, stated plainly rather than half-enforced.
- **No rate limiting or per-peer quota.** Only the global `max_interlocks` cap applies.
- **Slow readers are dropped, not buffered.** `EAGAIN` on write is treated as connection death, because the protocol is strict request/response and a client not draining its socket is broken.
- **No metrics, health endpoint, structured logging, or admin tool.** Diagnostics are stderr lines; the registry is observable only through the SDK. The daemon's budget is a 1 ms loop; an observability surface is a design problem of its own.
- **Real-time scheduling and CPU pinning are left to the deployer**, commented out in the systemd unit. Pinning without kernel-level isolation does not help, so the unit ships neither rather than shipping a half-measure that reads as a guarantee.
- **Socket is listening before its mode is applied.** `UnixListener::bind` sockets, binds, and listens in one call; the chmod follows. In the window the socket accepts connections at umask-derived permissions. The runtime directory's `0755` mode limits exposure to processes that can reach the path.

## Deferred implementation

### Attached response does not carry the tier

The Attached wire response carries only the id, not the tier. The SDK cannot enforce tier-specific permissions on attach, and an attacher cannot verify whether `free()` via `open_count` is appropriate for the tier it attached to. Fix requires wire ABI v2 (add a tier byte to the Attached response payload); the SDK can then refuse `attach_wait_counter` on a bare interlock and pick the right `free()` path. `WaitRace` polling (SDK-side 1 ms poll, scheduling overhead proportional to raced counters) is also retired by wire v2: `WaitOr` replaces it with daemon-side evaluation.

**When:** when a third-party consumer attaches to interlocks it did not create, or when `WaitRace` polling shows up in a profile.

### Boolean compositions

WaitAnd, WaitOr, WaitXor, WaitNand require daemon-side tier discriminants (tiers 5 to 8) not present in wire ABI v1.

**When:** when a consumer needs instantaneous boolean state composition (all open, any open).

### Non-Rust binding (FFI)

The Rust SDK is the complete implementation; a non-Rust binding wraps it via FFI. The prerequisite, `TimeoutPolicy::Error`, exists: a binding must select it, because an abort takes the host with it.

**When:** when a non-Rust consumer needs Abacus coordination.

### Cohort cron jitter measurement

The existing timing tests measure client-side scheduling latency on non-isolated cores, which conflates kernel scheduling noise with daemon delivery precision. The right test pins both daemon and waiters to isolated cores and measures daemon-side delivery jitter: nanoseconds past the millisecond boundary when each cron fires. 1000 waiters in 10 phase-staggered cohorts, SCHED_FIFO on the daemon core, 60s run.

**When:** before publishing the performance contract as a specification.

### Expand CI coverage

The gate is fmt, clippy, build, and test on `ubuntu-24.04`. Missing: an aarch64 job (the target is ARM), an MSRV job (1.85 declared, never tested), a scheduled `--ignored` run (54 ignored tests including all abuse and timing coverage), `--locked`, permissions, concurrency, timeout, and caching.

**When:** before a second contributor or a CI-gated merge policy.

### Harden the systemd unit

The unit runs as root with no `User=`, `NoNewPrivileges`, `ProtectSystem`, `PrivateTmp`, `RestrictAddressFamilies`, or `CapabilityBoundingSet`. `RuntimeDirectory` without `RuntimeDirectoryPreserve=restart` deletes the directory on crash-only restarts. `Restart=always` with `RestartSec=100ms` under the default `StartLimitBurst=5/10s` leaves the unit dead after a fast crash loop.

**When:** before deploying outside a controlled lab environment.

## Known defects (narrow, deferred for v0)

- **Descriptor exhaustion spins the accept loop.** `try_accept` error leaves the listener in the pollset with POLLIN asserted; the daemon busy-loops. Requires exhausting the fd table.
- **ProcessClock keepalive can erase a SENTINEL.** Unconditional `open_count` store from the clock overwrites a concurrent `free()`. Requires a specific race with the 40 ms tick.
- **Stale-socket detection can unlink a live socket.** A connect failure from permissions or transient backlog leads to `remove_file` on a path another daemon may own.
- **Decoders accept trailing bytes.** `decode_request`/`decode_response` do not check the cursor consumed the full payload.
- **Keepalive thread spawn panics from a library API.** Thread exhaustion produces a panic from `create_interlock` where every other failure returns `SdkError`.
- **Cron interval converts lossily at the boundary.** `saturating_mul` silently clamps a very large interval.
- **`interlock_arm` can CAS SENTINEL into `expiration_ns`.** `saturating_add` to `u64::MAX` is SENTINEL. `interlock_create` uses plain addition. Reachable only near `u64::MAX`.

### Verification gaps (no test identified)

- **Monotonic-forward WaitCounter TTL across SDK and lifecycle contract.** `wait_until` arms `2 * timeout_ms`, but no cross-boundary test verifies the arming stays monotonic forward under contention.
- **Socket mode/group deployment boundary.** No test verifies that `--socket-mode` and `--socket-group` produce the expected filesystem permissions.
- **Daemon-restart behavior across all wait tiers.** Individual tiers are tested, but no single test exercises the restart discovery path for every tier in one run.

**When:** any of these surfaces in production or a second contributor reviews the edge cases.

### Test kit defects

`ThreadDaemon::restart` is broken and dead. `raise_fd_limit` changes the process-wide soft limit permanently. `start_daemon` leaks via `mem::forget`. `RawClient::recv_response` allocates with no `MAX_PAYLOAD` check. Role dispatch is string-keyed.

### Unsafe blocks lack SAFETY comments

Most `unsafe` blocks in product code lack `// SAFETY:` comments. The test kit is annotated; product code largely is not. Enable `clippy::undocumented_unsafe_blocks` via `[workspace.lints]`.

**When:** before external code review or a second contributor.

## Measurement caveats

- `Stats::pct` is index selection, not interpolation; percentiles may not match tools that interpolate.
- Randomized tests reduce with `%`, which is biased. Adequate for stress coverage, not for uniform-sampling claims.
