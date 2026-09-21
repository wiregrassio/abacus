
# Contributing

Abacus RTS is a coordination plane for real-time compute: one shared-memory primitive (the interlock), a crash-only daemon that evaluates five tiers on a 1 ms cadence, and a typed Rust SDK over both. Before you write code, read `PHILOSOPHY.md` (the design law) and `CONVENTIONS.md` (repository, code, and documentation conventions). This document tells you how to build, test, and submit.

## Platform

Linux only. The guard is compile-time, in `crates/abacus-core/src/lib.rs`:

```rust
#[cfg(not(target_os = "linux"))]
compile_error!("abacus-rts is Linux-only: memfd, futex, SCM_RIGHTS");
```

A macOS build stops there. This is not an oversight to fix: the implementation rests on memfd with seals, futex, Unix-domain sockets with `SCM_RIGHTS`, signals, and `/proc`. Little-endian is also asserted at compile time, next to the `futex_addr` that depends on it (`crates/abacus-core/src/clock.rs`).

Toolchain: Rust 1.85 or newer, 2021 edition. One runtime dependency, `libc`.

## Build and test

These three commands are the CI gate. All three must pass.

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

The daemon binary lands at `target/release/abacus`. `cargo test` starts daemons on unique sockets under `/tmp` and stops them. One test (`daemon__default_socket_path_and_flag_forms`) validates the daemon's default socket path argument parsing and binds to `/run/abacus-rts/abacus.sock` if that path is writable.

Ignored suites, run deliberately:

```bash
cargo test --workspace -- --ignored                    # everything ignored, including roles
cargo test -p abacus-tests --test timing_loop -- --ignored --nocapture
cargo test -p abacus-tests --test timing_load -- --ignored --nocapture
cargo test -p abacus-tests --test abuse_wire -- --ignored --nocapture
cargo test -p abacus-tests --test soak_hour -- --ignored --nocapture
ABACUS_SOAK=1 cargo test -p abacus-tests --test soak_hour -- --ignored --nocapture
```

Process-level tests need the binary. `CARGO_BIN_EXE_abacus` supplies it inside `crates/abacus-daemon/tests/`; elsewhere `abacus_binary()` falls back to a sibling `abacus` in the target directory and panics with build instructions if it is absent. Build the same profile as the tests you run.

Randomized tests take `ABACUS_TEST_SEED` and print the seed on failure, so a failure is replayable:

```bash
ABACUS_TEST_SEED=12345 cargo test -p abacus-tests --test wire_crate
```

Operational probes:

```bash
cargo run -p abacus-tests --example probe -- bench /tmp/abacus-probe.sock 1000 5
cargo run -p abacus-tests --example probe -- cron /tmp/abacus-probe.sock 10 200
```

## Repository layout

| Path | Contents |
|------|----------|
| `crates/abacus-core` | Interlock layout, clock, futex, shared errors |
| `crates/abacus-wire` | Protocol v1 framing, codec, `SCM_RIGHTS` transfer |
| `crates/abacus-daemon` | The `abacus` binary plus `daemon`, `registry`, `transport` |
| `crates/abacus-client` | Typed SDK handles and compositions |
| `crates/abacus-tests` | Shared test kit, integration suites, probes |
| `docs/` | `ARCHITECTURE.md`, `CONTRACTS.md`, `LIFECYCLE.md`, `SURFACE.md` |
| `docs/` | `OPERATION.md`, `TESTING.md`, `BACKLOG.md` |
| `deploy/` | `abacus-rts.service` |

The crate split is an ABI boundary, not a filing convention. Shared-memory word layout, wire framing, descriptor cardinality, sentinel meaning, and futex behavior must stay synchronized across four independently compiled crates. `abacus-client` must not depend on `abacus-daemon`; both depend on `abacus-wire`, and `wire_crate.rs` asserts that by reading the manifests.

## The five test layers

| Layer | What it is | Where |
|-------|-----------|-------|
| L0 | Unit tests, no daemon loop: layout, arithmetic, codec | `crates/*/src/tests/`, inline `#[cfg(test)] mod tests` |
| L1 | In-process integration against `ThreadDaemon` | `crates/abacus-tests/tests/` (`interlock.rs`, `wait_*.rs`, `attached.rs`, `client.rs`) |
| L2 | Process-level: the real `abacus` binary as a child | `crates/abacus-daemon/tests/` |
| L3 | Hardware timing and load benchmarks | `crates/abacus-tests/tests/timing_loop.rs`, `timing_load.rs` |
| L4 | Abuse and soak | `crates/abacus-tests/tests/abuse_*.rs`, `soak_hour.rs` |

