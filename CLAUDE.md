
# Abacus

Lockless atomic coordination for real-time compute: one primitive (the interlock), a crash-only daemon that evaluates five wait tiers on a 1 ms cadence, and a Rust SDK of typed handles and compositions, Linux-only.

## Contracts

1. **Linux only.** memfd with seals, futex, Unix-domain sockets with SCM_RIGHTS, signals, `/proc`. A non-Linux build fails at a compile-time guard: `crates/abacus-core/src/lib.rs::compile_error!`.
2. **The interlock is exactly three u64 words, 24 bytes, at offsets 0, 8, 16.** Enforced by `crates/abacus-core/src/interlock.rs::Interlock` plus the `const _: () = assert!(size_of::<Interlock>() == INTERLOCK_SIZE)` static assertion; offsets verified in `crates/abacus-core/src/tests/interlock.rs::interlock__size_is_24_and_offsets_are_0_8_16`.
3. **`SENTINEL` (`u64::MAX`) is globally reserved and means terminated.** Any of the three words holding it terminates the interlock: `crates/abacus-core/src/interlock.rs::interlock_is_terminated`. Increments must not create or cross it: `crates/abacus-client/src/handle_ops.rs::increment`.
4. **Expiration is monotonic forward only.** A TTL write takes `max(current, now + ttl)` and refuses a terminated interlock: `crates/abacus-core/src/interlock.rs::interlock_arm`.
5. **The daemon reaps only on expired TTL or a SENTINEL word; overrun is informational.** Enforced in `crates/abacus-daemon/src/registry.rs::Registry::evaluate_all`.
6. **All interlocks are named; attach is by name only; the name `clock` is reserved for the daemon-owned clock at registry id 0.** Enforced in `crates/abacus-daemon/src/registry.rs::Registry::create` (reserved-name rejection) and `::Registry::attach` (clock shortcut); constants `CLOCK_NAME`, `CLOCK_ID`. The SDK also refuses `attach_interlock("clock")` in `crates/abacus-client/src/client.rs::AbacusClient::attach_interlock`.
7. **A watcher dies with its target and never follows a recreated name.** Targets carry a slot plus generation id; a mismatch resolves to `None` and reaps the watcher: `crates/abacus-daemon/src/registry.rs::Registry::watched_value` and the `Target::Slot { slot, id }` check.
8. **Recreating a live name reaps the previous entry and issues a strictly greater id.** Enforced in `crates/abacus-daemon/src/registry.rs::Registry::create` (`remove_slot` on collision, `next_id` increment).
9. **Wire protocol v1: a four-byte little-endian length prefix, a version byte, a tag byte, payload capped at 4096 bytes.** Enforced in `crates/abacus-wire/src/codec.rs::frame`, `::check_version`, and `crates/abacus-wire/src/framing.rs::FrameReader::next_frame`.
10. **Descriptors travel as SCM_RIGHTS ancillary data on the length prefix, at most one per response.** Enforced in `crates/abacus-wire/src/fdpass.rs::send_frame_with_fds` and `::recv_prefix_with_fds` (MSG_CTRUNC rejection); the expected count is declared by `crates/abacus-wire/src/codec.rs::expected_fd_count` and checked client-side in `crates/abacus-client/src/client.rs::ClientConn::recv_response`.
11. **One codec serves both endpoints: `abacus-wire`.** Neither the daemon nor the SDK may own the wire format. Asserted by `crates/abacus-tests/tests/wire_crate.rs::wire__client_manifest_has_no_daemon_dependency`.
12. **Decoders never panic on hostile input; truncation reports `needed > have`.** Enforced by the bounds-checked `crates/abacus-wire/src/codec.rs::Cursor` and covered by `crates/abacus-wire/src/codec.rs::tests::truncation_at_every_offset_reports_exact_needed`, `crates/abacus-tests/tests/wire_crate.rs::wire__random_bytes_never_panic`.
13. **A protocol fault closes the connection after one diagnostic line.** Enforced in `crates/abacus-daemon/src/daemon.rs::service_client` (`ClientOutcome::Drop` on `TransportError::Protocol`).
14. **The daemon is crash-only: a panic aborts, a restart comes back empty and wakes no one.** `panic = "abort"` in the release profile (`Cargo.toml`); restart semantics covered by `crates/abacus-tests/tests/timeout_policy.rs::daemon__restart_wakes_nobody_and_timers_time_out`.
15. **The evaluation loop advances on 1 ms boundaries anchored at startup, never accumulating drift.** Enforced in `crates/abacus-daemon/src/daemon.rs::daemon_run_with` (`next_due` recomputed from `anchor`).
16. **Clients keep their interlocks alive with one keepalive thread per client, not one per handle.** Enforced in `crates/abacus-client/src/touch.rs::Keepalive::register_inner` (single shared worker); asserted by `crates/abacus-tests/tests/regressions.rs::one_keepalive_thread_per_client`.
17. **A bare interlock handle drop does not free; only `free()` terminates.** Enforced by `crates/abacus-client/src/interlock.rs::Interlock::free` versus the absence of a freeing `Drop`; covered by `crates/abacus-tests/tests/interlock.rs::interlock__drop_does_not_free_but_lapses`.
18. **The clock memfd is handed out read-only and sealed against future writes.** Enforced in `crates/abacus-core/src/interlock.rs::interlock_create_clock` (`F_SEAL_FUTURE_WRITE`) and `::interlock_map_clock` (`PROT_READ`).
19. **Every interlock memfd is sealed against shrink and grow, so an attacher cannot SIGBUS its peers.** Enforced in `crates/abacus-core/src/interlock.rs::seal`.
20. **A timer that misses its fatal delivery margin is fatal by default.** `TimeoutPolicy::Abort` is the default (`crates/abacus-client/src/types.rs::TimeoutPolicy`) and is applied in `crates/abacus-client/src/wait_timer.rs::WaitTimer::on_timeout`; the margin floor is `MIN_FATAL_MARGIN_MS`.
21. **Futex waits compare only the low 32 bits of a 64-bit word and assume little-endian layout.** Enforced by `crates/abacus-core/src/clock.rs::futex_word` and the `const _: () = assert!(cfg!(target_endian = "little"))` guard beside `::futex_addr`.
22. **The registry enforces a name-length cap and an interlock cap; the clock is excluded from both.** Enforced in `crates/abacus-daemon/src/registry.rs::validate_name` (`MAX_NAME_LEN`) and `::Registry::create` (`max_interlocks`).
23. **Formatting, lint, build, and test all gate a revision; Clippy warnings are denied.** Configured in `rustfmt.toml` and `.github/workflows/`.
24. **Prose in this repo uses no em or en dashes.** Applies to all Markdown and doc comments.

