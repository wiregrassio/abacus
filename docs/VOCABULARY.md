
# Abacus RTS vocabulary

The canonical name for every concept in this codebase. When the docs, the code, and a
conversation all use the same word for the same thing, ambiguity dies. When they don't,
bugs hide.

## Primitives and layout

| Term | Meaning | Defined at |
|------|---------|------------|
| `CLOCK_ID` | Registry ID reserved for the daemon-owned clock: `0`. | `crates/abacus-daemon/src/registry.rs::CLOCK_ID` |
| `CLOCK_NAME` | Reserved interlock name `"clock"`; creating it is refused. | `crates/abacus-daemon/src/registry.rs::CLOCK_NAME` |
| `closed_count` | Second `u64` word at offset 8: monotonic completion counter, futex-waitable. | `crates/abacus-core/src/interlock.rs::Interlock::closed_count` |
| `CREATION_TTL_NANOS` | TTL stamped into `expiration_ns` at creation: 100 ms in nanoseconds. | `crates/abacus-core/src/interlock.rs::CREATION_TTL_NANOS` |
| `DEFAULT_MAX_INTERLOCKS` | Registry capacity excluding the clock: 4096. | `crates/abacus-daemon/src/registry.rs::DEFAULT_MAX_INTERLOCKS` |
| `expiration_ns` | Third `u64` word at offset 16: monotonic nanosecond deadline; `SENTINEL` means terminated. | `crates/abacus-core/src/interlock.rs::Interlock::expiration_ns` |
| `F_SEAL_FUTURE_WRITE` | Extra seal applied to the clock memfd so attachers cannot map it writable. | `crates/abacus-core/src/interlock.rs::F_SEAL_FUTURE_WRITE` |
| `futex_addr` | Casts an `AtomicU64` to the `*const u32` the kernel compares; assumes little-endian. | `crates/abacus-core/src/clock.rs::futex_addr` |
| `futex_wait` | Sleeps on a word's low 32 bits until woken, value change, or timeout. | `crates/abacus-core/src/clock.rs::futex_wait` |
| `futex_wake` | Wakes every waiter on a word. | `crates/abacus-core/src/clock.rs::futex_wake` |
| `futex_word` | Truncates a `u64` to the low 32 bits the futex compares. | `crates/abacus-core/src/clock.rs::futex_word` |
| `Interlock` (core) | The `#[repr(C)]` three-word shared structure: the primitive itself. | `crates/abacus-core/src/interlock.rs::Interlock` |
| `interlock_arm` | CAS-max extension of `expiration_ns`; never shortens, refuses `SENTINEL`. | `crates/abacus-core/src/interlock.rs::interlock_arm` |
| `interlock_create` | Creates a sealed memfd, maps it read-write, stamps the creation TTL. | `crates/abacus-core/src/interlock.rs::interlock_create` |
| `interlock_create_clock` | Same, plus `F_SEAL_FUTURE_WRITE` so handed-out descriptors map read-only. | `crates/abacus-core/src/interlock.rs::interlock_create_clock` |
| `interlock_dup_fd` | Duplicates the backing descriptor for SCM_RIGHTS handout. | `crates/abacus-core/src/interlock.rs::interlock_dup_fd` |
| `interlock_free` | Writes `SENTINEL` to `expiration_ns` only, then wakes both counters. | `crates/abacus-core/src/interlock.rs::interlock_free` |
| `interlock_is_terminated` | True if any of the three words is `SENTINEL`. | `crates/abacus-core/src/interlock.rs::interlock_is_terminated` |
| `interlock_reap` | Writes `SENTINEL` to all three words and wakes both counters. | `crates/abacus-core/src/interlock.rs::interlock_reap` |
| `INTERLOCK_SIZE` | Byte size of the shared region: 24. | `crates/abacus-core/src/interlock.rs::INTERLOCK_SIZE` |
| `InterlockHandle` | Reference-counted owner of a mapped region; `words()` returns the shared atomics. | `crates/abacus-core/src/interlock.rs::InterlockHandle` |
| `InterlockRegion` | Private inner holder of the `OwnedFd` plus mapping pointer; munmaps on drop. | `crates/abacus-core/src/interlock.rs::InterlockRegion` |
| `MAX_NAME_LEN` | Longest accepted interlock name in bytes: 255. | `crates/abacus-daemon/src/registry.rs::MAX_NAME_LEN` |
| `monotonic_now_nanos` | `CLOCK_MONOTONIC` read in nanoseconds; aborts on failure. | `crates/abacus-core/src/clock.rs::monotonic_now_nanos` |
| `ms_to_nanos` | Saturating milliseconds-to-nanoseconds conversion. | `crates/abacus-core/src/clock.rs::ms_to_nanos` |
| `NANOS_PER_MS` | 1_000_000. | `crates/abacus-core/src/clock.rs::NANOS_PER_MS` |
| `NANOS_PER_SEC` | 1_000_000_000. | `crates/abacus-core/src/clock.rs::NANOS_PER_SEC` |
| `open_count` | First `u64` word at offset 0: monotonic arming counter, futex-waitable. | `crates/abacus-core/src/interlock.rs::Interlock::open_count` |
| `SENTINEL` | `u64::MAX`, globally reserved: in any word it means terminated. | `crates/abacus-core/src/interlock.rs::SENTINEL` |

