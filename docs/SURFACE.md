
# Abacus RTS API surface

The SDK public interface. What a consumer calls, what it returns, how types compose. The
Rust SDK is the native implementation; a non-Rust binding (deferred) wraps it via FFI.

## Connection

`AbacusClient` (client.rs::AbacusClient) owns one socket connection, one read-only clock
handle, one keepalive, and the default timeout policy applied to timers it creates.

```
AbacusClient::connect(socket_path: &Path) -> Result<Self>
    Connects and attaches the daemon clock. Delegates to connect_with_timeout
    (client.rs::AbacusClient::connect).
    Errors: SdkError::Transport(TransportError::Io { operation: IoOperation::Connect, .. })
    when the socket cannot be opened (client.rs::ClientConn::connect); SdkError::MmapFailed
    if the clock descriptor cannot be mapped (client.rs::map_handle).
    Default: timeout = DEFAULT_TRANSPORT_TIMEOUT = Duration::from_secs(1)
    (types.rs::DEFAULT_TRANSPORT_TIMEOUT).

AbacusClient::connect_with_timeout(socket_path: &Path, timeout: Duration) -> Result<Self>
    Connects, sets the same value as both socket read and write timeout, then attaches
    "clock" through interlock_map_clock (read-only mapping)
    (client.rs::AbacusClient::connect_with_timeout).
    Errors: Transport Io with IoOperation::Connect or IoOperation::SetTimeout;
    Transport Io with errno rewritten from EAGAIN to ETIMEDOUT on any timed-out read or
    write (client.rs::ClientConn::map_timeout); Transport Protocol on frame or descriptor
    faults (client.rs::ClientConn::recv_response).
    Default: TimeoutPolicy::Abort (types.rs::TimeoutPolicy default) and
    min_fatal_margin_ms = MIN_FATAL_MARGIN_MS = 50 (types.rs::MIN_FATAL_MARGIN_MS).

set_timeout_policy(&mut self, policy: TimeoutPolicy)
    Sets the policy handed to every WaitTimer created afterwards
    (client.rs::AbacusClient::set_timeout_policy, create_wait_timer).

timeout_policy(&self) -> TimeoutPolicy
    Returns the current policy (client.rs::AbacusClient::timeout_policy).
    Default: TimeoutPolicy::Abort (types.rs::TimeoutPolicy).

set_min_fatal_margin_ms(&mut self, margin_ms: u64)
    Sets the floor used by timers created afterwards
    (client.rs::AbacusClient::set_min_fatal_margin_ms).

min_fatal_margin_ms(&self) -> u64
    Returns the floor (client.rs::AbacusClient::min_fatal_margin_ms).
    Default: MIN_FATAL_MARGIN_MS = 50 (types.rs::MIN_FATAL_MARGIN_MS).

transport_timeout(&self) -> Duration
    The socket read and write timeout in force (client.rs::AbacusClient::transport_timeout).

keepalive(&self) -> &Keepalive
    The single consolidated keepalive shared by every handle this client created
    (client.rs::AbacusClient::keepalive).

clock(&self) -> &ClockHandle
    The read-only clock attached at connect (client.rs::AbacusClient::clock).

is_connected(&self) -> bool
    MSG_PEEK | MSG_DONTWAIT recv of one byte. false on EOF (recv returns 0); true on
    EAGAIN or EWOULDBLOCK; false on any other errno
    (client.rs::AbacusClient::is_connected).

create_interlock(&mut self, name: &str) -> Result<Interlock>
    Creates tier 0 and registers it with the keepalive
    (client.rs::AbacusClient::create_interlock, interlock.rs::Interlock::new).
    Errors: SdkError mapped from the daemon error code (client.rs::daemon_error);
    SdkError::UnexpectedResponse for a non-Created, non-Error reply;
    SdkError::MmapFailed when the returned descriptor cannot be mapped.

attach_interlock(&mut self, name: &str) -> Result<AttachedInterlock>
    Attaches by name with a read-write mapping (interlock_map)
    (client.rs::AbacusClient::attach_interlock).
    Errors: SdkError::InvalidRequest { message: "use client.clock() to access the system
    clock" } when name == "clock"; SdkError::InterlockNotFound when the daemon returns
    ERR_INTERLOCK_NOT_FOUND; the other daemon error mappings.

attach_wait_counter(&mut self, name: &str) -> Result<AttachedWaitCounter>
    Attaches by name and maps through interlock_map_counter
    (client.rs::AbacusClient::attach_wait_counter).
    Errors: same as attach_interlock, without the reserved-name rejection.

create_wait_counter(&mut self, name: &str, watched_name: &str, watched_word: WatchedWord)
    -> Result<WaitCounter>
    Creates tier 1 with the watched name and word set on the request
    (client.rs::AbacusClient::create_wait_counter).
    Errors: SdkError::InterlockNotFound when the watched name is unknown to the daemon
    (registry.rs::Registry::resolve); SdkError::InvalidRequest for a rejected word or a
    missing field; the other daemon error mappings.

create_wait_timer(&mut self, name: &str) -> Result<WaitTimer>
    Creates tier 2 and hands the timer the clock handle, the client's timeout policy, and
    the client's min_fatal_margin_ms (client.rs::AbacusClient::create_wait_timer).
    Errors: daemon error mappings; SdkError::MmapFailed.

create_wait_cron(&mut self, name: &str, interval_ms: u64) -> Result<WaitCron>
    Creates tier 3. interval_ms is multiplied by 1_000_000 with saturating_mul to fill
    interval_ns (client.rs::AbacusClient::create_wait_cron).
    Errors: SdkError::InvalidRequest { message: "WaitCron interval_ms must be > 0" } when
    interval_ms == 0, rejected client-side before any I/O; daemon InvalidRequest when the
    resulting interval is under 1 ms (registry.rs::Registry::create).

create_wait_barrier(&mut self, name: &str, conditions: Vec<(String, WatchedWord, u64)>)
    -> Result<WaitBarrier>
    Creates tier 4. Each condition becomes (name, word discriminant, threshold)
    (client.rs::AbacusClient::create_wait_barrier).
    Errors: daemon InvalidRequest for an empty condition list; InterlockNotFound when any
    watched name is unknown (registry.rs::Registry::create).

create_process_clock(&mut self, name: &str) -> Result<ProcessClock>
    Creates a tier 0 interlock and wraps it as a liveness beacon
    (client.rs::AbacusClient::create_process_clock, process_clock.rs::ProcessClock::new).
    Errors: daemon error mappings; SdkError::MmapFailed.
```

