
# Abacus design

How and why Abacus is shaped the way it is. The primitive, the tiers, the daemon loop, the
SDK split, and the mechanisms that make crash-only coordination work. INTERFACE.md is the frozen
contract surface; PHILOSOPHY.md is the design constitution; this file is the rationale and
mechanism between them.

## The primitive

One primitive: the interlock. Three `AtomicU64` words in a `#[repr(C)]` struct,
`abacus-core/src/interlock.rs::Interlock`: `open_count` at offset 0, `closed_count` at
offset 8, `expiration_ns` at offset 16. `INTERLOCK_SIZE` is 24 bytes, asserted at compile time
against `size_of::<Interlock>()`.

The backing is a `memfd_create` file truncated to 24 bytes and mapped `MAP_SHARED`
(`interlock_create`, `mmap_interlock`). The mapping is reference counted: `InterlockHandle`
wraps an `Arc<InterlockRegion>`, and the last clone unmaps and closes the fd
(`InterlockRegion::drop`). The fd is handed to clients by `interlock_dup_fd`, which `dup`s the
memfd so both processes map the same page.

### Stored properties

| Offset | Word | Name | Who writes it |
|---|---|---|---|
| 0 | 0 | `open_count` | tier 0: creator and attachers (`Interlock::open`, `AttachedInterlock::open`). tier 1: creator sets the target by CAS-max (`WaitCounter::wait_until`). tier 2: the timer's single waiter sets the absolute clock target exactly, replacing any stale target (`WaitTimer::wait_ms_with_margin`); daemon seeds it at create. tier 3: daemon only (grid line). tier 4: daemon seeds 1 at create; creator increments to re-arm (`WaitBarrier::rearm`). clock: daemon only (`Registry::tick`). |
| 8 | 1 | `closed_count` | tier 0: creator and attachers (`Interlock::close`, `AttachedInterlock::close`). tiers 1 to 4: daemon only (`Registry::evaluate_all`). clock: daemon writes the start millisecond once at `Registry::with_limit`. |
| 16 | 2 | `expiration_ns` | Owner, monotonic forward only, by CAS-max (`interlock_arm`, driven by keepalive and explicit `touch`). Daemon stamps `SENTINEL` on reap (`interlock_reap`). Owner stamps `SENTINEL` on free (`interlock_free`). |

### Invariants

- Counters are monotonic and carry arbitrary increments. `handle_ops::increment` uses
  `fetch_add`.
- `expiration_ns` is `CLOCK_MONOTONIC` nanoseconds and never decreases: `interlock_arm` is a
  CAS-max loop that returns `Ok(())` when the requested deadline is already behind the current
  one. The deadline is capped at `SENTINEL - 1`, so no TTL, however large, arms an interlock
  into termination.
- `SENTINEL` (`u64::MAX`) in any word means terminated. `interlock_is_terminated` reads all
  three.
- Termination cannot be undone. `handle_ops::increment` never writes a word already at
  `SENTINEL`, and stores `SENTINEL` into a word the add would carry onto or past it; either way
  it wakes waiters and returns `SdkError::InterlockReaped`. `interlock_arm` refuses to write over a `SENTINEL` expiration.
- Values above 2^63 are outside the representable signed range; `handle_ops::value` computes
  `open - closed` with `wrapping_sub`.

### Sealing

`interlock_create` applies `F_SEAL_SHRINK | F_SEAL_GROW | F_SEAL_SEAL` before mapping. A
shrunk memfd faults every existing mapping, so a client holding a dup'd fd could kill the
daemon with one `ftruncate`. Sealing turns that into `EPERM`. `F_SEAL_SEAL` prevents a holder
from removing the other seals.

### Derived properties

| Name | Formula | Meaning |
|---|---|---|
| `value` | `(open as i64).wrapping_sub(closed as i64)` | Outstanding work. Positive = open, zero = closed, negative = overrun. `handle_ops::value`, inline in `evaluate_all`. |
| `open` (daemon) | `value > 0` | The watch is armed. Tiers 1 to 4 only fire while armed. |
| `terminated` | any word equals `SENTINEL` | `interlock_is_terminated`. |
| `alive` | `expiration_ns > now_ns` | `clock::expiration_alive`. Equality reads as dead. |
| `InterlockState` | sentinel check, then `clock_ns >= expiration_ns`, then sign of `value` | `Expired`, `Overrun`, `Open`, `Closed`. `types::interlock_state`. |
| `WaitState` | `closed == open` yields `Normal`; `closed > open` yields `Overrun`; `closed < open` yields `None` | SDK classification. `types::classify_wake`. |

