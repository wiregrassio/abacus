
# Conventions

Repository, code, and documentation conventions for this project. These exist so the
project reads as one coherent thing regardless of who wrote a given piece. They are
conventions, not the design law (the law is in PHILOSOPHY.md), but they are enforced
in review all the same.

## Repository layout

| Path | Contents |
|------|----------|
| `Cargo.toml` | Workspace root: members, shared package metadata, shared dependency versions, release profile (`panic = "abort"`). Source: root `CLAUDE.md`. |
| `Cargo.lock` | Committed, because the workspace ships a binary. Source: root `CLAUDE.md`. |
| `rustfmt.toml` | Formatter configuration, checked in CI. Source: root `CLAUDE.md`. |
| `crates/` | Five workspace crates. Source: `crates/CLAUDE.md`. |
| `docs/` | `OPERATION.md`, `TESTING.md`, `BACKLOG.md`. Source: `docs/CLAUDE.md`. |
| `docs/` | `ARCHITECTURE.md` (rationale), `CONTRACTS.md` (frozen boundaries), `LIFECYCLE.md` (mechanism), `SURFACE.md` (consumer API). Source: `docs/CLAUDE.md`. |
| `deploy/` | `abacus-rts.service`, the systemd unit. Source: `deploy/CLAUDE.md`. |
| `.github/workflows/` | CI: fmt, clippy, build, test. Source: root `CLAUDE.md`. |

Crate organization, per `crates/CLAUDE.md`:

| Crate | Role | Depends on |
|-------|------|------------|
| `abacus-core` | Shared-memory interlock layout, clock, futex, errors | `libc` |
| `abacus-wire` | Protocol v1 framing, codec, SCM_RIGHTS transfer | `abacus-core`, `libc` |
| `abacus-daemon` | `abacus` binary plus library (`daemon`, `registry`, `transport`) | `abacus-core`, `abacus-wire`, `libc` |
| `abacus-client` | Typed SDK handles and compositions | `abacus-core`, `abacus-wire`, `libc` |
| `abacus-tests` | Shared test kit plus integration suites and probes | all four, `libc` |

The crate split is an ABI boundary, not an organizational one: shared-memory word
layout, wire framing, descriptor transfer, sentinel values, and futex behavior must
stay synchronized across the independently compiled crates (root `CLAUDE.md`).

Test locations:

- L0 unit tests live beside the code: inline `#[cfg(test)] mod tests` (`crates/abacus-core/src/clock.rs`, `crates/abacus-wire/src/codec.rs`) or a `src/tests/` module tree (`crates/abacus-core/src/tests/mod.rs`, `crates/abacus-daemon/src/tests/mod.rs`).
- L2 process tests live in `crates/abacus-daemon/tests/`, with shared helpers in `crates/abacus-daemon/tests/common/mod.rs`.
- L1 through L4 integration tests live in `crates/abacus-tests/tests/`, one file per area.
- Shared test infrastructure lives in `crates/abacus-tests/src/lib.rs`.
- Operational probes live in `crates/abacus-tests/examples/probe.rs`.

## Language and tooling

- Rust 1.85 or newer, 2021 edition, with Cargo, rustfmt, and Clippy (root `CLAUDE.md`).
- One runtime dependency: `libc` (root `CLAUDE.md`).
- CI gates, all four required (root `CLAUDE.md`):

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo build --workspace --release
cargo test --workspace
```

- Linux only. The guard is compile-time, in `crates/abacus-core/src/lib.rs`:

```rust
#[cfg(not(target_os = "linux"))]
compile_error!("abacus-rts is Linux-only: memfd, futex, SCM_RIGHTS");
```

- Little-endian is also a compile-time assertion, in `crates/abacus-core/src/clock.rs`:
  `const _: () = assert!(cfg!(target_endian = "little"), "futex_addr assumes little-endian");`
- Layout invariants are compile-time assertions too: `crates/abacus-core/src/interlock.rs` asserts `size_of::<Interlock>() == INTERLOCK_SIZE`; `crates/abacus-client/src/types.rs` asserts `DEFAULT_TOUCH_TTL_MS == MIN_TOUCH_TTL_MS`.
- Public crates carry `#![warn(missing_docs)]` at the crate root (`crates/abacus-core/src/lib.rs`, `crates/abacus-wire/src/lib.rs`, `crates/abacus-daemon/src/lib.rs`, `crates/abacus-client/src/lib.rs`).