Transport timeout semantics: the value applies to each socket read and each socket write.
A timed-out operation surfaces as `TransportError::Io { errno: ETIMEDOUT }` because
`ClientConn::map_timeout` rewrites EAGAIN. A response payload longer than
`MAX_MESSAGE_SIZE` (4096, codec.rs::MAX_MESSAGE_SIZE) is rejected as
`ProtocolFault::FrameTooLarge`, and a descriptor count that differs from
`expected_fd_count` is rejected as `ProtocolFault::UnexpectedFd` or
`ProtocolFault::Truncated` (client.rs::ClientConn::recv_response).

Clock attachment on connect: `connect_with_timeout` issues `Request::AttachInterlock
{ name: "clock" }` and maps the descriptor with `interlock_map_clock`, which uses
`PROT_READ` only (interlock.rs::interlock_map_clock). The daemon seals the clock memfd
with `F_SEAL_FUTURE_WRITE` (interlock.rs::interlock_create_clock), so a writable mapping
of the clock descriptor fails.

## Interlock

The creator's handle for a tier 0 interlock (interlock.rs::Interlock). Construction
registers the handle with the keepalive at `DEFAULT_TOUCH_INTERVAL_MS` and
`default_touch_ttl_ms(DEFAULT_TOUCH_INTERVAL_MS)` (interlock.rs::Interlock::new).