Layer choice follows from the claim. Arithmetic and layout are L0. Behavior of a handle against a running evaluation loop is L1. Anything crossing a process, a signal, or a page boundary is L2 or higher: signal handling, socket lifecycle, stale-socket replacement, hostile clients, memfd truncation. A guarantee not exercised against a real process is not a guarantee.

L3 and L4 are `#[ignore]` by default because they are expensive and host-sensitive. That means the default `cargo test` run does not cover real-process resilience, resource recovery, or timing. Run them before changes to the loop, the registry, the transport, or keepalive.

## Adding a test

1. Pick the layer from the claim, per the table above.

2. Name it `area__claim`. The double underscore separates the area from the claim, and the claim is a sentence fragment stating expected behavior: `clock__futex_wait_returns_etimedout`, `registry__cron_first_target_is_next_grid_line`, `daemon__sigterm_exits_zero_and_removes_socket`, `wait_counter__timeout_returns_timeout_state_not_error`. One claim per test.

3. Open the file with `#![allow(non_snake_case)]` if it uses that pattern.

4. Synchronize with `wait_for` or `wait_for_value` (`crates/abacus-tests/src/lib.rs`), never a fixed sleep as the assertion's timing source. They take a deadline and a poll interval and return the elapsed duration, so the failure message can report how long the condition took:

```rust
wait_for(Duration::from_millis(300), Duration::from_millis(1), || {
    attached.state() == InterlockState::Expired
})
```

   Related helpers: `wait_for_daemon`, `on_thread`, `recv_within`, `wait_child`, `run_child`. A short `thread::sleep` is acceptable only to let a spawned waiter reach its futex before a stimulus is applied.

5. Put the observed state in the panic message, not just the claim:

```rust
panic!("interlock still {:?} {} ms after its owner was SIGKILLed: open={open} closed={closed}", ...)
```

6. Isolate anything that can kill the harness. An SDK abort, a hung request, or a `SIGBUS` from a hostile mapping runs in a re-executed child role:

```rust
#[test]
#[ignore = "role: process entry point for daemon__two_processes_share_one_interlock"]
fn role__create_and_advance() {
    let Some(args) = role_args("role__create_and_advance") else { return; };
    // ...
}
```

   The parent dispatches with `role_command(name, args)`, which re-executes the current test binary with `--exact --ignored --nocapture --test-threads=1` and passes arguments through `ABACUS_TEST_ROLE` and `ABACUS_TEST_ROLE_ARGS`, U+001F separated. Roles are addressed by string, so renaming a role function means updating every caller.

7. Serialize measurement tests. Every abuse, timing, and soak test begins:

```rust
let _serial = serialized();
```

   `serialized()` is a cross-binary file lock over `/tmp/abacus-test-serial.lock` plus a process-local mutex.

8. Use a unique socket per daemon via `unique_socket_path(label)`. Labels are capped at 40 bytes.

9. Ignore reasons carry a category prefix and a justification: `timing:`, `abuse:`, `soak:`, `role:`. For example `#[ignore = "timing: run with --ignored on hardware"]`.

10. Thresholds that differ between profiles use `cfg!(debug_assertions)` at the constant, not inside the assertion:

```rust
const EVALUATE_1000_MEDIAN_CEILING_US: u128 = if cfg!(debug_assertions) { 200 } else { 50 };
```

11. Randomized tests read their seed with `Rng::from_env` (or `Xorshift::from_env` in core) and print it on failure.

## Hardware timing tests

L3 assertions are only meaningful on isolated CPU cores: a Jetson-class board or equivalent with kernel-level CPU isolation. Affinity alone is not isolation. Without `isolcpus` (or equivalent), scheduler stalls beyond 10 ms remain possible, and the 50 ms fatal-margin floor (`MIN_FATAL_MARGIN_MS`) is an operational defense against observed jitter, not a proof of real-time behavior.

The test side reads `/sys/devices/system/cpu/isolated` through `daemon_core()` and falls back to the last online core when nothing is isolated. `timing_load.rs` pins the daemon to that core and the load threads elsewhere. Do not report a latency number without its profile: a figure without conditions is a number, not evidence. Measured numbers with their conditions belong in `docs/OPERATION.md`; the run procedure belongs in `docs/TESTING.md`.

If a timing test fails on a shared or loaded host, say so in the report rather than raising the ceiling. Raising a threshold to make a host pass is a change to the claim, and it needs the same scrutiny as a change to the code.

## Design rules that gate review

These come from `PHILOSOPHY.md`. A change that contradicts one is rejected as a design bug, not debated as a tradeoff.