## Naming

Crates: `abacus-` prefix, one word of role after it (`abacus-core`, `abacus-wire`,
`abacus-daemon`, `abacus-client`, `abacus-tests`). The daemon package builds a binary
target named `abacus` (`crates/abacus-daemon/CLAUDE.md`).

Functions and types:

- Core shared-memory operations use a `interlock_` verb prefix: `interlock_create`, `interlock_map`, `interlock_arm`, `interlock_reap`, `interlock_free`, `interlock_is_terminated`, `interlock_dup_fd`, `interlock_read_expiration` (`crates/abacus-core/src/interlock.rs`).
- Tier-specific mapping functions extend that prefix: `interlock_map_clock`, `interlock_map_counter`, `interlock_map_timer`, `interlock_map_cron`, `interlock_map_barrier` (`crates/abacus-core/src/interlock.rs`).
- Futex helpers use a `futex_` prefix: `futex_wait`, `futex_wake`, `futex_word`, `futex_addr` (`crates/abacus-core/src/clock.rs`).
- Codec functions pair as `encode_`/`decode_`: `encode_request`, `decode_request`, `encode_response`, `decode_response` (`crates/abacus-wire/src/codec.rs`).
- SDK handle types are the tier name in `CamelCase`: `Interlock`, `AttachedInterlock`, `AttachedWaitCounter`, `ClockHandle`, `WaitCounter`, `WaitTimer`, `WaitCron`, `WaitBarrier`, `WaitRace`, `ProcessClock` (`crates/abacus-client/src/lib.rs`).
- Bounded variants take a `_for` suffix: `wait_open_for`, `wait_close_for` (`crates/abacus-client/src/interlock.rs`).
- Errors are enums named for their layer: `Condition`, `ProtocolFault`, `TransportError`, `StartupError` (`crates/abacus-core/src/error.rs`), `SdkError` (`crates/abacus-client/src/client.rs`).

Test naming: `area__claim`. The double underscore separates the area from the claim the
test makes, and the claim is a sentence fragment stating the expected behavior:
`clock__futex_wait_returns_etimedout` (`crates/abacus-core/src/tests/clock.rs`),
`registry__cron_first_target_is_next_grid_line` (`crates/abacus-daemon/src/tests/registry.rs`),
`daemon__sigterm_exits_zero_and_removes_socket` (`crates/abacus-daemon/tests/daemon_process.rs`),
`wait_counter__timeout_returns_timeout_state_not_error` (`crates/abacus-tests/tests/wait_counter.rs`).
Every test file that uses this pattern opens with `#![allow(non_snake_case)]`
(`crates/abacus-core/src/tests/mod.rs`, `crates/abacus-tests/tests/interlock.rs`,
`crates/abacus-daemon/tests/common/mod.rs`).

Constants:

- UPPER_SNAKE for compile-time constants: `INTERLOCK_SIZE`, `CREATION_TTL_NANOS`, `SENTINEL` (`crates/abacus-core/src/interlock.rs`); `MAX_MESSAGE_SIZE`, `PROTOCOL_VERSION`, `LENGTH_PREFIX_BYTES`, `ERR_INVALID_REQUEST` (`crates/abacus-wire/src/codec.rs`); `CLOCK_NAME`, `CLOCK_ID`, `MAX_NAME_LEN`, `DEFAULT_MAX_INTERLOCKS` (`crates/abacus-daemon/src/registry.rs`); `DEFAULT_TOUCH_INTERVAL_MS`, `MIN_TOUCH_TTL_MS`, `MIN_FATAL_MARGIN_MS`, `DEFAULT_TRANSPORT_TIMEOUT` (`crates/abacus-client/src/types.rs`).
- snake_case functions for runtime defaults that derive from another value: `default_touch_ttl_ms(interval_ms)` (`crates/abacus-client/src/types.rs`), `daemon_core()`, `nproc_online()` (`crates/abacus-tests/src/lib.rs`).

Files:

- Module files are snake_case, named for the type or concern they hold: `wait_counter.rs`, `wait_timer.rs`, `process_clock.rs`, `handle_ops.rs` (`crates/abacus-client/src/`).
- Integration test files are named for the area: `wait_barrier.rs`, `permissions.rs`, `abuse_wire.rs`, `timing_loop.rs`, `timeout_policy.rs`, `stop_flag.rs`, `sentinel_increments.rs`, `is_reaped.rs`, `bounded_waits.rs`, `wire_crate.rs` (`crates/abacus-tests/tests/`).
- Abuse and timing suites carry a category prefix: `abuse_connections.rs`, `abuse_memory.rs`, `abuse_wire.rs`, `timing_load.rs`, `timing_loop.rs`, `soak_hour.rs`.

## Code style

- No em dashes and no en dashes anywhere: prose, docs, comments, or commit messages (root `CLAUDE.md`).
- Shortest-complete register (root `CLAUDE.md`). No filler, no hedging. Doc comments state the contract, not the feeling about it.
- Comment policy: comments explain a decision the code cannot state. What gets one:
  - Why a nonobvious constant exists (`crates/abacus-daemon/src/main.rs`, `install_stop_handlers` explains the `sa_flags = 0` choice).
  - Why a code path is deliberate rather than accidental (`crates/abacus-core/src/interlock.rs`: freeing writes only the expiration word).
  - Hazard notes on untyped cross-crate agreement, per the CLAUDE.md hazard sections.
  What does not get one: restating the function name, narrating obvious control flow, or explaining a type the signature already gives.
- Error handling:
  - `Result` propagation through typed error enums per layer. `abacus-core` returns `Result<T, Condition>` (`crates/abacus-core/src/error.rs`), `abacus-wire` returns `Result<T, ProtocolFault>` or `Result<T, TransportError>` (`crates/abacus-wire/src/codec.rs`, `crates/abacus-wire/src/framing.rs`), the SDK returns `Result<T, SdkError>` (`crates/abacus-client/src/client.rs`).
  - Cross-layer conversion is `From`, not ad hoc mapping: `impl From<Condition> for SdkError`, `impl From<TransportError> for SdkError` (`crates/abacus-client/src/client.rs`); `impl From<TransportError> for StartupError`, `impl From<Condition> for StartupError` (`crates/abacus-core/src/error.rs`).
  - Every error variant implements `Display` and `std::error::Error`, and every variant is covered by a display test (`crates/abacus-core/src/tests/error.rs::error__every_variant_displays`, `crates/abacus-client/src/tests.rs::types__sdk_error_every_variant_displays`).
  - `expect` messages name the operation, not the failure: `.expect("spawn keepalive thread")` (`crates/abacus-client/src/touch.rs`), `.expect("fresh clock cannot be reaped")` (`crates/abacus-daemon/src/registry.rs`), `.expect("interlock_create")` (`crates/abacus-core/src/tests/interlock.rs`).
  - Panic messages in test code carry the observed state, not just the claim: `panic!("interlock still {:?} {} ms after its owner was SIGKILLed: open={open} closed={closed}", ...)` (`crates/abacus-daemon/tests/daemon_clients.rs`).
  - Unrecoverable invariant violations abort with a diagnostic line first (`crates/abacus-core/src/clock.rs::monotonic_now_nanos`, `crates/abacus-client/src/wait_timer.rs::on_timeout`).
  - `panic = "abort"` in the release profile is deliberate: a panic terminates the daemon rather than unwinding through a half-evaluated registry (root `CLAUDE.md`).
- `unsafe` blocks: every one is a direct syscall or raw-pointer operation, kept minimal and scoped to that call. Safety reasoning is stated where the invariant is not local: `unsafe impl Send for InterlockRegion` and `unsafe impl Sync for InterlockRegion` (`crates/abacus-core/src/interlock.rs`) rest on the atomic word layout; `futex_addr` carries its little-endian assertion adjacent to it (`crates/abacus-core/src/clock.rs`); `cmsg_space_for_fds` is verified against `libc::CMSG_SPACE` by test (`crates/abacus-wire/src/fdpass.rs::cmsg_space_matches_libc`).

## Test conventions

Five layers (`docs/TESTING.md`, and the CLAUDE.md files that describe each directory):

