
# Abacus interface

The frozen interface surface: wire protocol, SDK API, errors, permissions, and termination
contracts. DESIGN.md says why and how the mechanism works internally. This file says what
the boundaries are and what a consumer calls.

## UDS surface

The daemon listens on a Unix stream socket (`transport::Server::create`). Clients connect,
send length-prefixed frames, and receive one response per request.

| Request | Wire tag | Response on success | Descriptors returned | Enforced in |
|---|---|---|---|---|
| `CreateInterlock` | `0x01` | `Created { id }` | 1 (dup of the interlock memfd) | `daemon::handle_request`, `registry::Registry::create_with_fd` |
| `AttachInterlock` | `0x02` | `Attached { id, tier }` | 1 (dup of the interlock memfd) | `daemon::handle_request`, `registry::Registry::attach` (validates the name: empty or over 255 bytes is `InvalidRequest`) |
| either | | `Error { code, message }` | 0 | `codec::Response::from_condition`, `codec::expected_fd_count` |

| Property | Contract | Enforced in |
|---|---|---|
| Connection lifetime | Persistent. Many requests per connection, served in arrival order. | `daemon::service_client` loop over `Connection::next_request` |
| Pipelining | Allowed. All frames already buffered are decoded and answered in one service pass. | `framing::FrameReader::next_frame`, `daemon::service_client` |
| Accept mode | Listener and accepted connections are nonblocking. | `transport::Server::create`, `daemon::daemon_run_with` |
| Client transport timeout | Default 1 s read and write timeout; `EAGAIN` is remapped to `ETIMEDOUT`. | `client::ClientConn::connect`, `client::ClientConn::map_timeout`, `types::DEFAULT_TRANSPORT_TIMEOUT` |
| Descriptor cardinality | Client rejects a response carrying more or fewer descriptors than `expected_fd_count`. | `client::ClientConn::recv_response`, `client::extract_single_fd` |
| Protocol fault | Daemon logs one line, sends `Error { ERR_INVALID_REQUEST }`, then drops the connection. | `daemon::service_client` |
| Peer EOF | Remaining buffered requests are answered, then the connection is dropped. | `daemon::service_client` (`closing` path) |
| Unwritable response | Write errors in the connection-error set (`EPIPE`, `ECONNRESET`, `EAGAIN`, `ENOTCONN`) drop the client without a log line. | `daemon::is_connection_error`, `daemon::service_client` |
| Socket file on start | A live socket at the path is refused; a stale socket file is replaced; a non-socket file is refused. | `transport::Server::create`, `transport::probe_connect_succeeds`, `transport::is_socket_file` |
| Socket file on stop | Removed when the `Server` is dropped. | `transport::Server::drop` |
| Socket permissions | Mode applied after bind (default `0o660`); optional group chown. | `transport::Server::apply_socket_options`, `daemon::DaemonConfig::new` |

### SDK connection

`AbacusClient` (client.rs::AbacusClient) owns one socket connection, one read-only clock
handle, one keepalive, one ProcessClock, and the default timeout policy applied to timers
it creates. Connecting creates the client's ProcessClock: every client has exactly one,
created at connect, not by a separate call.

`KeepalivePriority` (types.rs::KeepalivePriority) is the scheduling the SDK applies to the
client's keepalive thread; the caller chooses it, and the SDK never picks a real-time
priority on its own. `KeepalivePriority::Normal`, the default: at start the thread keeps
the scheduling it inherited from the thread that called `connect`; set on a running
thread, it moves the thread to SCHED_OTHER. `KeepalivePriority::Fifo(p)`: SCHED_FIFO at
priority `p`, 1 to 99, which needs CAP_SYS_NICE or an RLIMIT_RTPRIO at or above `p` and,
where the kernel has RT group scheduling, real-time runtime in the thread's cgroup. The
keepalive thread inherits the CPU affinity of the thread that called `connect`; the SDK
never changes it, so pinning that thread before connecting chooses the keepalive's core.