```
open(&self, h: u64) -> Result<(), SdkError>
    fetch_add h on open_count, then futex_wake (handle_ops.rs::increment).
    Errors: SdkError::InterlockReaped when the prior value was SENTINEL or when
    old > SENTINEL - h; in both cases the word is stored back as SENTINEL and woken.

close(&self, h: u64) -> Result<(), SdkError>
    Same on closed_count (handle_ops.rs::increment).
    Errors: SdkError::InterlockReaped under the same sentinel conditions.

peek(&self) -> (u64, u64)
    (open_count, closed_count) with Acquire loads (handle_ops.rs::peek).

value(&self) -> i64
    open as i64 wrapping_sub closed as i64 (handle_ops.rs::value).

state(&self) -> InterlockState
    Expired if any word is SENTINEL or clock_ns >= expiration_ns; else Overrun for
    value < 0, Open for value > 0, Closed for value == 0
    (handle_ops.rs::state, types.rs::interlock_state).

expiration_ns(&self) -> u64
    Acquire load of expiration_ns (handle_ops.rs::expiration_ns).

wait_open(&self, target: u64) -> Result<u64, SdkError>
    Blocks until open_count >= target; returns the observed value
    (handle_ops.rs::wait_word with timeout None).
    Errors: SdkError::InterlockReaped when the word is SENTINEL, when expiration_ns is
    SENTINEL, or when expiration_ns is in the past.

wait_close(&self, target: u64) -> Result<u64, SdkError>
    Same on closed_count.
    Errors: as wait_open.

wait_open_for(&self, target: u64, timeout: Duration) -> Result<Option<u64>, SdkError>
    Ok(Some(value)) on reach, Ok(None) on timeout (handle_ops.rs::wait_word).
    Errors: as wait_open.
    Default: each futex sleep is capped at DEFAULT_TIMEOUT_NANOS = 100_000_000
    (types.rs::DEFAULT_TIMEOUT_NANOS), then clamped to the remaining deadline.

wait_close_for(&self, target: u64, timeout: Duration) -> Result<Option<u64>, SdkError>
    Same on closed_count.
    Errors: as wait_open.

touch(&self, ms: u64) -> Result<(), SdkError>
    Monotonic-forward CAS of expiration_ns to max(current, now + ms)
    (handle_ops.rs::touch, interlock.rs::interlock_arm). A shorter TTL is a no-op.
    Errors: SdkError::InterlockReaped when expiration_ns is SENTINEL.

is_reaped(&self) -> bool
    True when any of the three words is SENTINEL, or when the registered touch handle
    reports reaped (interlock.rs::Interlock::is_reaped,
    interlock.rs::interlock_is_terminated, touch.rs::TouchHandle::is_reaped).

start_touch_thread(&mut self, interval_ms: u64, ttl_ms: u64) -> &TouchHandle
    Drops the existing registration and registers again with the given interval and TTL
    (interlock.rs::Interlock::start_touch_thread).
    Default: the registration is created at construction with
    DEFAULT_TOUCH_INTERVAL_MS = 40 (types.rs::DEFAULT_TOUCH_INTERVAL_MS) and
    default_touch_ttl_ms(40) = max(40 * 5, MIN_TOUCH_TTL_MS) = 200
    (types.rs::default_touch_ttl_ms, types.rs::MIN_TOUCH_TTL_MS).

stop_touch_thread(&mut self)
    Drops the registration; the entry is removed from the keepalive
    (interlock.rs::Interlock::stop_touch_thread, touch.rs::TouchHandle::drop).

free(&mut self)
    Stops the touch registration, then stamps expiration_ns to SENTINEL and wakes both
    counter futexes (interlock.rs::Interlock::free, interlock.rs::interlock_free).
```

Sentinel-aware increments: `open` and `close` return `Result` precisely because an
increment that starts from SENTINEL or that would cross it restores SENTINEL and reports
`InterlockReaped` rather than resurrecting a terminated interlock
(handle_ops.rs::increment).

### AttachedInterlock

The attacher's handle (interlock.rs::AttachedInterlock). Mapped read-write via
`interlock_map` (client.rs::AbacusClient::attach_interlock).

```
open(&self, h: u64) -> Result<(), SdkError>
close(&self, h: u64) -> Result<(), SdkError>
peek(&self) -> (u64, u64)
value(&self) -> i64
state(&self) -> InterlockState
expiration_ns(&self) -> u64
wait_open(&self, target: u64) -> Result<u64, SdkError>
wait_close(&self, target: u64) -> Result<u64, SdkError>
wait_open_for(&self, target: u64, timeout: Duration) -> Result<Option<u64>, SdkError>
wait_close_for(&self, target: u64, timeout: Duration) -> Result<Option<u64>, SdkError>
is_reaped(&self) -> bool
free(&self)
    Stamps open_count to SENTINEL and wakes both counter futexes
    (interlock.rs::AttachedInterlock::free). Takes &self, not &mut self.
```

Methods that do not exist on this type: `touch`, `start_touch_thread`,
`stop_touch_thread`. TTL extension is the creator's authority only; the compile-fail
enforcement is the absence of the methods from the `impl AttachedInterlock` block
(interlock.rs::AttachedInterlock). The permission rows are exercised at runtime in
permissions.rs::attacher_interlock_row and declared as markers in
crates/abacus-tests/src/permissions.rs (`attached__cannot_touch`,
`attached__cannot_start_touch_thread`).

### AttachedWaitCounter

Read-only view of a tier 1 interlock, mapped through `interlock_map_counter`
(client.rs::AbacusClient::attach_wait_counter, interlock.rs::AttachedWaitCounter).

```
peek(&self) -> (u64, u64)
    (target, delivered) as (open_count, closed_count) (handle_ops.rs::peek).

value(&self) -> i64
    open minus closed (handle_ops.rs::value).

completed_at(&self) -> u64
    closed_count: the value the daemon stamped on delivery
    (handle_ops.rs::completed_at).

is_reaped(&self) -> bool
    Any word is SENTINEL (interlock.rs::interlock_is_terminated).
```