| Layer | What it is | Where |
|-------|-----------|-------|
| L0 | Unit tests, no daemon loop | `crates/*/src/tests/`, inline `mod tests` |
| L1 | In-process integration against `ThreadDaemon` | `crates/abacus-tests/tests/` (`interlock.rs`, `wait_*.rs`, `attached.rs`, `client.rs`) |
| L2 | Process-level, real `abacus` binary as a child | `crates/abacus-daemon/tests/` |
| L3 | Hardware timing and load benchmarks | `crates/abacus-tests/tests/timing_loop.rs`, `timing_load.rs` |
| L4 | Abuse and soak | `crates/abacus-tests/tests/abuse_*.rs`, `soak_hour.rs` |

One claim per test, and the name is the claim. `interlock__arm_never_decrements`
(`crates/abacus-core/src/tests/interlock.rs`) asserts exactly that;
`wait_counter__cas_max_keeps_largest_target_under_contention`
(`crates/abacus-tests/tests/wait_counter.rs`) asserts exactly that.

Synchronization: `wait_for` and `wait_for_value` are the primitives, not fixed sleeps
(`crates/abacus-tests/src/lib.rs`). They take a deadline and a poll interval and return
the elapsed duration, so a failure message can report how long the condition took:

```rust
wait_for(Duration::from_millis(300), Duration::from_millis(1), || {
    attached.state() == InterlockState::Expired
})
```

Related helpers: `wait_for_daemon`, `on_thread`, `recv_within`, `wait_child`, `run_child`
(`crates/abacus-tests/src/lib.rs`). Short `thread::sleep` calls appear only to let a
spawned waiter reach its futex before a stimulus is applied, never as the assertion's
timing source.

Measurement tests take `serialized()`, a cross-binary file lock over
`/tmp/abacus-test-serial.lock` plus a process-local mutex
(`crates/abacus-tests/src/lib.rs::serialized`). Every abuse, timing, and soak test
begins `let _serial = serialized();`.

Child process entry points are `role__` functions, marked `#[test] #[ignore]` and
guarded by `role_args`:

```rust
#[test]
#[ignore = "role: process entry point for daemon__two_processes_share_one_interlock"]
fn role__create_and_advance() {
    let Some(args) = role_args("role__create_and_advance") else { return; };
    ...
}
```

Parents dispatch with `role_command(name, args)`, which re-executes the current test
binary with `--exact --ignored --nocapture --test-threads=1` and passes arguments
through `ABACUS_TEST_ROLE` and `ABACUS_TEST_ROLE_ARGS`, U+001F separated
(`crates/abacus-tests/src/lib.rs`). Roles exist where an SDK abort, a hung request, or a
`SIGBUS` from a hostile mapping must not kill the parent test binary
(`crates/abacus-tests/CLAUDE.md`).

Ignore reasons carry a category prefix and a justification:

| Prefix | Meaning | Example |
|--------|---------|---------|
| `timing:` | Hardware-dependent benchmark | `#[ignore = "timing: run with --ignored on hardware"]` (`timing_loop.rs`) |
| `abuse:` | Hostile-environment suite | `#[ignore = "abuse: run with --ignored"]` (`abuse_wire.rs`) |
| `soak:` | Long-running | `#[ignore = "soak: run with --ignored; ABACUS_SOAK=1 for the full hour, else 60 s"]` (`soak_hour.rs`) |
| `role:` | Child process entry point | `#[ignore = "role: process entry point for ..."]` (`daemon_process.rs`) |

Thresholds that differ between profiles use `cfg!(debug_assertions)` at the constant, not
inside the assertion:

```rust
const EVALUATE_1000_MEDIAN_CEILING_US: u128 = if cfg!(debug_assertions) { 200 } else { 50 };
```

(`crates/abacus-daemon/src/tests/registry.rs`; same pattern in `timing_loop.rs`,
`timing_load.rs`, and `daemon_process.rs`.)

Process-level tests run the real binary. The path comes from `CARGO_BIN_EXE_abacus`
(`crates/abacus-daemon/tests/common/mod.rs::BIN`) or from
`abacus_binary()` (`crates/abacus-tests/src/lib.rs`), which falls back to a sibling
`abacus` in the target directory and panics with build instructions when absent. Each
daemon gets a unique socket under `/tmp` via `unique_socket_path(label)`, and labels are
capped at 40 bytes. `cargo test` never touches a production socket (root `CLAUDE.md`).