## Tiers

| Term | Meaning | Defined at |
|------|---------|------------|
| `Tier::Interlock` (0) | Bare interlock: daemon reaps on expired TTL or `SENTINEL`, nothing else. | `crates/abacus-daemon/src/registry.rs::Tier::Interlock` |
| `Tier::WaitBarrier` (4) | Fires when every condition holds; re-armable by incrementing `open_count`. | `crates/abacus-daemon/src/registry.rs::Tier::WaitBarrier` |
| `Tier::WaitCounter` (1) | Watches another interlock's word and stamps `closed_count` when the target is reached. | `crates/abacus-daemon/src/registry.rs::Tier::WaitCounter` |
| `Tier::WaitCron` (2 lines, tier 3) | Fires on a grid line, then re-arms to the next line. | `crates/abacus-daemon/src/registry.rs::Tier::WaitCron` |
| `Tier::WaitTimer` (2) | Watches the clock's `open_count`; fires when the clock reaches the target. | `crates/abacus-daemon/src/registry.rs::Tier::WaitTimer` |
| `Tier::from_wire` | Maps a wire tier byte (0 to 4) to a `Tier`; anything else is invalid. | `crates/abacus-daemon/src/registry.rs::Tier::from_wire` |
| `Interlock` (SDK) | Owning handle for tier 0: open, close, touch, wait, free. | `crates/abacus-client/src/interlock.rs::Interlock` |
| `AttachedInterlock` | Non-owning handle for tier 0 obtained by attach: mutate and wait, no touch. | `crates/abacus-client/src/interlock.rs::AttachedInterlock` |
| `AttachedWaitCounter` | Read-only view of a tier 1 object: peek, value, `completed_at`, `is_reaped`. | `crates/abacus-client/src/interlock.rs::AttachedWaitCounter` |
| `ClockHandle` | Read-only handle on the daemon clock: `now_ms`, `start_time_ms`, `uptime_ms`, bounded waits. | `crates/abacus-client/src/interlock.rs::ClockHandle` |
| `ProcessClock` | Tier 0 interlock whose `open_count` the keepalive refreshes from the daemon clock. | `crates/abacus-client/src/process_clock.rs::ProcessClock` |
| `WaitBarrier` | SDK handle for tier 4: `wait`, `rearm`, `completed_at`, `free`. | `crates/abacus-client/src/wait_barrier.rs::WaitBarrier` |
| `WaitCounter` | SDK handle for tier 1: `wait_until` with a CAS-max target. | `crates/abacus-client/src/wait_counter.rs::WaitCounter` |
| `WaitCron` | SDK handle for tier 3: `wait` returns each grid fire, `interval_ms` is the grid. | `crates/abacus-client/src/wait_cron.rs::WaitCron` |
| `WaitRace` | SDK-only composition over several `WaitCounter`s, polled at 1 ms. | `crates/abacus-client/src/wait_race.rs::WaitRace` |
| `WaitTimer` | SDK handle for tier 2: `wait_ms`, `wait_until`, fatal margin, timeout policy. | `crates/abacus-client/src/wait_timer.rs::WaitTimer` |

## States and derived properties

