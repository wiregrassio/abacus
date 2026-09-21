
# Abacus RTS contracts

The frozen interface surface. If it is not an interface or a contract, it does not belong.
ARCHITECTURE.md says why. LIFECYCLE.md says how the daemon works internally. SURFACE.md
says what the SDK exposes. This file says what the boundaries are.

## UDS surface

The daemon listens on a Unix stream socket (`transport::Server::create`). Clients connect,
send length-prefixed frames, and receive one response per request.

| Request | Wire tag | Response on success | Descriptors returned | Enforced in |
|---|---|---|---|---|
| `CreateInterlock` | `0x01` | `Created { id }` | 1 (dup of the interlock memfd) | `daemon::handle_request`, `registry::Registry::create` |
| `AttachInterlock` | `0x02` | `Attached { id }` | 1 (dup of the interlock memfd) | `daemon::handle_request`, `registry::Registry::attach` |
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

## Create call

`Request::CreateInterlock` carries one field set; which optional fields are required is a
function of `tier`.

| Field | Type | Meaning | Valid values | Daemon use |
|---|---|---|---|---|
| `name` | string | Registry key | 1 to 255 bytes, not `clock` | `registry::validate_name`, `registry::Registry::create` |
| `tier` | u8 | Tier discriminant | 0..=4 | `registry::Tier::from_wire` |
| `watched_name` | optional string | Name of the interlock to watch | must resolve to a live entry or `clock` | `registry::Registry::resolve` |
| `watched_word` | optional u8 | Which word of the target to read | 0 (`open_count`), 1 (`closed_count`) | `registry::WatchedWord::from_wire` |
| `interval_ns` | optional u64 | Cron grid interval | > 0 and >= 1 ms after integer division | `registry::Registry::create` (`Tier::WaitCron` arm) |
| `conditions` | optional list of (string, u8, u64) | Barrier thresholds | non-empty; each name must resolve; each word in {0,1} | `registry::Registry::create` (`Tier::WaitBarrier` arm) |

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
| `Condition::InvalidRequest` | empty name, name over 255 bytes, name `clock`, live-count limit reached, missing tier field, `interval_ns` 0 or sub-millisecond, empty conditions list, `watched_word` not 0 or 1 | `registry::validate_name`, `registry::Registry::create`, `registry::WatchedWord::from_wire` |
| `Condition::InterlockNotFound` | `watched_name` or any condition name does not resolve | `registry::Registry::resolve` |
| `Condition::AllocationFailed` | memfd create, ftruncate, seal, mmap, or dup failed | `interlock::interlock_create`, `interlock::interlock_dup_fd` |

Creating over an existing live name is permitted: the old entry is reaped and removed, the
new one takes the name with a strictly larger id (`registry::Registry::create` remove-slot
path, `registry::Registry::remove_slot`).

The SDK rejects `create_wait_cron` with `interval_ms == 0` before it reaches the wire
(`client::Abacus RTSClient::create_wait_cron`), and rejects `attach_interlock("clock")`
(`client::Abacus RTSClient::attach_interlock`).

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
stamps both counters with the current clock millisecond (`process_clock::ProcessClock::new`).

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

## Trust model

| Subject | Trusted | Not trusted | Enforced in |
|---|---|---|---|
| Socket peers | Any process that can open the socket may create, attach, and terminate any name | Peer identity, peer intent | `daemon::handle_request` (no credential check) |
| Socket access control | Filesystem mode and group on the socket file | Anything else | `transport::Server::apply_socket_options`, `daemon::DaemonConfig::new` (default `0o660`) |
| Frame content | Nothing: version, tag, lengths, and UTF-8 are checked before use; tier, watched word, interval, and thresholds are validated in the registry | Raw numeric fields from the codec | `codec::decode_request`, `registry::Registry::create` |
| Shared memory | The daemon reads client-written words but treats any `SENTINEL` or lapsed expiration as termination rather than as a fault | Word values written by peers | `registry::Registry::evaluate_all` |
| memfd integrity | Backing size is fixed by seals, so a hostile `ftruncate` cannot shrink a mapping under the daemon | Attacher intent | `interlock::seal` (`F_SEAL_SHRINK \| F_SEAL_GROW \| F_SEAL_SEAL`) |
| Clock integrity | The clock memfd additionally carries `F_SEAL_FUTURE_WRITE`, so attachers cannot map it writable | Attacher intent | `interlock::interlock_create_clock`, `interlock::interlock_map_clock` |

