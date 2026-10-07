<purpose>
# examples/
Executable integration and operational examples for exercising a live Abacus daemon: pipeline coordination, protocol probing, real-time measurement, and cross-container synchronization.
</purpose>

<dependencies>
## Dependencies
Imports workspace crates `abacus_client`, `abacus_core`, and `abacus_tests`. Uses `libc` for Linux scheduling and affinity in `rtmeasure.rs`. Connects to a running daemon through a Unix socket.
</dependencies>

<consumed-by>

## Consumed By
Static import scan, directories importing `examples`:
- `docs`

(Real import edges, not inferred. A cross-service entry here is a reach into this directory's internals, a coupling to flag.)

</consumed-by>

<data-flow>
## Data Flow
- CLI arguments provide daemon socket paths, workload parameters, and output locations.
- Examples create or attach daemon interlocks, wait primitives, and process clocks through `abacus_client`.
- `pod.rs` and `xcontainer.rs` coordinate events across threads or processes.
- `probe.rs` prints daemon timing and protocol-behavior results.
- `rtmeasure.rs` writes environment metadata, raw samples, and summaries to its output directory.
</data-flow>

<known-hazards>
## Known Hazards
- HIGH: `rtmeasure.rs` assumes a specific Linux real-time environment and fixed CPU layout, changes thread scheduling to `SCHED_FIFO`, and requires `SYS_NICE`.
- HIGH: `rtmeasure.rs` load and soak modes intentionally saturate cores and can disrupt colocated workloads.
- MEDIUM: `probe.rs` `partial` mode deliberately holds an incomplete Unix-socket frame open, consuming a daemon connection during the test.
- MEDIUM: `xcontainer.rs` defaults to `TimeoutPolicy::Abort`, so daemon or producer failure terminates the process.
</known-hazards>

<files>
## Files
| File | Purpose |
|------|---------|
| `pod.rs` | Three-thread camera, inference, and coordinator pipeline using cumulative WaitCounter events. |
| `probe.rs` | Live-daemon benchmark, cron, barrier, and partial-frame protocol probe CLI. |
| `rtmeasure.rs` | Linux real-time sweep and keepalive-soak measurement harness with CSV and summary output. |
| `xcontainer.rs` | Producer and consumer example for daemon-mediated coordination across containers. |
</files>

<reference>
## Reference
`docs/OPERATION.md` records operational measurements produced by `probe.rs` and `rtmeasure.rs`.
</reference>