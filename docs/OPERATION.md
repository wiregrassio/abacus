# Operations

Running Abacus as a service, what it needs from the host, how to read it, and the measured
numbers behind the defaults.

## Install

```bash
cargo build --release -p abacus-daemon
sudo mkdir -p /etc/sysusers.d
sudo install -m 0644 deploy/abacus.sysusers /etc/sysusers.d/abacus.conf
sudo systemd-sysusers
sudo install -m 0755 target/release/abacus /usr/local/bin/abacus
sudo install -m 0644 deploy/abacus.service /etc/systemd/system/abacus.service
sudo systemctl daemon-reload
sudo systemctl enable --now abacus
systemctl status abacus
```

Some distributions (Ubuntu 20.04 among them) ship no `/etc/sysusers.d`; the `mkdir` creates
it. `systemd-sysusers` creates the `abacus` system user and group the unit runs as.

The unit runs `abacus --socket-path=/run/abacus/abacus.sock` as `abacus`, with no
capabilities, `NoNewPrivileges`, a read-only file system outside its runtime directory, a
private `/tmp`, and Unix sockets only. `RuntimeDirectory=` creates `/run/abacus`, and
`RuntimeDirectoryPreserve=yes` keeps it across restarts and stops until reboot, so container
bind mounts of the directory stay live. The unit is ordered `Before=docker.service`
(ordering only): at boot the directory exists before Docker mounts it, and at shutdown
containers stop before the daemon instead of aborting on a lapsed Abacus clock.

`Restart=always` with a 100 ms delay and `StartLimitIntervalSec=0`: systemd never stops
restarting the daemon, because every real-time process dies with it and a unit that gives up
keeps the whole line down. The cost is that a configuration error restarts about ten times a
second until it is fixed; the journal shows the failing step (a missing `abacus` user is
`status=217/USER`). On SIGTERM or SIGINT the daemon exits 0 within a millisecond and removes
its socket file. A stale socket file left by a SIGKILL is replaced on the next start; a live
daemon on the path makes the second instance exit 1.

## Socket path and permissions

Abacus trusts every local process that can open its socket (INTERFACE.md, Trust model). The
unit's `UMask=0117` creates the socket 0660, owned `abacus:abacus`, so it never accepts
connections at looser permissions; the daemon then applies `--socket-mode` (default 0660).
Host clients need membership in the `abacus` group (`sudo usermod -aG abacus <user>`, effective
at the next login). To use another group:

```
ExecStart=/usr/local/bin/abacus --socket-path=/run/abacus/abacus.sock --socket-group=<group>
```

`--socket-group` names a group that must exist; the daemon exits 1 if it does not.

Containerized clients bind-mount the directory `/run/abacus`, never the socket file: a restart
creates a new socket inode at the same path, and a file bind mount keeps pointing at the dead
one. Root in a container reaches the 0660 socket through `CAP_DAC_OVERRIDE` (in Docker's
default set); a non-root container user needs the `abacus` group's gid (`group_add`).

## Limits

Every interlock costs the daemon one file descriptor and one 24-byte mapping. The registry
cap is `--max-interlocks` (default 4096, excluding the clock); a create past the cap is
rejected with InvalidRequest. The connection cap is `--max-clients` (default 256); at the
cap, new connections wait in the kernel backlog until a client disconnects. If the process
hits its descriptor limit, the listener pauses until a disconnect frees a
descriptor. The unit sets `LimitNOFILE=16384` so the cap, the client connections, and the
fds in flight all fit. Names are 1 to 255 bytes.

## Timing contract

Abacus timing is best effort, at the level of an industrial controller: deliveries land
within single-digit milliseconds of their boundary, with occasional spikes. The claims hold
only when the daemon runs alone on an isolated core and every real-time client runs on its
own isolated core. Elsewhere Abacus works, with no timing claims.

One place a spike is not harmless. A keepalive stall longer than its TTL, a daemon stall
longer than the Abacus clock's TTL, or a delivery later than a fatal margin terminates by
design. TTLs and margins are therefore sized above the worst measured spike, not the median.

