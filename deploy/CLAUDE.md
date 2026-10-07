<purpose>
# deploy/
Systemd deployment assets for running the Abacus daemon as a sandboxed, unprivileged service with a dedicated system user, persistent runtime directory, Unix socket, restart policy, resource limits, and fixed CPU affinity.
</purpose>

<dependencies>
## Dependencies
- systemd and systemd-sysusers, verified against systemd 245.
- `/usr/local/bin/abacus`, including its `--socket-path` CLI, foreground operation, socket creation, and SIGTERM handling.
- Linux support for Unix sockets, memory locking, CPU affinity, and the configured sandbox directives.
- Host CPU 4 configured as an isolated CPU.
- Optional Docker ordering through `Before=docker.service`.
</dependencies>

<consumed-by>

## Consumed By
Static import scan (`deploy`): no in-repo consumers found.

</consumed-by>

<data-flow>
## Data Flow
- `abacus.sysusers` creates the `abacus` user and group before service startup.
- systemd creates `/run/abacus`, then launches `/usr/local/bin/abacus` with the socket path.
- The daemon creates a group-accessible Unix socket in the runtime directory.
- Process exits trigger unlimited restarts after 100 ms. SIGTERM shutdown has a two-second deadline.
</data-flow>

<known-hazards>
## Known Hazards
HIGH: `CPUAffinity=4` fails or misplaces the daemon when CPU 4 is absent or not isolated.
HIGH: Unlimited rapid restart can sustain a crash loop and repeated resource churn.
MEDIUM: Interlock capacity relies on an unverified one-descriptor-and-one-mapping-per-interlock model.
MEDIUM: Deployment behavior has no automated tests and depends on target-host verification.
MEDIUM: The service is coupled to the fixed binary path and socket CLI contract.
</known-hazards>

<files>
## Files
| File | Purpose |
|------|---------|
| `README.md` | Deployment contracts, verification status, unit rationale, and resource units. |
| `abacus.service` | Systemd unit defining daemon startup, supervision, sandboxing, limits, runtime directory, and CPU placement. |
| `abacus.sysusers` | systemd-sysusers entry creating the dedicated `abacus` user and group. |
</files>

<reference>
## Reference
See `README.md` for deployment contracts and verification details. See `docs/OPERATION.md` for installation commands and target timing data.
</reference>