<purpose>

# examples/

Operational probe binary and the pod topology example. The probe exercises a live daemon and reports timing, cron, barrier, and partial-request behavior. The pod example shows a three-thread processing pipeline coordinated by WaitCounters.

</purpose>

<dependencies>

## Dependencies
- External standard library facilities: command-line arguments, timing, threads, Unix-domain sockets, and byte writes.
- Sibling/workspace crate `abacus_client`: `AbacusClient`, `WaitState`, and `WatchedWord`.

</dependencies>

<consumed-by>

## Consumed By
Run manually against a live daemon. Not consumed by other crates.

</consumed-by>

<data-flow>

## Data Flow
- Command-line arguments enter `main` as a probe mode, Unix-socket path, and optional numeric parameters.
- `bench`, `cron`, and `barrier` create client-side Abacus objects through `abacus_client` and wait for daemon-produced completion results.
- Timing samples and completion states are collected locally, sorted, summarized as percentile/statistical output, and written to stdout.
- `partial` opens a raw Unix socket, writes two bytes of an assumed four-byte frame-length prefix, keeps the connection open for eight seconds, then closes it to probe incomplete-request handling.

</data-flow>

<known-hazards>

## Known Hazards
- HIGH: `bench` permits `n=0`, then indexes empty percentile/sample vectors (`elapsed[n - 1]`), causing a panic rather than producing a valid empty benchmark result.
- HIGH: `cron` permits `n <= 1`, leaving `deltas` empty and causing indexing/percentile panics; `interval_ms=0` additionally causes a modulo-by-zero panic.
- MEDIUM: `partial` hard-codes `[7, 0]` as two bytes of a four-byte length prefix without importing or validating the daemon protocol framing contract; protocol changes can invalidate the probe.
- MEDIUM: Benchmark and cron names are based only on the process ID, so repeated object creation in the same process or daemon-side persistence/name collisions can make probe runs fail.
- LOW: Percentile calculation uses a truncated index rather than an interpolated percentile definition, so reported p90/p99 values are sample-order approximations.

</known-hazards>

<files>

## Files
| File | Purpose |
|------|---------|
| `probe.rs` | CLI probe that connects to an Abacus daemon and measures wait timers, cron waits, barriers, and partial socket-frame behavior. |
| `pod.rs` | Three-thread processing pipeline showing producer/consumer/coordinator coordination via WaitCounters. |

</files>

<notes>

## Notes
The probe is operational tooling rather than a test harness: its module documentation states that measurements reported in `docs/OPERATION.md` originate from it.

</notes>

<reference>

## Reference

</reference>
