# Operations

Running Abacus as a service, what it needs from the host, how to read it, and the measured
numbers behind the defaults.

## Install

```bash
cargo build --release -p abacus-daemon
sudo install -m 0755 target/release/abacus /usr/local/bin/abacus
sudo install -m 0644 deploy/abacus.service /etc/systemd/system/abacus.service
sudo systemctl daemon-reload
sudo systemctl enable --now abacus
systemctl status abacus
```

The unit runs `abacus --socket-path=/run/abacus/abacus.sock` with `RuntimeDirectory=`
creating `/run/abacus`, `Restart=always` with a 100 ms delay, and `KillSignal=SIGTERM`.
On SIGTERM or SIGINT the daemon exits 0 within a millisecond and removes its socket file. A
stale socket file left by a SIGKILL is replaced on the next start; a live daemon on the path
makes the second instance exit 1.

## Socket path and permissions

Abacus trusts every local process that can open its socket (INTERFACE.md, Trust model). The
socket file is created and chmoded to `--socket-mode` (default 0660) after binding. To restrict it to a group:

```
ExecStart=/usr/local/bin/abacus --socket-path=/run/abacus/abacus.sock --socket-group=abacus
```

`--socket-group` names a group that must exist; the daemon exits 1 if it does not. Clients
then need membership in that group. Containerized clients reach the socket by bind-mounting
`/run/abacus` into the pod.

## Limits

Every interlock costs the daemon one file descriptor and one 24-byte mapping. The registry
cap is `--max-interlocks` (default 4096, excluding the clock); a create past the cap is
rejected with InvalidRequest. The unit sets `LimitNOFILE=16384` so the cap, the client
connections, and the fds in flight all fit. Names are 1 to 255 bytes.

## Scheduling and pinning

The daemon sets its own timer slack (1 ns). The unit pins it to core 4 with real-time
priority:

```
CPUSchedulingPolicy=fifo
CPUSchedulingPriority=50
CPUAffinity=4
```

The numbers below were taken unpinned. Take the same numbers pinned before treating the
change as settled: on a box with nothing else contending, the unpinned daemon already lands
timers on the boundary.

Pinning is not isolation. A daemon pinned to a shared core (one that other processes can
still be scheduled on) will stall past 10 ms about once in a few thousand waits. The
`CPUAffinity` line in the service unit must come with kernel-level isolation (`isolcpus` boot
parameter or a cpuset shield) to mean anything; without it, the OS scheduler can still place
other work on the pinned core, and the daemon's 1 ms evaluation loop stalls. The 50 ms
fatal-margin floor (`min_fatal_margin_ms`) is the defense until that kernel-level isolation
exists: it absorbs the scheduling jitter that pinning alone does not prevent.

## Reading the log

The daemon writes one line per event to stderr (`journalctl -u abacus`):

| Line | Meaning |
|------|---------|
| `abacus: daemon running on <path>` | listening |
| `abacus: stopped` | clean exit after SIGTERM or SIGINT |
| `abacus: fatal: socket path occupied by a live daemon: <path>` | a second instance; exit 1 |
| `abacus: protocol fault, closing client: <fault>` | one malformed frame from one client; that connection is closed. One line per fault, never per cycle. |
| `abacus: read error, closing client: <errno>` | a client socket failed mid-read |
| `abacus: response error, closing client: <errno>` | a response could not be written for a reason other than a dead peer |
| `abacus: PR_SET_TIMERSLACK failed, errno=<n>` | timer slack stayed at the kernel default; timing degrades by up to 50 us |
| `abacus: warning: mlockall failed, errno=<n> (page faults possible in hot loop)` | memory stayed unlocked; the daemon runs, with possible page-fault jitter (raise `LimitMEMLOCK`) |

A client that disconnects, is dropped for not reading its responses, or is dropped for a dead
socket produces no line. Interlocks are never logged: the registry is observable only through
the SDK.

## What a consumer sees when the daemon restarts

The daemon holds no durable state; a restart comes back empty. Existing mappings stay valid
(the memfds live as long as any holder maps them) but nothing evaluates them, and the clock
stops being refreshed. What the consumer sees first depends on its `TimeoutPolicy`.

Under `TimeoutPolicy::Abort` (the default) the keepalive decides. Within the clock's TTL
(100 ms) plus one keepalive interval (40 ms) of the daemon's death it finds the Abacus clock
lapsed, writes `abacus: DaemonClockLapsed: ...` on stderr, and aborts the process. There is
nothing to reconnect: the supervisor restarts the consumer, which connects to the new daemon
and recreates its names. A WaitTimer whose margin is shorter than that window can abort first,
with a `DeliveryTimeout` line instead.

Under `TimeoutPolicy::Error` the process keeps running and every call reports the death:

- `client.liveness()` reads `Liveness::DaemonClockLapsed` once the keepalive sees the lapse.
- A WaitTimer waiting through the restart hits its fatal margin, `max(2 * W, 50 ms)`, and the
  call returns `Err(DeliveryTimeout)`.