## Files

| File | Purpose |
|------|---------|
| `Cargo.toml` | Workspace root: the five members, shared package metadata, the single shared dependency version (`libc`), and the release profile with `panic = "abort"`. |
| `Cargo.lock` | Locked dependency graph (`libc` only). Committed because the workspace ships a binary. |
| `README.md` | Human-facing introduction: what Abacus is and the problem it solves. |
| `CONTRIBUTING.md` | Contribution rules: register, formatting gates, test layers, and what a change must not break. |
| `LICENSE` | Apache License 2.0. |
| `rustfmt.toml` | Formatter configuration; `cargo fmt --all -- --check` is a CI gate. |
| `.gitignore` | Excludes Cargo build output and local scratch paths. |
| `CLAUDE.md` | This document: the agent entry point and technical map. |

## Subdirectories

| Directory | Purpose |
|-----------|---------|
| `crates/` | The five workspace crates: `abacus-core` (24-byte interlock, memfd, futex, clock, shared errors), `abacus-wire` (v1 codec, framing, SCM_RIGHTS descriptor passing), `abacus-daemon` (registry, transport, 1 ms loop, the `abacus` binary), `abacus-client` (typed handles, wait tiers, keepalive, compositions), `abacus-tests` (shared test kit plus the L1 through L4 integration suites and the `probe` example). |
| `docs/` | Design philosophy (PHILOSOPHY), mechanism and rationale (DESIGN), frozen interface surface (INTERFACE), conventions, operation, and backlog. |
| `deploy/` | The systemd unit `abacus.service`: runtime directory, socket path argument, restart policy, descriptor limit, and the real-time scheduling knobs (SCHED_FIFO priority 50 on core 4). |
| `.github/workflows/` | CI on Linux: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo build --workspace`, `cargo test --workspace`. |