```
AbacusClient::connect(socket_path: &Path, clock_name: &str, dependencies: &[&str])
    -> Result<Self>
    Connects, attaches the daemon clock, creates a tier 0 interlock named clock_name with
    the given dependencies and no owner, and binds the keepalive to it. Delegates to
    connect_with_timeout (client.rs::AbacusClient::connect).
    Errors: SdkError::Transport(TransportError::Io { operation: IoOperation::Connect, .. })
    when the socket cannot be opened (client.rs::ClientConn::connect);
    SdkError::InterlockNotFound { name } when a dependency does not exist;
    SdkError::MmapFailed if the clock descriptor cannot be mapped (client.rs::map_handle);
    SdkError::KeepaliveSpawnFailed when the keepalive thread cannot be started.
    Default: timeout = DEFAULT_TRANSPORT_TIMEOUT = Duration::from_secs(1)
    (types.rs::DEFAULT_TRANSPORT_TIMEOUT).

AbacusClient::connect_with_timeout(socket_path: &Path, clock_name: &str,
    dependencies: &[&str], timeout: Duration) -> Result<Self>
    Connects, sets the same value as both socket read and write timeout, then attaches
    "clock" through interlock_map_clock (read-only mapping), creates the ProcessClock,
    and binds the keepalive to it
    (client.rs::AbacusClient::connect_with_timeout).
    Errors: Transport Io with IoOperation::Connect or IoOperation::SetTimeout;
    Transport Io with errno rewritten from EAGAIN to ETIMEDOUT on any timed-out read or
    write (client.rs::ClientConn::map_timeout); Transport Protocol on frame or descriptor
    faults (client.rs::ClientConn::recv_response);
    SdkError::InterlockNotFound { name } when a dependency does not exist;
    SdkError::InterlockReaped if the ProcessClock was terminated before it could be
    stamped or bound (process_clock.rs::ProcessClock::new refuses a SENTINEL word);
    SdkError::KeepaliveSpawnFailed when the keepalive thread cannot be started. When the
    keepalive cannot bind the new ProcessClock for either reason, connect frees it first,
    so the daemon reaps it on its next pass (client.rs::bind_or_free).
    Default: TimeoutPolicy::Abort (types.rs::TimeoutPolicy default) and
    min_fatal_margin_ms = MIN_FATAL_MARGIN_MS = 50 (types.rs::MIN_FATAL_MARGIN_MS).

AbacusClient::connect_waiting(socket_path: &Path, clock_name: &str,
    dependencies: &[&str], max_wait: Duration) -> Result<Self>
    Loops connect (default transport timeout). Retries on exactly two failures: a socket
    connect that fails with ENOENT, ECONNREFUSED, or EACCES
    (Transport(Io { operation: Connect, errno }), client.rs::retryable_connect_errno) and a
    missing dependency (InterlockNotFound). Any other connect errno, and every other error,
    returns at once. Retry interval: CONNECT_RETRY_INTERVAL = 100 ms
    (types.rs::CONNECT_RETRY_INTERVAL).
    Logging, on stderr, one line per distinct reason for the whole wait, never per retry:
    - "abacus: waiting up to <max_wait> for dependency \"<name>\""
    - "abacus: waiting up to <max_wait> for the Abacus daemon at <path>"
    When max_wait passes without success: SdkError::DependencyTimeout { waited, missing }.
    (client.rs::AbacusClient::connect_waiting).

set_timeout_policy(&mut self, policy: TimeoutPolicy)
    Sets the policy handed to every WaitTimer created afterwards and to the keepalive's
    liveness checks (client.rs::AbacusClient::set_timeout_policy). The keepalive runs
    under Abort from connect until this call.

timeout_policy(&self) -> TimeoutPolicy
    Returns the current policy (client.rs::AbacusClient::timeout_policy).
    Default: TimeoutPolicy::Abort (types.rs::TimeoutPolicy).

set_min_fatal_margin_ms(&mut self, margin_ms: u64)
    Sets the floor used by timers created afterwards
    (client.rs::AbacusClient::set_min_fatal_margin_ms).

min_fatal_margin_ms(&self) -> u64
    Returns the floor (client.rs::AbacusClient::min_fatal_margin_ms).
    Default: MIN_FATAL_MARGIN_MS = 50 (types.rs::MIN_FATAL_MARGIN_MS).

set_keepalive_priority(&self, priority: KeepalivePriority) -> Result<()>
    Sets the keepalive thread's scheduling with pthread_setschedparam: applied at once to
    the running thread and kept for any thread started later
    (client.rs::AbacusClient::set_keepalive_priority, touch.rs::Keepalive::set_priority).
    Call it before this process raises its own threads to real-time priority: a
    SCHED_OTHER keepalive starves behind a SCHED_FIFO loop on its core and a healthy
    process dies. The thread's CPU affinity is not changed.
    Errors: SdkError::InvalidRequest for KeepalivePriority::Fifo(p) with p outside 1 to
    99; SdkError::KeepalivePriorityFailed { errno } when the kernel refuses the change
    (for example EPERM without CAP_SYS_NICE or RLIMIT_RTPRIO), leaving the previous
    priority in force. There is no silent fallback. With no thread running the priority
    is only stored, and the call that starts the thread reports any refusal
    (touch.rs::spawn_thread).
    Default: KeepalivePriority::Normal.

keepalive_priority(&self) -> KeepalivePriority
    The priority last set (client.rs::AbacusClient::keepalive_priority,
    touch.rs::Keepalive::priority).
    Default: KeepalivePriority::Normal.

set_process_clock_ttl_ms(&self, ttl_ms: u64) -> Result<()>
    Sets the TTL the keepalive writes on this client's ProcessClock, raised to at least
    two touch intervals (80 ms at the default 40 ms interval), and arms the clock with it
    at once: a longer TTL takes effect now, a shorter one from the first touch whose
    now + ttl passes the current deadline (arming never moves a deadline back). Other
    handles' TTLs are unchanged. A shorter TTL detects this process's death sooner and
    tolerates shorter stalls of its keepalive: go below 200 ms only where the keepalive
    cannot be throttled (an isolated core, a full CPU quota, a real-time keepalive)
    (client.rs::AbacusClient::set_process_clock_ttl_ms,
    touch.rs::Keepalive::set_process_ttl_ms).
    Errors: SdkError::InterlockReaped when no process clock is bound (dropped after its
    death under TimeoutPolicy::Error) or the arm finds it terminated; the TTL is then
    unchanged.
    Default: DEFAULT_TOUCH_TTL_MS = 200 (types.rs::DEFAULT_TOUCH_TTL_MS).

process_clock_ttl_ms(&self) -> Option<u64>
    The ProcessClock TTL the keepalive writes, in milliseconds; None once the keepalive
    has dropped the clock after its death under TimeoutPolicy::Error
    (client.rs::AbacusClient::process_clock_ttl_ms, touch.rs::Keepalive::process_ttl_ms).
    Default: Some(200).

transport_timeout(&self) -> Duration
    The socket read and write timeout in force (client.rs::AbacusClient::transport_timeout).

keepalive(&self) -> &Keepalive
    The single consolidated keepalive shared by every handle this client created
    (client.rs::AbacusClient::keepalive).

process_clock(&self) -> &ProcessClock
    The client's ProcessClock, created at connect
    (client.rs::AbacusClient::process_clock).

clock(&self) -> &ClockHandle
    The read-only clock attached at connect (client.rs::AbacusClient::clock).

liveness(&self) -> Liveness
    Process liveness as last observed by the keepalive thread
    (client.rs::AbacusClient::liveness, touch.rs::Keepalive::liveness).
    Liveness::Alive until the keepalive finds the ProcessClock reaped
    (Liveness::ProcessClockReaped) or the Abacus clock's expiration lapsed
    (Liveness::DaemonClockLapsed) (types.rs::Liveness). Under TimeoutPolicy::Abort the
    keepalive aborts the process first, so a caller only ever reads Alive; under
    TimeoutPolicy::Error the keepalive records the cause here and keeps running.

is_connected(&self) -> bool
    false once the connection is poisoned (a send or receive on it failed); otherwise
    MSG_PEEK | MSG_DONTWAIT recv of one byte: false on EOF (recv returns 0); true on data,
    EAGAIN, or EWOULDBLOCK; false on any other errno
    (client.rs::AbacusClient::is_connected).
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

## Create call

`Request::CreateInterlock` carries one field set; which optional fields are required is a
function of `tier`.

| Field | Type | Meaning | Valid values | Daemon use |
|---|---|---|---|---|
| `name` | string | Registry key | 1 to 255 bytes, not `clock` | `registry::validate_name`, `registry::Registry::create` |
| `tier` | u8 | Tier discriminant | 0..=4 | `registry::Tier::from_wire` |
| `owner` | optional (string, u64) | ProcessClock name and registry id that owns this entry | name must resolve to a live entry with a matching id | `registry::Registry::create` |
| `dependencies` | list of string | Names this entry dies with, resolved at create | each name must resolve to a live entry | `registry::Registry::create` |
| `watched_name` | optional string | Name of the interlock to watch | must resolve to a live entry or `clock`, and must not be the entry's own name | `registry::Registry::resolve`, `registry::Registry::create_with_fd` |
| `watched_word` | optional u8 | Which word of the target to read | 0 (`open_count`), 1 (`closed_count`) | `registry::WatchedWord::from_wire` |
| `interval_ns` | optional u64 | Cron grid interval | > 0 and >= 1 ms after integer division | `registry::Registry::create` (`Tier::WaitCron` arm) |
| `conditions` | optional list of (string, u8, u64) | Barrier thresholds | non-empty; each name must resolve to a live entry and must not be the entry's own name; each word in {0,1} | `registry::Registry::create` (`Tier::WaitBarrier` arm) |

| Tier | Value | Required fields | Ignored fields |
|---|---|---|---|
| Interlock | 0 | name | all optional fields |
| WaitCounter | 1 | name, watched_name, watched_word | interval_ns, conditions |
| WaitTimer | 2 | name | all optional fields (watch is fixed to `clock.open_count`) |
| WaitCron | 3 | name, interval_ns | watched_name, watched_word, conditions |
| WaitBarrier | 4 | name, conditions | watched_name, watched_word, interval_ns |

| Error | Cause | Enforced in |
|---|---|---|
| `Error { ERR_INVALID_REQUEST }` | tier not in 0..=4 | `daemon::handle_request` |
| `Condition::InvalidRequest` | empty name, name over 255 bytes, name `clock`, live-count limit reached, missing tier field, `interval_ns` 0 or sub-millisecond, empty conditions list, `watched_word` not 0 or 1, owner or dependency names own name, `watched_name` or a condition name is own name | `registry::validate_name`, `registry::Registry::create`, `registry::WatchedWord::from_wire` |
| `Condition::InterlockReaped` | owner name not found, owner id does not match, or owner not alive (gone, replaced, or lapsed) | `registry::Registry::create` |
| `Condition::InterlockNotFound` | `watched_name`, any condition name, or any dependency name does not resolve or is not alive | `registry::Registry::resolve`, `registry::Registry::create`, `registry::Registry::target_alive` |

Alive means `registry::Registry::target_alive`: the entry exists with the resolved id, none
of its three words is `SENTINEL`, and its TTL has not lapsed. A lapsed entry the daemon has
not reaped yet is already dead to a create, as it will be to the next evaluation pass.

`registry::Registry::create` delegates to `registry::Registry::create_with_fd`, which the
daemon calls to get the descriptor in the same call. Every validation, `interlock_create`,
and the descriptor `dup` happen before any registry mutation, and the id is taken last, so a
refused or failed create changes nothing: an existing entry of the same name survives and no
id is consumed.
| `Condition::AllocationFailed` | memfd create, ftruncate, seal, mmap, or dup failed | `interlock::interlock_create`, `interlock::interlock_dup_fd` |

Creating over an existing live name is permitted: the old entry is reaped and removed, the
new one takes the name with a strictly larger id (`registry::Registry::create` remove-slot
path, `registry::Registry::remove_slot`).

The SDK rejects `create_wait_cron` with `interval_ms == 0` or with an `interval_ms` whose
nanoseconds overflow a u64 before it reaches the wire
(`client::AbacusClient::create_wait_cron`), and rejects `attach_interlock("clock")`
(`client::AbacusClient::attach_interlock`).

### SDK create and attach methods

```
create_interlock(&mut self, name: &str) -> Result<Interlock>
    Creates tier 0 and registers it with the keepalive
    (client.rs::AbacusClient::create_interlock, interlock.rs::Interlock::new).
    Errors: SdkError mapped from the daemon error code (client.rs::daemon_error);
    SdkError::UnexpectedResponse for a non-Created, non-Error reply;
    SdkError::MmapFailed when the returned descriptor cannot be mapped;
    SdkError::KeepaliveSpawnFailed when the keepalive thread cannot be started.
    Every create below registers with the keepalive the same way, so every one also
    returns KeepaliveSpawnFailed on a failed spawn (Interlock::new, WaitCounter::new,
    WaitTimer::new, WaitCron::new, WaitBarrier::new all return Result).