Randomized tests take their seed from `ABACUS_TEST_SEED` and print it on failure, so a
failure is replayable (`crates/abacus-tests/src/lib.rs::Rng::from_env`,
`crates/abacus-core/src/tests/mod.rs::Xorshift::from_env`).

## Documentation conventions

- `CLAUDE.md` maps every directory. Each one carries the same sections: Purpose, Dependencies, Consumed By, Data Flow, Known Hazards, Files, Subdirectories, Contracts, Notes, Reference. Hazards are severity-tagged (CRITICAL, HIGH, MEDIUM, LOW).
- Design docs cite `file::symbol`, not line numbers, so a citation survives an edit.
- Tables over prose for contracts: the tier table in the root `CLAUDE.md`, the file and subdirectory tables in every `CLAUDE.md`, the permission-model rows exercised by `crates/abacus-tests/tests/permissions.rs`.
- Design docs describe what exists. Deferred and planned work lives in `docs/BACKLOG.md` with its integration trigger, not in `docs/` (`docs/CLAUDE.md`).
- The four design documents separate concerns and must not blur: `ARCHITECTURE.md` is rationale, `CONTRACTS.md` is frozen boundaries, `LIFECYCLE.md` is internal mechanism, `SURFACE.md` is the consumer API (`docs/CLAUDE.md`).
- Operational evidence and measured numbers live in `docs/OPERATION.md`; the test procedure lives in `docs/TESTING.md`. Measured behavior does not silently redefine the ABI (`docs/CLAUDE.md`).
- `README.md` is the human introduction; the root `CLAUDE.md` is the technical hub.

## Git conventions

- Commit messages use the same shortest-complete register as the code, with no em or en dashes (root `CLAUDE.md`).
- Committed: `Cargo.lock` (the workspace ships a binary), source, docs, `deploy/`, `rustfmt.toml`, `.github/workflows/`.
- Not committed: `target/`, excluded by `.gitignore`.

## Deployment conventions

- The systemd unit is `deploy/abacus-rts.service`. It is the only file in `deploy/`.
- The unit orders after `local-fs.target`, creates its runtime directory, and launches `/usr/local/bin/abacus --socket-path=/run/abacus-rts/abacus.sock` (`deploy/CLAUDE.md`).
- Socket path: `/run/abacus-rts/abacus.sock`. The runtime directory is mode `0755`; the socket file itself is created by the daemon at mode `0660` by default (`crates/abacus-daemon/src/transport.rs::SocketOptions::default`, `crates/abacus-daemon/src/daemon.rs::DaemonConfig::new`). Confidentiality depends on the daemon-set socket permissions, not on the runtime-directory mode (`deploy/CLAUDE.md`).
- The daemon accepts `--socket-path`, `--socket-mode` (octal), `--socket-group`, `--max-interlocks`, plus `--help` and `--version`. Flags take either `--flag value` or `--flag=value` (`crates/abacus-daemon/src/main.rs::usage`).
- Restart policy: `Restart=always` with a 100 ms delay; stop is `SIGTERM` with `TimeoutStopSec=2` (`deploy/CLAUDE.md`).
- The file-descriptor limit assumes roughly one descriptor per interlock and leaves headroom above the documented default capacity of 4096 (`deploy/CLAUDE.md`, `crates/abacus-daemon/src/registry.rs::DEFAULT_MAX_INTERLOCKS`).
- Real-time scheduling and CPU affinity directives are present but disabled until operational probes justify deployment-specific pinning (`deploy/CLAUDE.md`). Pinning without kernel-level CPU isolation does not give timing isolation (`docs/CLAUDE.md`). Test-side CPU selection reads `/sys/devices/system/cpu/isolated` and falls back to the last online core (`crates/abacus-tests/src/lib.rs::daemon_core`).
- The daemon sets `PR_SET_TIMERSLACK` to 1 ns at startup and logs a diagnostic if the call fails (`crates/abacus-daemon/src/daemon.rs::set_timer_slack`).
- Probe scripts live in `crates/abacus-tests/examples/probe.rs`. One binary, subcommand first, socket path second, then optional numeric arguments: `probe bench|cron|barrier|partial <sock> [n] [ms]`. Each subcommand prints a single summary line of percentiles and counts to stdout.