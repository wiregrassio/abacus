# Abacus RTS design

The argument for why this system is shaped the way it is. CONTRACTS.md is the interface, LIFECYCLE.md the mechanism, SURFACE.md the SDK API; this file says why.

## The primitive

One primitive: the interlock. Three `AtomicU64` words in a `repr(C)` struct, `abacus-core/src/interlock.rs::Interlock`: `open_count` at offset 0, `closed_count` at offset 8, `expiration_ns` at offset 16. `INTERLOCK_SIZE` is 24 bytes, asserted at compile time against `size_of::<Interlock>()`.

The backing is a `memfd_create` file truncated to 24 bytes and mapped `MAP_SHARED` (`interlock_create`, `mmap_interlock`). The mapping is reference counted: `InterlockHandle` wraps an `Arc<InterlockRegion>`, and the last clone unmaps and closes the fd (`InterlockRegion::drop`). The fd is handed to clients by `interlock_dup_fd`, which `dup`s the memfd so both processes map the same page.

Invariants:

- Counters are monotonic and carry arbitrary increments. `handle_ops::increment` uses `fetch_add`.
- `expiration_ns` is CLOCK_MONOTONIC nanoseconds and never decreases: `interlock_arm` is a CAS-max loop that returns `Ok(())` when the requested deadline is already behind the current one.
- `SENTINEL` (`u64::MAX`) in any word means terminated. `interlock_is_terminated` reads all three.
- Termination cannot be undone. `handle_ops::increment` checks whether the pre-add value was `SENTINEL` or whether the add would cross it, restores `SENTINEL`, wakes waiters, and returns `SdkError::InterlockReaped`. `interlock_arm` refuses to write over a `SENTINEL` expiration.
- Values above 2^63 are outside the representable signed range; `handle_ops::value` computes `open - closed` with `wrapping_sub` and no defined tier reaches that magnitude.

Sealing: `interlock_create` applies `F_SEAL_SHRINK | F_SEAL_GROW | F_SEAL_SEAL` (`seal`) before mapping. A shrunk memfd makes every existing mapping fault on access, so a client holding a dup'd fd could kill the daemon with one `ftruncate`. Sealing turns that into `EPERM` for the caller. `F_SEAL_SEAL` prevents a holder from removing the other seals.

## Five tiers, one state machine

Every tier is the same 24 bytes. The tier is a property of the registry entry (`abacus-daemon/src/registry.rs::Tier`), not of the memory. What changes per tier is what the daemon does with the words each cycle (`Registry::evaluate_all`) and what the SDK lets the holder do with them.

| Tier | Daemon per cycle | SDK exposes |
| --- | --- | --- |
| 0 Interlock | Reap only: `SENTINEL` in any word or a lapsed `expiration_ns` triggers `interlock_reap` and slot removal | `client::Abacus RTSClient::create_interlock` returns `interlock::Interlock`: `open`, `close`, `peek`, `value`, `state`, `touch`, `wait_open`, `wait_close`, bounded variants, `free` |
| 1 WaitCounter | `Tier::WaitCounter` arm: reads the watched target's word through `Registry::watched_value`; if the counter is Open and the watched value reached `open_count`, stores the watched value into `closed_count` and `futex_wake`s | `create_wait_counter`, `wait_counter::WaitCounter::wait_until` (CAS-max target, arms 2x timeout, classifies the wake); attachers get the read-only `interlock::AttachedWaitCounter` |
| 2 WaitTimer | `Tier::WaitTimer` arm: target is the clock; when `clock_now_ms >= open_count`, stores `clock_now_ms` into `closed_count` and wakes | `create_wait_timer`, `wait_timer::WaitTimer::wait_ms`, `wait_ms_with_margin`, `wait_until` |
| 3 WaitCron | Same fire test, then re-arms `open_count` to `registry::next_grid_line(clock_now_ms, interval_ms)`; overflow reaps the entry | `create_wait_cron`, `wait_cron::WaitCron::wait`, which reports `Normal` when `completed_at % interval_ms == 0` and `Overrun` otherwise |
| 4 WaitBarrier | Evaluates every `(target, word, threshold)` condition; any dead target reaps the barrier, all met and Open stamps `closed_count = open_count` and wakes | `create_wait_barrier`, `wait_barrier::WaitBarrier::wait` and `rearm` (which is `increment(Word::Open, 1)`) |

The gate common to tiers 1 to 4 is "Open": `value = open_count - closed_count > 0`. Target liveness is checked regardless of the gate, so an idle watcher still dies with its target.

