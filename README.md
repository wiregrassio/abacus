
# Abacus

Abacus (Real-Time Scheduler): lockless atomic coordination for real-time compute. One primitive (the interlock), a crash-only daemon that evaluates five tiers on a 1 ms cadence, and a Rust SDK of typed handles.

An interlock is three `u64` words in a sealed memfd: `open_count`, `closed_count`, `expiration_ns`. The counters are futex-waitable, monotonic, and incremented by arbitrary amounts. Applications create and attach interlocks by name over a Unix-domain socket; the daemon hands back a duplicated descriptor via `SCM_RIGHTS`, and from then on all state lives in shared memory. Waiting costs a futex sleep, not a round trip.

The clock is itself an interlock named `clock`: `open_count` is the current monotonic millisecond, `closed_count` the daemon start time. A timer is a counter watching it.

## Design motivation

Abacus is built for pipelines where a missed wake is a dropped frame and a stuck wait is a
hung pipeline: machine vision, robotics, edge inference, video processing. In these systems
a process that crashes mid-cycle cannot be nursed back to health. The only correct recovery
is to terminate, let the supervisor restart it, and resume processing from a known-good
state. There is no time to reconstruct what was happening; there is only time to start
again.

This constraint shaped every design decision:

- **Crash-only.** The daemon holds no durable state. `panic = "abort"`. A restart comes back
  empty, and that is correct behavior. No journal, no snapshot, no reconciliation.
- **Fail-loud.** A timed wait that misses its deadline by more than 2x aborts the process by
  default, because a real-time stage that missed its budget has already failed. Silent
  degradation is the failure mode this design eliminates.
- **Lifecycle from the primitive.** Liveness is a deadline in shared memory. Stop extending it
  and you are dead by definition. The daemon reaps you; your peers learn from the memory they
  already hold. No release call, no reference counting, no cleanup protocol.
- **Minimal dependencies.** One runtime dependency: `libc`. No async runtime, no allocator, no
  framework. The daemon loop is a hand-rolled `ppoll` with nanosecond timeouts anchored to a
  fixed origin. For a 1 ms cadence, anything between you and the kernel is latency you chose
  to add.

## The five tiers

Every wait type is a contract layered over the one interlock primitive. None is a separate primitive.

| Tier | Daemon does, each cycle | SDK handle |
|------|-------------------------|------------|
| Interlock (0) | reap on expired TTL or SENTINEL | `Interlock`, `AttachedInterlock` |
| WaitCounter (1) | plus: wake when a watched word of another interlock reaches the target | `WaitCounter`, `AttachedWaitCounter` |
| WaitTimer (2) | plus: wake when the clock reaches the target | `WaitTimer` (fatal margin) |
| WaitCron (3) | plus: fire on a grid line, re-arm to the next | `WaitCron` |
| WaitBarrier (4) | plus: fire when N conditions all hold | `WaitBarrier` (re-armable) |

## Crates

| Crate | Purpose |
|-------|---------|
| `abacus-core` | The interlock: memfd allocation and sealing, shared-word layout, futex wait and wake, monotonic clock, shared error types. |
| `abacus-wire` | Wire protocol v2: length-prefixed binary frames, descriptor passing, incremental frame reading. Shared by both endpoints. |
| `abacus-daemon` | The `abacus` binary and library: socket server, named registry, tier evaluation on a 1 ms cadence. |
| `abacus-client` | The Rust SDK: typed handles, compositions, keepalive, timeout policy. |
| `abacus-tests` | Shared test kit and the integration, abuse, soak, and timing suites. |

## Platform

Linux only. The implementation depends on memfd with seals, futex, Unix-domain sockets with `SCM_RIGHTS`, signals, and `/proc`. A compile-time guard in `abacus-core` refuses any other target. Little-endian is assumed for futex half-word addressing.

Rust 1.85 or newer. The only runtime dependency is `libc`.

## Build and test

```bash
cargo build --workspace --release       # daemon binary at target/release/abacus
cargo test --workspace                  # unit, in-process integration, process-level
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

`cargo test` starts daemons on unique sockets under `/tmp` and stops them. It never touches a production socket. The process-level tests run the real binary as a child process.

Long-running timing, soak, and hostile-environment suites are marked `#[ignore]` and run with `--ignored`. See `docs/CONVENTIONS.md` for the procedure and `docs/OPERATION.md` for measured numbers.

## Deploy