## Scheduling and pinning

The daemon sets its own timer slack (1 ns) and locks its memory. The unit pins it to core 4
at the default scheduling policy:

```
CPUAffinity=4
```

The daemon is meant to be the only task on an isolated core, where only per-core kernel
threads and interrupts compete with it, so SCHED_FIFO buys little. A host that wants it adds
`CPUSchedulingPolicy=fifo`. On kernels built with `CONFIG_RT_GROUP_SCHED` (NVIDIA's Jetson
kernels among them) every cgroup but the root starts with no real-time runtime, and the
kernel refuses SCHED_FIFO there; such a host also needs `kernel.sched_rt_runtime_us=-1` or
real-time runtime for `system.slice`.

Pinning is not isolation. A daemon pinned to a shared core (one that other processes can
still be scheduled on) will stall past 10 ms about once in a few thousand waits. The
`CPUAffinity` line in the service unit must come with kernel-level isolation (`isolcpus` boot
parameter or a cpuset shield) to mean anything; without it, the OS scheduler can still place
other work on the pinned core, and the daemon's 1 ms evaluation loop stalls. The 100 ms
fatal-margin floor (`min_fatal_margin_ms`, equal to the daemon clock TTL) is the defense
until that kernel-level isolation exists: a WaitTimer's delivery-lateness tolerance is always
at least the clock TTL, so it never gives up before the keepalive's clock-lapse check does.

### Host tuning

The timing contract depends on host settings beyond `isolcpus`. Two files in `deploy/` make
them persistent across reboots:

```bash
sudo install -m 0644 deploy/90-abacus.conf /etc/sysctl.d/
sudo install -m 0644 deploy/abacus-host-tuning.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable abacus-host-tuning
sudo sysctl --system
sudo systemctl start abacus-host-tuning
```

`90-abacus.conf` (sysctl.d): disables RT bandwidth control (`sched_rt_runtime_us=-1`) so
SCHED_FIFO works in containers without per-cgroup budgets, moves the lockup watchdog off
isolated cores, and reduces `vm.stat_interval` to 10 s.

`abacus-host-tuning.service` (oneshot, Before=abacus.service): sets the default IRQ affinity
to cores 0-3, pins `eth0` IRQs to core 5 by name (stable across boots, unlike IRQ numbers),
and sets THP to `madvise`.

Both are non-destructive: the sysctl values are runtime-only (a reboot without the files
reverts to defaults), and the oneshot unit does nothing that a manual reboot undoes.

Boot parameters for the full deployment (in `/boot/extlinux/extlinux.conf` on Jetson, requires
a reboot):

```
isolcpus=managed_irq,domain,4-11 irqaffinity=0-3 skew_tick=1
```

`managed_irq` prevents the kernel from placing IRQs on isolated cores. `domain` removes them
from the scheduler's load-balancing domains. `skew_tick=1` offsets each core's timer tick to
reduce cross-core cache-line contention. These are stronger than the current `isolcpus=4-11`
alone and are recommended for production.

### Real-time clients

A client that runs SCHED_FIFO work on its core raises its keepalive above that work with
`AbacusClient::set_keepalive_priority`, before raising its own threads: a keepalive at the
default policy starves behind a SCHED_FIFO loop on its core, its ProcessClock lapses, and a
healthy process dies. The keepalive runs on the CPUs of the thread that called `connect`, so
pin that thread first. Containers that use SCHED_FIFO need `cap_add: [SYS_NICE]` and an
`rtprio` ulimit, and on `CONFIG_RT_GROUP_SCHED` kernels real-time runtime in Docker's cgroup
(`kernel.sched_rt_runtime_us=-1`, or Docker's `cpu-rt-runtime` with a per-container
`cpu_rt_runtime`); without it the kernel refuses with `EPERM` even with `SYS_NICE`.

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
| `abacus: heartbeat write failed: <path>: <errno>` | the heartbeat file could not be written; logged once on the transition from success to failure |
| `abacus: heartbeat write recovered: <path>` | the heartbeat file is writable again after a failure |
| `abacus: heartbeat remove failed: <path>: <errno>` | the heartbeat file could not be removed on shutdown (NotFound is not reported) |