Straight razor: except for the clock, every handed-out descriptor is mapped
`PROT_READ | PROT_WRITE` (`interlock::interlock_map`, `interlock_map_counter`,
`interlock_map_timer`, `interlock_map_cron`, `interlock_map_barrier`). Per-word write
discipline is a property of the SDK handle type, not of page protection. The only
mmap-enforced restriction is the clock, which is mapped `PROT_READ`
(`interlock::interlock_map_clock`).

## Interlock shape

| Property | Value | Enforced in |
|---|---|---|
| Layout | `#[repr(C)]` struct of three `AtomicU64` | `interlock::Interlock` |
| Size | 24 bytes | `interlock::INTERLOCK_SIZE`, compile-time `assert!(size_of::<Interlock>() == 24)` |
| Offsets | `open_count` 0, `closed_count` 8, `expiration_ns` 16 | `interlock::Interlock` field order; asserted in `src/tests/interlock.rs` |
| Backing | memfd with `MFD_CLOEXEC \| MFD_ALLOW_SEALING`, truncated to 24 bytes, sealed | `interlock::memfd_create`, `interlock::ftruncate`, `interlock::seal` |
| Mapping | `MAP_SHARED` | `interlock::mmap_interlock` |
| Sentinel | `SENTINEL` = `u64::MAX` in any word means terminated | `interlock::SENTINEL`, `interlock::interlock_is_terminated` |
| Endianness | Little-endian only; futex addresses the low 32 bits | `clock::futex_addr` with compile-time endian assert, `clock::futex_word` |

Wire protocol v1 frame layout:

| Segment | Bytes | Content |
|---|---|---|
| Length prefix | 4 | payload length, little-endian u32, `LENGTH_PREFIX_BYTES` |
| Version | 1 | `PROTOCOL_VERSION` = 1 |
| Tag | 1 | request or response discriminant |
| Body | variable | tag-specific, see below |

Strings are a little-endian u16 byte length followed by UTF-8 bytes
(`codec::encode_string_into`, `codec::Cursor::string`).

| Tag | Name | Body field order |
|---|---|---|
| `0x01` | CreateInterlock | name (string), tier (u8), then: tier 1 -> watched_name (string), watched_word (u8); tier 3 -> interval_ns (u64 LE); tier 4 -> count (u16 LE) then count times [name (string), word (u8), threshold (u64 LE)]; tiers 0, 2, and unknown -> nothing |
| `0x02` | AttachInterlock | name (string) |
| `0x81` | Created | id (u64 LE) |
| `0x82` | Attached | id (u64 LE) |
| `0x88` | Error | code (u8), message (string) |

Enforced in `codec::encode_request`, `codec::encode_response`, `codec::decode_request`,
`codec::decode_response`. Descriptors travel as `SCM_RIGHTS` ancillary data attached to the
frame's first bytes, never inside the payload (`fdpass::send_frame_with_fds`,
`fdpass::recv_prefix_with_fds`).

## Field semantics by tier

### Interlock (tier 0)

| Word | Meaning | Written by | Read by |
|---|---|---|---|
| open_count | Monotonic count of opens | Creator and attachers (`Interlock::open`, `AttachedInterlock::open`) | All handles, daemon |
| closed_count | Monotonic count of closes | Creator and attachers (`Interlock::close`, `AttachedInterlock::close`) | All handles, daemon |
| expiration_ns | Monotonic-clock deadline | Creator via `touch` or keepalive (`handle_ops::touch`, `interlock::interlock_arm`); daemon on reap | Daemon (`registry::Registry::evaluate_all`), SDK waiters |

### WaitCounter (tier 1)

| Word | Meaning | Written by | Read by |
|---|---|---|---|
| open_count | Target value on the watched word | Creator, CAS-max (`WaitCounter::wait_until`) | Daemon comparison |
| closed_count | Watched value at delivery | Daemon only (`registry::Registry::evaluate_all`) | Creator, attached view (`AttachedWaitCounter::completed_at`) |
| expiration_ns | Deadline, armed to `2 * timeout_ms` | Creator (`WaitCounter::wait_until` via `interlock_arm`), keepalive | Daemon |