### Shared memory topology

One memfd per interlock, 24 bytes, mapped once per process that holds it.

The daemon maps read-write (`PROT_READ | PROT_WRITE` via `mmap_interlock`), keeps the
`InterlockHandle` for the entry's lifetime, and hands clients a `dup` of the fd over
`SCM_RIGHTS`. The client maps through a tier-specific function in
`abacus-core/src/interlock.rs`:

- `interlock_map`: `PROT_READ | PROT_WRITE`. Bare interlocks and process clocks.
- `interlock_map_counter`, `_timer`, `_cron`, `_barrier`: `PROT_READ | PROT_WRITE`. Per-word
  write discipline is enforced by the SDK type, not by page protection.
- `interlock_map_clock`: `PROT_READ`. The only mmap-enforced restriction.

`InterlockHandle` is `Send` and `Sync`: the region is shared memory designed for concurrent
multi-process access and every access goes through atomics.

## Five tiers, one state machine

Every tier is the same 24 bytes. The tier is a property of the registry entry
(`registry.rs::Tier`), not of the memory. What changes per tier is what the daemon does with
the words each cycle (`Registry::evaluate_all`) and what the SDK lets the holder do with them.

| Tier | Daemon per cycle | SDK exposes |
|---|---|---|
| 0 Interlock | Reap only: `SENTINEL` or lapsed TTL triggers `interlock_reap` and slot removal | `create_interlock` returns `Interlock`: `open`, `close`, `peek`, `value`, `state`, `touch`, `wait_open`, `wait_close`, bounded variants, `free` |
| 1 WaitCounter | Reads watched target's word via `watched_value`; if Open and watched >= `open_count`, stores watched into `closed_count` and wakes | `create_wait_counter`, `WaitCounter::wait_until` (CAS-max target, arms 2x timeout, classifies wake); attachers get read-only `AttachedWaitCounter` |
| 2 WaitTimer | Target is the clock; when `clock_now_ms >= open_count`, stores `clock_now_ms` into `closed_count` and wakes | `create_wait_timer`, `WaitTimer::wait_ms`, `wait_ms_with_margin`, `wait_until` (one waiter at a time, exact target) |
| 3 WaitCron | Same fire test as tier 2, then re-arms `open_count` to `next_grid_line(clock_now_ms, interval_ms)`; overflow reaps | `create_wait_cron`, `WaitCron::wait` reports `Normal` when `completed_at % interval_ms == 0`, `Overrun` otherwise |
| 4 WaitBarrier | Evaluates every `(target, word, threshold)` condition; any dead target reaps, all met and Open stamps `closed_count = open_count` and wakes | `create_wait_barrier`, `WaitBarrier::wait` and `rearm` |

The gate common to tiers 1 to 4 is "Open": `value > 0`. Target liveness is checked regardless,
so an idle watcher still dies with its target.

Initialization per tier in `Registry::create`: WaitTimer starts both words at the current
clock ms, WaitCron starts `open_count` at the next grid line and `closed_count` at the clock,
WaitBarrier starts `(1, 0)` so it is Open immediately, tiers 0 and 1 start `(0, 0)`.

### States

```mermaid
stateDiagram-v2
    [*] --> Closed: interlock_create, words (0, 0), TTL now + 100 ms
    Closed --> Open: open(h), value > 0
    Open --> Closed: close(h) to value == 0
    Open --> Overrun: close(h) past open
    Closed --> Overrun: close(h) with open == 0
    Overrun --> Closed: open(h) back to parity
    Overrun --> Open: open(h) past closed
    Closed --> Closed: interlock_arm (touch), expiration moves forward
    Open --> Open: interlock_arm (touch)
    Overrun --> Overrun: interlock_arm (touch)
    Closed --> Expired: TTL lapse or SENTINEL
    Open --> Expired: TTL lapse or SENTINEL
    Overrun --> Expired: TTL lapse or SENTINEL
    Expired --> [*]: registry slot removed, mapping stays valid
```