A client that disconnects, is dropped for not reading its responses, or is dropped for a dead
socket produces no line. Interlocks are never logged: the registry is observable only through
the SDK.

## Heartbeat

The daemon writes `<socket-path>.heartbeat` once per second with the current epoch
timestamp. With the default socket path that is `/run/abacus/abacus.sock.heartbeat`.
Each daemon writes its own file, so two daemons on different sockets in the same
directory never collide. External health checks can verify the daemon is alive without
connecting to the socket:

```
test $(($(date +%s) - $(cat /run/abacus/abacus.sock.heartbeat))) -lt 5
```

Docker HEALTHCHECK example:

```dockerfile
HEALTHCHECK --interval=10s --timeout=2s --retries=3 \
  CMD test $(($(date +%s) - $(cat /run/abacus/abacus.sock.heartbeat))) -lt 5
```

The file is removed on clean shutdown. After a SIGKILL it remains with a stale
timestamp; the age check detects this within seconds. A write failure (read-only
directory, full disk) is logged once on the transition to failure and once on recovery;
the daemon continues evaluating interlocks.

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
Re-measured 2026-10-06 on the AGX with the daemon on isolated core 4: recovery in 760 us,
burst in 28.4 ms.

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

**Daemon restart under systemd**

Measured 2026-10-06 on the AGX (L4T R35.5, kernel 5.10.192-tegra, `isolcpus=4-11`), the unit as
shipped, from journal timestamps, 4 SIGKILL cycles. The daemon is listening again 160 to
215 ms after a SIGKILL: systemd schedules the restart 154 to 206 ms after the exit
(`RestartSec=100ms` plus systemd's own latency on this host), and the new process is
listening 7 to 8 ms after it starts. A `systemctl stop` exits cleanly in 3 ms.

**Isolated-core decomposition (`examples/rtmeasure`, daemon on core 4 via systemd)**

Measured 2026-10-06 on the AGX (L4T R35.5, kernel 5.10.192-tegra, `isolcpus=4-11`, MAXN,
clocks pinned, idle states disabled, `sched_rt_runtime_us=-1`, IRQs 70-75 on core 5,
workqueue cpumask 0-3). Daemon at SCHED_OTHER alone on core 4 via `abacus.service`. Spinner
on core 6 and waiter on core 7, both SCHED_FIFO 80. 60 s per cell, 60,001 samples each.
Cyclictest baseline: max 4-5 us on cores 6-11 over 60 s.

Clock lateness: how far past each millisecond boundary the daemon stamps the clock word.
This is the daemon's own jitter (wake from `ppoll` plus the atomic store).

| Entries | Load | n | p50 us | p99 us | p999 us | max us |
|---------|------|---|--------|--------|---------|--------|
| 10 | idle | 60001 | 4.3 | 7.8 | 9.3 | 12.1 |
| 100 | idle | 60001 | 4.5 | 8.3 | 9.6 | 13.3 |
| 1000 | idle | 60001 | 5.3 | 7.9 | 9.4 | 17.2 |
| 4000 | idle | 60001 | 5.5 | 8.1 | 9.7 | 17.4 |
| 10 | saturated | 60001 | 4.1 | 5.1 | 5.5 | 13.0 |
| 100 | saturated | 60001 | 4.3 | 5.3 | 5.8 | 12.6 |
| 1000 | saturated | 60001 | 5.2 | 6.6 | 7.0 | 101.7 |
| 4000 | saturated | 60001 | 5.4 | 7.2 | 7.6 | 17.8 |

Delivery lateness: how far past the millisecond boundary a WaitTimer delivery is stamped.
This is the clock lateness plus the evaluation sweep up to the timer's slot (the timer is
placed last in the registry, so its delivery is the worst case).