Initialization per tier happens in `Registry::create`: WaitTimer starts both words at the current clock ms, WaitCron starts `open_count` at the next grid line and `closed_count` at the clock, WaitBarrier starts `(1, 0)` so it is Open immediately, tiers 0 and 1 start `(0, 0)`.

One composition is SDK only: `wait_race::WaitRace` polls N `WaitCounter`s every 1 ms, returns the first whose `closed_count` advanced past its entry snapshot, and returns `InterlockReaped` as soon as any counter is terminated.

## The clock is an interlock

The daemon owns one interlock named `clock` at registry id 0 (`registry::CLOCK_NAME`, `registry::CLOCK_ID`). `Registry::with_limit` creates it through `interlock_create_clock`, stores the daemon start time in milliseconds into both `closed_count` and `open_count`, and arms it with `CLOCK_TTL_NANOS` (100 ms).

Each cycle `Registry::tick` stores `now_ns / NANOS_PER_MS` into `open_count`, evaluates every entry, `futex_wake`s `open_count`, and re-arms the clock via `refresh_clock_expiration`. The clock is never reaped: it lives outside the slab, so `evaluate_all` never sees it.

Because it is the same primitive, a WaitTimer is a WaitCounter on `clock.open_count`: `Registry::create` for `Tier::WaitTimer` sets `watch = Some((Target::Clock, WatchedWord::OpenCount))`, and `Registry::resolve` maps the name `clock` to `Target::Clock`. A client can also name `clock` as the watched interlock of an ordinary WaitCounter.

Protections the clock gets that other interlocks do not:

- `interlock_create_clock` maps the memfd read-write *first*, then applies `F_SEAL_SHRINK | F_SEAL_GROW | F_SEAL_FUTURE_WRITE | F_SEAL_SEAL`. `F_SEAL_FUTURE_WRITE` (declared locally as `0x0010`, absent from the libc crate) leaves the daemon's existing writable mapping intact and blocks any new writable mapping of the same file.
- Clients map it through `interlock_map_clock`, which passes `PROT_READ` only. With the seal in place, a client that attempts `interlock_map` on the clock fd gets `AllocationFailed { step: Mmap }`.
- `Abacus RTSClient::attach_interlock` rejects the name `clock` with `InvalidRequest` pointing at `client.clock()`; `Registry::create` rejects `clock` as a reserved name.

The clock handle is attached once, on connect (`Abacus RTSClient::connect_with_timeout` calls `do_attach(conn, "clock", interlock_map_clock)`), and exposed as the read-only `interlock::ClockHandle`.

## Daemon and SDK

The daemon is the loop; the SDK is everything a client can do without one.

The daemon decides: whether a name can be created (`Registry::create` validates the name length, the reserved name, the interlock cap, and the per-tier required fields), what a watcher watches (`Registry::resolve` binds a name to `Target::Slot { slot, id }` at create time), when a wait condition holds (`evaluate_all`), when an interlock dies (`SENTINEL` in any word, or `expiration_alive(exp, now_ns)` false), and what the current time is (`tick`).

The SDK handles locally:

- Futex waits. `handle_ops::wait_word` loops on `futex_wait` with a `DEFAULT_TIMEOUT_NANOS` (100 ms) poll cadence, re-checking `SENTINEL` and the expiration each pass; the tiered waits (`WaitCounter::wait_until`, `WaitTimer::wait_ms_with_margin`, `WaitCron::wait`, `WaitBarrier::wait`) each run their own loop with the classification their contract requires.
- Keepalive. `touch::Keepalive` arms expirations from one client-wide thread.
- Timeout policy. `types::TimeoutPolicy` (`Abort` by default, `Error` on request) decides what a `WaitTimer` does when its fatal margin elapses (`WaitTimer::on_timeout`).
- Target arithmetic. CAS-max on `open_count` in `WaitCounter::wait_until` and `WaitTimer::wait_ms_with_margin`, so concurrent waits keep the largest target.
- Wake classification. `types::classify_wake` maps `(open, closed)` to `Normal`, `Overrun`, or "not delivered yet".

The split is where the shared memory ends. Anything that can be done by reading or writing the three words is done in the client's own process with no syscall to the daemon. The UDS round trip exists only to create or attach a name and receive an fd (`Request::CreateInterlock`, `Request::AttachInterlock`, answered with `Response::Created` or `Response::Attached` plus one fd over `SCM_RIGHTS`). After that the socket carries nothing: a `WaitCron` loop calls `wait()` forever with no further round trips, because the daemon re-arms it in shared memory.

## TTL and touch

