# Philosophy

This is the project's constitution. Abacus RTS is a coordination plane for real-time compute: one
shared-memory primitive, a daemon that evaluates it on a 1 ms cadence, and a typed SDK
over both. Every architectural choice in this repository traces to one of the laws below. A design
that contradicts a law is a bug in the design, not an argument to relax the law.

The context that produced these rules: processes on one host that must hand off work at frame
cadence, where a missed wake is a dropped frame and a stuck wait is a hung pipeline. In that
setting a coordination service that can hang, lie, or come back holding stale opinions is worse
than one that is simply absent. The laws fall out of taking that seriously, and they are the
Apollo guidance posture applied to userspace: few states, all of them known, and a restart that
lands in a correct one every time.

<one-primitive>

## 1. One primitive, five contracts

**There is exactly one shared object. Every wait type is a contract layered over it by daemon
evaluation and SDK handles, never a second primitive.**

The primitive is `Interlock`: three `AtomicU64` words in a sealed memfd, `open_count`,
`closed_count`, `expiration_ns`, asserted at compile time to be exactly `INTERLOCK_SIZE` (24)
bytes at offsets 0, 8, 16. The daemon's five tiers (`Tier::Interlock`, `WaitCounter`, `WaitTimer`,
`WaitCron`, `WaitBarrier`) are branches in one `evaluate_all` pass over one registry of one record
type. The clock is not special-cased infrastructure: it is an interlock named `clock` at registry
ID 0, whose `open_count` is the current monotonic millisecond and whose `closed_count` is the
daemon start time. A timer is a counter watching the clock. A cron is a timer that re-arms.

The closed operation set on that primitive is small and deliberate:
`interlock_create`, `interlock_map`, `interlock_arm`, `interlock_reap`, `interlock_free`,
`interlock_is_terminated`, `interlock_dup_fd`, plus `futex_wait` and `futex_wake`. The wire
vocabulary is smaller still: `CreateInterlock` and `AttachInterlock`, two requests, three
responses. There is no release call, no cancel, no command channel, no update. Everything a
caller can express is expressible in those verbs.

The payoff is exhaustive testability. `registry.rs` tests enumerate every tier's initial word
values, every fire condition, every reap condition, and every sentinel word, because the set is
enumerable. A system that grew a primitive per use case would have an unbounded surface, and
unbounded surfaces cannot be trusted at the layer a pipeline's liveness depends on. New capability
comes from composition: `WaitRace` is SDK-side composition over existing `WaitCounter` handles,
not a sixth tier. When a proposed feature would require the daemon to understand what the counters
*mean*, it belongs in the application, not here.

</one-primitive>

<computed-state>

## 2. State is computed from facts, never stored as a flag

**Every observable condition is derived from the three words and the current clock. There is no
status field, because a status field can disagree with reality.**

`interlock_state(open, closed, expiration_ns, clock_ns)` is a pure function returning
`Closed`, `Open`, `Overrun`, or `Expired`. Four states, total coverage, no default arm. "Open" is
`open > closed`. "Overrun" is `open < closed`. "Expired" is `clock >= expiration_ns` or any word
holding `SENTINEL`. Liveness is `interlock_is_terminated`: a single `SENTINEL` in any of the three
words means dead, checked identically by the daemon's reaper and by every SDK handle's
`is_reaped`.

This is why there is no release operation and no reference counting. A holder stays alive by
pushing `expiration_ns` forward through `interlock_arm`, which is a CAS-max loop: it never moves
expiration backward and it refuses to write over `SENTINEL`. Stop arming and the record is
reclaimed at the next evaluation pass. A process that dies with `SIGKILL` releases nothing and
needs to release nothing; its TTL simply lapses. The process-level test asserts exactly this: a
child is killed, and within 300 ms the interlock reads `Expired` and the name no longer resolves.