Absent: `open`, `close`, `touch`, `free`, `wait_until`
(crates/abacus-tests/src/permissions.rs markers
`attached_wait_counter__is_read_only`, `__has_no_close`, `__has_no_touch`,
`__has_no_free`, `__has_no_wait_until`). It can observe the target set by the owner, the
value the daemon stamped on delivery, and termination.

### ClockHandle

The daemon clock as the SDK sees it (interlock.rs::ClockHandle). Mapped with
`interlock_map_clock`, which requests `PROT_READ` only
(interlock.rs::interlock_map_clock). `open_count` is the current monotonic millisecond,
`closed_count` the daemon start millisecond (registry.rs::Registry::with_limit,
registry.rs::Registry::tick).

```
peek(&self) -> (u64, u64)
    (now_ms, start_ms) (handle_ops.rs::peek).

now_ms(&self) -> u64
    Acquire load of open_count (interlock.rs::ClockHandle::now_ms).

start_time_ms(&self) -> u64
    Acquire load of closed_count (handle_ops.rs::completed_at).

uptime_ms(&self) -> i64
    now minus start (interlock.rs::ClockHandle::uptime_ms, handle_ops.rs::value).

value(&self) -> i64
    Same computation (handle_ops.rs::value).

wait_open(&self, target: u64) -> Result<u64, SdkError>
    Blocks until the clock millisecond reaches target.
    Errors: SdkError::InterlockReaped on a SENTINEL or lapsed word
    (handle_ops.rs::wait_word).

wait_close(&self, target: u64) -> Result<u64, SdkError>
    Errors: as wait_open.

wait_open_for(&self, target: u64, timeout: Duration) -> Result<Option<u64>, SdkError>
    Ok(None) on timeout.
    Errors: as wait_open.

wait_close_for(&self, target: u64, timeout: Duration) -> Result<Option<u64>, SdkError>
    Errors: as wait_open.
```

Absent: `open`, `close`, `touch`, `free`
(crates/abacus-tests/src/permissions.rs markers `clock__has_no_open`, `__has_no_close`,
`__has_no_touch`, `__has_no_free`).

## WaitCounter

Tier 1. Created through `create_wait_counter(name, watched_name, watched_word)`
(client.rs::AbacusClient::create_wait_counter). Registered with the keepalive at
construction using `DEFAULT_TOUCH_INTERVAL_MS` and `default_touch_ttl_ms(40)` = 200
(wait_counter.rs::WaitCounter::new).

```
wait_until(&self, target: u64, timeout_ms: u64) -> Result<WaitResult, SdkError>
    CAS-max loop raises open_count to target, never lowering it: the loop breaks when
    target <= current (wait_counter.rs::WaitCounter::wait_until). Then arms the TTL with
    interlock_arm(handle, timeout_nanos * 2) and futex-waits on closed_count.
    Returns WaitResult { completed_at: closed_count, state } where state comes from
    classify_wake: Normal when closed == open, Overrun when closed > open
    (types.rs::classify_wake). A futex ETIMEDOUT followed by another pass with no
    delivery returns WaitResult { completed_at, state: WaitState::Timeout }: a timeout is
    a WaitState, not an error.
    Errors: SdkError::InterlockReaped when open_count, closed_count, or expiration_ns is
    SENTINEL; SdkError::InterlockReaped from interlock_arm when expiration is already
    SENTINEL.
    TTL: 2x the timeout, applied monotonically forward by interlock_arm
    (interlock.rs::interlock_arm).

completed_at(&self) -> u64
    closed_count (handle_ops.rs::completed_at).

touch(&self, ms: u64) -> Result<(), SdkError>
    Monotonic-forward TTL extension.
    Errors: SdkError::InterlockReaped when expiration_ns is SENTINEL.

peek(&self) -> (u64, u64)
value(&self) -> i64
is_reaped(&self) -> bool
    Any word is SENTINEL, or the touch registration reports reaped
    (wait_counter.rs::WaitCounter::is_reaped).

free(&mut self)
    Drops the touch registration, then stamps expiration_ns to SENTINEL
    (wait_counter.rs::WaitCounter::free, interlock.rs::interlock_free).
```

Absent: `close` (crates/abacus-tests/src/permissions.rs marker
`wait_counter__has_no_close`). The daemon owns `closed_count` for this tier
(registry.rs::Registry::evaluate_all, arm `Tier::WaitCounter`).

## WaitTimer

Tier 2. Created through `create_wait_timer(name)`, which passes the clock handle, the
client's `TimeoutPolicy`, and the client's `min_fatal_margin_ms`
(client.rs::AbacusClient::create_wait_timer, wait_timer.rs::WaitTimer::new). Registered
with the keepalive at `DEFAULT_TOUCH_INTERVAL_MS` and TTL 200.