Liveness is a deadline in the third word. An owner that stops extending it is dead by definition.

`touch::Keepalive::register_inner` floors the requested TTL at `2 * interval_ms`, arms it synchronously via `interlock_arm` before returning (so the TTL is already extended when the handle reaches the caller), pushes an `Entry` with `next_due_ns = now + interval_ns`, and starts the thread if it is not already running.

Defaults, from `types`:

- `DEFAULT_TOUCH_INTERVAL_MS = 40`.
- `default_touch_ttl_ms(interval)` is `max(5 * interval, MIN_TOUCH_TTL_MS)`; `MIN_TOUCH_TTL_MS = 200`, so `DEFAULT_TOUCH_TTL_MS = 200` (asserted equal at compile time). The comment on `MIN_TOUCH_TTL_MS` states the reason: a consumer stalled by CFS quota throttling (a 100 ms period) must survive a full period plus scheduling slack.
- `CREATION_TTL_NANOS = 100_000_000` (100 ms) is what a fresh interlock carries out of `interlock_create`, before any keepalive arm.

The keepalive thread is consolidated, one per `Keepalive` value and therefore one per `Abacus RTSClient` (`Abacus RTSClient::connect_with_timeout` constructs `Keepalive::new()`; every create passes `&self.keepalive`). `touch::run` walks the entry list, arms each entry whose `next_due_ns` has passed, computes the earliest next due across all entries (capped at `IDLE_SLEEP`, 100 ms), and sleeps on a `Condvar` until then. An entry whose `interlock_arm` returns `InterlockReaped` sets its `reaped` flag and is removed from the list, which is what `TouchHandle::is_reaped` reports. The thread holds only a `Weak<Inner>`; when the last `Keepalive` and `TouchHandle` are dropped, `Arc::strong_count` falls to 1 and the thread clears `thread_running` and returns.

`ProcessClock` uses `Keepalive::register_with_clock`: on every touch it also stores the clock's `open_count` into the interlock's `open_count`, so `closed_count` is the start time, `open_count` is the last touch, and `value` is uptime in milliseconds.

## Death detection

An owner that dies stops touching. Nothing else happens, and nothing else needs to.

Each cycle `Registry::evaluate_all` reads all three words of every live entry. If any word is `SENTINEL`, or if `expiration_alive(exp, now_ns)` is false (equal counts as expired), it calls `interlock_reap`, which stamps all three words with `SENTINEL` and `futex_wake`s both counters, then marks the slot dead. After the pass, `remove_slot` reaps again (idempotent), drops the name from the index, returns the slot to the free list, and decrements the live count.

A peer sees this in one of three ways, all from shared memory:

- Blocked in a wait: the `futex_wake` from `interlock_reap` returns the waiter, which reads `SENTINEL` and returns `SdkError::InterlockReaped` (`handle_ops::wait_word`, and the per-tier loops).
- Polling: `interlock_is_terminated` or `state()` reads `InterlockState::Expired` (`types::interlock_state` checks `SENTINEL` in any word before the TTL comparison).
- Watching: a `WaitCounter` or `WaitBarrier` whose target is gone gets `None` from `Registry::watched_value` and is reaped in the same pass.

An attacher that no longer has write access to the expiration word terminates through a counter instead: `AttachedInterlock::free` stores `SENTINEL` into `open_count` and wakes both words.

Speed: detection is bounded by the TTL plus one cycle. With the defaults (200 ms TTL, 1 ms cycle) the daemon reaps within roughly 200 ms of the last touch. A delayed detection means the name stays occupied and watchers stay blocked for longer; the failure mode is latency, not incorrectness, because `expiration_ns` is absolute and the comparison does not depend on when the daemon looks.

Waiters have their own escape hatch independent of the daemon. `WaitTimer::wait_ms_with_margin` arms the TTL and its own deadline to the same margin (`max(2 * W, min_fatal_margin_ms)`, with `MIN_FATAL_MARGIN_MS = 50`), and when the deadline passes with nothing delivered and the expiration still live, calls `on_timeout`: abort with a diagnostic under `TimeoutPolicy::Abort`, `Err(SdkError::RtsTimeout)` under `TimeoutPolicy::Error`. A daemon that stops delivering is discovered by the client on its own clock.

## Self-reaping on name recreation

Names are claimed by creation, not by negotiation. `Registry::create` looks up the name in the index and, if present, calls `remove_slot` on the old slot before installing the new entry. `remove_slot` reaps the old interlock (all three words to `SENTINEL`, both counters woken).

The invariants this enforces:

- Every old handle to that name sees a terminal state. The old `InterlockHandle` maps the same page, so its next `peek`, `state`, `touch`, `open`, `close`, or blocked wait reports `InterlockReaped` or `Expired`.
- The new handle is fresh. `Registry::create` allocates a new memfd through `interlock_create` (counters zero, TTL 100 ms) and assigns a new id from `next_id`, which only increases.
- Watchers do not silently retarget. A watcher stores `Target::Slot { slot, id }` captured at create time; `Registry::watched_value` compares the stored id against the current occupant's id and returns `None` on a mismatch, which reaps the watcher. A counter that was watching the old interlock dies rather than start reporting on its replacement.
- The cap is not consumed twice. `Registry::create` skips the limit check when the name already exists, and `live` is decremented by `remove_slot` before the new entry is pushed.

This is the right behavior for crash-only systems because the restarting process has no way to know whether its previous incarnation is gone, and no obligation to find out. It creates its name; whatever held that name before is now terminated and every holder of the old handle learns it from the memory it already has mapped. There is no reconnection handshake, no ownership lease, and no state to reconcile.

## The loop

`daemon::daemon_run_with` anchors on `monotonic_now_nanos()` at startup and sets `next_due = anchor + NANOS_PER_MS`. Each iteration:

1. Checks the stop flag (`AtomicBool`, set by the `SIGTERM` and `SIGINT` handler installed in `main::install_stop_handlers` with `sa_flags = 0`, no `SA_RESTART`, so `ppoll` returns `EINTR` and the flag is seen at once).
2. Computes `timespec_from_nanos(next_due.saturating_sub(now))` and calls `ppoll` over the listener fd plus every client fd. Behind schedule means a zero timeout: evaluate now, then resume the grid.
3. On a positive return, accepts every pending connection (`Server::try_accept` in a loop, each set non-blocking), then services readable clients before hangups, so a client that sends a request and shuts down its write side still gets read.
4. If `now >= next_due`, calls `Registry::tick(now)` and recomputes `next_due = anchor + ((now - anchor) / NANOS_PER_MS + 1) * NANOS_PER_MS`.

Precision comes from three places: `ppoll` with a nanosecond `timespec` rather than a millisecond poll timeout; `daemon::set_timer_slack`, which calls `prctl(PR_SET_TIMERSLACK, 1)` to drop the default 50 microsecond slack so the wake lands at the boundary; and deriving `next_due` from the fixed `anchor` rather than from the previous wake, so cycle boundaries stay on the grid regardless of how long any one cycle took.

Early wakes for client I/O do not consume a boundary. The cycle number is recomputed from wall time, so N I/O wakeups in one millisecond still produce exactly one evaluation, and a stall subsumes the boundaries it missed instead of replaying them. `WaitCron` inherits the same property: `next_grid_line` always returns the first line strictly after the current clock, so a stalled cron fires once and re-arms forward.

The clock advance and every tier evaluation happen inside `tick`, so the clock word and the delivery it triggers are written in the same pass: a timer targeting the current millisecond is stamped by the same cycle that publishes that millisecond.

## Trust model

The system trusts local processes that can open the socket. It does not trust them with the daemon's life.

What a local process can do: connect, create and attach interlocks by name, receive a dup'd memfd, read and write the words of any interlock it holds, terminate any interlock it holds (via `expiration_ns` as a creator, via `open_count` as an attacher), claim any name it likes including one another process is using, and send arbitrary bytes on the socket.