- Every wait path that carries the clock (bare interlocks, WaitCron, WaitBarrier, and the
  clock) returns `InterlockReaped` within 100 ms (the clock's TTL). The clock interlock is
  daemon-owned and read-only to clients; after daemon death its expiration lapses and every
  such wait loop detects it.
- A WaitCounter's `wait_until` returns `WaitState::Timeout` at its cadence and never delivers.
- The old connection is dead: `is_connected()` reads false and the next create or attach
  fails with `Transport(Io { errno: ETIMEDOUT })` or `ConnectionClosed`. Reconnect, which
  creates a new ProcessClock, and recreate.

The design response is the same either way: recreate and resume, or crash and let the
supervisor restart the consumer. Abort makes the second choice for the consumer.

## Abort and crash handler latency

A client that aborts (ProcessClockReaped or DaemonClockLapsed under `TimeoutPolicy::Abort`)
calls `std::process::abort()`, which raises SIGABRT. On a host whose
`/proc/sys/kernel/core_pattern` pipes to a crash handler (Ubuntu's apport on the AGX), the
aborting process is not reaped by its parent until the handler finishes. Measured on the AGX
(5.10-tegra, apport enabled): 192 to 259 ms for a test binary, about 1.2 s for `sh`. The
daemon-side cascade does not wait for the client's exit; a supervisor's restart does. Disabling
apport eliminates the delay; the cost is losing crash reports.

## Measured numbers

Jetson AGX Orin, kernel 5.10.192-tegra, 12 cores (cores 4-11 isolated via `isolcpus`),
MAXN power mode, release profile. These are single-run samples; tail values (p99, max) vary
across runs. The thresholds in the timing test suite are set from the measured envelope with
margin.

**Idle (daemon on isolated core, no load)**

| Metric | n | p50 | p90 | p99 | max |
|--------|---|-----|-----|-----|-----|
| `wait_ms(5)` elapsed (us) | 2000 | 4999 | 5005 | 5020 | 5629 |
| `wait_ms(1)` elapsed (us) | 500 | 999 | 1008 | 1754 | 2031 |
| `wait_ms(20)` elapsed (us) | 300 | 19999 | 20009 | 20019 | 20022 |
| WaitCounter latency (us) | 500 | 953 | 959 | 963 | 964 |
| WaitBarrier latency (us) | 500 | 835 | 860 | 899 | 1062 |
| Cron 10 ms delta (ms) | 499 | 10 | 10 | 10 | 11 |
| Idle CPU over 5 s | | | | | < 1% |

**Production profile (2 load threads on cores 0-1, daemon isolated on core 4)**

| Metric | n | p50 | p90 | p99 | max |
|--------|---|-----|-----|-----|-----|
| `wait_ms(5)` elapsed (us) | 2000 | 4999 | 5003 | 6036 | 12546 |
| `wait_ms(2)` elapsed (us) | 2000 | 1999 | 2004 | 3039 | 12139 |
| `wait_ms(1)` elapsed (us) | 500 | 999 | 1008 | 2045 | 4385 |
| Cron 10 ms: 0 of 200 off grid | | | | | |

**Stress profile (44 load threads on cores 0-3/5-11, daemon isolated on core 4)**

| Metric | n | p50 | p90 | p99 | max |
|--------|---|-----|-----|-----|-----|
| `wait_ms(5)` elapsed (us) | 2000 | 4999 | 5001 | 5896 | 16878 |
| `wait_ms(2)` elapsed (us) | 2000 | 1999 | 2004 | 3738 | 18276 |
| Daemon CPU over 3 s | | | | | 0 ticks |

**1000 live interlocks**

| Metric | n | p50 | p90 | p99 | max |
|--------|---|-----|-----|-----|-----|
| `wait_ms(5)` elapsed (us) | 2000 | 4999 | 5010 | 5031 | 8264 |
| Daemon CPU over 10 s | | | | | 21 ticks |

**Burst recovery (2000 pipelined attach requests)**

Clock resumed advancing within 1.0 ms (one evaluation cadence). The burst took 27.9 ms total.

**Liveness cascade (process clock ownership and dependencies)**

Measured 2026-09-30 by `tests/timing_liveness.rs`, release, 3 runs of 20. Unlike the tables
above, daemon, harness, and child processes ran at default affinity (cores 0-3) and default
scheduling, power mode not recorded: these bound an unpinned deployment. Chain A <- B <- C,
C owning one interlock. Per run: random 0 to 40 ms delays before starting B and C (so the
keepalives do not share a touch phase), a random 0 to 80 ms delay before SIGKILLing A.

| Metric (ms) | n | min | p50 | max | Bound asserted |
|--------|---|-----|-----|-----|-----|
| SIGKILL of A to C's clock and interlock reaped | 60 | 161 | 183 to 185 | 200 | TTL + 3 + 10 = 213 |
| C's clock reaped to C's `ProcessClockReaped` line | 60 | 0 | 22 to 26 | 38 | interval + 10 = 50 |
| C's clock reaped to C's process exit (apport enabled) | 60 | 198 | 232 to 233 | 270 | none (host crash handler) |

The reap floor is TTL minus one touch interval (the kill lands anywhere in A's last interval);
the detection spread is one touch interval. Exit is dominated by apport; see "Abort and crash
handler latency".

Reading the numbers: on an idle isolated core the loop lands on the boundary (median 4999 us
for a 5 ms wait). Under load the median is unchanged and the tail reaches 12 to 18 ms,
which is why the fatal margin has a 50 ms floor. The worst `wait_ms(5)` delivery seen under
stress was 16.9 ms, leaving 33.1 ms of margin. A noisier host needs a larger floor
(`AbacusClient::set_min_fatal_margin_ms`) or the pinning lines above.

To reproduce: build release, start `target/release/abacus --socket-path=/tmp/abacus-probe.sock`,
then `cargo test --workspace --release --no-fail-fast -- --ignored --nocapture`. The timing
suite runs the production, stress, and idle profiles with proper core pinning. The probe program
(`examples/probe`) runs individual measurements interactively.