| Term | Meaning | Defined at |
|------|---------|------------|
| `classify_wake` | Turns `(open, closed)` into `Normal` (equal), `Overrun` (closed greater), or `None` (not delivered). | `crates/abacus-client/src/types.rs::classify_wake` |
| `InterlockState::Closed` | Alive and `value == 0`. | `crates/abacus-client/src/types.rs::InterlockState::Closed` |
| `InterlockState::Expired` | Any word is `SENTINEL`, or the clock has reached `expiration_ns`. | `crates/abacus-client/src/types.rs::InterlockState::Expired` |
| `InterlockState::Open` | Alive and `value > 0`. | `crates/abacus-client/src/types.rs::InterlockState::Open` |
| `InterlockState::Overrun` | Alive and `value < 0`: more closes than opens, informational only. | `crates/abacus-client/src/types.rs::InterlockState::Overrun` |
| `interlock_state` | Pure function deriving `InterlockState` from the three words plus the current clock. | `crates/abacus-client/src/types.rs::interlock_state` |
| `is_reaped` | Handle-level predicate: terminated words, or the keepalive entry reported reaped. | `crates/abacus-client/src/interlock.rs::Interlock::is_reaped` |
| `ttl` | Time left before `expiration_ns`; alive while `expiration_ns > now`. | `crates/abacus-core/src/clock.rs::expiration_alive` |
| `terminated` | Any of the three words equals `SENTINEL`. | `crates/abacus-core/src/interlock.rs::interlock_is_terminated` |
| `value` | `open_count - closed_count` as a signed difference. | `crates/abacus-client/src/handle_ops.rs::value` |
| `WaitResult` | A delivery: `completed_at` plus the `WaitState` describing it. | `crates/abacus-client/src/types.rs::WaitResult` |
| `WaitState::Normal` | Delivered exactly at the target. | `crates/abacus-client/src/types.rs::WaitState::Normal` |
| `WaitState::Overrun` | Delivered past the target: the daemon stamped a later value. | `crates/abacus-client/src/types.rs::WaitState::Overrun` |
| `WaitState::Timeout` | The wait window elapsed with nothing delivered. | `crates/abacus-client/src/types.rs::WaitState::Timeout` |
| `Word` | Internal selector naming which counter an SDK operation touches: `Open` or `Closed`. | `crates/abacus-client/src/handle_ops.rs::Word` |

## Daemon concepts

| Term | Meaning | Defined at |
|------|---------|------------|
| `daemon_run` | Runs the loop on a socket path with default configuration until the stop flag is set. | `crates/abacus-daemon/src/daemon.rs::daemon_run` |
| `daemon_run_with` | Runs the loop with an explicit `DaemonConfig`: create server, registry, ppoll, tick. | `crates/abacus-daemon/src/daemon.rs::daemon_run_with` |
| `DaemonConfig` | Socket path, socket mode, optional socket group, registry capacity. | `crates/abacus-daemon/src/daemon.rs::DaemonConfig` |
| `Entry` (registry) | One live registration: id, name, handle, tier, watch, interval, conditions. | `crates/abacus-daemon/src/registry.rs::Entry` |
| `evaluate_all` | One pass over every live slot: reap the dead, fire each tier, collect dead slots. | `crates/abacus-daemon/src/registry.rs::Registry::evaluate_all` |
| `next_grid_line` | Next multiple of the cron interval strictly after the given millisecond. | `crates/abacus-daemon/src/registry.rs::next_grid_line` |
| `Registry` | Named interlock table: slots, free list, name index, live count, the clock. | `crates/abacus-daemon/src/registry.rs::Registry` |
| `refresh_clock_expiration` | Re-arms the clock's own TTL each tick so the clock never lapses. | `crates/abacus-daemon/src/registry.rs::Registry::refresh_clock_expiration` |
| `remove_slot` | Reaps the entry, drops its name from the index, returns the slot to the free list. | `crates/abacus-daemon/src/registry.rs::Registry::remove_slot` |
| `service_client` | Reads available bytes, decodes every complete request, replies, decides keep or drop. | `crates/abacus-daemon/src/daemon.rs::service_client` |
| `Slot` | Index into `Registry::slots`; reused after removal, paired with `id` to detect reuse. | `crates/abacus-daemon/src/registry.rs::Target::Slot` |
| `Target` | What a watcher points at: `Clock`, or a `Slot` pinned by its generation `id`. | `crates/abacus-daemon/src/registry.rs::Target` |
| `tick` | Advance the clock word to the current millisecond, evaluate all, wake, re-arm the clock. | `crates/abacus-daemon/src/registry.rs::Registry::tick` |
| `watched_value` | Reads a target's chosen word, returning `None` if the target is gone or `SENTINEL`. | `crates/abacus-daemon/src/registry.rs::Registry::watched_value` |
| recreate | Create over an existing name: the old entry is reaped, a new id is issued, the live count is unchanged. | `crates/abacus-daemon/src/registry.rs::Registry::create` (test `recreate_reaps_old_and_ids_increase`) |