attach_interlock(&mut self, name: &str) -> Result<AttachedInterlock>
    Attaches by name with a read-write mapping (interlock_map), whatever the entry's tier:
    the reply's tier is not checked (client.rs::AbacusClient::attach_interlock).
    Errors: SdkError::InvalidRequest { message: "use client.clock() to access the system
    clock" } when name == "clock"; SdkError::InterlockNotFound when the daemon returns
    ERR_INTERLOCK_NOT_FOUND; the other daemon error mappings.

attach_wait_counter(&mut self, name: &str) -> Result<AttachedWaitCounter>
    Attaches by name, checks the tier the daemon reports, and only then maps through
    interlock_map_counter. Refuses any tier other than 1, before mapping, with
    SdkError::InvalidRequest { message: "\"<name>\" is tier <t>, not a WaitCounter" }
    (client.rs::AbacusClient::attach_wait_counter, client.rs::do_attach_request).
    Errors: same as attach_interlock without the reserved-name rejection, plus
    SdkError::InvalidRequest for a tier mismatch.

create_wait_counter(&mut self, name: &str, watched_name: &str, watched_word: WatchedWord)
    -> Result<WaitCounter>
    Creates tier 1 with the watched name and word set on the request
    (client.rs::AbacusClient::create_wait_counter).
    Errors: SdkError::InterlockNotFound when the watched name is unknown to the daemon or
    not alive (registry.rs::Registry::resolve, registry.rs::Registry::target_alive);
    SdkError::InvalidRequest for a rejected word, a missing field, or a watched name equal
    to name; the other daemon error mappings.

create_wait_timer(&mut self, name: &str) -> Result<WaitTimer>
    Creates tier 2 and hands the timer the clock handle, the client's timeout policy, and
    the client's min_fatal_margin_ms (client.rs::AbacusClient::create_wait_timer).
    Errors: daemon error mappings; SdkError::MmapFailed.

create_wait_cron(&mut self, name: &str, interval_ms: u64) -> Result<WaitCron>
    Creates tier 3. interval_ms is multiplied by 1_000_000 with checked_mul to fill
    interval_ns (client.rs::AbacusClient::create_wait_cron, client.rs::checked_interval_ns).
    Errors: SdkError::InvalidRequest { message: "WaitCron interval_ms must be > 0" } when
    interval_ms == 0, and SdkError::InvalidRequest { message: "WaitCron interval_ms
    overflows nanoseconds" } when the product overflows a u64, both rejected client-side
    before any I/O; daemon InvalidRequest when the resulting interval is under 1 ms
    (registry.rs::Registry::create).

create_wait_barrier(&mut self, name: &str, conditions: Vec<(String, WatchedWord, u64)>)
    -> Result<WaitBarrier>
    Creates tier 4. Each condition becomes (name, word discriminant, threshold)
    (client.rs::AbacusClient::create_wait_barrier).
    Errors: daemon InvalidRequest for an empty condition list or a condition naming the
    barrier itself; InterlockNotFound when any watched name is unknown or not alive
    (registry.rs::Registry::create).

```

### Initialization values

Every interlock is created by `interlock::interlock_create`: counters zero, expiration
`now + CREATION_TTL_NANOS` (100 ms). The registry then stamps tier-specific words.

| Tier | open_count | closed_count | expiration_ns |
|---|---|---|---|
| Interlock (0) | 0 | 0 | now + 100 ms |
| WaitCounter (1) | 0 | 0 | now + 100 ms |
| WaitTimer (2) | clock now (ms) | clock now (ms) | now + 100 ms |
| WaitCron (3) | next grid line = `(now/interval + 1) * interval` | clock now (ms) | now + 100 ms |
| WaitBarrier (4) | 1 | 0 | now + 100 ms |
| clock (id 0) | monotonic now (ms) | monotonic now (ms) | now + 100 ms |

Enforced in `interlock::interlock_create`, `interlock::CREATION_TTL_NANOS`,
`registry::Registry::create` (per-tier match), `registry::next_grid_line`,
`registry::Registry::with_limit`, `interlock::interlock_create_clock`.

Client-side handles arm on construction: keepalive registration calls `interlock_arm` with
the touch TTL before returning (`touch::Keepalive::register_inner`). A `ProcessClock` also
stamps both counters with the current clock millisecond, and refuses with `InterlockReaped`
if either already reads `SENTINEL` (`process_clock::ProcessClock::new`).

## Interlock (tier 0)

### Field semantics

| Word | Meaning | Written by | Read by |
|---|---|---|---|
| open_count | Monotonic count of opens | Creator and attachers (`Interlock::open`, `AttachedInterlock::open`) | All handles, daemon |
| closed_count | Monotonic count of closes | Creator and attachers (`Interlock::close`, `AttachedInterlock::close`) | All handles, daemon |
| expiration_ns | Monotonic-clock deadline | Creator via `touch` or keepalive (`handle_ops::touch`, `interlock::interlock_arm`); daemon on reap | Daemon (`registry::Registry::evaluate_all`), SDK waiters |

### SDK: Interlock (creator handle)

The creator's handle for a tier 0 interlock (interlock.rs::Interlock). Construction
registers the handle with the keepalive at `DEFAULT_TOUCH_INTERVAL_MS` and
`default_touch_ttl_ms(DEFAULT_TOUCH_INTERVAL_MS)` (interlock.rs::Interlock::new).

```
open(&self, h: u64) -> Result<(), SdkError>
    Adds h to open_count by compare-and-swap, then futex_wake (handle_ops.rs::increment).
    Errors: SdkError::InterlockReaped when the word is already SENTINEL (it is not
    written) or when old >= SENTINEL - h, so the sum would reach or pass SENTINEL (the
    word is stored as SENTINEL); the word is woken in both cases.

close(&self, h: u64) -> Result<(), SdkError>
    Same on closed_count (handle_ops.rs::increment).
    Errors: SdkError::InterlockReaped under the same sentinel conditions.

peek(&self) -> (u64, u64)
    (open_count, closed_count) with Acquire loads, closed_count loaded first: open only
    grows, and grows before closed, so the pair never reads closed past open
    (handle_ops.rs::peek).

value(&self) -> i64
    open as i64 wrapping_sub closed as i64, from peek (handle_ops.rs::value).

state(&self) -> InterlockState
    Expired if any word is SENTINEL or clock_ns >= expiration_ns; else Overrun for
    value < 0, Open for value > 0, Closed for value == 0. Loads closed_count before
    open_count, as peek does (handle_ops.rs::state, types.rs::interlock_state).

expiration_ns(&self) -> u64
    Acquire load of expiration_ns (handle_ops.rs::expiration_ns).

wait_open(&self, target: u64) -> Result<u64, SdkError>
    Blocks until open_count >= target; returns the observed value
    (handle_ops.rs::wait_word with timeout None).
    Errors: SdkError::InterlockReaped when the word is SENTINEL, when expiration_ns is
    SENTINEL, or when expiration_ns is in the past; likewise when the daemon clock's
    expiration_ns is SENTINEL or in the past (the daemon is dead).

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

start_touch_thread(&mut self, interval_ms: u64, ttl_ms: u64)
    -> Result<&TouchHandle, SdkError>
    Drops the existing registration and registers again with the given interval and TTL
    (interlock.rs::Interlock::start_touch_thread).
    Errors: as Keepalive::register (InterlockReaped, KeepaliveSpawnFailed); on error the
    interlock is left with no keepalive registration.
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
increment never writes a word already at SENTINEL, and an addition that would carry a live
word onto or past SENTINEL stores SENTINEL instead; both report `InterlockReaped` rather
than resurrecting a terminated interlock (handle_ops.rs::increment).

### SDK: AttachedInterlock

The attacher's handle (interlock.rs::AttachedInterlock). Mapped read-write via
`interlock_map` (client.rs::AbacusClient::attach_interlock). `tier()` is the tier the
daemon reported at attach (client.rs::AbacusClient::attach_interlock,
interlock.rs::AttachedInterlock::tier).

```
tier(&self) -> u8
    0 bare interlock, 1 WaitCounter, 2 WaitTimer, 3 WaitCron, 4 WaitBarrier. Fixed for
    the handle's life; a name recreated later is a new entry, which this handle sees only
    as termination.
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

## WaitCounter (tier 1)

### Field semantics

| Word | Meaning | Written by | Read by |
|---|---|---|---|
| open_count | Target value on the watched word | Creator, CAS-max (`WaitCounter::wait_until`) | Daemon comparison |
| closed_count | Watched value at delivery | Daemon only (`registry::Registry::evaluate_all`) | Creator, attached view (`AttachedWaitCounter::completed_at`) |
| expiration_ns | Deadline, armed to `2 * timeout_ms` | Creator (`WaitCounter::wait_until` via `interlock_arm`), keepalive | Daemon |

### SDK: WaitCounter (creator handle)

Tier 1. Created through `create_wait_counter(name, watched_name, watched_word)`
(client.rs::AbacusClient::create_wait_counter). Registered with the keepalive at
construction using `DEFAULT_TOUCH_INTERVAL_MS` and `default_touch_ttl_ms(40)` = 200
(wait_counter.rs::WaitCounter::new).

```
wait_until(&self, target: u64, timeout_ms: u64) -> Result<WaitResult, SdkError>
    CAS-max loop raises open_count to target, never lowering it: the loop breaks when
    target <= current (wait_counter.rs::WaitCounter::wait_until). Then arms the TTL with
    interlock_arm(handle, timeout_nanos * 2) and futex-waits on closed_count against one
    absolute deadline, now + timeout_ms: a wake short of delivery (spurious, EINTR, or a
    closed_count change below the target) sleeps only for what remains, never a fresh
    timeout. Returns WaitResult { completed_at: closed_count, state } where state comes
    from classify_wake: Normal when closed == open, Overrun when closed > open
    (types.rs::classify_wake). Once the deadline passes with no delivery it returns
    WaitResult { completed_at, state: WaitState::Timeout }: a timeout is a WaitState, not
    an error. timeout_ms == 0 checks once and returns Timeout if nothing was delivered.
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

### SDK: AttachedWaitCounter

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

## WaitTimer (tier 2)

### Field semantics

| Word | Meaning | Written by | Read by |
|---|---|---|---|
| open_count | Target clock millisecond | Creator, set exactly by the timer's single waiter (`WaitTimer::wait_ms_with_margin`) | Daemon comparison against `clock.open_count` |
| closed_count | Clock millisecond at delivery | Daemon only | Creator (`WaitTimer::completed_at`) |
| expiration_ns | Fatal margin deadline | Creator (`interlock_arm` with `margin_ms`), keepalive | Daemon, creator |

### SDK: WaitTimer

Tier 2. Created through `create_wait_timer(name)`, which passes the clock handle, the
client's `TimeoutPolicy`, and the client's `min_fatal_margin_ms`
(client.rs::AbacusClient::create_wait_timer, wait_timer.rs::WaitTimer::new). Registered
with the keepalive at `DEFAULT_TOUCH_INTERVAL_MS` and TTL 200.

One waiter at a time: a wait started while another is in progress on the same timer returns
`SdkError::InvalidRequest` (wait_timer.rs::WaiterGuard). `peek`, `is_reaped`, and
`completed_at` may be called from any thread during a wait.

```
timeout_policy(&self) -> TimeoutPolicy
    The policy captured at construction (wait_timer.rs::WaitTimer::timeout_policy).
    Default: TimeoutPolicy::Abort (types.rs::TimeoutPolicy).

margin_for(&self, ms: u64) -> u64
    max(2 * ms, min_margin_ms), the doubling saturating at u64::MAX
    (wait_timer.rs::WaitTimer::margin_for).
    Default floor: MIN_FATAL_MARGIN_MS = 50 (types.rs::MIN_FATAL_MARGIN_MS), overridable
    per client with set_min_fatal_margin_ms.

wait_ms(&self, ms: u64) -> Result<WaitResult, SdkError>
    Calls wait_ms_with_margin(ms, self.margin_for(ms)) (wait_timer.rs::WaitTimer::wait_ms).

wait_ms_with_margin(&self, ms: u64, margin_ms: u64) -> Result<WaitResult, SdkError>
    Reads the clock, takes the timer's single waiter slot, sets open_count to exactly
    clock_now + ms (replacing any stale target a timed-out wait left; never over a
    SENTINEL), arms the TTL to margin_ms, then futex-waits on closed_count until
    classify_wake yields a state (wait_timer.rs::WaitTimer::wait_ms_with_margin).
    ms == 0 returns at once with WaitResult { completed_at: clock_now, state: Normal }
    and arms nothing.
    Errors: SdkError::InterlockReaped when the clock word is SENTINEL, when the timer's
    open_count or closed_count is SENTINEL, when expiration_ns is SENTINEL, or when
    ms == 0 and the handle is already terminated; SdkError::InvalidRequest { message:
    "margin_ms (N) must exceed wait (M)" } when margin_ms <= ms; SdkError::InvalidRequest
    when another wait is in progress on this timer;
    SdkError::InterlockReaped from interlock_arm on a SENTINEL expiration.
    On the margin expiring: TimeoutPolicy::Error returns Err(SdkError::DeliveryTimeout);
    TimeoutPolicy::Abort prints an "abacus: DeliveryTimeout" diagnostic to stderr and calls
    std::process::abort (wait_timer.rs::WaitTimer::on_timeout).

wait_until(&self, timestamp_ms: u64) -> Result<WaitResult, SdkError>
    Reads the clock once and waits timestamp_ms.saturating_sub(clock_now) from that same
    reading, with margin_for of that wait, so the target is exactly timestamp_ms and a
    past timestamp returns at once as a zero wait (wait_timer.rs::WaitTimer::wait_until).
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

## WaitCron (tier 3)

### Field semantics

| Word | Meaning | Written by | Read by |
|---|---|---|---|
| open_count | Next grid line, in clock milliseconds | Daemon (re-arm), registry at create | Daemon comparison |
| closed_count | Clock millisecond at last fire | Daemon only | Creator (`WaitCron::wait`, `completed_at`) |
| expiration_ns | Deadline | Keepalive | Daemon |

### SDK: WaitCron

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
    the loop, when expiration_ns is SENTINEL, or when expiration_ns is in the past; likewise
    when the daemon clock's expiration_ns is SENTINEL or in the past.
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
`next_grid_line` returns `None` on a zero interval or on overflow; at re-arm, `None` reaps the
cron.

## WaitBarrier (tier 4)

### Field semantics

| Word | Meaning | Written by | Read by |
|---|---|---|---|
| open_count | Arm generation; starts at 1, incremented to re-arm | Registry at create, creator (`WaitBarrier::rearm`) | Daemon |
| closed_count | Set equal to open_count when all conditions hold | Daemon only | Creator (`WaitBarrier::wait`) |
| expiration_ns | Deadline | Keepalive | Daemon |

### SDK: WaitBarrier

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
    the clock word is SENTINEL, when expiration_ns is SENTINEL, when expiration_ns is
    in the past, or when the daemon clock's expiration_ns is SENTINEL or in the past.
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

## Clock (reserved, id 0)

### Field semantics

| Word | Meaning | Written by | Read by |
|---|---|---|---|
| open_count | Current monotonic millisecond | Daemon only (`registry::Registry::tick`) | Everyone, read-only mapping |
| closed_count | Daemon start millisecond | Daemon at registry construction | Everyone |
| expiration_ns | Refreshed to now + 100 ms each tick | Daemon (`registry::Registry::refresh_clock_expiration`) | Nobody else |

### SDK: ClockHandle

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

## Keepalive and TTL

### TTL rules

| Rule | Contract | Enforced in |
|---|---|---|
| Expiration is absolute | `expiration_ns` is a `CLOCK_MONOTONIC` nanosecond deadline | `clock::monotonic_now_nanos`, `interlock::interlock_arm` |
| Arming is monotonic forward | `interlock_arm` CAS-maxes `max(current, now + ttl)`; a shorter TTL is a no-op | `interlock::interlock_arm` |
| Arming a terminated interlock fails | `expiration_ns == SENTINEL` returns `Condition::InterlockReaped` and leaves the sentinel intact | `interlock::interlock_arm` |
| Arming never terminates | The deadline `now + ttl` saturates and is capped at `SENTINEL - 1`, so a huge TTL never writes `SENTINEL` | `interlock::interlock_arm` |
| Creation TTL | 100 ms (`CREATION_TTL_NANOS`) | `interlock::interlock_create` |
| Clock TTL | 100 ms, refreshed every tick | `registry::CLOCK_TTL_NANOS`, `registry::Registry::refresh_clock_expiration` |
| Reap condition | `expiration_ns <= now` (`expiration_alive` is strict greater-than) | `clock::expiration_alive`, `registry::Registry::evaluate_all` |
| Keepalive interval | `DEFAULT_TOUCH_INTERVAL_MS` = 40 ms | `types::DEFAULT_TOUCH_INTERVAL_MS`, `interlock::Interlock::new` |
| Keepalive TTL | `max(interval * 5, MIN_TOUCH_TTL_MS)`, floor 200 ms | `types::default_touch_ttl_ms`, `types::MIN_TOUCH_TTL_MS` |
| ProcessClock TTL | default `default_touch_ttl_ms(40)` = 200 ms; per client via `set_process_clock_ttl_ms`, raised to at least `2 * interval` | `touch::Keepalive::bind_process`, `touch::Keepalive::set_process_ttl_ms` |
| Registration TTL floor | An interval of 0 is clamped to 1 ms first; the TTL is then raised to at least `2 * interval` | `touch::Keepalive::register_inner` |
| One thread per client | A single keepalive thread serves every registered handle | `touch::Keepalive::register_inner`, `touch::run` |
| Keepalive stop | Dropping or stopping a `TouchHandle` deregisters it; the interlock is then reaped after at most its remaining TTL | `touch::TouchHandle::drop`, `touch::Keepalive::deregister` |
| Reaped detection | A failed arm sets the entry's reaped flag and removes it from the keepalive set | `touch::run`, `touch::TouchHandle::is_reaped` |
| Timer margin | `margin = max(2 * wait_ms, min_fatal_margin_ms)`, default floor `MIN_FATAL_MARGIN_MS` = 50 ms | `wait_timer::WaitTimer::margin_for`, `types::MIN_FATAL_MARGIN_MS` |
| Explicit margin | `wait_ms_with_margin` requires `margin_ms > ms`, else `SdkError::InvalidRequest` | `wait_timer::WaitTimer::wait_ms_with_margin` |
| Counter TTL | `wait_until(target, timeout_ms)` arms `2 * timeout_ms` | `wait_counter::WaitCounter::wait_until` |
| Daemon clock lapse | A wait that finds the daemon clock's `expiration_ns` at `SENTINEL` or in the past treats the daemon as dead and returns `InterlockReaped` | `handle_ops::wait_word`, `wait_cron::WaitCron::wait`, `wait_barrier::WaitBarrier::wait` |

### SDK: Keepalive

One `Keepalive` per `AbacusClient`, created in `connect_with_timeout` and cloned into
every handle it makes (client.rs::AbacusClient::connect_with_timeout,
touch.rs::Keepalive). It is a single background thread shared by all registered handles.

```
Keepalive::new() -> Self
    Empty registry, no thread yet (touch.rs::Keepalive::new). Also the Default impl.

register(&self, handle: InterlockHandle, interval_ms: u64, ttl_ms: u64)
    -> Result<TouchHandle, SdkError>
    Clamps an interval_ms of 0 to 1, then raises ttl_ms to at least 2 * interval_ms, arms
    the handle immediately with that TTL, spawns the thread named "abacus-keepalive" if it
    is not already running, and adds an entry due at now + interval
    (touch.rs::Keepalive::register_inner).
    Errors: SdkError::InterlockReaped if the immediate arm finds the interlock terminated;
    SdkError::KeepaliveSpawnFailed { message } if the thread cannot be started. Nothing is
    registered on error.

register_with_clock(&self, handle: InterlockHandle, clock: InterlockHandle,
    interval_ms: u64, ttl_ms: u64) -> Result<TouchHandle, SdkError>
    Same, plus the entry carries a clock handle: at registration and on every tick the
    registered handle's open_count becomes max(its current value, the clock's open_count),
    and a handle already at SENTINEL is never written (touch.rs::stamp_last_seen,
    touch.rs::Keepalive::register_with_clock, touch.rs::run).
    Errors: as register, plus InterlockReaped if the registration stamp finds open_count
    at SENTINEL.

thread_running(&self) -> bool
registered(&self) -> usize

set_priority(&self, priority: KeepalivePriority) -> Result<(), SdkError>
    Sets the thread's scheduling with pthread_setschedparam: applied at once to a running
    thread and kept for any thread started later (touch.rs::Keepalive::set_priority,
    touch.rs::apply_priority). The thread's CPU affinity is not changed: it keeps the
    affinity it inherited from the thread that started it.
    Errors: SdkError::InvalidRequest for KeepalivePriority::Fifo(p) with p outside 1 to
    99; SdkError::KeepalivePriorityFailed { errno } on a kernel refusal, leaving the
    previous priority in force (no silent fallback). With no thread running the priority
    is only stored; the call that starts the thread applies it and reports a refusal as
    KeepalivePriorityFailed, with the thread left running at its inherited scheduling and
    the stored priority returned to Normal (touch.rs::spawn_thread).

priority(&self) -> KeepalivePriority
    The scheduling last set with set_priority (touch.rs::Keepalive::priority).
    Default: KeepalivePriority::Normal.

set_process_ttl_ms(&self, ttl_ms: u64) -> Result<(), SdkError>
    Sets the TTL written on the bound process clock, raised to at least two touch
    intervals as register does, and arms the clock with it at once; a shorter TTL takes
    effect from the first touch whose now + ttl passes the current deadline
    (touch.rs::Keepalive::set_process_ttl_ms).
    Errors: SdkError::InterlockReaped when no process clock is bound (never bound, or
    dropped after its death under TimeoutPolicy::Error) or the arm finds it terminated;
    the TTL is then unchanged.

process_ttl_ms(&self) -> Option<u64>
    The TTL written on the process clock in milliseconds; None when no process clock is
    bound (touch.rs::Keepalive::process_ttl_ms).
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
`thread_running = false` and returns (touch.rs::run). Every exit of `run`, a panic
included, clears `thread_running` (touch.rs::RunningGuard), so the next registration
starts a new thread, which receives the stored priority.

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

A read-only view of the client's liveness beacon, created at connect
(process_clock.rs::ProcessClock, client.rs::AbacusClient::connect_with_timeout).
Construction stores the current daemon clock millisecond into both `closed_count` and
`open_count`, refusing with `InterlockReaped` if either word already reads `SENTINEL`; the
keepalive touches it from then on
(process_clock.rs::ProcessClock::new, touch.rs::run).

The keepalive owns process liveness. On every iteration, before walking entries, when a
ProcessClock is bound:

1. If the Abacus clock's `expiration_ns` has lapsed: die with `DaemonClockLapsed`.
2. If the ProcessClock is due: a failed `interlock_arm` or a `false` from
   `stamp_last_seen` dies with `ProcessClockReaped`; otherwise set its next due time.

Dying under `Abort`: `eprintln!` exactly one of these lines, then `std::process::abort()`:
- `abacus: ProcessClockReaped: process clock "<name>" (id <id>) was reaped; aborting`
- `abacus: DaemonClockLapsed: the Abacus clock expired at <exp> ns, now <now> ns; aborting`

Dying under `Error`: record the cause as the client's `Liveness` (`ProcessClockReaped` or
`DaemonClockLapsed`, read through `AbacusClient::liveness`), set the process clock to None,
and carry on with the other entries. The daemon's cascade reaps every owned interlock, so
every handle reports `InterlockReaped` from shared memory.