**Closed.** `value == 0`, alive and unstamped. A fresh interlock enters here:
`interlock_create` writes (0, 0) and `expiration_ns = now + CREATION_TTL_NANOS`. Re-entered
whenever `closed_count` catches up to `open_count`. A tier 1 to 4 object in this state is not
evaluated for firing because `open` is false.

**Open.** `value > 0`. Entered by `increment` on `Word::Open`, by a tier 1 or 2 target CAS,
by the daemon seeding a cron grid line, or by a barrier re-arm. For tiers 1 to 4 this is the
armed state.

**Overrun.** `value < 0`. Entered when `closed_count` passes `open_count`. Informational only;
the daemon does not reap on overrun.

**Expired.** Either any word equals `SENTINEL`, or `!expiration_alive(exp, now_ns)`. On the
daemon side, entry runs `interlock_reap` (all three words to `SENTINEL`, both counters woken),
then `remove_slot` drops the registry entry. The shared mapping remains valid for every holder;
they read `SENTINEL` and report `InterlockReaped`.

## The clock

The daemon owns one interlock named `clock` at registry id 0 (`CLOCK_NAME`, defined in
`abacus_core::interlock` so the SDK shares it and re-exported by the registry; `CLOCK_ID`).
`Registry::with_limit` creates it through `interlock_create_clock`, stores the daemon start
time in milliseconds into both words, and arms it with `CLOCK_TTL_NANOS` (100 ms).

Each cycle `Registry::tick` stores `now_ns / NANOS_PER_MS` into `open_count`, re-arms via
`refresh_clock_expiration`, runs `evaluate_all`, and wakes `open_count`. The clock lives
outside the slab, so `evaluate_all` never sees it and it is never reaped.

Because it is the same primitive, a WaitTimer is a WaitCounter on `clock.open_count`:
`Registry::create` for `Tier::WaitTimer` sets
`watch = Some((Target::Clock, WatchedWord::OpenCount))`, and `Registry::resolve` maps the name
`clock` to `Target::Clock`. A client can also name `clock` as the watched interlock of an
ordinary WaitCounter.

### Clock protections

- `interlock_create_clock` maps read-write first, then applies
  `F_SEAL_SHRINK | F_SEAL_GROW | F_SEAL_FUTURE_WRITE | F_SEAL_SEAL`.
  `F_SEAL_FUTURE_WRITE` (declared locally as `0x0010`) leaves the daemon's existing writable
  mapping intact and blocks any new writable mapping.
- Clients map through `interlock_map_clock` with `PROT_READ` only.
- `AbacusClient::attach_interlock` rejects the name `clock` with `InvalidRequest`;
  `Registry::create` rejects `clock` as a reserved name.

The clock handle is attached once on connect and exposed as the read-only `ClockHandle`.

The clock is read-only for clients because every reader in the system derives time from it.
`WaitTimer` computes its target from `clock.open_count`, `WaitBarrier::wait` reports
`completed_at` from it, `ProcessClock` stamps it into its own `open_count`, and the daemon
fires every tier 2 and tier 3 interlock against it. A single writable client could move the
whole system's clock.

## Daemon evaluation

### The loop

`daemon::daemon_run_with` anchors on `first_tick_boundary(monotonic_now_nanos())`, the first
absolute millisecond boundary of `CLOCK_MONOTONIC` after startup, and sets `next_due = anchor`.
Each iteration:

1. Check the stop flag (`AtomicBool`, set by the `SIGTERM`/`SIGINT` handler with `sa_flags = 0`,
   no `SA_RESTART`, so `ppoll` returns `EINTR` and the flag is seen at once).
2. Compute `timespec_from_nanos(next_due.saturating_sub(now))` and `ppoll` over the listener
   fd plus every client fd. Behind schedule means a zero timeout.
3. Accept every pending connection (non-blocking), service readable clients before hangups.
4. Drop clients whose `revents` carry `POLLHUP`/`POLLERR`/`POLLNVAL` or whose
   `service_client` returns `ClientOutcome::Drop`.
5. If `now >= next_due`, call `registry.tick(now)` and recompute
   `next_due = anchor + ((now - anchor) / NANOS_PER_MS + 1) * NANOS_PER_MS`.
6. If the client set changed, rebuild the pollfd set in place (`rebuild_pollfds`).