```
timeout_policy(&self) -> TimeoutPolicy
    The policy captured at construction (wait_timer.rs::WaitTimer::timeout_policy).
    Default: TimeoutPolicy::Abort (types.rs::TimeoutPolicy).

margin_for(&self, ms: u64) -> u64
    ms.saturating_mul(2).max(self.min_margin_ms) (wait_timer.rs::WaitTimer::margin_for).
    Default floor: MIN_FATAL_MARGIN_MS = 50 (types.rs::MIN_FATAL_MARGIN_MS), overridable
    per client with set_min_fatal_margin_ms.

wait_ms(&self, ms: u64) -> Result<WaitResult, SdkError>
    Calls wait_ms_with_margin(ms, self.margin_for(ms)) (wait_timer.rs::WaitTimer::wait_ms).

wait_ms_with_margin(&self, ms: u64, margin_ms: u64) -> Result<WaitResult, SdkError>
    Reads the clock, CAS-maxes open_count to clock_now + ms, arms the TTL to margin_ms,
    then futex-waits on closed_count until classify_wake yields a state
    (wait_timer.rs::WaitTimer::wait_ms_with_margin).
    ms == 0 returns at once with WaitResult { completed_at: clock_now, state: Normal }
    and arms nothing.
    Errors: SdkError::InterlockReaped when the clock word is SENTINEL, when the timer's
    open_count or closed_count is SENTINEL, when expiration_ns is SENTINEL, or when
    ms == 0 and the handle is already terminated; SdkError::InvalidRequest { message:
    "margin_ms (N) must exceed wait (M)" } when margin_ms <= ms;
    SdkError::InterlockReaped from interlock_arm on a SENTINEL expiration.
    On the margin expiring: TimeoutPolicy::Error returns Err(SdkError::RtsTimeout);
    TimeoutPolicy::Abort prints an "abacus: RTSTimeout" diagnostic to stderr and calls
    std::process::abort (wait_timer.rs::WaitTimer::on_timeout).

wait_until(&self, timestamp_ms: u64) -> Result<WaitResult, SdkError>
    Reads the clock and calls wait_ms(timestamp_ms.saturating_sub(clock_now)), so a past
    timestamp becomes wait_ms(0) and returns at once
    (wait_timer.rs::WaitTimer::wait_until).
    Errors: as wait_ms.

completed_at(&self) -> u64
touch(&self, ms: u64) -> Result<(), SdkError>
    Errors: SdkError::InterlockReaped on a SENTINEL expiration.
peek(&self) -> (u64, u64)
is_reaped(&self) -> bool
    Any word is SENTINEL, or the touch registration reports reaped
    (wait_timer.rs::WaitTimer::is_reaped).
free(&mut self)
    Drops the touch registration, then stamps expiration_ns to SENTINEL
    (wait_timer.rs::WaitTimer::free).
```

Absent: `close` (crates/abacus-tests/src/permissions.rs marker
`wait_timer__has_no_close`). There is no `attach_wait_timer` on `AbacusClient`
(marker `client__has_no_attach_wait_timer`).

## WaitCron

Tier 3. Created through `create_wait_cron(name, interval_ms)`
(client.rs::AbacusClient::create_wait_cron). Registered with the keepalive at
`DEFAULT_TOUCH_INTERVAL_MS` and TTL 200 (wait_cron.rs::WaitCron::new).

```
interval_ms(&self) -> u64
    The interval captured at construction (wait_cron.rs::WaitCron::interval_ms).

wait(&self) -> Result<WaitResult, SdkError>
    Records the current closed_count, then futex-waits until closed_count exceeds it:
    the next grid fire (wait_cron.rs::WaitCron::wait). state is WaitState::Normal when
    completed_at % interval_ms == 0, WaitState::Overrun otherwise.
    Errors: SdkError::InterlockReaped when closed_count is SENTINEL on entry or during
    the loop, when expiration_ns is SENTINEL, or when expiration_ns is in the past.
    Default: each futex sleep is capped at DEFAULT_TIMEOUT_NANOS = 100_000_000
    (types.rs::DEFAULT_TIMEOUT_NANOS).

completed_at(&self) -> u64
peek(&self) -> (u64, u64)
is_reaped(&self) -> bool
free(&mut self)
    Drops the touch registration, then stamps expiration_ns to SENTINEL
    (wait_cron.rs::WaitCron::free).
```