`set_timeout_policy` on `AbacusClient` forwards to the keepalive, so the liveness policy
takes effect at once. `connect` starts the keepalive under `Abort`; a death the keepalive
observes before `set_timeout_policy(Error)` runs still aborts.

```
uptime_ms(&self) -> i64
    open_count minus closed_count: last_seen minus start
    (process_clock.rs::ProcessClock::uptime_ms, handle_ops.rs::value).

last_seen_ms(&self) -> u64
    Acquire load of open_count: the clock value at the last keepalive tick
    (process_clock.rs::ProcessClock::last_seen_ms).

start_time_ms(&self) -> u64
    closed_count, fixed at construction (handle_ops.rs::completed_at).

name(&self) -> &str
    The ProcessClock name passed to connect (process_clock.rs::ProcessClock::name).

dependencies(&self) -> &[String]
    The dependency names passed to connect, in order. Names as given: the daemon
    resolved each to whatever held that name at create time, and this clock dies with
    those entries (process_clock.rs::ProcessClock::dependencies).

id(&self) -> u64
    The registry id assigned at creation (process_clock.rs::ProcessClock::id).

is_reaped(&self) -> bool
    True when any of the three words is SENTINEL
    (process_clock.rs::ProcessClock::is_reaped, interlock::interlock_is_terminated).
```