| Entries | Load | n | p50 us | p99 us | p999 us | max us |
|---------|------|---|--------|--------|---------|--------|
| 10 | idle | 60001 | 5.0 | 8.6 | 10.1 | 12.9 |
| 100 | idle | 60001 | 5.3 | 9.0 | 10.3 | 14.1 |
| 1000 | idle | 60001 | 29.2 | 32.1 | 33.6 | 42.0 |
| 4000 | idle | 60001 | 130.4 | 136.2 | 138.9 | 144.2 |
| 10 | saturated | 60001 | 4.3 | 5.3 | 5.8 | 13.6 |
| 100 | saturated | 60001 | 5.2 | 6.4 | 7.0 | 13.8 |
| 1000 | saturated | 60001 | 34.0 | 36.6 | 37.5 | 132.1 |
| 4000 | saturated | 60001 | 127.0 | 138.9 | 142.3 | 147.8 |

Stamp to running: time from the daemon's delivery stamp to the waiter thread scheduled after
the futex wake. Pure OS scheduler overhead.

| Entries | Load | n | p50 us | p99 us | p999 us | max us |
|---------|------|---|--------|--------|---------|--------|
| 10-4000 | idle | 60001 | 4.0-5.0 | 5.1-5.7 | 5.6-6.0 | 6.3-7.7 |
| 10-4000 | saturated | 60001 | 4.0-5.1 | 4.6-5.8 | 4.9-6.1 | 5.5-7.9 |

Clock lateness is flat across entry counts (the daemon stamps once before evaluation). The
saturation load (four threads copying 32 MiB buffers on cores 0-3) does not leak into the
daemon's isolated core at p50 or p99. The occasional spike to 100 us (one sample at 1000
entries saturated) is consistent with a cache-line transfer or an unmoved IRQ. Delivery
lateness scales linearly with entries: the daemon evaluates sequentially, adding roughly 30 us
per 1000 entries. Idle and saturated curves track closely (idle 1000: p50 29.2 us, saturated
1000: p50 34.0 us), confirming that memory-bandwidth pressure on cores 0-3 does not
meaningfully affect evaluation. In the deployment range (10 to 100 entries), worst delivery
lateness is 14 us. Nothing crossed 100 us except the 1000 and 4000 entry headroom cells
(evaluation cost, not jitter).

Zero `spinner_late`, `clock_skips`, `timeouts`, or `unmatched` samples in any cell.

**Keepalive soak (SCHED_FIFO, Convoy-shaped core)**

Measured 2026-10-06 on the same host, daemon via systemd on core 4. On core 11: keepalive
at SCHED_FIFO 80, capture thread at SCHED_FIFO 90 (3 ms busy every 30 ms), inference thread
at SCHED_FIFO 70 (25 ms busy, 1 ms idle), ProcessClock TTL 100 ms. Soak survived 600 s. The
100 ms TTL holds on a Convoy-shaped core with a SCHED_FIFO keepalive.

Reading the numbers: on an idle isolated core the loop lands on the boundary (median 4999 us
for a 5 ms wait, 4.3 us clock lateness). Under load the median is unchanged and the tail
reaches 12 to 18 ms for `wait_ms` (which includes client-side futex and scheduling) while the
daemon's own contribution stays under 18 us. The 100 ms fatal-margin floor equals the
daemon clock TTL; it is set by the consistency requirement that a WaitTimer never gives up
before the keepalive's clock-lapse check, not by jitter measurements.

To reproduce: build release, start `target/release/abacus --socket-path=/tmp/abacus-probe.sock`,
then `cargo test --workspace --release --no-fail-fast -- --ignored --nocapture`. The timing
suite runs the production, stress, and idle profiles with proper core pinning. The `rtmeasure`
example (`examples/rtmeasure`) decomposes the daemon's contribution from the scheduling
overhead; it runs in a container with `SYS_NICE` against the systemd service. The probe program
(`examples/probe`) runs individual measurements interactively.
