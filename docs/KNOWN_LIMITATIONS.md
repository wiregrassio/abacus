
# Known limitations

What Abacus RTS deliberately does not do. Each entry states the decision, the reason, and the
consequence for a caller. Anything that cannot be written that way is a defect, not a
limitation. Deferred wire-v2 work is in `BACKLOG.md` and cross-referenced, not restated.

All performance guarantees apply on an isolated core (`isolcpus` or cpuset shield).
Non-isolated operation is best-effort and explicitly out of contract.

## Platform

- **Linux only.** memfd with seals, futex, SCM_RIGHTS, signals, `/proc`. A compile-time
  guard refuses any other target rather than degrading.
- **Little-endian only**, for futex half-word addressing. A `const` assert fails the build
  instead of waiting on the wrong half of the word.

## Primitive

- **Futex waits compare the low 32 bits only.** This is a kernel constraint, not a design
  choice. A change confined to the upper half costs one poll cadence (100 ms), never a lost
  wake.
- **`SENTINEL` (`u64::MAX`) is globally reserved.** Termination is detectable from shared
  memory with no side channel, at the cost of one unusable counter value. An application
  that must count to `u64::MAX` cannot use a counter word for it.
- **Freeing writes one word per path.** Creator `free()` stamps `expiration_ns`; attacher
  `free()` stamps `open_count`. Callers detect termination through `interlock_is_terminated`,
  which reads all three words.

## SDK

- **`WaitTimer` aborts the process by default.** The blocking wait is the liveness check.
  A missed fatal margin means the coordination plane is gone and there is nothing correct
  left to do. Hosts that cannot be aborted select `TimeoutPolicy::Error`.
- **Wait targets are CAS-max.** Waiters on one handle are not isolated from each other.
  One handle is one shared object, not a per-caller channel; a second waiter cannot lower
  the target. Use separate interlocks for independent waits.
- **`attach_wait_counter` cannot verify the tier.** The `Attached` response carries only an
  id. Attaching a non-tier-1 name yields a handle whose reads are meaningless rather than
  an error. Deferred to wire v2 (`BACKLOG.md`).
- **`is_connected` detects EOF, not health.** It reports true for a daemon that is alive but
  wedged. Only a completed request proves service.
- **`WaitRace` is SDK-side polling at 1 ms.** It adds scheduling overhead proportional to
  the number of raced counters. This is a v0 mechanism; daemon-side evaluation of races is
  deferred to wire v2.

## Daemon and deployment

- **Crash-only, no state recovery, no restart notification.** A restart comes back empty;
  existing mappings stay valid but orphaned. Waiters discover the restart through the clock's
  100 ms TTL. Recreate and resume, or crash and let the supervisor restart the consumer.
  This is the design, not a gap.
- **No peer authentication.** No `SO_PEERCRED` check: any process that can open the socket
  can create over any name and terminate any writable object. Security rests on socket
  ownership and mode, stated plainly rather than half-enforced.
- **No rate limiting or per-peer quota.** Only the global `max_interlocks` cap applies.
- **Slow readers are dropped, not buffered.** `EAGAIN` on write is treated as connection
  death, because the protocol is strict request/response and a client not draining its
  socket is broken.
- **No metrics, health endpoint, structured logging, or admin tool.** Diagnostics are stderr
  lines; the registry is observable only through the SDK. The daemon's budget is a 1 ms loop;
  an observability surface is a design problem of its own.
- **Real-time scheduling and CPU pinning are left to the deployer**, commented out in the
  systemd unit. Pinning without kernel-level isolation does not help, so the unit ships
  neither rather than shipping a half-measure that reads as a guarantee.
- **Socket is listening before its mode is applied.** `UnixListener::bind` sockets, binds,
  and listens in one call; the chmod follows. In the window the socket accepts connections
  at umask-derived permissions. The runtime directory's `0755` mode limits exposure to
  processes that can reach the path.

## Measurement caveats

- `Stats::pct` is index selection, not interpolation; percentiles may not match tools that
  interpolate.
- Randomized tests reduce with `%`, which is biased. Adequate for stress coverage, not for
  uniform-sampling claims.
