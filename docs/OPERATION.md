# Operations

Running Abacus RTS as a service, what it needs from the host, how to read it, and the measured
numbers behind the defaults.

## Install

```bash
cargo build --release -p abacus-daemon
sudo install -m 0755 target/release/abacus /usr/local/bin/abacus
sudo install -m 0644 deploy/abacus-rts.service /etc/systemd/system/abacus-rts.service
sudo systemctl daemon-reload
sudo systemctl enable --now abacus-rts
systemctl status abacus-rts
```

The unit runs `abacus --socket-path=/run/abacus-rts/abacus.sock` with `RuntimeDirectory=`
creating `/run/abacus-rts`, `Restart=always` with a 100 ms delay, and `KillSignal=SIGTERM`.
On SIGTERM or SIGINT the daemon exits 0 within a millisecond and removes its socket file. A
stale socket file left by a SIGKILL is replaced on the next start; a live daemon on the path
makes the second instance exit 1.

## Socket path and permissions

Abacus RTS trusts every local process that can open its socket (CONTRACTS.md, Trust model). The
socket file is created and chmoded to `--socket-mode` (default 0660) after binding. To restrict it to a group:

```
ExecStart=/usr/local/bin/abacus --socket-path=/run/abacus-rts/abacus.sock --socket-group=abacus
```

`--socket-group` names a group that must exist; the daemon exits 1 if it does not. Clients
then need membership in that group. Containerized clients reach the socket by bind-mounting
`/run/abacus-rts` into the pod.

## Limits

Every interlock costs the daemon one file descriptor and one 24-byte mapping. The registry
cap is `--max-interlocks` (default 4096, excluding the clock); a create past the cap is
rejected with InvalidRequest. The unit sets `LimitNOFILE=16384` so the cap, the client
connections, and the fds in flight all fit. Names are 1 to 255 bytes.

## Scheduling and pinning

The daemon runs unpinned, SCHED_OTHER, and sets only its own timer slack (1 ns). Whether to
pin it to an isolated core with real-time priority is a deployment decision; the unit carries
the three lines commented out:

```
#CPUSchedulingPolicy=fifo
#CPUSchedulingPriority=50
#CPUAffinity=11
```

The numbers below were taken without them. Take the same numbers with them before committing
to the change: on a box with nothing else contending, the unpinned daemon already lands
timers on the boundary.

Pinning is not isolation. A daemon pinned to a shared core (one that other processes can
still be scheduled on) will stall past 10 ms about once in a few thousand waits. The
`CPUAffinity` line in the service unit must come with kernel-level isolation (`isolcpus` boot
parameter or a cpuset shield) to mean anything; without it, the OS scheduler can still place
other work on the pinned core, and the daemon's 1 ms evaluation loop stalls. The 50 ms
fatal-margin floor (`min_fatal_margin_ms`) is the defense until that kernel-level isolation
exists: it absorbs the scheduling jitter that pinning alone does not prevent.

## Reading the log

The daemon writes one line per event to stderr (`journalctl -u abacus-rts`):

| Line | Meaning |
|------|---------|
| `abacus: daemon running on <path>` | listening |
| `abacus: stopped` | clean exit after SIGTERM or SIGINT |
| `abacus: fatal: socket path occupied by a live daemon: <path>` | a second instance; exit 1 |
| `abacus: protocol fault, closing client: <fault>` | one malformed frame from one client; that connection is closed. One line per fault, never per cycle. |
| `abacus: read error, closing client: <errno>` | a client socket failed mid-read |
| `abacus: response error, closing client: <errno>` | a response could not be written for a reason other than a dead peer |
| `abacus: PR_SET_TIMERSLACK failed, errno=<n>` | timer slack stayed at the kernel default; timing degrades by up to 50 us |

A client that disconnects, is dropped for not reading its responses, or is dropped for a dead
socket produces no line. Interlocks are never logged: the registry is observable only through
the SDK.

## What a consumer sees when the daemon restarts

The daemon holds no durable state; a restart comes back empty. Existing mappings stay valid
(the memfds live as long as any holder maps them) but nothing evaluates them:

- A WaitTimer waiting through the restart hits its fatal margin, `max(2 * W, 50 ms)`: under
  `TimeoutPolicy::Abort` (default) the process aborts with an `RTSTimeout` line on stderr;
  under `TimeoutPolicy::Error` the call returns `Err(RtsTimeout)`.
- Every wait path (bare interlocks, WaitCron, WaitBarrier, and the clock) returns
  `InterlockReaped` within 100 ms (the clock's TTL). The clock interlock is daemon-owned and
  read-only to clients; after daemon death its expiration lapses and every wait loop detects it.
- A WaitCounter's `wait_until` returns `WaitState::Timeout` at its cadence and never delivers.
- The old connection is dead: `is_connected()` reads false and the next create or attach
  fails with `Transport(Io { errno: ETIMEDOUT })` or `ConnectionClosed`. Reconnect and recreate.

The design response to every case is the same: recreate and resume, or crash and let the
supervisor restart the consumer.

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

Reading the numbers: on an idle isolated core the loop lands on the boundary (median 4999 us
for a 5 ms wait). Under load the median is unchanged and the tail reaches 12 to 18 ms,
which is why the fatal margin has a 50 ms floor. The worst `wait_ms(5)` delivery seen under
stress was 16.9 ms, leaving 33.1 ms of margin. A noisier host needs a larger floor
(`Abacus RTSClient::set_min_fatal_margin_ms`) or the pinning lines above.

To reproduce: build release, start `target/release/abacus --socket-path=/tmp/abacus-probe.sock`,
then `cargo test --workspace --release --no-fail-fast -- --ignored --nocapture`. The timing
suite runs the production, stress, and idle profiles with proper core pinning. The probe program
(`examples/probe`) runs individual measurements interactively.