## SDK concepts

| Term | Meaning | Defined at |
|------|---------|------------|
| `Abacus RTSClient` | Blocking UDS connection plus the attached clock, keepalive, and timeout policy. | `crates/abacus-client/src/client.rs::Abacus RTSClient` |
| `DEFAULT_TIMEOUT_NANOS` | Futex wait slice used by open-ended waits: 100 ms. | `crates/abacus-client/src/types.rs::DEFAULT_TIMEOUT_NANOS` |
| `DEFAULT_TOUCH_INTERVAL_MS` | Keepalive re-arm interval: 40 ms. | `crates/abacus-client/src/types.rs::DEFAULT_TOUCH_INTERVAL_MS` |
| `DEFAULT_TOUCH_TTL_MS` | Default TTL a keepalive arms: 200 ms, equal to `MIN_TOUCH_TTL_MS`. | `crates/abacus-client/src/types.rs::DEFAULT_TOUCH_TTL_MS` |
| `default_touch_ttl_ms` | TTL for an interval: five intervals, floored at `MIN_TOUCH_TTL_MS`. | `crates/abacus-client/src/types.rs::default_touch_ttl_ms` |
| `DEFAULT_TRANSPORT_TIMEOUT` | Socket read and write timeout: one second. | `crates/abacus-client/src/types.rs::DEFAULT_TRANSPORT_TIMEOUT` |
| `interlock_map` | Maps a received descriptor read-write: bare interlocks. | `crates/abacus-core/src/interlock.rs::interlock_map` |
| `interlock_map_barrier` | Maps a tier 4 descriptor read-write. | `crates/abacus-core/src/interlock.rs::interlock_map_barrier` |
| `interlock_map_clock` | Maps a descriptor `PROT_READ` only: the clock. | `crates/abacus-core/src/interlock.rs::interlock_map_clock` |
| `interlock_map_counter` | Maps a tier 1 descriptor read-write. | `crates/abacus-core/src/interlock.rs::interlock_map_counter` |
| `interlock_map_cron` | Maps a tier 3 descriptor read-write. | `crates/abacus-core/src/interlock.rs::interlock_map_cron` |
| `interlock_map_timer` | Maps a tier 2 descriptor read-write. | `crates/abacus-core/src/interlock.rs::interlock_map_timer` |
| `Keepalive` | One worker thread per client that re-arms every registered handle's TTL. | `crates/abacus-client/src/touch.rs::Keepalive` |
| `margin_for` | Fatal delivery margin for a wait: `2 * ms`, floored at the client's minimum. | `crates/abacus-client/src/wait_timer.rs::WaitTimer::margin_for` |
| `MIN_FATAL_MARGIN_MS` | Floor for a timer's fatal delivery margin: 50 ms. | `crates/abacus-client/src/types.rs::MIN_FATAL_MARGIN_MS` |
| `MIN_TOUCH_TTL_MS` | Floor for any keepalive TTL: 200 ms. | `crates/abacus-client/src/types.rs::MIN_TOUCH_TTL_MS` |
| `register_with_clock` | Keepalive registration that also copies the daemon clock into `open_count`. | `crates/abacus-client/src/touch.rs::Keepalive::register_with_clock` |
| `TimeoutPolicy::Abort` | Default: a missed fatal margin prints and aborts the process. | `crates/abacus-client/src/types.rs::TimeoutPolicy::Abort` |
| `TimeoutPolicy::Error` | A missed fatal margin returns `SdkError::RtsTimeout` instead. | `crates/abacus-client/src/types.rs::TimeoutPolicy::Error` |
| `touch` | Arm a handle's TTL forward by the given milliseconds. | `crates/abacus-client/src/handle_ops.rs::touch` |
| `TouchHandle` | Registration receipt: reports `is_reaped`, deregisters on `stop` or drop. | `crates/abacus-client/src/touch.rs::TouchHandle` |
| `WatchedWord::ClosedCount` | Wire discriminant 1: watch the target's `closed_count`. | `crates/abacus-client/src/types.rs::WatchedWord::ClosedCount` |
| `WatchedWord::OpenCount` | Wire discriminant 0: watch the target's `open_count`. | `crates/abacus-client/src/types.rs::WatchedWord::OpenCount` |