`deploy/` contains a systemd unit, a sysusers entry, a sysctl.d file, and a oneshot tuning
service. Together they install the daemon as a sandboxed service on an isolated core with
persistent host tuning that survives reboot. Install steps, the socket permission model,
container bind-mount pattern, and the heartbeat health check are in `docs/OPERATION.md`.

The daemon is crash-only by design. `panic = "abort"` is deliberate: a panic terminates the
process rather than unwinding through a half-evaluated registry. A restart comes back empty,
wakes no one, and every timer discovers the restart through its own fatal margin. systemd
never gives up restarting. The daemon writes `/run/abacus/heartbeat` once per second for
external health checks.

Every local process that can open the socket can create over names, attach to any object, and
terminate writable objects. Deployment security rests on socket ownership and mode.

## Performance

All guarantees hold on an isolated core (`isolcpus` with host tuning from `deploy/`).
Non-isolated operation is best-effort. Measured on a Jetson AGX Orin (L4T R35.5, kernel
5.10.192-tegra, `isolcpus=managed_irq,domain,4-11`, MAXN, clocks pinned).

| Metric | Value |
|--------|-------|
| Clock stamp lateness (daemon jitter) | p50 4-6 us, max 18 us |
| Delivery lateness (10-100 entries) | p50 4-5 us, max 14 us |
| Delivery lateness (4000 entries) | p50 127-130 us |
| Futex wake to waiter running | p50 4-5 us, max 9 us |
| Keepalive soak (FIFO, 100 ms TTL) | survived 600 s |
| Daemon restart (clean) | 12 ms |
| Full-line restart (containers) | 1.08 s |
| Cron 10 ms drift | 0 of 200 off grid |
| Cyclictest baseline (cores 6-11) | max 4-5 us |

720,000 samples per metric, 60 s per cell. Full tables with environment and conditions in
`docs/OPERATION.md`.

## Why not X?

Coordination between processes on one host has two failure axes: **wake latency** (does the
signal arrive inside the frame budget?) and **crash liveness** (if the signaling process dies,
does the pipeline wedge?). Every common alternative fails at least one.

| | Wake latency | Crash-safe | Lifecycle | Dependencies | Scope |
|---|---|---|---|---|---|
| Raw futex | sub-us | no | none | libc | mechanism |
| POSIX semaphore | sub-us | no | none | libc | mechanism |
| pthread condvar | sub-us | partial | none | libc | mechanism |
| eventfd | ~us (syscall each) | no | none | libc | mechanism |
| Redis pub/sub | 30-200+ us | yes | yes | Redis server | service |
| D-Bus | ms | yes | yes | dbus-daemon | service |
| iceoryx2 | sub-us | yes | yes | iceoryx2 runtime | framework |
| **Abacus** | **sub-us** | **yes** | **yes (TTL + reaper)** | **libc** | **primitive** |

The fast primitives (futex, semaphore, condvar, eventfd) provide the wake mechanism but no
crash recovery: a process that dies leaves waiters stuck and no one learns about it from the
primitive itself. The crash-safe services (Redis, D-Bus) add socket transport and
serialization overhead that exceeds a sub-millisecond frame budget.

[iceoryx2](https://github.com/eclipse-iceoryx/iceoryx2) is the closest comparison: Rust
core, shared memory, sub-microsecond, with stale-resource cleanup after process death. It is
a service-oriented zero-copy IPC framework (pub/sub, request-response, service discovery,
C/C++ bindings). Abacus is not a data plane. It is a coordination primitive: three u64 words
in a sealed memfd, a crash-only daemon, and a typed SDK with `libc` as the only dependency.
Use iceoryx2 to move buffers through a service graph. Use Abacus to coordinate timing,
liveness, and sequencing between processes that need to survive each other's crashes.

## Documentation

- `docs/PHILOSOPHY.md`: the design constitution, eight laws governing every architectural choice.
- `docs/DESIGN.md`: mechanism and rationale: the primitive, five tiers, the clock, daemon evaluation, TTL, death detection, trust model.
- `docs/INTERFACE.md`: frozen interface surface: wire ABI v2, SDK API, per-tier field semantics, permissions, errors, termination.
- `docs/CONVENTIONS.md`: code style, naming, test conventions (layers, adding tests, timing procedure).
- `docs/OPERATION.md`: installation, permissions, capacity, scheduling, measured performance.
- `docs/BACKLOG.md`: design limits, deferred work (WaitRace polling, FFI, boolean compositions), known defects.

## License

Apache-2.0. See `LICENSE`.