Precision comes from three places: `ppoll` with a nanosecond `timespec` rather than a
millisecond poll timeout; `prctl(PR_SET_TIMERSLACK, 1)` to drop the default 50 us slack; and
deriving `next_due` from the fixed anchor, so cycle boundaries stay on the grid regardless of
how long any one cycle took. The anchor sits on an absolute millisecond boundary because an
anchor at an arbitrary phase lets wake latency flip `now / 1 ms`, and the clock stutters (an
advance of 0, then 2); on the absolute grid each on-time tick advances it by exactly one.

The recomputation is the catch-up rule: a stall of N milliseconds skips directly to the next
boundary after now instead of firing N times in a burst. Early wakes for client I/O do not
consume a boundary: the loop services the socket, finds `now < next_due`, and does not tick.

The clock advance, its expiration refresh, and every tier evaluation happen inside `tick`, so
the clock word and the delivery it triggers are written in the same pass.

### Clock advancement

`Registry::tick(now_ns)` computes `current_ms = now_ns / NANOS_PER_MS`, stores it into
`clock.open_count` with `Ordering::Release`, calls `refresh_clock_expiration` (re-arms for
100 ms), runs `evaluate_all(current_ms, now_ns)`, and futex-wakes `open_count`. The refresh
comes before the evaluation: after a stall longer than the clock's TTL, evaluating first would
find the clock lapsed and reap every entry that depends on it. A clock that cannot be re-armed
aborts the daemon rather than leaving every client's liveness root dead.

Precision is one millisecond, truncated from `CLOCK_MONOTONIC` nanoseconds.
`monotonic_now_nanos` aborts the process if `clock_gettime` fails. The anchor is
`CLOCK_MONOTONIC` itself: `open_count` is the absolute monotonic millisecond.
`closed_count` is the start millisecond and never moves, so `open_count - closed_count` is
daemon uptime.

### Evaluation order

`registry.rs::evaluate_all(clock_now_ms, now_ns)` walks every slot in index order. For each
live entry:

1. Load all three words with `Ordering::Acquire`.
2. Sentinel check. If any word equals `SENTINEL`, call `interlock_reap`, push to `dead`,
   `continue`. Runs before TTL check because a freed object's `expiration_ns` is `SENTINEL`,
   which as a raw number would read as alive under `expiration_alive`.
3. TTL check. If `!expiration_alive(exp, now_ns)` (that is, `exp <= now_ns`), reap, mark dead,
   `continue`. Runs before tier logic so a lapsed object never fires.
4. Dependency check. If any target in `depends_on` fails `target_alive` (slot empty, id
   mismatch, any word `SENTINEL`, or its TTL lapsed), reap, mark dead, `continue`. The owner
   or a dependency died, so this entry dies with it, whatever its tier. Because the TTL is
   part of the check, a target that lapses this pass takes its dependents with it in the same
   pass, whether they sit in lower or higher slots.
5. Compute `value` and `open = value > 0`.
6. Tier dispatch:
   - **Interlock:** nothing.
   - **WaitTimer:** if `open && clock_now_ms >= open_count`, store `clock_now_ms` into
     `closed_count`, futex-wake.
   - **WaitCron:** if `open && clock_now_ms >= open_count`, store `clock_now_ms` into
     `closed_count`, futex-wake, re-arm to `next_grid_line(clock_now_ms, interval_ms)`;
     overflow reaps.
   - **WaitCounter:** resolve watch via `watched_value(target, word)`. `None` (the target
     fails `target_alive`, or the watched word is `SENTINEL`) means target gone: reap the
     watcher. On
     `Some(watched)` and `open && watched >= open_count`, store `watched` into `closed_count`,
     futex-wake.
   - **WaitBarrier:** iterate conditions via `watched_value`. Any `None` sets `target_gone`
     and reaps immediately. Otherwise, if `open && all_met`, store `open_count` into
     `closed_count`, futex-wake.
7. After the sweep, drain `dead` through `remove_slot` (reap again idempotently, remove name,
   return slot to free list, decrement `live`).

The clock is not in `slots` and is never evaluated or reaped by this loop.

## Per-tier composition

### Interlock (tier 0)

`Tier::Interlock` has an empty match arm in `evaluate_all`. The daemon reaps on sentinel or
lapsed TTL and does nothing else: no watch, no stamp. Both counters are writable by the
creator and by any attacher. Waiters block directly on counter words via
`handle_ops::wait_word`, which re-checks sentinel and expiration on every pass and sleeps at
most `DEFAULT_TIMEOUT_NANOS` (100 ms) per futex call.