Flags drift; arithmetic does not. A stored `is_active` boolean written by a process that then
crashes is a lie the system must be taught to disbelieve. Three monotonic numbers and a clock
cannot lie about anything except their own contents, and the sentinel makes even that detectable.
`WaitCounter::wait_until` uses the same discipline for targets: a CAS-max into `open_count`, so a
target only ever moves forward and concurrent waiters converge on the largest.

</computed-state>

<crash-dont-limp>

## 3. Crash, do not limp

**Every deviation from a known-good state terminates something: a request, a connection, a
handle, or the process. Nothing runs degraded.**

The daemon binary is built with `panic = "abort"`. A panic terminates the process rather than
unwinding through a half-evaluated registry, because a half-evaluated registry is a state nobody
enumerated and nobody tested. `monotonic_now_nanos` aborts if `clock_gettime` fails or returns an
unrepresentable timespec: there is no plausible fallback time, and a plausible-but-wrong clock is
worse than no clock. A protocol fault closes the client connection after one stderr line, one line
per fault, not one per loop cycle. A client that never reads its responses is dropped, not
buffered for.

The sharpest instance is `WaitTimer`'s default `TimeoutPolicy::Abort`. If the daemon does not
deliver within the fatal margin (`2 * wait`, floored at `MIN_FATAL_MARGIN_MS`), the client process
aborts with a diagnostic naming the wait, the margin, the target, and both counters. This is
deliberate: a real-time stage that missed its wake by more than double its budget has already
failed its deadline, and the only honest outcomes are stop or be restarted. `TimeoutPolicy::Error`
exists for callers that can genuinely recover, and it is opt-in precisely so the recovery path is
a decision someone made rather than a default someone inherited.

Configuration follows the same rule. The daemon parses its flags up front and exits 2 on anything
unknown or malformed; an octal socket mode that will not parse is a startup failure, not a
silently substituted default. `Server::create` refuses to bind a path occupied by a live daemon
and refuses to remove a non-socket file, reporting which case it saw. Failures are pushed to the
earliest possible moment, because a fault detected at startup costs a restart while the same fault
detected mid-flight costs a pipeline.

</crash-dont-limp>

<cold-restart>

## 4. A restart is a cold start

**The daemon holds no durable state. Coming back empty is correct behavior, not data loss.**

The registry lives in memory over anonymous memfds. There is no journal, no snapshot, no
reconciliation on start. A restarted daemon creates a fresh clock, stamps a new start time into
`closed_count`, and serves an empty namespace. `Server`'s `Drop` removes the socket file; a
`SIGKILL` leaves it behind and the next start detects a stale socket (nothing accepts on it) and
replaces it. Both paths lead to the same known-good state.

Consumers discover the restart through their own mechanisms rather than through a notification
channel that would itself need to survive the restart. A timer waiter learns because its fatal
margin elapses with no delivery. A connected client learns because `is_connected` flips when the
socket reaches EOF. A name from before the restart simply does not resolve:
`SdkError::InterlockNotFound`. The process test asserts the whole sequence, that a waiter dies by
its margin and a fresh client immediately creates and waits successfully on the new daemon.

Recreating a name that already exists is not an error either. The registry reaps the old entry,
issues a new ID, and hands back a fresh record; the displaced holder's next operation returns
`InterlockReaped`. A restarted producer therefore just recreates its interlocks and continues, no
cleanup, no teardown protocol, no orphan sweep. The alternative, persisting coordination state so
a restart can resume, is exactly the failure this law forbids: persisted coordination survives a
crash in a shape that has drifted from reality, and reconciling it turns a two-hundred-millisecond
restart into an open-ended debugging session.

</cold-restart>

<no-waiter-blocks-forever>

## 5. No wait is unbounded

**Every blocking call has a ceiling in the code path itself, not in the caller's discipline.**

