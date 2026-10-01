# Philosophy

This is the project's constitution. Every architectural choice traces to one of the laws
below. A design that contradicts a law is a bug in the design, not an argument to relax
the law.

The context that produced these rules: processes on one host that must hand off work at
frame cadence, where a missed wake is a dropped frame and a stuck wait is a hung pipeline.
A coordination service that can hang, lie, or come back holding stale opinions is worse than
one that is simply absent. The laws fall out of taking that seriously: few states, all of
them known, and a restart that lands in a correct one every time.

<one-primitive>

## 1. One primitive, five contracts

**There is exactly one shared object. Every wait type is a contract layered over it by
daemon evaluation and SDK handles, never a second primitive.**

The primitive is a fixed-size shared-memory record: a small number of monotonic words
and a deadline, mapped into every participating process. The daemon's wait tiers are
branches in one evaluation pass over one registry of one record type. The system clock
is not special infrastructure: it is an instance of the same primitive, whose counter
carries the current time. A timer is a counter watching the clock. A cron is a timer
that re-arms.

The operation set on that primitive is small and deliberate: create, map, arm, reap,
free, check termination, duplicate the descriptor. The wire vocabulary is smaller still:
two requests, three responses. There is no release call, no cancel, no command channel,
no update.

The payoff is exhaustive testability. The test suite enumerates every tier's initial
values, every fire condition, every reap condition, and every terminal state, because
the set is enumerable. A system that grew a primitive per use case would have an unbounded
surface, and unbounded surfaces cannot be trusted at the layer a pipeline's liveness
depends on. New capability comes from composition in the SDK, not from new tier types in
the daemon. When a proposed feature would require the daemon to understand what the
counters *mean*, it belongs in the application, not here.

Process liveness is the test case. A process's liveness is an ordinary record that its SDK keeps
extending. That a record is owned by a process, or that a process depends on another, is a
watch: the record names what it dies with, resolved to an identity when it is created, and the
daemon reaps it when that identity is gone, exactly as it reaps any watcher whose target is gone.
The daemon never learns what a process is. Create gains the list of what the new record dies
with; the vocabulary does not gain a word.

</one-primitive>

<computed-state>

## 2. State is computed from facts, never stored as a flag

**Every observable condition is derived from the stored words and the current clock. There
is no status field, because a status field can disagree with reality.**

State is a pure function of the record's words and the monotonic clock. Four states, total
coverage, no default arm. Liveness is a single check: a reserved terminal value in any word
means dead, evaluated identically by the daemon and by every SDK handle.

There is no release operation and no reference counting. A holder stays alive by pushing its
deadline forward in a compare-and-swap loop that never moves the deadline backward and
refuses to write over a terminal value. Stop extending and the record is reclaimed at the
next evaluation pass. A process killed by signal releases nothing and needs to release
nothing; its deadline simply lapses.

Flags drift; arithmetic does not. A stored "is_active" boolean written by a process that
then crashes is a lie the system must be taught to disbelieve. Monotonic numbers and a clock
cannot lie about anything except their own contents, and the terminal sentinel makes even
that detectable.

</computed-state>

<crash-dont-limp>

## 3. Crash, do not limp

**Every deviation from a known-good state terminates something: a request, a connection,
a handle, or the process. Nothing runs degraded.**

A panic terminates the daemon process rather than unwinding through a half-evaluated
registry, because a half-evaluated registry is a state nobody enumerated and nobody tested.
A failed clock read aborts: there is no plausible fallback time, and a plausible-but-wrong
clock is worse than no clock. A protocol fault closes the client connection after one
diagnostic line. A client that never reads its responses is dropped, not buffered for.

The sharpest instance is the default timeout policy for timed waits. If the daemon does not
deliver within the fatal margin, the client process aborts with a diagnostic. This is
deliberate: a real-time stage that missed its wake by more than double its budget has
already failed its deadline, and the only honest outcomes are stop or be restarted. An
error-returning policy exists for callers that can genuinely recover, and it is opt-in
precisely so the recovery path is a decision someone made rather than a default someone
inherited.

The same rule governs dependencies. A process whose dependency died aborts: a producer whose
storage is gone, or a consumer whose producer is gone, has nothing correct left to do. The
coordination daemon is every process's first dependency, so its death aborts everyone. No
notification carries the news; each process reads its own death from the record it already
has mapped, and the deaths cascade down the declared dependencies on their own.

Configuration follows the same rule. The daemon parses its flags up front and exits on
anything unknown or malformed. A socket path occupied by a live daemon is refused. Failures
are pushed to the earliest possible moment, because a fault detected at startup costs a
restart while the same fault detected mid-flight costs a pipeline.

</crash-dont-limp>

<cold-restart>

## 4. A restart is a cold start

**The daemon holds no durable state. Coming back empty is correct behavior, not data loss.**

The registry lives in memory. There is no journal, no snapshot, no reconciliation on start.
A restarted daemon creates a fresh clock and serves an empty namespace. A clean shutdown
removes the socket file; a kill leaves it behind and the next start detects the stale socket
and replaces it. Both paths lead to the same known-good state.

Consumers discover the restart through their own mechanisms rather than through a
notification channel that would itself need to survive the restart. A timed waiter learns
because its fatal margin elapses with no delivery. A connected client learns because the
socket reaches EOF. A name from before the restart simply does not resolve.

Recreating a name that already exists is not an error. The old entry is reaped, a new
identifier is issued, and the displaced holder's next operation reports termination. A
restarted producer just recreates its names and continues: no cleanup, no teardown protocol,
no orphan sweep.

The alternative, persisting coordination state so a restart can resume, is exactly the
failure this law forbids: persisted coordination survives a crash in a shape that has
drifted from reality, and reconciling it turns a fast restart into an open-ended debugging
session.