### WaitCounter (tier 1)

Watches one word (`OpenCount` or `ClosedCount`) of one other interlock, named at create time
and resolved by `Registry::resolve` into `Target::Clock` or `Target::Slot { slot, id }`.
Identity, not name: `watched_value` compares `entry.id != id`, returning `None` on mismatch.
Because ids are strictly increasing, a name recreated over the old one produces a new id and
the old watcher's target resolves to `None`.

The daemon stamps `closed_count` with the watched value (not the target), so an overshoot is
visible as `Overrun`. It fires when `open && watched >= open_count`. If the target is reaped,
freed, lapsed, or recreated, `watched_value` returns `None` and the watcher is reaped in the
same pass, even if idle. A WaitCounter may not watch its own name (`InvalidRequest`), and a
create naming a target that is not alive is refused (`InterlockNotFound`).

SDK: `WaitCounter::wait_until(target, timeout_ms)` CAS-maxes `target` into `open_count`, arms
TTL to `2 * timeout_ms`, then loops on `closed_count` with `futex_wait` against one absolute
deadline `timeout_ms` out, so a wake short of delivery sleeps only for what remains. It
returns `WaitState::Timeout` once that deadline passes with nothing delivered.

### WaitTimer (tier 2)

Created with `watch = Some((Target::Clock, WatchedWord::OpenCount))` and both counters seeded
to the current clock millisecond. The daemon fires when `open && clock_now_ms >= open_count`
and stamps `closed_count` with `clock_now_ms` (same discipline as tier 1: watched value, not
target), so a late wake reads as `Overrun`.

SDK: `WaitTimer::wait_ms_with_margin(ms, margin_ms)` reads the clock, returns immediately with
`Normal` if `ms == 0`, rejects `margin_ms <= ms`, takes the timer's single waiter slot (a
concurrent second wait returns `InvalidRequest`), sets `open_count` to exactly
`clock_now + ms` (never over `SENTINEL`, replacing a stale target a timed-out wait left), arms
TTL to `margin_ms`, and loops on `closed_count`. `wait_until` reads the clock once and waits
from that reading. If `now_ns >= deadline_ns`
without delivery, `on_timeout` applies the policy: `TimeoutPolicy::Error` returns
`SdkError::DeliveryTimeout`; `TimeoutPolicy::Abort` (default) prints diagnostics and calls
`std::process::abort()`. The margin defaults to `max(2 * ms, MIN_FATAL_MARGIN_MS)` where
`MIN_FATAL_MARGIN_MS` is 50. A daemon restart is discovered exactly here: nobody stamps and the
margin expires.

### WaitCron (tier 3)

Also watches `Target::Clock / OpenCount`, with an `interval_ms` derived from the wire's
`interval_ns` (registry rejects zero and anything under 1 ms). At create, `closed_count` is
the current clock millisecond and `open_count` is `next_grid_line(clock_now_ms, interval_ms)`.

`next_grid_line(now_ms, interval_ms)` is
`(now_ms / interval_ms).checked_add(1)?.checked_mul(interval_ms)`: checked arithmetic, returns
the first grid line strictly after `now_ms`, or `None` on a zero interval or overflow.

Firing and re-arm in one step: stamp `closed_count = clock_now_ms`, wake, store
`next_grid_line(clock_now_ms, interval_ms)` into `open_count`. Overflow reaps.

Missed grid lines are skipped, not replayed. Re-arm is computed from `clock_now_ms`, not the
previous target, so a stall across N lines produces one stamp and one re-arm forward. The SDK
reads the skew: `WaitCron::wait` reports `Normal` when `closed % interval_ms == 0` and
`Overrun` otherwise.

### WaitBarrier (tier 4)

Created with a non-empty vector of `(name, watched_word, threshold)` conditions, each name
resolved to a `Target` at create time (identity-tracked like a tier 1 watch). The daemon seeds
`open_count = 1`, `closed_count = 0`, so the barrier is born armed.

`evaluate_all` iterates the conditions: any target that fails `target_alive` reaps
immediately; any
`watched < threshold` clears `all_met`. Fires only when `open && all_met`, stamping
`closed_count = open_count` exactly. That exactness lets `WaitBarrier::wait` treat
`closed >= open` as delivery and always report `Normal` with `completed_at` from the clock.