## Limits

| Limit | Value | Enforced in | Error to caller |
|---|---|---|---|
| Max live interlocks (excludes clock) | `DEFAULT_MAX_INTERLOCKS` = 4096, overridable by `--max-interlocks` | `registry::Registry::create`, `main::parse_args` | `Condition::InvalidRequest` containing "limit reached" |
| Max name length | `MAX_NAME_LEN` = 255 bytes | `registry::validate_name` | `Condition::InvalidRequest` containing "name too long" |
| Min name length | 1 byte | `registry::validate_name` | `Condition::InvalidRequest` containing "must not be empty" |
| Max frame payload | `MAX_MESSAGE_SIZE` = 4096 bytes | `codec::frame`, `framing::FrameReader::next_frame`, `client::ClientConn::recv_response` | `ProtocolFault::FrameTooLarge`; daemon answers `ERR_INVALID_REQUEST` and drops the connection |
| Max encoded string | 65535 bytes | `codec::encode_string_into` | `ProtocolFault::FrameTooLarge` on encode |
| Max descriptors per message | `MAX_FDS_PER_MESSAGE` = 1 | `fdpass::send_frame_with_fds`, `fdpass::recv_prefix_with_fds` | `ProtocolFault::FrameTooLarge` on send; `ProtocolFault::Truncated` on `MSG_CTRUNC` |
| Listen backlog | `LISTEN_BACKLOG` = 64 | `transport::Server::create` | connect fails at the kernel |
| Descriptor budget | One memfd per live interlock plus one per connection; exhaustion surfaces as `AllocationStep::MemfdCreate` or `AllocationStep::Dup` | `interlock::memfd_create`, `interlock::interlock_dup_fd` | `Error { ERR_ALLOCATION_FAILED }` |