</cold-restart>

<no-waiter-blocks-forever>

## 5. No wait is unbounded

**Every blocking call has a ceiling in the code path itself, not in the caller's
discipline.**

No wait ever issues an untimed kernel sleep. Every blocking path loops with a fixed polling
ceiling, re-reading the watched word, re-checking for termination, and re-checking whether
the daemon's clock has lapsed. After daemon death, the clock's short deadline lapses and
every wait path returns a termination error within one polling interval.

The reason waits are capped rather than indefinite is a real limitation of the underlying
kernel primitive: the kernel compares only the low 32 bits of a 64-bit word, so a change
confined to the upper half is invisible to the comparison. Rather than argue this away, the
design closes it with a ceiling: the worst case is one extra polling interval, not a
permanently sleeping thread. Correctness does not rest on the kernel noticing every
transition.

The daemon obeys the same rule from the other side. Its poll timeout is always the time
remaining until the next millisecond boundary, computed from a fixed anchor so cadence does
not drift with processing time. It never blocks waiting on a client and never buffers for a
slow reader. Bounded variants of every wait return a timeout value rather than raising an
exception: a timeout is data, not a fault.

</no-waiter-blocks-forever>

<authority-at-the-boundary>

## 6. Authority is granted by mapping, enforced by the kernel

**What a process may do to a shared record is decided at map time and enforced by page
protection and file seals, not by checks the daemon must remember to perform.**

Every shared-memory backing is sealed at creation so it cannot be resized. A hostile process
cannot shrink a shared record out from under its peers; the attempt fails at the kernel.
The system clock carries an additional seal that blocks any new writable mapping after the
daemon's own, so no client can alter the system's notion of time.

Beyond that boundary the primitive is deliberately open. The daemon does not gate operations
on caller identity and does not care which process writes which word. A restriction like
"only the creator may advance a counter" would be an assumption about use: correct for one
pattern, wrong for others. An assumption baked into the primitive is a seam, and a seam gets
worked around with untested code rather than fixed.

The trust boundary is therefore the socket, and it is stated rather than implied: every
local process that can open the socket can create names, attach to anything, and terminate
what it can write to. That is enforced by socket ownership and file mode, not pretended
away with in-band permission checks that a determined caller could bypass by writing to the
shared page directly.

</authority-at-the-boundary>

<abi-is-the-boundary>

## 7. The module split is an ABI, not a filing convention

**Shared-memory layout, wire framing, descriptor cardinality, and sentinel meaning must
hold across independently compiled modules. Where the compiler cannot enforce that, tests
must.**

The shared-memory layout and the wire protocol are each owned by a single module shared
between both endpoints, so that neither the daemon nor the SDK becomes the other's
dependency. A structural test asserts this by reading the dependency manifests: the client
must not depend on the daemon, and both must depend on the shared codec.

Because language types stop at the compilation boundary and the shared page has no type at
all, the invariants are pinned with compile-time assertions and exhaustive tests. Record
size, field offsets, and byte order are all asserted at compile time. The codec is tested by
truncating every valid payload at every offset and by thousands of random-byte and bit-flip
mutations that must never panic. Descriptor counts are checked at the consumer.

Where the ABI is currently insufficient, that is recorded rather than papered over. A gap
named in a document is a known state. A gap patched with inference is an unknown one.

</abi-is-the-boundary>

<the-shape-of-a-test>

## 8. A guarantee not exercised against a real process is not a guarantee

**Behavior that crosses a process, a signal, or a page boundary is tested across that
boundary, with the dangerous half isolated in a child.**

Three test layers exist because three kinds of claim exist. Unit tests check layout,
arithmetic, and codec behavior. In-process tests run the daemon loop in a thread for fast
behavioral coverage. Process-level tests spawn the real binary, and they are what prove the
claims that matter operationally: a clean signal exits cleanly and removes the socket; a
kill leaves a stale socket that the next start replaces; a second instance on a live socket
exits with a specific message; an idle daemon is nearly free.

Anything that can kill the test harness runs in a child process. An abort from a missed
deadline would take down the whole test binary, so the abort happens in a child and the
parent inspects the exit status. A hostile shared-memory operation would fault the mapper,
so the attacker maps in a child. The alternative is either not testing the dangerous path
or testing a weakened version of it, and an untested dangerous path is precisely the path
that will be taken at the worst moment.

Timing claims carry their environment with them. The documentation states plainly that CPU
pinning alone is not isolation: without kernel-level CPU isolation, scheduler stalls remain
possible. Measured numbers live in the operations guide with the conditions that produced
them. A latency figure without its measurement profile is a number, not evidence.

</the-shape-of-a-test>

---

<how-the-laws-relate>

## How the laws relate

They reinforce each other. One primitive (1) keeps the state space small enough that
computing state from facts (2) is possible at all: a few words and a clock are enough
because there is nothing else to describe. Computed state makes crash-don't-limp (3)
tractable, since the deviations worth crashing on are enumerable rather than open-ended.
Crash-don't-limp is only safe because a restart is a cold start (4): terminating is cheap
when nothing must be reconciled afterward. Cold restart in turn is only safe because no wait
is unbounded (5): a waiter that could block forever would survive the daemon that was
supposed to wake it, and the herd would stop. Kernel-enforced authority (6) is what lets
the primitive stay open without becoming a liability, and openness is what keeps the
primitive set from growing (back to 1). The ABI discipline (7) is what makes all of the
above true across independently compiled modules instead of true only within one. And
process-level testing (8) is what turns every preceding law from an intention into a checked
fact. Weaken any one and the others start costing more than they should.

</how-the-laws-relate>