Re-arm is `WaitBarrier::rearm`: increment `open_count` by 1, restoring `open > closed`.

## SDK-only compositions

### WaitRace

`wait_race::WaitRace` takes a vector of `WaitCounter` handles, snapshots each `closed_count`,
and polls: for each counter, checks `is_reaped()` (returning `InterlockReaped` for the whole
race if any member died) and `peek()`, returning `(index, WaitResult)` for the first counter
whose `closed_count` exceeds its snapshot. Sleeps 1 ms between passes. SDK-only because the
daemon has no tier for "fire when any of N fires."

### ProcessClock

Every client has exactly one, created by `AbacusClient::connect` as a tier 0 interlock named
by the caller. It is not a tier: the daemon treats it like any other bare interlock. What makes
it a process clock is the SDK. At construction it stamps the daemon clock's `open_count` into
both counters (`closed_count` is the fixed start time), refusing with `InterlockReaped` if
either already reads `SENTINEL` (`ProcessClock::new` is fallible), and the client's keepalive
owns it from
then on (`Keepalive::bind_process`): every touch arms its TTL and copies the daemon clock
millisecond into `open_count` through `stamp_last_seen`, a compare-and-swap that refuses to
overwrite `SENTINEL`, so a reap or an attacher's `free()` can never be erased. `uptime_ms()` is
`last_seen - start`. Any process can watch it with a `WaitCounter` on `OpenCount`, or declare it
a dependency.

### Ownership and dependencies

Every entry may carry `depends_on`: the targets it dies with. Two sources fill it, and the
daemon does not distinguish them. The owner: every SDK create sends its client's ProcessClock
as `(name, id)`, and the daemon resolves the name and requires the id to match, so a process
whose clock was replaced cannot create, and so cannot displace, a successor's names in the up to
one keepalive interval before it notices its own death. Dependencies: a ProcessClock's create
carries the names the process depends on, resolved at create like `WaitBarrier` conditions, so
"depend on X" means whatever holds the name X now, and a dependency that does not exist yet
refuses the create (`InterlockNotFound`), which is start-order gating for free.

Both resolve to `Target::Slot { slot, id }`, the same identity a watcher holds, and die the same
way: step 4 of the evaluation order. A dependency can only resolve to an entry that already
exists, and a create may not name itself, so the graph is acyclic by construction; nothing
checks for cycles because none can form. There is no ownership table, no reference count, and
no release call: ownership is a watch.

`AbacusClient::connect_waiting` wraps the gating for supervised processes: it retries a connect
that fails with `ENOENT`, `ECONNREFUSED`, or `EACCES` (no socket yet, nobody listening yet, or
a socket whose mode the daemon has not applied yet; `retryable_connect_errno`) or a missing
dependency every `CONNECT_RETRY_INTERVAL` (100 ms), logs one line per distinct reason for the
whole wait, and returns `DependencyTimeout` at a caller-chosen ceiling rather than letting a
supervisor crash-loop the process through a planned outage. Any other error returns at once.

## Daemon and SDK split

The daemon is the loop; the SDK is everything a client can do without one.

The daemon decides: whether a name can be created (validates name length, reserved name,
interlock cap, per-tier fields), what a watcher watches (`Registry::resolve` binds a name to
`Target::Slot { slot, id }` at create time), when a wait condition holds, when an interlock
dies, and what the current time is.

