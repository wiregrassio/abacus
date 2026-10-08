
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
- **WaitCounter targets are CAS-max; a WaitTimer takes one waiter.** Waiters on one WaitCounter are not isolated from each other: one handle is one shared object, not a per-caller channel, and a second waiter cannot lower the target. A WaitTimer refuses a second concurrent wait with `InvalidRequest`. Use separate interlocks for independent waits.
- **`is_connected` detects EOF, not health.** It reports true for a daemon that is alive but wedged. Only a completed request proves service.
- **Selecting `TimeoutPolicy::Error` leaves an Abort window.** `connect` starts the keepalive under `Abort`, and `set_timeout_policy` can only run after `connect` returns; a liveness lapse in that gap aborts the process.
- **Read-only views are SDK convention.** `attach_interlock` maps any tier read-write, and attaching a ProcessClock is read-write; the daemon hands out the same descriptor for every tier. Only the clock is protected by its memfd seals.
- **`WatchedWord` discriminants are defined twice.** The SDK (`types::WatchedWord`) and the daemon (`registry::WatchedWord`) each define 0 and 1; nothing shared ties them together.

### Daemon

- **Crash-only, no state recovery, no restart notification.** A restart comes back empty; existing mappings stay valid but orphaned. Waiters discover the restart through the clock's 100 ms TTL. Recreate and resume, or crash and let the supervisor restart the consumer. This is the design, not a gap.
- **No peer authentication.** No `SO_PEERCRED` check: any process that can open the socket can create over any name and terminate any writable object. Recreating any name reaps its holder with no ownership check. Security rests on socket ownership and group mode, stated plainly rather than half-enforced.
- **No rate limiting or per-peer quota.** Only the global `max_interlocks` cap applies.
- **Slow readers are dropped, not buffered.** `EAGAIN` on write is treated as connection death, because the protocol is strict request/response and a client not draining its socket is broken.
- **No metrics, health endpoint, structured logging, or admin tool.** Diagnostics are stderr lines; the registry is observable only through the SDK. The daemon's budget is a 1 ms loop; an observability surface is a design problem of its own.
- **CPU pinning assumes an isolated core.** The unit pins the daemon to core 4 at the default scheduling policy, correct only where the host boots with `isolcpus` covering core 4 and places nothing else there. Without kernel-level isolation the pin is a half-measure that reads as a guarantee, so check the boot parameters before deploying.
- **Socket is listening before its mode is applied.** `UnixListener::bind` sockets, binds, and listens in one call; the chmod and chown follow. In the window the socket accepts connections at umask-derived permissions. The shipped unit closes it with `UMask=0117` (the socket is born 0660); a daemon started any other way, including the test kit's in-process daemons (a process-wide umask is not an option there), still has it. Clients retry `EACCES` (`connect_waiting`).
- **The daemon clock's TTL has no slack beyond a CFS period.** The clock is armed for 100 ms each tick, against the SDK's 200 ms keepalive floor. A daemon stall longer than 100 ms lapses the clock, and every `Abort`-policy client aborts with `DaemonClockLapsed`.
- **Diagnostics are blocking stderr writes.** The keepalive's abort line and the daemon's log lines are plain writes to stderr. A stalled log sink can delay an abort or stall the daemon loop.

## Deferred implementation

### WaitRace polling

`WaitRace` performs SDK-side 1 ms polling (scheduling overhead proportional to raced counters). `WaitOr` replaces it with daemon-side evaluation, requiring a new daemon tier.

**When:** when `WaitRace` polling shows up in a profile.

### ~~WaitCounter waits cannot see a dead daemon under Error policy~~ (resolved)

Resolved: `WaitCounter` now carries the daemon clock handle, and `wait_until` checks it on
every loop iteration (including the zero-timeout path) via `check_live`. `WaitRace::wait`
checks each member's clock on every pass via `is_clock_dead`. Both return
`InterlockReaped` when the clock's expiration is `SENTINEL` or earlier than now.

### Boolean compositions

WaitAnd, WaitOr, WaitXor, WaitNand require daemon-side tier discriminants (tiers 5 to 8) not present in wire ABI v2.

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

## Known defects (narrow, deferred for v0)

- **Startup probe blocks on a full socket backlog.** `probe_socket` uses a blocking `UnixStream::connect` with no timeout. If the daemon's accept queue is full (a stalled daemon with many queued clients), a new instance's startup probe hangs indefinitely. A non-blocking connect with a 1 to 2 second poll timeout would bound it. The EACCES/unlink half of this issue was fixed in the daemon-hardening sprint (probe now refuses on EACCES instead of unlinking).
- **Decoders accept trailing bytes.** `decode_request`/`decode_response` do not check the cursor consumed the full payload.

### Wire hardening (deferred)

Three items for hardening against untrusted or buggy clients. Low risk on a locked-down Jetson where every client is Convoy; relevant if the socket becomes reachable by untrusted code or if a container image switches to Alpine (musl).

- **Reject trailing bytes after decode.** `decode_request`/`decode_response` should verify the cursor consumed the full payload (~5 lines per decoder).
- **Enforce `MAX_MESSAGE_SIZE` on receive.** The frame reader should reject a length prefix above the cap before allocating (~10 lines in `framing.rs`).
- **musl-compatible ancillary data in `fdpass.rs`.** The SCM_RIGHTS code assumes glibc `cmsghdr` field widths. musl uses different padding on some architectures. Needs conditional compilation or runtime sizing.

**When:** before an Alpine-based container image connects to the daemon, or before the socket is exposed beyond Convoy.

### Verification gaps (no test identified)

- **Monotonic-forward WaitCounter TTL across SDK and lifecycle contract.** `wait_until` arms `2 * timeout_ms`, but no cross-boundary test verifies the arming stays monotonic forward under contention.
- **Socket mode/group deployment boundary.** No test verifies that `--socket-mode` and `--socket-group` produce the expected filesystem permissions.
- **Daemon-restart behavior across all wait tiers.** Individual tiers are tested, but no single test exercises the restart discovery path for every tier in one run.

**When:** any of these surfaces in production or a second contributor reviews the edge cases.

### Test kit defects

`raise_fd_limit` changes the process-wide soft limit permanently. `start_daemon` leaks its `ThreadDaemon`: it is never dropped, so the daemon runs until process exit. Role dispatch is string-keyed.

- **Timing tests fail intermittently off an isolated core.** `daemon__loop_keeps_1ms_cadence_after_request_burst`, `wait_cron__reports_overrun_when_off_grid`, and `cron_fires_on_grid_without_drift` depend on scheduling the host does not guarantee without isolation. `wait_cron__reports_overrun_when_off_grid` assumes the SIGSTOP lands within one 10 ms grid step of a fire; measured in docker under load it failed 3 of 40 at `a909452`, and 0 of 20 on the AGX.
- **Test doc comments cite documents that no longer exist.** Many cite `CONTRACTS.md`, `LIFECYCLE.md`, and `SURFACE.md`.

### Unsafe blocks lack SAFETY comments

Most `unsafe` blocks in product code lack `// SAFETY:` comments. The test kit is annotated; product code largely is not. Enable `clippy::undocumented_unsafe_blocks` via `[workspace.lints]`.

**When:** before external code review or a second contributor.

## Measurement caveats

- `Stats::pct` is index selection, not interpolation; percentiles may not match tools that interpolate.
- Randomized tests reduce with `%`, which is biased. Adequate for stress coverage, not for uniform-sampling claims.
