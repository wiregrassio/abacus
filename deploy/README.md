# deploy/

This directory contains the systemd deployment unit for the Abacus daemon. It creates the daemon's runtime directory, starts the installed binary with a Unix-socket path, and restarts the process when it exits.

Install the unit through the host's normal systemd unit path, reload systemd, and enable or start `abacus.service`. The binary must be installed at `/usr/local/bin/abacus`.

The unit pins the daemon to core 4 with SCHED_FIFO priority 50. Those settings are host-specific: they assume the target boots with `isolcpus` covering that core. Measure with and without before treating them as settled.

<contracts>

## Contracts
- `/usr/local/bin/abacus` exists, is executable, and accepts `--socket-path=/run/abacus/abacus.sock`.
- The daemon remains in the foreground because `Type=simple` treats the launched process as the service.
- systemd creates `/run/abacus` before process startup and removes the runtime directory according to systemd runtime-directory lifecycle rules.
- The daemon must create and manage `abacus.sock` within the supplied runtime directory.
- The daemon must handle `SIGTERM` and finish shutdown within two seconds to avoid forced termination.
- The process receives a soft and hard file-descriptor limit of 16,384.
- Every process exit, including a successful exit, triggers restart after 100 ms.
- FIFO scheduling requires host permissions and kernel/systemd support; CPU affinity value `4` assumes that CPU exists and is kernel-isolated.
- The stated interlock capacity depends implicitly on the daemon's one-file-descriptor-and-one-mapping-per-interlock resource model.

</contracts>

<units-table>

## Units Table
| Symbol | Unit | Enforced By |
|--------|------|-------------|
| `LimitNOFILE` | file descriptors | systemd resource limit |
| `RestartSec` | milliseconds | systemd duration parser |
| `TimeoutStopSec` | seconds | systemd stop supervision |
| `RuntimeDirectoryMode` | Unix permission bits (octal) | systemd |
| `CPUSchedulingPriority` | FIFO scheduling priority | systemd/kernel |
| `CPUAffinity` | logical CPU index | systemd/kernel |

</units-table>

<test-inventory>

## Test Inventory
No test files are supplied for this directory.

- Unit-file syntax and systemd loading: NO TEST
- Runtime-directory creation and permissions: NO TEST
- Executable path and CLI compatibility: NO TEST
- Socket creation and accessibility: NO TEST
- Restart-loop behavior: NO TEST
- SIGTERM shutdown within two seconds: NO TEST
- File-descriptor capacity assumptions: NO TEST
- FIFO scheduling and CPU affinity: NO TEST

</test-inventory>

<cross-boundary-verification>

## Cross-Boundary Verification
- systemd unit syntax and directive support: NOT VERIFIED.
- `/usr/local/bin/abacus` installation path: NOT VERIFIED.
- `--socket-path` command-line contract: NOT VERIFIED.
- Daemon foreground behavior required by `Type=simple`: NOT VERIFIED.
- Daemon creation of `/run/abacus/abacus.sock`: NOT VERIFIED.
- Daemon SIGTERM handling within two seconds: NOT VERIFIED.
- One descriptor and one mapping per interlock: NOT VERIFIED.
- Host support for FIFO priority and CPU affinity: NOT VERIFIED.

</cross-boundary-verification>

<symbol-table>

## Symbol Table
### abacus.service
| Symbol | Kind | Purpose | Rationale |
|--------|------|---------|-----------|

</symbol-table>