Reusing an existing name does not consume a new live slot: the limit check is skipped when
the name is already present (`registry::Registry::create`).

## Trust model and permissions

### Trust model

| Subject | Trusted | Not trusted | Enforced in |
|---|---|---|---|
| Socket peers | Any process that can open the socket may create, attach, and terminate any name | Peer identity, peer intent | `daemon::handle_request` (no credential check) |
| Socket access control | Filesystem mode and group on the socket file | Anything else | `transport::Server::apply_socket_options`, `daemon::DaemonConfig::new` (default `0o660`) |
| Frame content | Nothing: version, tag, lengths, and UTF-8 are checked before use; tier, watched word, interval, and thresholds are validated in the registry | Raw numeric fields from the codec | `codec::decode_request`, `registry::Registry::create` |
| Shared memory | The daemon reads client-written words but treats any `SENTINEL` or lapsed expiration as termination rather than as a fault | Word values written by peers | `registry::Registry::evaluate_all` |
| memfd integrity | Backing size is fixed by seals, so a hostile `ftruncate` cannot shrink a mapping under the daemon | Attacher intent | `interlock::seal` (`F_SEAL_SHRINK \| F_SEAL_GROW \| F_SEAL_SEAL`) |
| Clock integrity | The clock memfd additionally carries `F_SEAL_FUTURE_WRITE`, so attachers cannot map it writable | Attacher intent | `interlock::interlock_create_clock`, `interlock::interlock_map_clock` |

Straight razor: the daemon hands out the same kind of descriptor for every tier, and except
for the clock, every handed-out descriptor is mapped `PROT_READ | PROT_WRITE`, including
`attach_interlock` of any tier and of a ProcessClock (`interlock::interlock_map`, `interlock_map_counter`,
`interlock_map_timer`, `interlock_map_cron`, `interlock_map_barrier`). Per-word write
discipline is a property of the SDK handle type, not of page protection. The only
mmap-enforced restriction is the clock, which is mapped `PROT_READ`
(`interlock::interlock_map_clock`).

### Permission model (SDK-enforced)

Rows are handle types; columns are the three words plus lifecycle operations. "type" means
the method simply does not exist on the type; "mmap" means the page protection forbids it.

| Handle | open_count | closed_count | expiration_ns | free | touch control |
|---|---|---|---|---|---|
| `Interlock` (creator, tier 0) | read/write (`open`) | read/write (`close`) | read/write (`touch`) | `free` stamps expiration | `start_touch_thread`, `stop_touch_thread` |
| `AttachedInterlock` | read/write (`open`) | read/write (`close`) | read only (`expiration_ns`); no `touch` (type) | `free` stamps open_count with `SENTINEL` | none (type) |
| `WaitCounter` (creator) | read/write via CAS-max target (`wait_until`) | read only (`completed_at`); no `close` (type) | write via `touch` and `wait_until` arming | `free` stamps expiration | keepalive-registered |
| `AttachedWaitCounter` | read only (`peek`) | read only (`completed_at`) | not exposed (type) | none (type) | none (type) |
| `WaitTimer` (creator) | read/write via the single waiter's exact target | read only (`completed_at`); no `close` (type) | write via `touch` and margin arming | `free` stamps expiration | keepalive-registered |
| `WaitCron` (creator) | read only (`peek`) | read only (`completed_at`) | keepalive only | `free` stamps expiration | keepalive-registered |
| `WaitBarrier` (creator) | read/write via `rearm` (+1) | read only (`completed_at`) | keepalive only | `free` stamps expiration | keepalive-registered |
| `ProcessClock` (read-only view) | read (`last_seen_ms`); written by keepalive from the daemon clock | read (`start_time_ms`) | keepalive only | none (type) | keepalive-managed (bound at connect) |
| `ClockHandle` | read only (mmap) | read only (mmap) | read only (mmap) | none (type) | none (type) |

Enforced in: `interlock::Interlock`, `interlock::AttachedInterlock`,
`interlock::AttachedWaitCounter`, `interlock::ClockHandle`, `wait_counter::WaitCounter`,
`wait_timer::WaitTimer`, `wait_cron::WaitCron`, `wait_barrier::WaitBarrier`,
`process_clock::ProcessClock`.

Only the clock is mmap-enforced (`interlock::interlock_map_clock` maps `PROT_READ`, and the
clock memfd carries `F_SEAL_FUTURE_WRITE` so a writable map fails with an
`AllocationFailed { step: Mmap }`). Every other restriction in the table is type-enforced.

Sentinel safety on increments: `handle_ops::increment` never writes a word whose old value
is `SENTINEL`, and terminates (stores `SENTINEL` into) a word an addition would carry onto or
past `SENTINEL`; either way it wakes waiters and returns `InterlockReaped`. A terminated
interlock cannot be resurrected by arithmetic.

## Interlock shape and wire protocol

### Interlock shape

| Property | Value | Enforced in |
|---|---|---|
| Layout | `#[repr(C)]` struct of three `AtomicU64` | `interlock::Interlock` |
| Size | 24 bytes | `interlock::INTERLOCK_SIZE`, compile-time `assert!(size_of::<Interlock>() == 24)` |
| Offsets | `open_count` 0, `closed_count` 8, `expiration_ns` 16 | `interlock::Interlock` field order; asserted in `src/tests/interlock.rs` |
| Backing | memfd with `MFD_CLOEXEC \| MFD_ALLOW_SEALING`, truncated to 24 bytes, sealed | `interlock::memfd_create`, `interlock::ftruncate`, `interlock::seal` |
| Mapping | `MAP_SHARED` | `interlock::mmap_interlock` |
| Sentinel | `SENTINEL` = `u64::MAX` in any word means terminated | `interlock::SENTINEL`, `interlock::interlock_is_terminated` |
| Endianness | Little-endian only; futex addresses the low 32 bits | `clock::futex_addr` with compile-time endian assert, `clock::futex_word` |

### Wire protocol v2

Frame layout:

| Segment | Bytes | Content |
|---|---|---|
| Length prefix | 4 | payload length, little-endian u32, `LENGTH_PREFIX_BYTES` |
| Version | 1 | `PROTOCOL_VERSION` = 2 |
| Tag | 1 | request or response discriminant |
| Body | variable | tag-specific, see below |

A v1 frame (version byte 1) is rejected as `ProtocolFault::UnsupportedVersion { version: 1 }`.

Strings are a little-endian u16 byte length followed by UTF-8 bytes
(`codec::encode_string_into`, `codec::Cursor::string`).

| Tag | Name | Body field order |
|---|---|---|
| `0x01` | CreateInterlock | name (string), tier (u8), owner flag (u8: 0 absent, 1 present), then when present owner name (string) and owner id (u64 LE), then dependency count (u16 LE) and that many names (string), then: tier 1 -> watched_name (string), watched_word (u8); tier 3 -> interval_ns (u64 LE); tier 4 -> count (u16 LE) then count times [name (string), word (u8), threshold (u64 LE)]; tiers 0, 2, and unknown -> nothing |
| `0x02` | AttachInterlock | name (string) |
| `0x81` | Created | id (u64 LE) |
| `0x82` | Attached | id (u64 LE), tier (u8) |
| `0x88` | Error | code (u8), message (string) |

Owner flag decoding: a value other than 0 or 1 is `ProtocolFault::InvalidFlag { field: "owner", value }`. Dependency count over `u16::MAX` encodes to `ProtocolFault::FrameTooLarge`, as conditions do. Dependency `Vec` is pre-sized with `min(count, MAX_MESSAGE_SIZE / 2)` to bound allocation from untrusted input.

Enforced in `codec::encode_request`, `codec::encode_response`, `codec::decode_request`,
`codec::decode_response`. Descriptors travel as `SCM_RIGHTS` ancillary data attached to the
frame's first bytes, never inside the payload (`fdpass::send_frame_with_fds`,
`fdpass::recv_prefix_with_fds`).

Daemon error codes: `ERR_INTERLOCK_REAPED` 0x01 (the daemon returns it when a create's
owner is gone or replaced), `ERR_INTERLOCK_NOT_FOUND` 0x02,
`ERR_ALLOCATION_FAILED` 0x03, `ERR_INVALID_REQUEST` 0x04 (`codec`, mapped in
`client::daemon_error`).