`handle_ops::wait_word` never issues an untimed futex wait. It loops with
`DEFAULT_TIMEOUT_NANOS` (100 ms) as the per-iteration ceiling, and on each wake it re-reads the
word, re-checks for `SENTINEL`, and re-checks whether the daemon's clock expiration has lapsed.
The clock interlock is daemon-owned and read-only to clients, so the client's keepalive thread
cannot renew it. After daemon death, the clock's 100 ms TTL lapses and bare interlocks,
`WaitCron`, `WaitBarrier`, and `ClockHandle` return `InterlockReaped` within one polling
interval. `WaitTimer` detects death through its own fatal margin; `WaitCounter` returns
`WaitState::Timeout` at its cadence. The bounded variants (`wait_open_for`, `wait_close_for`)
return `Ok(None)` on timeout: a timeout is a value, not an exception.

The reason futex waits are capped rather than indefinite is recorded in the code and in the
hazard tables: `futex_word` truncates a `u64` to its low 32 bits, so a change confined to a
counter's upper half is invisible to the kernel comparison. That is a real hole, and rather than
argue it away, the design closes it with a ceiling: the worst case is one extra polling interval,
not a permanently sleeping thread. Correctness does not rest on the kernel noticing every
transition.

The daemon obeys the same rule from the other side. Its `ppoll` timeout is always the time
remaining until the next millisecond boundary, computed from a fixed anchor so cadence does not
drift with processing time. It never blocks waiting on a client and never buffers for a slow reader. On an isolated core,
socket work does not measurably postpone `registry.tick`: a burst of 2000 pipelined requests
delays the clock by one cadence (1 ms). The timing suite asserts the consequence directly: after
a burst, the clock word resumes advancing within one evaluation cycle.

</no-waiter-blocks-forever>

<authority-at-the-boundary>

## 6. Authority is granted by mapping, enforced by the kernel

**What a process may do to an interlock is decided at map time and enforced by page protection and
memfd seals, not by checks the daemon must remember to perform.**

Every memfd is sealed at creation with `F_SEAL_SHRINK | F_SEAL_GROW | F_SEAL_SEAL`. A hostile
attacher cannot `ftruncate` a shared record out from under its mappers; the attempt returns
`EPERM`, and the abuse suite spawns a real attacker process to prove it, in a child process
precisely because a successful shrink would `SIGBUS` the mapper. The clock adds
`F_SEAL_FUTURE_WRITE` and is handed out through `interlock_map_clock` with `PROT_READ` only:
attempting a writable mapping of the clock fd fails at `mmap`, so no client can scribble on the
system's notion of time. The daemon retains the one writable mapping it made before sealing.

Beyond that boundary the primitive is deliberately open. The daemon does not gate `open` or
`close` on caller identity, does not ask which receipt a writer holds, and does not care which
process increments which word. A restriction like "only the creator may advance `closed_count`"
would be an assumption about use: correct for producer/consumer handoff, wrong for worker
dispatch, wrong for barrier completion by a third party. An assumption baked into the primitive is
a seam, and a seam gets worked around with untested code rather than fixed. Misuse of an open
interlock by application code is a bug in that code; the state machines in `docs/` are the
specification that makes such misuse visible in testing.

The trust boundary is therefore the socket, and it is stated rather than implied: every local
process that can open the socket can create over names, attach to anything, and terminate what it
can write to. That is enforced by socket ownership and mode (`0660` by default, group settable at
start), documented in the operations guide, and not pretended away with in-band permission checks
that a determined caller could bypass by writing to the shared page directly.

</authority-at-the-boundary>

<abi-is-the-boundary>

## 7. The crate split is an ABI, not a filing convention

**Shared-memory layout, wire framing, descriptor cardinality, and sentinel meaning must hold
across independently compiled crates. Where the compiler cannot enforce that, tests must.**

`abacus-core` owns the 24-byte layout and the sentinel. `abacus-wire` owns framing, tags, error
codes, and `SCM_RIGHTS` transfer, and it is shared rather than owned by either endpoint precisely
so that neither the daemon nor the SDK becomes the other's dependency. `wire_crate.rs`
asserts this structurally by reading the manifests: `abacus-client` must not depend on
`abacus-daemon`, and both must depend on `abacus-wire`, so one codec serves both sides.