The SDK handles locally: futex waits (each tier's loop with its own classification), keepalive,
timeout policy, target arithmetic (CAS-max for a WaitCounter, an exact target for a
WaitTimer's single waiter), and wake classification.

The split is where the shared memory ends. Anything that can be done by reading or writing the
three words is done in the client's process with no syscall to the daemon. The UDS round trip
exists only to create or attach a name and receive an fd. After that the socket carries
nothing: a `WaitCron` loop calls `wait()` forever with no further round trips, because the
daemon re-arms it in shared memory.

### SDK state interpretation

`classify_wake(open, closed)` is the whole classification:

- `closed == open`: `Some(Normal)`, delivered exactly at the target.
- `closed > open`: `Some(Overrun)`, delivered past the target.
- `closed < open`: `None`, not yet delivered.

`WaitState::Timeout` is never produced by `classify_wake`. It is synthesized by the waiter when
its own bound expires without delivery.

The sentinel check always precedes classification. Every wait loop loads both counters and
returns `InterlockReaped` if either is `SENTINEL` before calling `classify_wake`. The
expiration word is also checked: `SENTINEL` or a lapsed deadline yields `InterlockReaped`.

## TTL and keepalive

Liveness is a deadline in the third word. An owner that stops extending it is dead by
definition.

`Keepalive::register_inner` clamps a zero `interval_ms` to 1 ms, floors the requested TTL at
`2 * interval_ms`, arms it synchronously via `interlock_arm` before returning, starts the
thread if not running, and pushes an `Entry` with `next_due_ns = now + interval_ns`. Every
step that can fail returns an error before anything is registered: a terminated interlock is
`InterlockReaped`, and a thread the OS refuses is `SdkError::KeepaliveSpawnFailed`, never a
panic. `register`, `register_with_clock`, `Interlock::start_touch_thread`, and the five
handle constructors behind the `create_*` calls are all fallible for that reason.

Defaults from `types`:

- `DEFAULT_TOUCH_INTERVAL_MS = 40`.
- `default_touch_ttl_ms(interval)` is `max(5 * interval, MIN_TOUCH_TTL_MS)`;
  `MIN_TOUCH_TTL_MS = 200`, so `DEFAULT_TOUCH_TTL_MS = 200` (compile-time assertion).
  The floor exists because a consumer stalled by CFS quota throttling (100 ms period) must
  survive a full period plus scheduling slack.
- `CREATION_TTL_NANOS = 100_000_000` (100 ms): what a fresh interlock carries before any
  keepalive arm.

The keepalive thread is consolidated: one per `Keepalive`, one per `AbacusClient`.
`touch::run` walks the entry list, arms each due entry, computes the earliest next due (capped
at `IDLE_SLEEP`, 100 ms), and sleeps on a `Condvar`. An entry whose `interlock_arm` returns
`InterlockReaped` is flagged and removed. The thread holds only a `Weak<Inner>` and exits when
the last strong reference drops.

The keepalive also owns process liveness. On every pass, before touching its entries, it checks
the daemon clock's expiration and, when due, touches the ProcessClock. A lapsed daemon clock
(`DaemonClockLapsed`) or a ProcessClock that will not arm or stamp (`ProcessClockReaped`) is
death. Under `TimeoutPolicy::Abort`, the default, it writes one diagnostic line and aborts the
process; under `TimeoutPolicy::Error` it records why the process died as a `Liveness`
(`ProcessClockReaped` or `DaemonClockLapsed`, read through `AbacusClient::liveness`), stops
touching the clock and nothing else, and the cascade leaves every handle reporting
`InterlockReaped`. `liveness()` reads `Alive` until then; under `Abort` a caller never sees
anything else, because the process is gone first. The keepalive starts under `Abort` at
connect, so a host that selects `Error` does so after the keepalive is already running. The ProcessClock lives in the
keepalive's shared state rather than as a registered entry, so it keeps no strong reference to
the thread: the process clock stops, and lapses, when the client and every handle are gone.

The keepalive thread's scheduling is the caller's (`set_keepalive_priority`), never the SDK's.
It is applied with `pthread_setschedparam` to the running thread and to any thread started
later, and a refusal is an error (`KeepalivePriorityFailed`), never a fallback. The thread
inherits the connecting thread's CPU affinity and, under `KeepalivePriority::Normal`, its
scheduling. This matters on real-time deployments: a SCHED_OTHER keepalive beside a
SCHED_FIFO loop on its core starves, its ProcessClock lapses, and a healthy process dies. The
ProcessClock TTL is per client (`set_process_clock_ttl_ms`), default 200 ms for the CFS reason
above; a shorter one belongs only where the keepalive cannot be throttled.

## Death detection

An owner that dies stops touching. Nothing else happens, and nothing else needs to.

Each cycle `evaluate_all` reads all three words. If any is `SENTINEL` or
`expiration_alive(exp, now_ns)` is false, it calls `interlock_reap` (all three words to
`SENTINEL`, both counters woken), marks the slot dead, and `remove_slot` returns it to the
free list.

A peer sees this in one of three ways, all from shared memory:

- Blocked in a wait: `futex_wake` returns the waiter, which reads `SENTINEL` and returns
  `InterlockReaped`.
- Polling: `interlock_is_terminated` or `state()` reads `Expired`.
- Watching: a `WaitCounter` or `WaitBarrier` whose target is gone or lapsed gets `None` from
  `watched_value` and is reaped in the same pass.
- Depending: an entry whose owner or dependency is gone fails the dependency check and is
  reaped, and so is everything that depends on it.

The cascade is how a process death propagates. A process is SIGKILLed; its keepalive stops;
its ProcessClock lapses one TTL later; the daemon reaps it, then every interlock it owned and
every ProcessClock that depended on it, then everything those owned. The first level goes in
the same pass as the lapse whatever its slot, because `target_alive` reads the lapsed TTL
directly; each further level depends on an entry this pass reaps rather than one that lapsed,
so it goes in the same pass from a higher slot and waits one pass from a lower slot. Each dependent process's keepalive finds its own clock reaped
within one touch interval and aborts. Measured on the AGX (`OPERATION.md`): reap of a
three-level chain at most one TTL plus a few cycles after the kill, and each abort detected
within one keepalive interval of its reap.

An attacher terminates through a counter instead: `AttachedInterlock::free` stores `SENTINEL`
into `open_count` and wakes both words.

Detection is bounded by the TTL plus one cycle. With defaults (200 ms TTL, 1 ms cycle) the
daemon reaps within roughly 200 ms. The failure mode is latency, not incorrectness, because
`expiration_ns` is absolute.

Waiters have their own escape hatch: `WaitTimer::wait_ms_with_margin` arms its own deadline to
the margin (`max(2 * W, MIN_FATAL_MARGIN_MS)`), and when the deadline passes with nothing
delivered, calls `on_timeout`. A daemon that stops delivering is discovered by the client on
its own clock.

### Self-reaping on name recreation

Names are claimed by creation, not by negotiation. `Registry::create` looks up the name and,
if present, calls `remove_slot` on the old slot before installing the new entry.

The invariants this enforces:

- Every old handle sees a terminal state.
- The new handle is fresh: new memfd, new id from `next_id` (only increases).
- Watchers do not retarget: `watched_value` compares the stored id against the occupant's id
  and returns `None` on mismatch, reaping the watcher.
- The cap is not consumed twice: `Registry::create` skips the limit check when the name exists,
  and `remove_slot` decrements `live` before the new entry is pushed.

This is the right behavior for crash-only systems: the restarting process creates its name,
whatever held it before is terminated, and every holder learns from the memory it already has
mapped. No reconnection handshake, no ownership lease, no state to reconcile.

## Trust model

The system trusts local processes that can open the socket. It does not trust them with the
daemon's life.

What a local process can do: connect, create and attach by name, receive a dup'd memfd, read
and write any interlock it holds, terminate any it holds (via `expiration_ns` as creator, via
`open_count` as attacher), claim any name including one another process is using, send
arbitrary bytes.

What it cannot do: shrink or grow a memfd (sealed), map the clock writable
(`F_SEAL_FUTURE_WRITE`), create a name `clock`, exceed `MAX_NAME_LEN` (255) or the interlock
cap (default 4096), pass fds into the daemon (no control buffer on reads), force allocation on
a declared length (prefix rejected before reading), or hold the loop hostage (one non-blocking
read per client per cycle into a bounded buffer).

Protocol faults end the connection: one `Response::invalid_request` best effort, then drop.
A client not draining its socket is broken and is dropped. A disconnect is not a reap; the
client's interlocks die by TTL or not at all.

The socket permission model is filesystem permissions: `Server::create` refuses a live-daemon
path, refuses a non-socket path, unlinks a stale socket, then applies `chown`/`chmod`.
`Server::drop` removes the socket file.

That boundary is right because the socket only hands out fds to shared memory already sealed
against the one operation that could hurt a peer. The failure mode of abuse is a terminated
interlock, which every holder is built to survive.

## Out of scope

The daemon does not persist anything. A restart comes back empty. Waiters discover it through
their own fatal margin or poll cadence; the daemon wakes nobody on the way down.

The daemon keeps no ownership table and no connection state. `service_client` removes a
hung-up client from the poll set and nothing else. An owned interlock dies because it watches
its owner's clock, not because the daemon remembers who created it: interlocks die by TTL, by
`SENTINEL`, or by a dead dependency.

The futex compares 32 bits. `futex_word` takes the low half, guarded by a compile-time
little-endian assertion. An increment exactly at 2^32 in the load-to-syscall window costs one
poll cadence and nothing more.

Details in BACKLOG.md.