## Wire protocol

| Term | Meaning | Defined at |
|------|---------|------------|
| `cmsg_space_for_fds` | Const control-buffer size for `n` descriptors, matching `CMSG_SPACE`. | `crates/abacus-wire/src/fdpass.rs::cmsg_space_for_fds` |
| `ERR_ALLOCATION_FAILED` | Error code `0x03`: the daemon could not allocate the interlock. | `crates/abacus-wire/src/codec.rs::ERR_ALLOCATION_FAILED` |
| `ERR_INTERLOCK_NOT_FOUND` | Error code `0x02`: attach by name found nothing. | `crates/abacus-wire/src/codec.rs::ERR_INTERLOCK_NOT_FOUND` |
| `ERR_INTERLOCK_REAPED` | Error code `0x01`: the named object is terminated. | `crates/abacus-wire/src/codec.rs::ERR_INTERLOCK_REAPED` |
| `ERR_INVALID_REQUEST` | Error code `0x04`: request rejected on validation or protocol fault. | `crates/abacus-wire/src/codec.rs::ERR_INVALID_REQUEST` |
| `expected_fd_count` | One descriptor for `Created` and `Attached`, zero for `Error`. | `crates/abacus-wire/src/codec.rs::expected_fd_count` |
| `FrameReader` | Bounded incremental reader: `fill` from the stream, `next_frame` pops payloads. | `crates/abacus-wire/src/framing.rs::FrameReader` |
| `LENGTH_PREFIX_BYTES` | Four-byte little-endian payload length prefix. | `crates/abacus-wire/src/codec.rs::LENGTH_PREFIX_BYTES` |
| `MAX_FDS_PER_MESSAGE` | Descriptors carried by one response: 1. | `crates/abacus-wire/src/fdpass.rs::MAX_FDS_PER_MESSAGE` |
| `MAX_FRAME_BYTES` | Prefix plus maximum payload. | `crates/abacus-wire/src/codec.rs::MAX_FRAME_BYTES` |
| `MAX_MESSAGE_SIZE` | Largest payload accepted or produced: 4096 bytes. | `crates/abacus-wire/src/codec.rs::MAX_MESSAGE_SIZE` |
| `PROTOCOL_VERSION` | Leading payload byte of every v1 frame: 1. | `crates/abacus-wire/src/codec.rs::PROTOCOL_VERSION` |
| `ReadOutcome::Progress` | The fill read bytes into the buffer. | `crates/abacus-wire/src/framing.rs::ReadOutcome::Progress` |
| `ReadOutcome::WouldBlock` | The socket had nothing to read. | `crates/abacus-wire/src/framing.rs::ReadOutcome::WouldBlock` |
| `recv_prefix_with_fds` | Receives the length prefix together with any `SCM_RIGHTS` descriptors. | `crates/abacus-wire/src/fdpass.rs::recv_prefix_with_fds` |
| `Request::AttachInterlock` | Tag `0x02`: attach by name. | `crates/abacus-wire/src/codec.rs::Request::AttachInterlock` |
| `Request::CreateInterlock` | Tag `0x01`: name, tier, plus the tier's fields. | `crates/abacus-wire/src/codec.rs::Request::CreateInterlock` |
| `Response::Attached` | Tag `0x82`: registry id, one descriptor. | `crates/abacus-wire/src/codec.rs::Response::Attached` |
| `Response::Created` | Tag `0x81`: registry id, one descriptor. | `crates/abacus-wire/src/codec.rs::Response::Created` |
| `Response::Error` | Tag `0x88`: error code plus message, no descriptor. | `crates/abacus-wire/src/codec.rs::Response::Error` |
| `send_frame_with_fds` | Sends a frame with descriptors as `SCM_RIGHTS` ancillary data. | `crates/abacus-wire/src/fdpass.rs::send_frame_with_fds` |