The daemon holds the next grid line in `open_count` and re-arms it after each fire with
`next_grid_line(clock_now_ms, interval_ms)`, which skips over missed lines rather than
bursting (registry.rs::Registry::evaluate_all, registry.rs::next_grid_line).

## WaitBarrier

Tier 4. Created through `create_wait_barrier(name, conditions)` where each condition is
`(String, WatchedWord, u64)` (client.rs::AbacusClient::create_wait_barrier). The daemon
initializes `open_count` to 1 and `closed_count` to 0 (registry.rs::Registry::create).
Registered with the keepalive at `DEFAULT_TOUCH_INTERVAL_MS` and TTL 200
(wait_barrier.rs::WaitBarrier::new).

```
wait(&self) -> Result<WaitResult, SdkError>
    Futex-waits on closed_count until closed >= open, then reads the clock and returns
    WaitResult { completed_at: clock_now, state: WaitState::Normal }
    (wait_barrier.rs::WaitBarrier::wait). completed_at comes from the clock handle, not
    from the barrier's own words.
    Errors: SdkError::InterlockReaped when closed_count or open_count is SENTINEL, when
    the clock word is SENTINEL, when expiration_ns is SENTINEL, or when expiration_ns is
    in the past.
    Default: each futex sleep is capped at DEFAULT_TIMEOUT_NANOS = 100_000_000.

rearm(&self) -> Result<(), SdkError>
    Increments open_count by 1 (wait_barrier.rs::WaitBarrier::rearm,
    handle_ops.rs::increment).
    Errors: SdkError::InterlockReaped under the sentinel conditions of increment.

completed_at(&self) -> u64
    closed_count (handle_ops.rs::completed_at).

peek(&self) -> (u64, u64)
is_reaped(&self) -> bool
    Any word is SENTINEL, or the touch registration reports reaped
    (wait_barrier.rs::WaitBarrier::is_reaped).
free(&mut self)
    Drops the touch registration, then stamps expiration_ns to SENTINEL
    (wait_barrier.rs::WaitBarrier::free).
```

On fire the daemon stores `open_count` into `closed_count`, so the barrier reports
Normal with `closed_count == open_count` (registry.rs::Registry::evaluate_all, arm
`Tier::WaitBarrier`). A barrier whose watched target disappears is reaped by the daemon
in the same pass.

## Keepalive

One `Keepalive` per `AbacusClient`, created in `connect_with_timeout` and cloned into
every handle it makes (client.rs::AbacusClient::connect_with_timeout,
touch.rs::Keepalive). It is a single background thread shared by all registered handles.

```
Keepalive::new() -> Self
    Empty registry, no thread yet (touch.rs::Keepalive::new). Also the Default impl.

register(&self, handle: InterlockHandle, interval_ms: u64, ttl_ms: u64) -> TouchHandle
    Normalizes ttl_ms to ttl_ms.max(interval_ms * 2).max(1), arms the handle immediately
    with that TTL, adds an entry due at now + interval, and spawns the thread named
    "abacus-keepalive" if it is not already running
    (touch.rs::Keepalive::register_inner). If the immediate arm fails the entry is not
    added and the returned TouchHandle reports reaped.

register_with_clock(&self, handle: InterlockHandle, clock: InterlockHandle,
    interval_ms: u64, ttl_ms: u64) -> TouchHandle
    Same, plus the entry carries a clock handle whose open_count is copied into the
    registered handle's open_count at registration and on every tick
    (touch.rs::Keepalive::register_with_clock, touch.rs::run).

thread_running(&self) -> bool
registered(&self) -> usize
```

Each tick the thread walks every registered entry whose `next_due_ns` has passed, calls
`interlock_arm(handle, ttl_ns)`, copies the clock word if the entry has one, and sets
`next_due_ns = now + interval_ns`. An entry whose arm fails has its reaped flag set and
is removed from the list. The thread then sleeps on a condvar until the earliest
`next_due_ns`, bounded above by `IDLE_SLEEP` = 100 ms (touch.rs::IDLE_SLEEP,
touch.rs::run).

Defaults for handles registered by the SDK: interval `DEFAULT_TOUCH_INTERVAL_MS` = 40
(types.rs::DEFAULT_TOUCH_INTERVAL_MS), TTL `default_touch_ttl_ms(40)` =
`max(40 * 5, MIN_TOUCH_TTL_MS)` = 200 (types.rs::default_touch_ttl_ms,
types.rs::MIN_TOUCH_TTL_MS, types.rs::DEFAULT_TOUCH_TTL_MS, which is asserted equal to
`MIN_TOUCH_TTL_MS` by a const assertion in types.rs).