What it cannot do: shrink or grow a memfd (every interlock is sealed at creation; the dup shares the seals), obtain a writable mapping of the clock (`F_SEAL_FUTURE_WRITE`), create an interlock named `clock`, exceed `MAX_NAME_LEN` (255 bytes) or the interlock cap (`DEFAULT_MAX_INTERLOCKS`, 4096, configurable via `--max-interlocks`), pass fds into the daemon (the daemon's `FrameReader::fill` reads with no control buffer), force the daemon to allocate on a declared length (`FrameReader::next_frame` rejects a prefix over `MAX_MESSAGE_SIZE` before reading the bytes), or hold the loop hostage with a partial frame (every client read is one non-blocking `read` per cycle into a bounded buffer).

Protocol faults end the connection. `daemon::service_client` sends one `Response::invalid_request` best effort and returns `ClientOutcome::Drop`, because the stream is desynced past that point. A write that fails with `EPIPE`, `ECONNRESET`, `EAGAIN`, or `ENOTCONN` on a read or write operation is classified by `daemon::is_connection_error` as a dead connection: the protocol is strict request/response, so a client that is not draining its socket is broken and is dropped. A disconnect is not a reap; the client's interlocks die by TTL or not at all.

The socket permission model is filesystem permissions. `transport::Server::create` refuses a path where a live daemon answers a probe connect (`SocketPathOccupied { live_daemon: true }`), refuses a non-socket object at that path, and unlinks a stale socket file. It then applies `SocketOptions` before the listener goes non-blocking and starts accepting: `chown` to the resolved group when `--socket-group` is given (`resolve_group` via `getgrnam_r`), then `chmod` to `socket_mode`, default `0o660`. `Server::drop` removes the socket file.

That boundary is right because the socket only hands out fds to shared memory that is already sealed against the one operation that could hurt a peer. Everything beyond that point is a local process writing to memory it was given, and the failure mode of abuse is a terminated interlock, which every holder is already built to survive.

## Shared memory topology

One memfd per interlock, 24 bytes, mapped once per process that holds it.

The daemon maps read-write. `interlock_create` and `interlock_create_clock` both pass `PROT_READ | PROT_WRITE` to `mmap_interlock`, and the `Registry` keeps that `InterlockHandle` for the entry's lifetime. It hands clients a `dup` of the fd (`interlock_dup_fd`) over `SCM_RIGHTS` (`abacus-wire/src/fdpass.rs::send_frame_with_fds`, one fd per `Created` or `Attached` frame as declared by `codec::expected_fd_count`).

The client maps through a tier-specific function chosen by the SDK call, all defined in `abacus-core/src/interlock.rs`:

- `interlock_map`: `PROT_READ | PROT_WRITE`. Used by `create_interlock`, `attach_interlock`, and `create_process_clock`.
- `interlock_map_counter`, `interlock_map_timer`, `interlock_map_cron`, `interlock_map_barrier`: `PROT_READ | PROT_WRITE`. Which words a holder may write is enforced by the SDK type, not by the mapping: `WaitCounter` has no `close`, `AttachedWaitCounter` has no mutators at all.
- `interlock_map_clock`: `PROT_READ`. Used by `Abacus RTSClient::connect_with_timeout` for the clock.

The clock is read-only for clients because every reader in the system derives time from it. `WaitTimer` computes its target from `clock.open_count`, `WaitBarrier::wait` reports `completed_at` from it, `ProcessClock` stamps it into its own `open_count`, and the daemon fires every tier-2 and tier-3 interlock against it. A single writable client could move the whole system's clock. `PROT_READ` is the client's own mapping flag; `F_SEAL_FUTURE_WRITE` is what makes it not merely a convention, since it prevents any process from creating a writable mapping of that fd after the daemon's.

`InterlockHandle` is `Send` and `Sync`: the region is shared memory designed for concurrent multi-process access and every access goes through atomics.

## Out of scope

The daemon does not persist anything. A restart comes back empty: no names, no registry, no state to reconcile. Waiters discover it through their own fatal margin (`WaitTimer::on_timeout`) or their own poll cadence; the daemon wakes nobody on the way down.

The daemon does not track ownership. `daemon::service_client` removes a hung-up client from the poll set and nothing else; interlocks die by TTL. There is no per-connection interlock table and no cleanup on disconnect.

The wire carries no tier on attach. `Response::Attached` has only an id, so `Abacus RTSClient::attach_wait_counter` requires the caller to know the name is a WaitCounter; attaching a different tier returns a handle with the wrong contract.

`ERR_INTERLOCK_REAPED` (`0x01`) is reserved in v1 and never produced by the daemon: create and attach cannot observe a reap, and the SDK raises `InterlockReaped` from the words.

First-of-N is SDK-only. `wait_race::WaitRace` polls at 1 ms; the module note records that the daemon-side replacement is `WaitOr` in wire v2.

The futex compares 32 bits. `clock::futex_word` takes the low half and `futex_addr` casts the `AtomicU64` pointer to `*const u32`, guarded by a compile-time assertion that the target is little-endian. An increment that is an exact multiple of 2^32 landing in the load-to-syscall window costs one poll cadence and nothing more.

Deployment, not code: which core the daemon runs on (the loop calls `prctl(PR_SET_TIMERSLACK, 1)` but never `sched_setaffinity`); who may open the socket beyond the mode and group the daemon applies; the `RLIMIT_NOFILE` the daemon inherits, which bounds how many interlocks it can allocate before `--max-interlocks` is reached; and supervision and restart of the `abacus` binary, whose only lifecycle contributions are the `SIGTERM` and `SIGINT` handlers, exit code 0 on a clean stop, and socket file removal on `Server::drop`.