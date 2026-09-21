
# Abacus

Abacus RTS (Real-Time Scheduler): lockless atomic coordination for real-time compute. One primitive (the interlock), a crash-only daemon that evaluates five tiers on a 1 ms cadence, and a Rust SDK of typed handles.

An interlock is three `u64` words in a sealed memfd: `open_count`, `closed_count`, `expiration_ns`. The counters are futex-waitable, monotonic, and incremented by arbitrary amounts. Applications create and attach interlocks by name over a Unix-domain socket; the daemon hands back a duplicated descriptor via `SCM_RIGHTS`, and from then on all state lives in shared memory. Waiting costs a futex sleep, not a round trip.

The clock is itself an interlock named `clock`: `open_count` is the current monotonic millisecond, `closed_count` the daemon start time. A timer is a counter watching it.

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
| `abacus-wire` | Wire protocol v1: length-prefixed binary frames, descriptor passing, incremental frame reading. Shared by both endpoints. |
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

Long-running timing, soak, and hostile-environment suites are marked `#[ignore]` and run with `--ignored`. See `docs/TESTING.md` for the procedure and `docs/OPERATION.md` for measured numbers.

## Deploy

`deploy/abacus-rts.service` is the systemd unit. It creates `/run/abacus-rts` and launches the daemon on `/run/abacus-rts/abacus.sock`. Install steps, the socket permission model, and the file-descriptor limit are in `docs/OPERATION.md`.

The daemon is crash-only by design. `panic = "abort"` is deliberate: a panic terminates the process rather than unwinding through a half-evaluated registry. A restart comes back empty, wakes no one, and every timer discovers the restart through its own fatal margin.

Every local process that can open the socket can create over names, attach to any object, and terminate writable objects. Deployment security rests on socket ownership and mode.

## Performance

All guarantees hold on an isolated core (`isolcpus` or cpuset shield). Non-isolated operation is best-effort.

| Metric | Idle | Production load | Stress (44 threads) |
|--------|------|-----------------|---------------------|
| `wait_ms(5)` p99 | 5.0 ms | 6.0 ms | 5.9 ms |
| `wait_ms(5)` max | 5.6 ms | 12.5 ms | 16.9 ms |
| Cron 10 ms drift | 0 of 200 off grid | 0 of 200 off grid | |
| Daemon idle CPU | < 1% | | 0 ticks / 3 s |
| 1000 interlocks eval | 21 ticks / 10 s | | |

Cadence is 1 ms with sub-millisecond median jitter on an isolated core. Full measurement
tables with conditions in `docs/OPERATION.md`.

Full measurement tables with conditions in `docs/OPERATION.md`.

## CLAUDE.md convention

`CLAUDE.md` files are agent context maps, auto-injected root-to-leaf by Claude Code when it
reads a file in the directory. They serve as machine-readable directory maps. `README.md` files
are for humans and live at crate roots and top-level directories only.

## Documentation

- `docs/ARCHITECTURE.md`: why the primitive, the tiers, the loop, and the TTL model are shaped this way.
- `docs/CONTRACTS.md`: the frozen daemon, SDK, shared-memory, and wire contracts.
- `docs/LIFECYCLE.md`: interlock states and daemon evaluation order.
- `docs/SURFACE.md`: the public Rust SDK.
- `docs/OPERATION.md`: installation, permissions, capacity, scheduling, measured performance.
- `docs/TESTING.md`: test layers and cadence.
- `docs/BACKLOG.md`: deferred work, including the wire v2 changes that boolean compositions and tier-aware attachment require.

## License

Apache-2.0. See `LICENSE`.