### WaitTimer (tier 2)

| Word | Meaning | Written by | Read by |
|---|---|---|---|
| open_count | Target clock millisecond | Creator, CAS-max (`WaitTimer::wait_ms_with_margin`) | Daemon comparison against `clock.open_count` |
| closed_count | Clock millisecond at delivery | Daemon only | Creator (`WaitTimer::completed_at`) |
| expiration_ns | Fatal margin deadline | Creator (`interlock_arm` with `margin_ms`), keepalive | Daemon, creator |

### WaitCron (tier 3)

| Word | Meaning | Written by | Read by |
|---|---|---|---|
| open_count | Next grid line, in clock milliseconds | Daemon (re-arm), registry at create | Daemon comparison |
| closed_count | Clock millisecond at last fire | Daemon only | Creator (`WaitCron::wait`, `completed_at`) |
| expiration_ns | Deadline | Keepalive | Daemon |

### WaitBarrier (tier 4)

| Word | Meaning | Written by | Read by |
|---|---|---|---|
| open_count | Arm generation; starts at 1, incremented to re-arm | Registry at create, creator (`WaitBarrier::rearm`) | Daemon |
| closed_count | Set equal to open_count when all conditions hold | Daemon only | Creator (`WaitBarrier::wait`) |
| expiration_ns | Deadline | Keepalive | Daemon |

### clock (reserved, id 0)

| Word | Meaning | Written by | Read by |
|---|---|---|---|
| open_count | Current monotonic millisecond | Daemon only (`registry::Registry::tick`) | Everyone, read-only mapping |
| closed_count | Daemon start millisecond | Daemon at registry construction | Everyone |
| expiration_ns | Refreshed to now + 100 ms each tick | Daemon (`registry::Registry::refresh_clock_expiration`) | Nobody else |

### Derived properties

| Property | Formula | Defined in |
|---|---|---|
| value | `(open_count as i64).wrapping_sub(closed_count as i64)` | `handle_ops::value`, `types::interlock_state` |
| ttl | `expiration_ns - now`; alive when `expiration_ns > now` | `clock::expiration_alive` |
| terminated | any word equals `SENTINEL` | `interlock::interlock_is_terminated` |
| uptime (clock) | `open_count - closed_count` | `interlock::ClockHandle::uptime_ms` |

### States

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

## Daemon contract (1 ms loop)

Each cycle, in order (`daemon::daemon_run_with`):

| Step | Action | Enforced in |
|---|---|---|
| 1 | Check the stop flag; break if set | `daemon::daemon_run_with` |
| 2 | `ppoll` on the listener plus every client, timeout = `next_due - now` | `daemon::daemon_run_with`, `daemon::build_pollfds` |
| 3 | Accept every pending connection, set nonblocking | `transport::Server::try_accept` |
| 4 | For readable clients, read available bytes and answer every complete request | `daemon::service_client` |
| 5 | Remove clients that closed, faulted, or became unwritable | `daemon::service_client`, `daemon::is_connection_error` |
| 6 | If `now >= next_due`, run `registry.tick(now)` and recompute `next_due` | `daemon::daemon_run_with` |
| 7 | Rebuild the pollfd set if the client set changed | `daemon::build_pollfds` |

`registry::Registry::tick` stores `now_ns / NANOS_PER_MS` into `clock.open_count`, runs
`evaluate_all`, futex-wakes the clock word, then refreshes the clock expiration.

`registry::Registry::evaluate_all` visits every occupied slot in order and, per entry:

| Check | Action |
|---|---|
| any word is `SENTINEL` | `interlock_reap`, mark slot dead |
| `!expiration_alive(exp, now_ns)` | `interlock_reap`, mark slot dead |
| tier Interlock | nothing further |
| tier WaitTimer, value > 0 and `clock_now_ms >= open_count` | store `clock_now_ms` into closed_count, futex wake |
| tier WaitCron, value > 0 and `clock_now_ms >= open_count` | store `clock_now_ms` into closed_count, futex wake, re-arm open_count to `next_grid_line(clock_now_ms, interval_ms)`; on overflow, reap |
| tier WaitCounter, target slot missing, id mismatch, or watched word is `SENTINEL` | `interlock_reap`, mark slot dead |
| tier WaitCounter, value > 0 and watched >= open_count | store the watched value into closed_count, futex wake |
| tier WaitBarrier, any condition target gone | `interlock_reap`, mark slot dead |
| tier WaitBarrier, value > 0 and every watched value >= its threshold | store open_count into closed_count, futex wake |

Watcher lifetime: a watcher is identified by `(slot, id)`. A recreated name occupies a new
id, so a watcher never retargets; it is reaped (`registry::Target::Slot`,
`registry::Registry::watched_value`).

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
| Anchor | `anchor = monotonic_now_nanos()` at startup; first due at `anchor + 1 ms` | `daemon::daemon_run_with` |
| next_due | `anchor + ((now - anchor) / NANOS_PER_MS + 1) * NANOS_PER_MS` | `daemon::daemon_run_with` |
| Catch-up | A late tick advances `next_due` past all missed boundaries; exactly one evaluation runs | same expression |
| EINTR | `ppoll` returning `EINTR` continues the loop; any other error is `StartupError::EventLoopFailed` | `daemon::daemon_run_with` |

## TTL rules

| Rule | Contract | Enforced in |
|---|---|---|
| Expiration is absolute | `expiration_ns` is a `CLOCK_MONOTONIC` nanosecond deadline | `clock::monotonic_now_nanos`, `interlock::interlock_arm` |
| Arming is monotonic forward | `interlock_arm` CAS-maxes `max(current, now + ttl)`; a shorter TTL is a no-op | `interlock::interlock_arm` |
| Arming a terminated interlock fails | `expiration_ns == SENTINEL` returns `Condition::InterlockReaped` and leaves the sentinel intact | `interlock::interlock_arm` |
| Creation TTL | 100 ms (`CREATION_TTL_NANOS`) | `interlock::interlock_create` |
| Clock TTL | 100 ms, refreshed every tick | `registry::CLOCK_TTL_NANOS`, `registry::Registry::refresh_clock_expiration` |
| Reap condition | `expiration_ns <= now` (`expiration_alive` is strict greater-than) | `clock::expiration_alive`, `registry::Registry::evaluate_all` |
| Keepalive interval | `DEFAULT_TOUCH_INTERVAL_MS` = 40 ms | `types::DEFAULT_TOUCH_INTERVAL_MS`, `interlock::Interlock::new` |
| Keepalive TTL | `max(interval * 5, MIN_TOUCH_TTL_MS)`, floor 200 ms | `types::default_touch_ttl_ms`, `types::MIN_TOUCH_TTL_MS` |
| Registration TTL floor | TTL is raised to at least `2 * interval` and at least 1 ms | `touch::Keepalive::register_inner` |
| One thread per client | A single keepalive thread serves every registered handle | `touch::Keepalive::register_inner`, `touch::run` |
| Keepalive stop | Dropping or stopping a `TouchHandle` deregisters it; the interlock is then reaped after at most its remaining TTL | `touch::TouchHandle::drop`, `touch::Keepalive::deregister` |
| Reaped detection | A failed arm sets the entry's reaped flag and removes it from the keepalive set | `touch::run`, `touch::TouchHandle::is_reaped` |
| Timer margin | `margin = max(2 * wait_ms, min_fatal_margin_ms)`, default floor `MIN_FATAL_MARGIN_MS` = 50 ms | `wait_timer::WaitTimer::margin_for`, `types::MIN_FATAL_MARGIN_MS` |
| Explicit margin | `wait_ms_with_margin` requires `margin_ms > ms`, else `SdkError::InvalidRequest` | `wait_timer::WaitTimer::wait_ms_with_margin` |
| Counter TTL | `wait_until(target, timeout_ms)` arms `2 * timeout_ms` | `wait_counter::WaitCounter::wait_until` |

## Wake outcomes (SDK-side)

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
`wait_barrier::WaitBarrier::wait`, `wait_cron::WaitCron::wait`).

