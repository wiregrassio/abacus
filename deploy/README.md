# deploy/

This directory contains the systemd deployment for the Abacus daemon: the unit `abacus.service`, which runs the installed binary as a dedicated user under a sandbox, keeps its runtime directory across restarts, and restarts the process whenever it exits; and `abacus.sysusers`, which creates that user.

Install both through the host's normal systemd paths, run `systemd-sysusers`, reload systemd, and enable `abacus.service`; `docs/OPERATION.md` has the exact commands. The binary must be installed at `/usr/local/bin/abacus`.

The unit pins the daemon to core 4 at the default scheduling policy. That is host-specific: it assumes the target boots with `isolcpus` covering core 4 and places nothing else there.

<contracts>

## Contracts
- `/usr/local/bin/abacus` exists, is executable, and accepts `--socket-path=/run/abacus/abacus.sock`.
- The `abacus` user and group exist before the unit starts (`abacus.sysusers`); without them the unit fails at the USER step.
- The daemon remains in the foreground because `Type=simple` treats the launched process as the service.
- systemd creates `/run/abacus` (mode 0755, owned by `abacus`) before process startup and, with `RuntimeDirectoryPreserve=yes`, removes it only at reboot.
- The daemon creates and manages `abacus.sock` within the runtime directory; `UMask=0117` makes it 0660 from creation.
- The daemon handles `SIGTERM` and finishes shutdown within two seconds to avoid forced termination.
- The process receives a soft and hard file-descriptor limit of 16,384 and an unlimited locked-memory limit.
- Every process exit, including a successful exit, triggers restart after 100 ms, without limit (`StartLimitIntervalSec=0`).
- The daemon needs no capability; the bounding set is empty and `NoNewPrivileges` is set.
- CPU affinity value `4` assumes that CPU exists and is kernel-isolated.
- The stated interlock capacity depends implicitly on the daemon's one-file-descriptor-and-one-mapping-per-interlock resource model.

</contracts>

<units-table>

## Units Table
| Symbol | Unit | Enforced By |
|--------|------|-------------|
| `LimitNOFILE` | file descriptors | systemd resource limit |
| `RestartSec` | milliseconds | systemd duration parser |
| `StartLimitIntervalSec` | seconds (0 disables the start limit) | systemd |
| `TimeoutStopSec` | seconds | systemd stop supervision |
| `RuntimeDirectoryMode` | Unix permission bits (octal) | systemd |
| `UMask` | Unix permission bits (octal), masked off new files | systemd/kernel |
| `CPUAffinity` | logical CPU index | systemd/kernel |

</units-table>

<test-inventory>

## Test Inventory
No test files are supplied for this directory. Verified by hand on the target (Jetson AGX Orin, 2026-10-06): unit syntax (`systemd-analyze verify`), user, sandbox, affinity, socket mode, restart after SIGKILL, directory preserved across restart and stop, no start limit after repeated kills (`docs/OPERATION.md` has the restart timings).

- Unit-file syntax and systemd loading: NO AUTOMATED TEST
- Runtime-directory creation, permissions, and preservation: NO AUTOMATED TEST
- Socket creation mode: NO AUTOMATED TEST
- Restart-loop behavior: NO AUTOMATED TEST
- SIGTERM shutdown within two seconds: NO AUTOMATED TEST
- File-descriptor capacity assumptions: NO AUTOMATED TEST
- CPU affinity: NO AUTOMATED TEST

</test-inventory>

<cross-boundary-verification>

## Cross-Boundary Verification
- systemd unit syntax and directive support: verified on systemd 245.
- `/usr/local/bin/abacus` installation path: verified by hand.
- `--socket-path` command-line contract: verified by hand.
- Daemon creation of `/run/abacus/abacus.sock` at 0660: verified by hand.
- Daemon SIGTERM handling within two seconds: verified by hand (3 ms).
- One descriptor and one mapping per interlock: NOT VERIFIED.

</cross-boundary-verification>

<symbol-table>

## Symbol Table
### abacus.service
| Symbol | Kind | Purpose | Rationale |
|--------|------|---------|-----------|
| `Before=docker.service` | ordering | start before Docker, stop after it | the socket directory exists before containers mount it; containers stop before the daemon |
| `StartLimitIntervalSec=0` | restart policy | never stop restarting | every real-time process dies with the daemon |
| `User=abacus` | identity | run unprivileged | the daemon needs no privilege |
| `RuntimeDirectoryPreserve=yes` | lifecycle | keep `/run/abacus` across restart and stop | container bind mounts of the directory stay live |
| `UMask=0117` | permissions | socket born 0660 | no window at looser permissions before `--socket-mode` |
| `CPUAffinity=4` | placement | pin to the isolated core | the daemon is the only task there |

### abacus.sysusers
| Symbol | Kind | Purpose | Rationale |
|--------|------|---------|-----------|
| `u abacus` | sysusers entry | system user and group `abacus` | the unit's `User=` and `Group=`; clients join the group to reach the socket |

</symbol-table>