## Error vocabulary

| Term | Meaning | Defined at |
|------|---------|------------|
| `AllocationStep::Dup` | `dup` of the backing descriptor failed. | `crates/abacus-core/src/error.rs::AllocationStep::Dup` |
| `AllocationStep::Ftruncate` | Sizing the memfd to 24 bytes failed. | `crates/abacus-core/src/error.rs::AllocationStep::Ftruncate` |
| `AllocationStep::MemfdCreate` | `memfd_create` failed. | `crates/abacus-core/src/error.rs::AllocationStep::MemfdCreate` |
| `AllocationStep::Mmap` | Mapping the region failed. | `crates/abacus-core/src/error.rs::AllocationStep::Mmap` |
| `AllocationStep::Seal` | Applying file seals failed. | `crates/abacus-core/src/error.rs::AllocationStep::Seal` |
| `Condition::AllocationFailed` | Allocation failed at a named step with an errno. | `crates/abacus-core/src/error.rs::Condition::AllocationFailed` |
| `Condition::InterlockNotFound` | No registry entry for the name. | `crates/abacus-core/src/error.rs::Condition::InterlockNotFound` |
| `Condition::InterlockReaped` | The object is terminated. | `crates/abacus-core/src/error.rs::Condition::InterlockReaped` |
| `Condition::InvalidRequest` | Semantically rejected request, with a message. | `crates/abacus-core/src/error.rs::Condition::InvalidRequest` |
| `IoOperation` | Names the syscall that failed: `Bind`, `Listen`, `Accept`, `Connect`, `SendMsg`, `RecvMsg`, `Read`, `Write`, `Stat`, `Unlink`, `SetNonBlocking`, `SetTimeout`, `Chmod`, `Chown`, `Poll`. | `crates/abacus-core/src/error.rs::IoOperation` |
| `ProtocolFault::FrameTooLarge` | Declared or encoded length exceeds the maximum. | `crates/abacus-core/src/error.rs::ProtocolFault::FrameTooLarge` |
| `ProtocolFault::InvalidUtf8` | A length-prefixed string was not UTF-8. | `crates/abacus-core/src/error.rs::ProtocolFault::InvalidUtf8` |
| `ProtocolFault::MissingField` | Encoding a tier without its required field. | `crates/abacus-core/src/error.rs::ProtocolFault::MissingField` |
| `ProtocolFault::Truncated` | Needed more bytes or descriptors than were present. | `crates/abacus-core/src/error.rs::ProtocolFault::Truncated` |
| `ProtocolFault::UnexpectedFd` | A response carried more descriptors than its type allows. | `crates/abacus-core/src/error.rs::ProtocolFault::UnexpectedFd` |
| `ProtocolFault::UnknownTag` | Message tag byte is not part of v1. | `crates/abacus-core/src/error.rs::ProtocolFault::UnknownTag` |
| `ProtocolFault::UnsupportedVersion` | Leading version byte is not `PROTOCOL_VERSION`. | `crates/abacus-core/src/error.rs::ProtocolFault::UnsupportedVersion` |
| `SdkError::AllocationFailed` | Daemon reported allocation failure. | `crates/abacus-client/src/client.rs::SdkError::AllocationFailed` |
| `SdkError::InterlockNotFound` | Attach by name failed. | `crates/abacus-client/src/client.rs::SdkError::InterlockNotFound` |
| `SdkError::InterlockReaped` | The mapped object is terminated. | `crates/abacus-client/src/client.rs::SdkError::InterlockReaped` |
| `SdkError::InvalidRequest` | Rejected by the daemon or by the SDK before sending. | `crates/abacus-client/src/client.rs::SdkError::InvalidRequest` |
| `SdkError::MmapFailed` | Mapping a received descriptor failed. | `crates/abacus-client/src/client.rs::SdkError::MmapFailed` |
| `SdkError::RtsTimeout` | The daemon did not deliver within the fatal margin, under `TimeoutPolicy::Error`. | `crates/abacus-client/src/client.rs::SdkError::RtsTimeout` |
| `SdkError::Transport` | Wrapped `TransportError`. | `crates/abacus-client/src/client.rs::SdkError::Transport` |
| `SdkError::UnexpectedResponse` | Response type or error code the SDK does not recognize. | `crates/abacus-client/src/client.rs::SdkError::UnexpectedResponse` |
| `StartupError::Allocation` | Daemon startup failed allocating the clock or registry. | `crates/abacus-core/src/error.rs::StartupError::Allocation` |
| `StartupError::EventLoopFailed` | `ppoll` failed permanently with an errno. | `crates/abacus-core/src/error.rs::StartupError::EventLoopFailed` |
| `StartupError::Transport` | Daemon startup failed binding or configuring the socket. | `crates/abacus-core/src/error.rs::StartupError::Transport` |
| `TransportError::ConnectionClosed` | Peer closed the connection. | `crates/abacus-core/src/error.rs::TransportError::ConnectionClosed` |
| `TransportError::Io` | A syscall failed: operation plus errno. | `crates/abacus-core/src/error.rs::TransportError::Io` |
| `TransportError::Protocol` | A `ProtocolFault` surfaced through the transport. | `crates/abacus-core/src/error.rs::TransportError::Protocol` |
| `TransportError::SocketPathOccupied` | The path holds a live daemon or a non-socket object. | `crates/abacus-core/src/error.rs::TransportError::SocketPathOccupied` |