```
TouchHandle::is_reaped(&self) -> bool
    Reads the shared flag the thread sets when an arm fails (touch.rs::TouchHandle).

TouchHandle::stop(&self)
    Deregisters the entry and notifies the thread
    (touch.rs::TouchHandle::stop, touch.rs::Keepalive::deregister).
```

On drop: `TouchHandle::drop` calls `stop`, removing the entry
(touch.rs::TouchHandle::drop). The thread holds only a `Weak` to the shared state; when
every `Keepalive` clone and the last strong reference are gone, the thread sets
`thread_running = false` and returns (touch.rs::run).

## SDK-only compositions

### WaitRace

First-of-N over a set of `WaitCounter` values, implemented entirely in the SDK
(wait_race.rs::WaitRace).

```
WaitRace::new(counters: Vec<WaitCounter>) -> Self

wait(&self) -> Result<(usize, WaitResult), SdkError>
    Snapshots each counter's closed_count once, then polls all counters in order,
    sleeping 1 ms between passes (wait_race.rs::WaitRace::wait). Returns the index of
    the first counter whose closed_count exceeds its snapshot, with
    WaitResult { completed_at: closed, state } where state comes from classify_wake and
    falls back to WaitState::Timeout if classification yields None.
    Errors: SdkError::InvalidRequest { message: "WaitRace over zero counters" } for an
    empty set; SdkError::InterlockReaped as soon as any member reports is_reaped.
    Trap: the snapshot is taken inside wait. A delivery that landed before wait was
    called is not a win, because the snapshot already includes it.

len(&self) -> usize
is_empty(&self) -> bool
counters(&self) -> &[WaitCounter]
```

Free: there is no `free` on `WaitRace`. Dropping it drops the owned `WaitCounter`
values, and each counter's own `free` or drop path applies (wait_counter.rs::WaitCounter).

### ProcessClock

A liveness and uptime beacon built on a bare tier 0 interlock
(process_clock.rs::ProcessClock, client.rs::AbacusClient::create_process_clock).
Construction stores the current daemon clock millisecond into both `closed_count` and
`open_count`, then registers with `register_with_clock` so every keepalive tick stamps
`open_count` with the daemon clock (process_clock.rs::ProcessClock::new,
touch.rs::run).

```
uptime_ms(&self) -> i64
    open_count minus closed_count: last_seen minus start
    (process_clock.rs::ProcessClock::uptime_ms, handle_ops.rs::value).

last_seen_ms(&self) -> u64
    Acquire load of open_count: the clock value at the last keepalive tick
    (process_clock.rs::ProcessClock::last_seen_ms).

start_time_ms(&self) -> u64
    closed_count, fixed at construction (handle_ops.rs::completed_at).

is_reaped(&self) -> bool
    Any word is SENTINEL, or the touch registration is absent or reports reaped
    (process_clock.rs::ProcessClock::is_reaped, note is_none_or).

free(&mut self)
    Drops the touch registration, then stamps expiration_ns to SENTINEL
    (process_clock.rs::ProcessClock::free).
```

## Termination

| Path | Words written | Source |
|---|---|---|
| Creator `free` (`Interlock`, `WaitCounter`, `WaitTimer`, `WaitCron`, `WaitBarrier`, `ProcessClock`) | `expiration_ns` = SENTINEL; both counter futexes woken | interlock.rs::interlock_free |
| Attacher `free` (`AttachedInterlock`) | `open_count` = SENTINEL; both counter futexes woken | interlock.rs::AttachedInterlock::free |
| Daemon reap | `open_count`, `closed_count`, `expiration_ns` all = SENTINEL; both counter futexes woken | interlock.rs::interlock_reap |

Every creator `free` first drops its `TouchHandle`, deregistering the entry from the
keepalive before stamping (interlock.rs::Interlock::free,
wait_counter.rs::WaitCounter::free, wait_timer.rs::WaitTimer::free,
wait_cron.rs::WaitCron::free, wait_barrier.rs::WaitBarrier::free,
process_clock.rs::ProcessClock::free).

What peers observe: any handle mapped onto the same memfd sees the SENTINEL word through
`interlock_is_terminated`, which tests all three words
(interlock.rs::interlock_is_terminated). Blocked waiters wake and return
`SdkError::InterlockReaped` because the wait loops check for SENTINEL and for a lapsed
`expiration_ns` on every pass (handle_ops.rs::wait_word,
wait_counter.rs::WaitCounter::wait_until, wait_timer.rs::WaitTimer::wait_ms_with_margin,
wait_cron.rs::WaitCron::wait, wait_barrier.rs::WaitBarrier::wait). Subsequent
`touch`/`open`/`close` calls return `SdkError::InterlockReaped`
(interlock.rs::interlock_arm, handle_ops.rs::increment). The daemon's next evaluation
pass promotes any single SENTINEL word into a full reap and removes the registry entry,
after which `attach` by that name returns `Condition::InterlockNotFound`
(registry.rs::Registry::evaluate_all, registry.rs::Registry::remove_slot,
registry.rs::Registry::attach).