- **One primitive, five contracts.** There is exactly one shared object, three `AtomicU64` words in a sealed memfd. New wait types come from composition (as `WaitRace` does over `WaitCounter`), not from a sixth tier. If a feature needs the daemon to understand what the counters mean, it belongs in the application.
- **State is computed, never stored as a flag.** `interlock_state(open, closed, expiration_ns, clock_ns)` is total over four states with no default arm. No status field, no reference counting, no release call. Liveness is a `SENTINEL` in any of the three words.
- **Crash, do not limp.** The daemon binary is `panic = "abort"`. A protocol fault closes the connection after one stderr line. Bad flags exit 2 at startup. `WaitTimer` defaults to `TimeoutPolicy::Abort`; `TimeoutPolicy::Error` is opt-in so recovery is a decision someone made.
- **A restart is a cold start.** No journal, no snapshot, no reconciliation. Coming back empty is correct. Do not add persistence to make a restart resumable.
- **No wait is unbounded.** Every blocking call carries a ceiling in the code path. `handle_ops::wait_word` loops with `DEFAULT_TIMEOUT_NANOS` (100 ms) per iteration and re-checks sentinel and expiration on each wake, because `futex_word` truncates to the low 32 bits and a high-half-only change is invisible to the kernel. The daemon's `ppoll` timeout is the remainder to the next millisecond boundary, computed from a fixed anchor.
- **Authority is kernel-enforced.** Every memfd is sealed at creation with `F_SEAL_SHRINK | F_SEAL_GROW | F_SEAL_SEAL`; the clock adds `F_SEAL_FUTURE_WRITE` and maps `PROT_READ` only. The daemon does not gate `open` or `close` on caller identity. The trust boundary is the socket, stated rather than implied.
- **The crate split is an ABI.** Pin invariants with compile-time assertions where possible and with exhaustive tests where not: `size_of::<Interlock>() == INTERLOCK_SIZE`, offsets at 0, 8, 16, truncation at every offset returning `Truncated { needed > have }`, bit-flip and random-input fuzz that must never panic, descriptor counts checked against `expected_fd_count`.

Where the ABI is currently insufficient (wire v1's `Attached` response carries an ID but no tier), that gap is recorded in `docs/BACKLOG.md` with an integration trigger. Do not patch a named gap with a runtime heuristic.

## Code style

- No em dashes and no en dashes anywhere: prose, docs, comments, commit messages.
- Shortest-complete register. No filler, no hedging. Doc comments state the contract.
- Comments explain a decision the code cannot state: a nonobvious constant, a deliberate path, a hazard on untyped cross-crate agreement. They do not restate the function name or narrate control flow.
- Naming: `interlock_` prefix for core operations, `futex_` for futex helpers, `encode_`/`decode_` pairs in the codec, CamelCase tier names for SDK handles, `_for` suffix for bounded variants, UPPER_SNAKE for compile-time constants, snake_case functions for derived runtime defaults.
- Errors are typed per layer: `Condition` in core, `ProtocolFault` and `TransportError` in wire, `SdkError` in the client. Cross-layer conversion is `From`, not ad hoc mapping. Every variant implements `Display` and `std::error::Error`, and every variant is covered by a display test.
- `expect` messages name the operation, not the failure: `.expect("spawn keepalive thread")`.
- Unrecoverable invariant violations print a diagnostic line and then abort.
- Every `unsafe` block is a direct syscall or raw-pointer operation, kept minimal and scoped to that call, with safety reasoning stated where the invariant is not local.
- Public crates carry `#![warn(missing_docs)]` at the crate root.

## Documentation

- Every directory carries a `CLAUDE.md` with the same sections: Purpose, Dependencies, Consumed By, Data Flow, Known Hazards, Files, Subdirectories, Contracts, Notes, Reference. Hazards are severity-tagged (CRITICAL, HIGH, MEDIUM, LOW). If you add a directory, add its `CLAUDE.md`. If you add a hazard, tag it.
- Cite `file::symbol`, not line numbers.
- Tables over prose for contracts.
- The four design documents do not blur: `ARCHITECTURE.md` is rationale, `CONTRACTS.md` is frozen boundaries, `LIFECYCLE.md` is internal mechanism, `SURFACE.md` is the consumer API.
- Design docs describe what exists. Deferred and planned work goes to `docs/BACKLOG.md` with its integration trigger.
- Measured numbers go to `docs/OPERATION.md` with the conditions that produced them. Measured behavior does not silently redefine the ABI.

## Commits and pull requests

- Commit messages use the same shortest-complete register as the code, with no em or en dashes.
- Committed: `Cargo.lock` (the workspace ships a binary), source, docs, `deploy/`, `rustfmt.toml`, `.github/workflows/`.
- Not committed: `target/`.
- Before opening a pull request, run all three CI gates. If your change touches the daemon loop, the registry, the transport, keepalive, or the wire codec, also run the relevant `--ignored` suites and include the output in the description.
- State which law or contract the change serves, and which test now exercises it. A behavior change without a test that names the claim will be sent back.