## Conventions (naming patterns)

| Pattern | Meaning | Example |
|---------|---------|---------|
| `area__claim` | Test name: subject area, two underscores, the behavior claimed. | `crates/abacus-core/src/tests/clock.rs::clock__futex_wait_returns_etimedout` |
| `role__name` | An `#[ignore]` test used as a child-process entry point, dispatched by name. | `crates/abacus-daemon/tests/daemon_clients.rs::role__hold_interlock` |
| `abuse__`, `timing__`, `soak__` | Ignored suites: hostile input, hardware timing, long-running. | `crates/abacus-tests/tests/abuse_wire.rs::abuse__random_bytes_on_the_socket_10k_frames` |
| `_ms` suffix | Milliseconds. Clock words and SDK intervals are milliseconds. | `crates/abacus-client/src/wait_cron.rs::WaitCron::interval_ms` |
| `_ns` suffix | Nanoseconds. Monotonic deadlines and wire cron intervals are nanoseconds. | `crates/abacus-core/src/interlock.rs::Interlock::expiration_ns` |
| `_us` suffix | Microseconds, used only in timing statistics and test thresholds. | `crates/abacus-tests/tests/timing_loop.rs::WAIT5_P99_LIMIT_US` |
| CAS-max | Compare-and-swap that only moves a word forward; a lower value is a no-op. | `crates/abacus-core/src/interlock.rs::interlock_arm` |
| `interlock_*` free functions | Core operations on a handle rather than methods, so daemon and SDK share them. | `crates/abacus-core/src/interlock.rs::interlock_free` |
| `interlock_map_*` per tier | One mapping function per tier so protection flags stay tier-specific. | `crates/abacus-core/src/interlock.rs::interlock_map_clock` |
| `create_*` / `attach_*` | Client verbs: `create` allocates and owns, `attach` maps an existing name. | `crates/abacus-client/src/client.rs::Abacus RTSClient::attach_interlock` |
| `TAG_` / `ERR_` prefixes | Wire discriminants: `TAG_` for message types, `ERR_` for error codes. | `crates/abacus-wire/src/codec.rs::ERR_INVALID_REQUEST` |
| `TIER_` prefix | Raw tier byte constants used by the test kit's raw client. | `crates/abacus-tests/src/lib.rs::TIER_WAIT_BARRIER` |
| `DEFAULT_` / `MIN_` / `MAX_` prefixes | Configured default, enforced floor, enforced ceiling. | `crates/abacus-client/src/types.rs::MIN_FATAL_MARGIN_MS` |
| `wait_*_for` suffix | Bounded variant of a wait: returns `Ok(None)` on timeout instead of blocking. | `crates/abacus-client/src/interlock.rs::Interlock::wait_open_for` |
| `ABACUS_TEST_SEED` | Environment variable making randomized test sequences replayable. | `crates/abacus-tests/src/lib.rs::Rng::from_env` |