| Waiter | Timeout result |
|---|---|
| `WaitCounter::wait_until` | `Ok(WaitResult { state: Timeout, completed_at })` after the futex reports `ETIMEDOUT` and one more delivery check fails |
| `WaitTimer::wait_ms*` | `on_timeout`: `TimeoutPolicy::Error` yields `Err(SdkError::RtsTimeout)`; `TimeoutPolicy::Abort` prints a diagnostic and calls `std::process::abort` |
| `Interlock::wait_open_for` and siblings | `Ok(None)` |
| `WaitCron::wait`, `WaitBarrier::wait` | no timeout; they return only on delivery or reap |

`WaitCron` classifies independently: a fire whose `closed_count` is a multiple of the
interval is `Normal`, otherwise `Overrun` (`wait_cron::WaitCron::wait`).

`WaitTimer::wait_ms(0)` returns `Normal` with `completed_at` set to the current clock
millisecond without arming anything, unless the handle is already terminated
(`wait_timer::WaitTimer::wait_ms_with_margin`).

## Errors

### `Condition` (`abacus_core::error::Condition`)

| Variant | Produced by |
|---|---|
| `InterlockReaped` | `interlock::interlock_arm` on a sentinel expiration |
| `InterlockNotFound { name }` | `registry::Registry::attach`, `registry::Registry::resolve` |
| `AllocationFailed { step, errno }` | `interlock::memfd_create`, `ftruncate`, `seal`, `mmap_interlock`, `interlock_dup_fd` |
| `InvalidRequest { message }` | `registry::validate_name`, `registry::Registry::create`, `registry::WatchedWord::from_wire` |

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

| Variant | Produced by |
|---|---|
| `InterlockReaped` | daemon code `ERR_INTERLOCK_REAPED`; sentinel or lapsed expiration seen in any SDK wait or increment path (`handle_ops::increment`, `handle_ops::wait_word`) |
| `InterlockNotFound { name }` | daemon code `ERR_INTERLOCK_NOT_FOUND` |
| `AllocationFailed { message }` | daemon code `ERR_ALLOCATION_FAILED` |
| `Transport(TransportError)` | connect, send, receive, and framing failures |
| `MmapFailed { message }` | `client::map_handle` when the mapping function fails |
| `InvalidRequest { message }` | daemon code `ERR_INVALID_REQUEST`; SDK-side rejections (`attach_interlock("clock")`, `create_wait_cron(0)`, `WaitRace` over zero counters, `margin_ms <= ms`) |
| `UnexpectedResponse { message }` | wrong response tag for the request, or an unrecognized daemon error code |
| `RtsTimeout` | `wait_timer::WaitTimer::on_timeout` under `TimeoutPolicy::Error` |

Daemon error codes: `ERR_INTERLOCK_REAPED` 0x01, `ERR_INTERLOCK_NOT_FOUND` 0x02,
`ERR_ALLOCATION_FAILED` 0x03, `ERR_INVALID_REQUEST` 0x04 (`codec`, mapped in
`client::daemon_error`).

## Permission model (SDK-enforced)

Rows are handle types; columns are the three words plus lifecycle operations. "type" means
the method simply does not exist on the type; "mmap" means the page protection forbids it.

| Handle | open_count | closed_count | expiration_ns | free | touch control |
|---|---|---|---|---|---|
| `Interlock` (creator, tier 0) | read/write (`open`) | read/write (`close`) | read/write (`touch`) | `free` stamps expiration | `start_touch_thread`, `stop_touch_thread` |
| `AttachedInterlock` | read/write (`open`) | read/write (`close`) | read only (`expiration_ns`); no `touch` (type) | `free` stamps open_count with `SENTINEL` | none (type) |
| `WaitCounter` (creator) | read/write via CAS-max target (`wait_until`) | read only (`completed_at`); no `close` (type) | write via `touch` and `wait_until` arming | `free` stamps expiration | keepalive-registered |
| `AttachedWaitCounter` | read only (`peek`) | read only (`completed_at`) | not exposed (type) | none (type) | none (type) |
| `WaitTimer` (creator) | read/write via CAS-max target | read only (`completed_at`); no `close` (type) | write via `touch` and margin arming | `free` stamps expiration | keepalive-registered |
| `WaitCron` (creator) | read only (`peek`) | read only (`completed_at`) | keepalive only | `free` stamps expiration | keepalive-registered |
| `WaitBarrier` (creator) | read/write via `rearm` (+1) | read only (`completed_at`) | keepalive only | `free` stamps expiration | keepalive-registered |
| `ProcessClock` (creator) | read (`last_seen_ms`); written by keepalive from the daemon clock | read (`start_time_ms`) | keepalive only | `free` stamps expiration | keepalive-registered |
| `ClockHandle` | read only (mmap) | read only (mmap) | read only (mmap) | none (type) | none (type) |