Because Rust types stop at the crate boundary and the shared page has no type at all, the
invariants are pinned with compile-time assertions and exhaustive tests. `size_of::<Interlock>()`
is asserted equal to `INTERLOCK_SIZE`; field offsets are asserted at 0, 8, 16; little-endianness
is asserted at compile time next to the `futex_addr` that depends on it. The codec is tested by
truncating every valid payload at every offset and requiring `Truncated { needed > have }`, and by
ten thousand bit-flip mutations and ten thousand random buffers that must never panic. Descriptor
counts are checked at the consumer: the client compares received fds against
`expected_fd_count(&response)` and treats too few as `Truncated` and too many as `UnexpectedFd`.

Where the ABI is currently insufficient, that is recorded rather than papered over. Wire v1's
`Attached` response carries an ID but no tier, so the SDK cannot enforce tier-specific attachment
rules; this sits in the backlog as a v2 change with an explicit trigger, not as a runtime
heuristic that guesses. A gap named in a document is a known state. A gap patched with inference
is an unknown one.

</abi-is-the-boundary>

<the-shape-of-a-test>

## 8. A guarantee not exercised against a real process is not a guarantee

**Behavior that crosses a process, a signal, or a page boundary is tested across that boundary,
with the dangerous half isolated in a child.**

Three layers exist because three kinds of claim exist. In-crate tests check layout, arithmetic,
and codec behavior. `ThreadDaemon` runs the loop in-process for fast behavioral coverage.
`ProcessDaemon` spawns the actual `abacus` binary, and it is what proves the claims that matter
operationally: `SIGTERM` exits zero and removes the socket; `SIGKILL` leaves a stale socket the
next start replaces; a second instance on a live socket exits 1 with a specific message; an idle
daemon burns under five clock ticks in three seconds.

Anything that can kill the test harness runs in a re-executed child role, addressed by ignored
test name through `role_command`. A `WaitTimer` abort would take down the whole integration binary,
so it aborts in a child and the parent asserts `SIGABRT`. A truncated memfd would `SIGBUS` its
mapper, so the attacker maps in a child and the parent inspects the exit status. This is not
ceremony: the alternative is either not testing the abort path or testing a weakened version of it,
and an untested abort path is precisely the path that will be taken at the worst moment.

Timing claims carry their environment with them. The load profiles pin the daemon to an isolated
core and the load threads elsewhere, and the documentation states plainly that affinity alone is
not isolation: without kernel-level CPU isolation, scheduler stalls beyond 10 ms remain possible,
so the 50 ms fatal-margin floor is an operational defense against observed jitter, not a proof of
real-time behavior. Measured numbers live in the operations guide with the conditions that
produced them. A latency figure without its profile is a number, not evidence.

</the-shape-of-a-test>

---

<how-the-laws-relate>

## How the laws relate

They reinforce each other. One primitive (1) keeps the state space small enough that computing
state from facts (2) is possible at all: three words and a clock are enough because there is
nothing else to describe. Computed state makes crash-don't-limp (3) tractable, since the
deviations worth crashing on are enumerable rather than open-ended. Crash-don't-limp is only safe
because a restart is a cold start (4): terminating is cheap when nothing must be reconciled
afterward. Cold restart in turn is only safe because no wait is unbounded (5): a waiter that could
block forever would survive the daemon that was supposed to wake it, and the herd would stop.
Kernel-enforced authority (6) is what lets the primitive stay open without becoming a liability,
and openness is what keeps the primitive set from growing (back to 1). The ABI discipline (7) is
what makes all of the above true across four separately compiled crates instead of true only
within one. And process-level testing (8) is what turns every preceding law from an intention into
a checked fact. Weaken any one and the others start costing more than they should.

</how-the-laws-relate>