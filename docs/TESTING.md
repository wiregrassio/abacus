# Testing

The suite is the yardstick. Run it once a quarter, read red or green, act only on red.
Everything is a Rust test reachable from `cargo test`; no scripts, no external harness.

## Quarterly run

Linux only (memfd, futex, UDS). Run on an isolated-core Linux host in a dedicated
working directory.

1. Sync and build release:

   ```bash
   rsync -a --delete --exclude target --exclude .git --exclude Cargo.lock \
     ./ <jetson-host>:~/abacus-rts-tests/
   ssh <jetson-host> 'export PATH=$HOME/.cargo/bin:$PATH; cd ~/abacus-rts-tests && \
     cargo build --workspace --release'
   ```

2. Default suite, L0 to L2. Must be green. Finishes in under 30 s. `--no-fail-fast` makes
   cargo run every test binary instead of stopping at the first red crate, so one run lists
   every red test.

   ```bash
   cargo test --workspace --no-fail-fast
   ```

3. Timing, abuse, and soak, L3 and L4. Must be green. Runs on an otherwise idle machine
   (timing thresholds assume it). Add `ABACUS_SOAK=1` for the full one-hour soak; without it
   the soak test runs a 60 s abbreviated pass with the same assertions.

   ```bash
   cargo test --workspace --release --no-fail-fast -- --ignored
   ```

4. `cargo audit` if installed. The only dependency is `libc`; note its version from
   `Cargo.lock`.

5. Anything red: the test's name is the claim that broke, and its doc comment cites the
   CONTRACTS.md, LIFECYCLE.md, or SURFACE.md section it guards.

## Layers

| Layer | Home | Default run | Budget |
|-------|------|-------------|--------|
| L0 unit | `#[cfg(test)]` modules inside each crate (`src/tests/`) | yes | under 2 s |
| L1 in-process integration | `crates/abacus-tests/tests/*.rs`, daemon as a thread | yes | under 15 s |
| L2 process-level | `crates/abacus-daemon/tests/daemon_*.rs`, daemon as a child process | yes | under 15 s |
| L3 timing and load | `crates/abacus-tests/tests/timing_*.rs`, `#[ignore = "timing: ..."]` | no | under 3 min |
| L4 abuse and soak | `crates/abacus-tests/tests/abuse_*.rs`, `soak_*.rs`, `#[ignore = "abuse: ..."]`, `"soak: ..."` | no | under 5 min |

Run one layer or one file: `cargo test -p abacus-tests --test wait_timer`,
`cargo test -p abacus-daemon --test daemon_process`, `cargo test -p abacus-core --lib`.
Add `-- --ignored` for L3 and L4 files. Either thread count must pass:
`-- --test-threads=1` and `-- --test-threads=12`.

## Reading a failure

- The libtest summary names the test. Its doc comment names the contract or finding.
- Assertion messages print the observed state: counters, expiration, elapsed, and for timing
  tests the percentile table (`n`, `p50`, `p90`, `p99`, `max`).
- Randomized tests print their seed. Replay with `ABACUS_TEST_SEED=<seed>`.
- Tests named `role__*` are not tests. They are process entry points: the test binary
  re-executed as a child by the test named in their ignore reason. They report `ok` and do
  nothing when run directly.

## Adding a test

- Name it `area__claim`. One claim per test. Doc comment cites the section or finding.
- Use the kit in `crates/abacus-tests/src/lib.rs`: `ThreadDaemon` (L1), `ProcessDaemon`
  (L2 to L4), `RawClient` for anything hostile or below the SDK, `wait_for` for every wait,
  `Stats` for anything measured, `Rng` for anything randomized.
- Never a fixed sleep as synchronization. Never a shared daemon between tests. Never an
  `unwrap()` on the result the test is about.
- A test that measures time, CPU, or load holds `abacus_tests::serialized()` for its whole
  body. Tests in one binary run on parallel threads; two load generators at once make
  every number meaningless, and the guard removes the need for a thread-count flag.
- Load tests come in three shapes: unpinned 2x oversubscription (the worst case),
  the production profile (12 cores, 6 pegged, daemon pinned to one core), and a stress
  profile (4x on every non-daemon core). `ProcessDaemon::start_pinned`, `Load::cpu_on`,
  and `pin_current_thread` build them. The pinned profiles assert an isolated core; on a
  box where other processes can still run on the daemon's core they stay red and say so.
- Every ignored test carries a reason starting with `timing:`, `abuse:`, `soak:`, or `role:`.
- A client that can shrink a memfd must run in a role child process, along
  with any later check that maps the same daemon's memory. A shrunk memfd SIGBUSes every
  mapper.