Enforced in: `interlock::Interlock`, `interlock::AttachedInterlock`,
`interlock::AttachedWaitCounter`, `interlock::ClockHandle`, `wait_counter::WaitCounter`,
`wait_timer::WaitTimer`, `wait_cron::WaitCron`, `wait_barrier::WaitBarrier`,
`process_clock::ProcessClock`.

Only the clock is mmap-enforced (`interlock::interlock_map_clock` maps `PROT_READ`, and the
clock memfd carries `F_SEAL_FUTURE_WRITE` so a writable map fails with an
`AllocationFailed { step: Mmap }`). Every other restriction in the table is type-enforced.

Sentinel safety on increments: `handle_ops::increment` rejects an increment whose old value
is `SENTINEL` or that would wrap past `SENTINEL`, restores `SENTINEL`, wakes waiters, and
returns `InterlockReaped`. A terminated interlock cannot be resurrected by arithmetic.

## Termination

| Path | What it stamps | What peers observe | Daemon's next pass |
|---|---|---|---|
| Creator `free` (`Interlock::free`, `WaitCounter::free`, `WaitTimer::free`, `WaitCron::free`, `WaitBarrier::free`, `ProcessClock::free`) | `expiration_ns = SENTINEL`, futex-wakes both counters | `is_reaped()` true, `state()` Expired, `touch` returns `InterlockReaped`, waiters return `InterlockReaped` | `evaluate_all` sees the sentinel, calls `interlock_reap`, removes the slot and the name |
| Attacher `free` (`AttachedInterlock::free`) | `open_count = SENTINEL`, futex-wakes both counters | Same as above for every mapper | Same: sentinel in any word reaps |
| Handle drop | Nothing is stamped; the `TouchHandle` deregisters | The interlock keeps its current expiration and lapses naturally | Reaped when `expiration_ns <= now` |
| TTL lapse | Nothing by the owner | Waiters see a lapsed expiration and return `InterlockReaped` | `expiration_alive` false, `interlock_reap`, slot removed |
| Daemon reap (`interlock::interlock_reap`) | All three words set to `SENTINEL`, both counters futex-woken | `interlock_is_terminated` true from any mapper | Slot pushed to the free list, name removed from the index |
| Name replacement | Old entry passed through `remove_slot`, which calls `interlock_reap` | Old handle sees all-sentinel words | New entry occupies a (possibly reused) slot with a strictly larger id |

The mapping and its descriptor stay valid after termination: the page is freed only when the
last `InterlockHandle` clone drops (`interlock::InterlockRegion::drop` calls `munmap`).
`interlock_free` writes only the expiration word, so consumers must treat `SENTINEL` in any
word as terminal (`interlock::interlock_is_terminated`).

Daemon shutdown: `SIGTERM` and `SIGINT` set the stop flag; the loop exits, the `Server` drops,
the socket file is removed, and the process exits 0 (`main::on_stop_signal`,
`main::install_stop_handlers`, `transport::Server::drop`). A restart comes back with an empty
registry; old mappings remain valid but nothing advances them, so waiters discover the
restart through their own TTL or margin.

## Deferred

- Wire v2 tier metadata on `Attached`: v1 carries only an id, so `attach_wait_counter` cannot verify the tier and non-attachable tiers cannot be refused at the wire boundary.
- Boolean compositions (tiers 5 and above): no wire discriminants exist; `WaitRace` polls at 1 ms in the SDK instead (`wait_race::WaitRace::wait`).
- Non-Rust binding: no FFI surface exists; such a binding would have to force `TimeoutPolicy::Error` because the default `Abort` terminates the host process.