What drop without `free` does: the handle's `TouchHandle` drops, which deregisters the
entry from the keepalive (touch.rs::TouchHandle::drop). No word is written. The TTL
stops being extended, and the daemon reaps the interlock on its next pass after
`expiration_ns` has passed, with `expiration_alive(exp, now)` returning false
(registry.rs::Registry::evaluate_all, clock.rs::expiration_alive). The mapping itself is
released when the last `InterlockHandle` clone drops and `munmap` runs
(interlock.rs::InterlockRegion::drop).

## Error types

### SdkError

client.rs::SdkError. Derives `Debug, Clone, PartialEq, Eq`, implements `Display` and
`std::error::Error`. `Result<T>` is `std::result::Result<T, SdkError>`
(client.rs::Result).

| Variant | Produced by |
|---|---|
| `InterlockReaped` | Increment from or across SENTINEL (handle_ops.rs::increment); any wait loop observing a SENTINEL word or a lapsed `expiration_ns` (handle_ops.rs::wait_word, wait_counter.rs, wait_timer.rs, wait_cron.rs, wait_barrier.rs); `interlock_arm` on a SENTINEL expiration (interlock.rs::interlock_arm); daemon reply code `ERR_INTERLOCK_REAPED` (client.rs::daemon_error) |
| `InterlockNotFound { name }` | Daemon reply code `ERR_INTERLOCK_NOT_FOUND`, with the reply message carried as `name` (client.rs::daemon_error) |
| `AllocationFailed { message }` | Daemon reply code `ERR_ALLOCATION_FAILED` (client.rs::daemon_error); `Condition::AllocationFailed` rendered to a string (client.rs::From<Condition>) |
| `InvalidRequest { message }` | `attach_interlock("clock")` (client.rs::AbacusClient::attach_interlock); `create_wait_cron` with `interval_ms == 0` (client.rs::AbacusClient::create_wait_cron); `WaitRace::wait` over zero counters (wait_race.rs::WaitRace::wait); `wait_ms_with_margin` with `margin_ms <= ms` (wait_timer.rs::WaitTimer::wait_ms_with_margin); daemon reply code `ERR_INVALID_REQUEST` (client.rs::daemon_error) |
| `RtsTimeout` | `WaitTimer` margin expiry under `TimeoutPolicy::Error` (wait_timer.rs::WaitTimer::on_timeout) |
| `Transport(TransportError)` | Connect, timeout set, frame send, frame receive, decode, and descriptor-count faults (client.rs::ClientConn); `extract_single_fd` when the reply carries a count other than one (client.rs::extract_single_fd) |
| `MmapFailed { message }` | The mapping function returns `Condition`, rendered to a string (client.rs::map_handle) |
| `UnexpectedResponse { message }` | A reply that is neither the expected `Created`/`Attached` nor `Error` (client.rs::AbacusClient::do_create, client.rs::do_attach); an unrecognized daemon error code, message `"unknown daemon error 0x{code:02x}: {message}"` (client.rs::daemon_error) |
| `ConnectionClosed` | Not an `SdkError` variant. It is `TransportError::ConnectionClosed` (error.rs::TransportError), reachable as `SdkError::Transport(TransportError::ConnectionClosed)` when a read hits EOF (framing.rs::read_exact, fdpass.rs::recv_prefix_with_fds) |

### From conversions

```
impl From<TransportError> for SdkError
    Wraps the value in SdkError::Transport (client.rs::From<TransportError>).

impl From<Condition> for SdkError
    Condition::InterlockReaped                -> SdkError::InterlockReaped
    Condition::InterlockNotFound { name }     -> SdkError::InterlockNotFound { name }
    Condition::AllocationFailed { .. }        -> SdkError::AllocationFailed
                                                 { message: condition.to_string() }
    Condition::InvalidRequest { message }     -> SdkError::InvalidRequest { message }
    (client.rs::From<Condition>)
```

`handle_ops::touch` and `interlock_arm` produce `Condition` and are converted through
this impl (handle_ops.rs::touch).

## Deferred

Wire v2: tier metadata on the attach response, enabling tier-correct handle construction
and enforcement of non-attachable tiers, plus discriminants for boolean compositions.

Non-Rust FFI binding: must select `TimeoutPolicy::Error`, since the default
`TimeoutPolicy::Abort` calls `std::process::abort` in the host process
(wait_timer.rs::WaitTimer::on_timeout).