## Derived properties and states

| Property | Formula | Defined in |
|---|---|---|
| value | `(open_count as i64).wrapping_sub(closed_count as i64)` | `handle_ops::value`, `types::interlock_state` |
| ttl | `expiration_ns - now`; alive when `expiration_ns > now` | `clock::expiration_alive` |
| terminated | any word equals `SENTINEL` | `interlock::interlock_is_terminated` |
| uptime (clock) | `open_count - closed_count` | `interlock::ClockHandle::uptime_ms` |

`types::interlock_state(open, closed, expiration_ns, clock_ns)` evaluates in this order:

| Order | Condition | State |
|---|---|---|
| 1 | any of open, closed, expiration equals `SENTINEL` | Expired |
| 2 | `clock_ns >= expiration_ns` | Expired |
| 3 | value < 0 | Overrun |
| 4 | value > 0 | Open |
| 5 | value == 0 | Closed |

Enforced in `types::interlock_state`; surfaced by `Interlock::state` and
`AttachedInterlock::state` via `handle_ops::state`.

## Wake outcomes

`types::classify_wake(open, closed)`:

| Condition | Result |
|---|---|
| `closed == open` | `Some(WaitState::Normal)` |
| `closed > open` | `Some(WaitState::Overrun)` |
| `closed < open` | `None` (no delivery yet, keep waiting) |

Sentinel is checked before classification: every wait loop loads both counters and returns
`Err(SdkError::InterlockReaped)` if either equals `SENTINEL`, before calling
`classify_wake` (`wait_counter::WaitCounter::wait_until`,
`wait_timer::WaitTimer::wait_ms_with_margin`). The expiration word is also checked:
`SENTINEL` or a lapsed deadline yields `InterlockReaped` (`handle_ops::wait_word`,
`wait_barrier::WaitBarrier::wait`, `wait_cron::WaitCron::wait`), and the same three loops
treat a daemon clock expiration at `SENTINEL` or in the past as a dead daemon.

| Waiter | Timeout result |
|---|---|
| `WaitCounter::wait_until` | `Ok(WaitResult { state: Timeout, completed_at })` once `timeout_ms` has elapsed since the call with no delivery; wakes short of delivery do not restart the timeout |
| `WaitTimer::wait_ms*` | `on_timeout`: `TimeoutPolicy::Error` yields `Err(SdkError::DeliveryTimeout)`; `TimeoutPolicy::Abort` prints a diagnostic and calls `std::process::abort` |
| `Interlock::wait_open_for` and siblings | `Ok(None)` |
| `WaitCron::wait`, `WaitBarrier::wait` | no timeout; they return only on delivery or reap |

`WaitCron` classifies independently: a fire whose `closed_count` is a multiple of the
interval is `Normal`, otherwise `Overrun` (`wait_cron::WaitCron::wait`).

`WaitTimer::wait_ms(0)` returns `Normal` with `completed_at` set to the current clock
millisecond without arming anything, unless the handle is already terminated
(`wait_timer::WaitTimer::wait_ms_with_margin`).

## Termination

| Path | What it stamps | What peers observe | Daemon's next pass |
|---|---|---|---|
| Creator `free` (`Interlock::free`, `WaitCounter::free`, `WaitTimer::free`, `WaitCron::free`, `WaitBarrier::free`) | `expiration_ns = SENTINEL`, futex-wakes both counters | `is_reaped()` true, `state()` Expired, `touch` returns `InterlockReaped`, waiters return `InterlockReaped` | `evaluate_all` sees the sentinel, calls `interlock_reap`, removes the slot and the name |
| Attacher `free` (`AttachedInterlock::free`) | `open_count = SENTINEL`, futex-wakes both counters | Same as above for every mapper | Same: sentinel in any word reaps |
| Handle drop | Nothing is stamped; the `TouchHandle` deregisters | The interlock keeps its current expiration and lapses naturally | Reaped when `expiration_ns <= now` |
| TTL lapse | Nothing by the owner | Waiters see a lapsed expiration and return `InterlockReaped` | `expiration_alive` false, `interlock_reap`, slot removed |
| Daemon reap (`interlock::interlock_reap`) | All three words set to `SENTINEL`, both counters futex-woken | `interlock_is_terminated` true from any mapper | Slot pushed to the free list, name removed from the index |
| Name replacement | Old entry passed through `remove_slot`, which calls `interlock_reap` | Old handle sees all-sentinel words | New entry occupies a (possibly reused) slot with a strictly larger id |

The mapping and its descriptor stay valid after termination: the page is freed only when the
last `InterlockHandle` clone drops (`interlock::InterlockRegion::drop` calls `munmap`).
`interlock_free` writes only the expiration word, so consumers must treat `SENTINEL` in any
word as terminal (`interlock::interlock_is_terminated`).

Every creator `free` first drops its `TouchHandle`, deregistering the entry from the
keepalive before stamping (interlock.rs::Interlock::free,
wait_counter.rs::WaitCounter::free, wait_timer.rs::WaitTimer::free,
wait_cron.rs::WaitCron::free, wait_barrier.rs::WaitBarrier::free).

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

Daemon shutdown: `SIGTERM` and `SIGINT` set the stop flag; the loop exits, the `Server` drops,
the socket file is removed, and the process exits 0 (`main::on_stop_signal`,
`main::install_stop_handlers`, `transport::Server::drop`). A restart comes back with an empty
registry; old mappings remain valid but nothing advances them, so waiters discover the
restart through their own TTL or margin.

## Daemon contract (1 ms loop)

Each cycle, in order (`daemon::daemon_run_with`):

| Step | Action | Enforced in |
|---|---|---|
| 1 | Check the stop flag; break if set | `daemon::daemon_run_with` |
| 2 | `ppoll` on the listener plus every client, timeout = `next_due - now` | `daemon::daemon_run_with`, `daemon::rebuild_pollfds` |
| 3 | Accept every pending connection, set nonblocking | `transport::Server::try_accept` |
| 4 | For readable clients, read available bytes and answer every complete request | `daemon::service_client` |
| 5 | Remove clients that closed, faulted, or became unwritable | `daemon::service_client`, `daemon::is_connection_error` |
| 6 | If `now >= next_due`, run `registry.tick(now)` and recompute `next_due` | `daemon::daemon_run_with` |
| 7 | Rebuild the pollfd set if the client set changed | `daemon::rebuild_pollfds` |

`registry::Registry::tick` stores `now_ns / NANOS_PER_MS` into `clock.open_count`, refreshes
the clock expiration, runs `evaluate_all`, then futex-wakes the clock word. The clock is
current before anything is evaluated against it: after a stall longer than its TTL,
evaluating first would reap every clock dependent. A clock that cannot be re-armed aborts
the daemon (`registry::Registry::refresh_clock_expiration`).

`registry::Registry::evaluate_all` visits every occupied slot in order and, per entry:

| Check | Action |
|---|---|
| any word is `SENTINEL` | `interlock_reap`, mark slot dead |
| `!expiration_alive(exp, now_ns)` | `interlock_reap`, mark slot dead |
| any `depends_on` target not `target_alive` (gone, id mismatch, any word `SENTINEL`, or TTL lapsed) | `interlock_reap`, mark slot dead (the owner or a dependency died) |
| tier Interlock | nothing further |
| tier WaitTimer, value > 0 and `clock_now_ms >= open_count` | store `clock_now_ms` into closed_count, futex wake |
| tier WaitCron, value > 0 and `clock_now_ms >= open_count` | store `clock_now_ms` into closed_count, futex wake, re-arm open_count to `next_grid_line(clock_now_ms, interval_ms)`; on overflow, reap |
| tier WaitCounter, target not `target_alive` or watched word is `SENTINEL` | `interlock_reap`, mark slot dead |
| tier WaitCounter, value > 0 and watched >= open_count | store the watched value into closed_count, futex wake |
| tier WaitBarrier, any condition target not `target_alive` | `interlock_reap`, mark slot dead |
| tier WaitBarrier, value > 0 and every watched value >= its threshold | store open_count into closed_count, futex wake |

Watcher lifetime: a watcher is identified by `(slot, id)`. A recreated name occupies a new
id, so a watcher never retargets; it is reaped (`registry::Target::Slot`,
`registry::Registry::watched_value`).

Because `target_alive` reads the target's TTL, a target whose TTL has lapsed is dead to its
dependents and watchers in the same pass that reaps it, whatever their slot order.

Missed cron lines: the daemon stamps the current clock millisecond and re-arms to the next
grid line after that millisecond. No burst, no catch-up fires
(`registry::Registry::evaluate_all` cron arm, `registry::next_grid_line`).

