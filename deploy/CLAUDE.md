<purpose>

# deploy/

Defines the systemd deployment and supervision policy for the Abacus coordination daemon.

</purpose>

<dependencies>

## Dependencies
- systemd, including `local-fs.target`, runtime-directory management, restart supervision, and resource-limit directives.
- The external daemon executable at `/usr/local/bin/abacus`, which must accept `--socket-path`.

</dependencies>

<consumed-by>

## Consumed By
Installed by the deployer. Not consumed by other crates.

</consumed-by>

<data-flow>

## Data Flow
- systemd reads `abacus-rts.service` when the unit is installed and enabled.
- systemd creates `/run/abacus-rts` with mode `0755`, then launches `/usr/local/bin/abacus --socket-path=/run/abacus-rts/abacus.sock`.
- The daemon exposes its Unix-domain socket at the configured path.
- Process exits cause systemd to restart the daemon after 100 ms; stop requests deliver `SIGTERM` with a two-second shutdown deadline.

</data-flow>

<known-hazards>

## Known Hazards
HIGH: `Restart=always` with a 100 ms delay can create a persistent rapid restart loop for configuration errors, clean exits, or a missing/incompatible executable.
HIGH: The unit hard-codes both the executable location and socket-path CLI contract; installation-path or argument changes make deployment fail at runtime.
MEDIUM: `TimeoutStopSec=2` may force-kill the daemon before cleanup completes if graceful shutdown takes longer than two seconds.
MEDIUM: The runtime directory is mode `0755`; socket confidentiality and authorization therefore depend on socket permissions established by the daemon.

</known-hazards>

<files>

## Files
| File | Purpose |
|------|---------|
| `abacus-rts.service` | Configures systemd startup, runtime-directory creation, resource limits, restart behavior, shutdown handling, and optional real-time scheduling for the daemon. |

</files>

<notes>

## Notes
The real-time scheduling and CPU-affinity settings are intentionally disabled until operational probes justify deployment-specific pinning.

The file-descriptor limit assumes approximately one descriptor per interlock and leaves headroom above the documented default capacity.

</notes>

<reference>

## Reference
Full contracts, units, symbol table, test inventory, and cross-boundary verification: see `README.md`.

</reference>