The clock is never evaluated and never reaped: it is held outside the slot table
(`registry::Registry` field `clock`).

### Loop timing

| Property | Contract | Enforced in |
|---|---|---|
| Sleep primitive | `ppoll` with a nanosecond `timespec` | `daemon::daemon_run_with`, `clock::timespec_from_nanos` |
| Timer slack | `prctl(PR_SET_TIMERSLACK, 1)` at startup | `daemon::set_timer_slack` |
| Anchor | `anchor` = the first absolute millisecond boundary of `CLOCK_MONOTONIC` after startup, which is also the first due time; ticks land on absolute boundaries, so the clock word advances by exactly one per on-time tick | `daemon::first_tick_boundary`, `daemon::daemon_run_with` |
| next_due | `anchor + ((now - anchor) / NANOS_PER_MS + 1) * NANOS_PER_MS` | `daemon::daemon_run_with` |
| Catch-up | A late tick advances `next_due` past all missed boundaries; exactly one evaluation runs | same expression |
| EINTR | `ppoll` returning `EINTR` continues the loop; any other error is `StartupError::EventLoopFailed` | `daemon::daemon_run_with` |

## Errors

### `Condition` (`abacus_core::error::Condition`)

| Variant | Produced by |
|---|---|
| `InterlockReaped` | `interlock::interlock_arm` on a sentinel expiration |
| `InterlockNotFound { name }` | `registry::Registry::attach`, `registry::Registry::resolve`, `registry::Registry::create_with_fd` (a watched, condition, or dependency target that is not alive) |
| `AllocationFailed { step, errno }` | `interlock::memfd_create`, `ftruncate`, `seal`, `mmap_interlock`, `interlock_dup_fd` |
| `InvalidRequest { message }` | `registry::validate_name` (on create and attach), `registry::Registry::create_with_fd`, `registry::WatchedWord::from_wire` |

`AllocationStep` variants: `MemfdCreate`, `Ftruncate`, `Seal`, `Mmap`, `Dup`
(`error::AllocationStep`).

### `ProtocolFault` (`abacus_core::error::ProtocolFault`)

| Variant | Produced by |
|---|---|
| `Truncated { needed, have }` | `codec::Cursor::take`, `framing::read_exact`, `fdpass::recv_prefix_with_fds` on `MSG_CTRUNC`, `client::extract_single_fd` |
| `FrameTooLarge { len, max }` | `codec::frame`, `codec::encode_string_into`, `framing::FrameReader::next_frame`, `client::ClientConn::recv_response`, `fdpass::send_frame_with_fds` |
| `UnsupportedVersion { version }` | `codec::check_version` |
| `UnknownTag { tag }` | `codec::decode_request`, `codec::decode_response` |
| `InvalidUtf8` | `codec::Cursor::string` |
| `MissingField { field }` | `codec::encode_request` for tiers 1, 3, 4 |
| `InvalidFlag { field, value }` | `codec::decode_request` when the owner flag is not 0 or 1 |
| `UnexpectedFd` | `client::ClientConn::recv_response` when descriptors exceed `expected_fd_count` |

### `TransportError` (`abacus_core::error::TransportError`)

| Variant | Produced by |
|---|---|
| `Protocol { fault }` | any decode or framing fault crossing the transport |
| `Io { operation, errno }` | `transport::Server::create`, `Server::try_accept`, `Connection::set_nonblocking`, `framing::write_all`, `framing::FrameReader::fill`, `fdpass::sendmsg_retry`, `fdpass::recvmsg_retry`, `client::ClientConn::connect` |
| `ConnectionClosed` | `framing::FrameReader::fill` and `framing::read_exact` on EOF, `fdpass::recv_prefix_with_fds` on zero-length receive |
| `SocketPathOccupied { path, live_daemon }` | `transport::Server::create` |

`IoOperation` variants: `Bind`, `Listen`, `Accept`, `Connect`, `SendMsg`, `RecvMsg`, `Read`,
`Write`, `Stat`, `Unlink`, `SetNonBlocking`, `SetTimeout`, `Chmod`, `Chown`, `Poll`
(`error::IoOperation`).

### `StartupError` (`abacus_core::error::StartupError`)

| Variant | Produced by |
|---|---|
| `Transport(TransportError)` | `daemon::daemon_run_with` when `Server::create` fails |
| `Allocation(Condition)` | `daemon::daemon_run_with` when `Registry::with_limit` fails |
| `EventLoopFailed { errno }` | `daemon::daemon_run_with` when `ppoll` fails with anything but `EINTR` |

The binary prints `abacus: fatal: {e}` and exits 1 on any `StartupError`; a bad argument
exits 2; `--help` and `--version` exit 0 (`main::main`, `main::parse_args`).

### `SdkError` (`abacus_client::client::SdkError`)

client.rs::SdkError. Derives `Debug, Clone, PartialEq, Eq`, implements `Display` and
`std::error::Error`. `Result<T>` is `std::result::Result<T, SdkError>`
(client.rs::Result).

| Variant | Produced by |
|---|---|
| `InterlockReaped` | Increment from or across SENTINEL (handle_ops.rs::increment); any wait loop observing a SENTINEL word or a lapsed `expiration_ns` (handle_ops.rs::wait_word, wait_counter.rs, wait_timer.rs, wait_cron.rs, wait_barrier.rs); `interlock_arm` on a SENTINEL expiration (interlock.rs::interlock_arm); daemon reply code `ERR_INTERLOCK_REAPED` (client.rs::daemon_error) |
| `InterlockNotFound { name }` | Daemon reply code `ERR_INTERLOCK_NOT_FOUND`, with the reply message carried as `name` (client.rs::daemon_error) |
| `AllocationFailed { message }` | Daemon reply code `ERR_ALLOCATION_FAILED` (client.rs::daemon_error); `Condition::AllocationFailed` rendered to a string (client.rs::From<Condition>) |
| `InvalidRequest { message }` | `attach_interlock("clock")` (client.rs::AbacusClient::attach_interlock); `create_wait_cron` with `interval_ms == 0` or an `interval_ms` whose nanoseconds overflow (client.rs::AbacusClient::create_wait_cron, client.rs::checked_interval_ns); `WaitRace::wait` over zero counters (wait_race.rs::WaitRace::wait); an out-of-range `KeepalivePriority::Fifo` (touch.rs::Keepalive::set_priority, client.rs::AbacusClient::set_keepalive_priority); `wait_ms_with_margin` with `margin_ms <= ms`, or a second concurrent wait on one WaitTimer (wait_timer.rs::WaitTimer::wait_ms_with_margin); daemon reply code `ERR_INVALID_REQUEST` (client.rs::daemon_error) |
| `DeliveryTimeout` | `WaitTimer` margin expiry under `TimeoutPolicy::Error` (wait_timer.rs::WaitTimer::on_timeout) |
| `Transport(TransportError)` | Connect, timeout set, frame send, frame receive, decode, and descriptor-count faults (client.rs::ClientConn); `extract_single_fd` when the reply carries a count other than one (client.rs::extract_single_fd) |
| `MmapFailed { message }` | The mapping function returns `Condition`, rendered to a string (client.rs::map_handle) |
| `UnexpectedResponse { message }` | A reply that is neither the expected `Created`/`Attached` nor `Error` (client.rs::AbacusClient::do_create, client.rs::do_attach); an unrecognized daemon error code, message `"unknown daemon error 0x{code:02x}: {message}"` (client.rs::daemon_error) |
| `DependencyTimeout { waited, missing }` | `connect_waiting` exceeded `max_wait` without success; `missing` is the dependency name or socket path (client.rs::AbacusClient::connect_waiting) |
| `KeepaliveSpawnFailed { message }` | The keepalive thread could not be started (for example `EAGAIN` at the thread limit); nothing was registered. From `Keepalive::register`, `register_with_clock`, `Interlock::start_touch_thread`, every `create_*` (through `Interlock::new`, `WaitCounter::new`, `WaitTimer::new`, `WaitCron::new`, `WaitBarrier::new`), and `connect`, which frees its new ProcessClock first (touch.rs::spawn_thread, client.rs::bind_or_free) |
| `KeepalivePriorityFailed { errno }` | The kernel refused a keepalive scheduling change (`pthread_setschedparam`), for example `EPERM` without CAP_SYS_NICE or RLIMIT_RTPRIO. From `Keepalive::set_priority` and `AbacusClient::set_keepalive_priority`, leaving the previous priority in force; and from any call that starts the keepalive thread with a stored real-time priority, which leaves the thread at its inherited scheduling and the stored priority at `Normal` (touch.rs::spawn_thread) |
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

See BACKLOG.md.
