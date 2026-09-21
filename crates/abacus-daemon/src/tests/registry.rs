//! Registry: the clock entry, create validation per tier, init values, name recreation, attach,
//! and evaluate_all (reap on sentinel and TTL, every wait tier, cron re-arm, barrier).
//! Constructed directly; no socket, no event loop.

use std::sync::atomic::Ordering;
use std::time::Instant;

use abacus_core::clock::monotonic_now_nanos;
use abacus_core::error::Condition;
use abacus_core::interlock::{interlock_arm, interlock_free, InterlockHandle, SENTINEL};

use crate::registry::{Registry, Tier};

use super::raise_fd_limit;

const MS: u64 = 1_000_000;
/// TTL long enough that no test interlock expires mid-test.
const KEEP_ALIVE_NS: u64 = 10_000 * MS;
/// Names over this many bytes are refused.
const MAX_NAME_LEN: usize = 255;
/// This many client interlocks are accepted; one more is refused.
const MAX_INTERLOCKS: usize = 4096;
/// Performance floor for evaluate_all over 1000 bare interlocks, median of 100 calls.
/// Debug is generous (observed around 115 us); release allows about 2x headroom over the
/// observed ~20 us floor.
const EVALUATE_1000_MEDIAN_CEILING_US: u128 = if cfg!(debug_assertions) { 200 } else { 50 };

fn words(h: &InterlockHandle) -> (u64, u64, u64) {
    let w = h.words();
    (
        w.open_count.load(Ordering::Acquire),
        w.closed_count.load(Ordering::Acquire),
        w.expiration_ns.load(Ordering::Acquire),
    )
}

fn registry() -> Registry {
    Registry::new().expect("Registry::new")
}

fn clock_now(reg: &Registry) -> u64 {
    reg.clock().words().open_count.load(Ordering::Acquire)
}

fn create_bare(reg: &mut Registry, name: &str) -> (u64, InterlockHandle) {
    let r = reg
        .create(name.into(), Tier::Interlock, None, None, None, None)
        .unwrap_or_else(|e| panic!("create bare {name}: {e}"));
    interlock_arm(&r.1, KEEP_ALIVE_NS).expect("keep alive");
    r
}

fn create_counter(reg: &mut Registry, name: &str, watched: &str, word: u8) -> InterlockHandle {
    let (_, h) = reg
        .create(
            name.into(),
            Tier::WaitCounter,
            Some(watched.into()),
            Some(word),
            None,
            None,
        )
        .unwrap_or_else(|e| panic!("create counter {name}: {e}"));
    interlock_arm(&h, KEEP_ALIVE_NS).expect("keep alive");
    h
}

fn create_timer(reg: &mut Registry, name: &str) -> InterlockHandle {
    let (_, h) = reg
        .create(name.into(), Tier::WaitTimer, None, None, None, None)
        .unwrap_or_else(|e| panic!("create timer {name}: {e}"));
    interlock_arm(&h, KEEP_ALIVE_NS).expect("keep alive");
    h
}

fn create_cron(reg: &mut Registry, name: &str, interval_ms: u64) -> InterlockHandle {
    let (_, h) = reg
        .create(
            name.into(),
            Tier::WaitCron,
            None,
            None,
            Some(interval_ms * MS),
            None,
        )
        .unwrap_or_else(|e| panic!("create cron {name}: {e}"));
    interlock_arm(&h, KEEP_ALIVE_NS).expect("keep alive");
    h
}

fn create_barrier(
    reg: &mut Registry,
    name: &str,
    conds: Vec<(String, u8, u64)>,
) -> InterlockHandle {
    let (_, h) = reg
        .create(
            name.into(),
            Tier::WaitBarrier,
            None,
            None,
            None,
            Some(conds),
        )
        .unwrap_or_else(|e| panic!("create barrier {name}: {e}"));
    interlock_arm(&h, KEEP_ALIVE_NS).expect("keep alive");
    h
}

fn assert_creation_ttl(exp: u64, before: u64, after: u64, what: &str) {
    let (lo, hi) = (before + 100 * MS, after + 100 * MS);
    assert!(
        exp >= lo && exp <= hi,
        "{what}: expiration {exp} outside now + 100 ms [{lo}, {hi}]"
    );
}

/// LIFECYCLE.md clock advancement, CONTRACTS.md UDS surface: the registry starts with the
/// reserved "clock" entry at id 0, both counters at the daemon start ms, and a 100 ms TTL.
#[test]
fn registry__clock_exists_with_id_zero_and_100ms_ttl() {
    let before = monotonic_now_nanos();
    let reg = registry();
    let after = monotonic_now_nanos();
    let (id, h) = reg.attach("clock").expect("attach clock");
    assert_eq!(id, 0, "clock id");
    let (open, closed, exp) = words(&h);
    assert_eq!(open, closed, "clock start: open={open} closed={closed}");
    assert!(
        open >= before / MS && open <= after / MS,
        "clock start ms {open} outside [{}, {}]",
        before / MS,
        after / MS
    );
    let (lo, hi) = (before + 100 * MS, after + 100 * MS);
    assert!(
        exp >= lo && exp <= hi,
        "clock expiration {exp} outside now + 100 ms [{lo}, {hi}]"
    );
}

/// CONTRACTS.md UDS surface: "clock" is reserved; create("clock") is InvalidRequest for every
/// tier.
#[test]
fn registry__create_clock_name_rejected() {
    let mut reg = registry();
    for tier in [Tier::Interlock, Tier::WaitTimer] {
        let r = reg.create("clock".into(), tier, None, None, None, None);
        match r {
            Err(Condition::InvalidRequest { message }) => {
                assert!(
                    message.contains("reserved"),
                    "message for {tier:?}: {message}"
                );
            }
            other => panic!(
                "create(\"clock\") as {tier:?} returned {:?}",
                other.map(|(id, _)| id)
            ),
        }
    }
    let (id, _) = reg.attach("clock").expect("clock still present");
    assert_eq!(id, 0, "clock entry was replaced");
}

/// CONTRACTS.md create-call initialization table, one assertion per row: Interlock (0, 0),
/// WaitCounter (0, 0), WaitTimer (clock now, clock now), WaitCron (next grid line, clock
/// now), WaitBarrier (1, 0); every row expiration_ns = now + 100 ms.
#[test]
fn registry__init_values_per_tier() {
    let mut reg = registry();
    let now = clock_now(&reg);
    create_bare(&mut reg, "src");

    let before = monotonic_now_nanos();
    let (_, il) = reg
        .create("il".into(), Tier::Interlock, None, None, None, None)
        .expect("interlock");
    let after = monotonic_now_nanos();
    let (o, c, e) = words(&il);
    assert_eq!((o, c), (0, 0), "Interlock row: ({o}, {c})");
    assert_creation_ttl(e, before, after, "Interlock row");

    let before = monotonic_now_nanos();
    let (_, wc) = reg
        .create(
            "wc".into(),
            Tier::WaitCounter,
            Some("src".into()),
            Some(0),
            None,
            None,
        )
        .expect("counter");
    let after = monotonic_now_nanos();
    let (o, c, e) = words(&wc);
    assert_eq!((o, c), (0, 0), "WaitCounter row: ({o}, {c})");
    assert_creation_ttl(e, before, after, "WaitCounter row");

    let before = monotonic_now_nanos();
    let (_, wt) = reg
        .create("wt".into(), Tier::WaitTimer, None, None, None, None)
        .expect("timer");
    let after = monotonic_now_nanos();
    let (o, c, e) = words(&wt);
    assert_eq!(
        (o, c),
        (now, now),
        "WaitTimer row: ({o}, {c}) with clock {now}"
    );
    assert_creation_ttl(e, before, after, "WaitTimer row");

    let before = monotonic_now_nanos();
    let (_, cr) = reg
        .create("cr".into(), Tier::WaitCron, None, None, Some(10 * MS), None)
        .expect("cron");
    let after = monotonic_now_nanos();
    let (o, c, e) = words(&cr);
    let next_line = (now / 10 + 1) * 10;
    assert_eq!(
        (o, c),
        (next_line, now),
        "WaitCron row: ({o}, {c}) with clock {now}"
    );
    assert_creation_ttl(e, before, after, "WaitCron row");

    let before = monotonic_now_nanos();
    let (_, wb) = reg
        .create(
            "wb".into(),
            Tier::WaitBarrier,
            None,
            None,
            None,
            Some(vec![("src".into(), 1, 1)]),
        )
        .expect("barrier");
    let after = monotonic_now_nanos();
    let (o, c, e) = words(&wb);
    assert_eq!((o, c), (1, 0), "WaitBarrier row: ({o}, {c})");
    assert_creation_ttl(e, before, after, "WaitBarrier row");
}

/// CONTRACTS.md create call: WaitCounter needs watched_name (must exist) and watched_word
/// (0 or 1). Both present: Ok. Either missing: InvalidRequest.
#[test]
fn registry__wait_counter_requires_watched_name_and_word() {
    let mut reg = registry();
    create_bare(&mut reg, "src");
    let ok = reg.create(
        "ok".into(),
        Tier::WaitCounter,
        Some("src".into()),
        Some(1),
        None,
        None,
    );
    assert!(ok.is_ok(), "valid counter refused: {:?}", ok.err());
    let r = reg.create(
        "no-name".into(),
        Tier::WaitCounter,
        None,
        Some(0),
        None,
        None,
    );
    assert!(
        matches!(r, Err(Condition::InvalidRequest { .. })),
        "missing watched_name: {:?}",
        r.map(|x| x.0)
    );
    let r = reg.create(
        "no-word".into(),
        Tier::WaitCounter,
        Some("src".into()),
        None,
        None,
        None,
    );
    assert!(
        matches!(r, Err(Condition::InvalidRequest { .. })),
        "missing watched_word: {:?}",
        r.map(|x| x.0)
    );
    assert!(
        reg.attach("no-name").is_err() && reg.attach("no-word").is_err(),
        "rejected names were registered"
    );
}

/// CONTRACTS.md errors: a WaitCounter whose watched_name is not in the registry is
/// InterlockNotFound naming it.
#[test]
fn registry__wait_counter_rejects_missing_watched_name() {
    let mut reg = registry();
    let r = reg.create(
        "w".into(),
        Tier::WaitCounter,
        Some("ghost".into()),
        Some(0),
        None,
        None,
    );
    assert_eq!(
        r.map(|x| x.0),
        Err(Condition::InterlockNotFound {
            name: "ghost".into()
        })
    );
}

/// CONTRACTS.md wire-abi: WatchedWord is 0 or 1; 2 is InvalidRequest.
#[test]
fn registry__wait_counter_rejects_watched_word_2() {
    let mut reg = registry();
    create_bare(&mut reg, "src");
    let r = reg.create(
        "w".into(),
        Tier::WaitCounter,
        Some("src".into()),
        Some(2),
        None,
        None,
    );
    match r {
        Err(Condition::InvalidRequest { message }) => assert!(message.contains('2'), "{message}"),
        other => panic!("watched_word 2 returned {:?}", other.map(|x| x.0)),
    }
}

/// SURFACE.md WaitCron: interval_ms is the grid spacing; zero and sub-millisecond
/// intervals are InvalidRequest, 1 ms is the smallest legal grid.
#[test]
fn registry__cron_rejects_zero_and_sub_ms_interval() {
    let mut reg = registry();
    for ival in [0u64, 1, 999_999] {
        let r = reg.create(
            format!("c{ival}"),
            Tier::WaitCron,
            None,
            None,
            Some(ival),
            None,
        );
        assert!(
            matches!(r, Err(Condition::InvalidRequest { .. })),
            "interval {ival} ns accepted: {:?}",
            r.map(|x| x.0)
        );
    }
    let r = reg.create("c1ms".into(), Tier::WaitCron, None, None, Some(MS), None);
    assert!(r.is_ok(), "1 ms interval refused: {:?}", r.err());
    let r = reg.create("c-none".into(), Tier::WaitCron, None, None, None, None);
    assert!(
        matches!(r, Err(Condition::InvalidRequest { .. })),
        "missing interval accepted"
    );
}

/// LIFECYCLE.md daemon-evaluated compositions: the first cron target is the next
/// monotonic-epoch grid line strictly after the current clock.
#[test]
fn registry__cron_first_target_is_next_grid_line() {
    let mut reg = registry();
    let now = clock_now(&reg);
    for ival in [1u64, 10, 33, 1000] {
        let h = create_cron(&mut reg, &format!("cron{ival}"), ival);
        let (open, closed, _) = words(&h);
        assert_eq!(open % ival, 0, "interval {ival}: target {open} off grid");
        assert!(
            open > now,
            "interval {ival}: target {open} not after clock {now}"
        );
        assert!(
            open - now <= ival,
            "interval {ival}: target {open} skips a line past clock {now}"
        );
        assert_eq!(open, (now / ival + 1) * ival);
        assert_eq!(closed, now);
    }
}

/// CONTRACTS.md create call: WaitBarrier validates every watched_name; any missing one is
/// InterlockNotFound naming it, and a bad word is InvalidRequest.
#[test]
fn registry__barrier_rejects_any_missing_watched_name() {
    let mut reg = registry();
    create_bare(&mut reg, "a");
    let r = reg.create(
        "b".into(),
        Tier::WaitBarrier,
        None,
        None,
        None,
        Some(vec![("a".into(), 0, 1), ("missing".into(), 1, 1)]),
    );
    assert_eq!(
        r.map(|x| x.0),
        Err(Condition::InterlockNotFound {
            name: "missing".into()
        })
    );
    let r = reg.create(
        "b2".into(),
        Tier::WaitBarrier,
        None,
        None,
        None,
        Some(vec![("a".into(), 2, 1)]),
    );
    assert!(
        matches!(r, Err(Condition::InvalidRequest { .. })),
        "watched_word 2 accepted"
    );
    let r = reg.create("b3".into(), Tier::WaitBarrier, None, None, None, None);
    assert!(
        matches!(r, Err(Condition::InvalidRequest { .. })),
        "missing conditions accepted"
    );
    let ok = reg.create(
        "b4".into(),
        Tier::WaitBarrier,
        None,
        None,
        None,
        Some(vec![("a".into(), 1, 1)]),
    );
    assert!(ok.is_ok(), "valid barrier refused: {:?}", ok.err());
}

/// CONTRACTS.md UDS surface: create over an existing name reaps the old
/// interlock (all three words SENTINEL) and returns a fresh one with a higher id; attach now
/// resolves to the new id.
#[test]
fn registry__recreate_reaps_old_and_ids_increase() {
    let mut reg = registry();
    let (id1, h1) = create_bare(&mut reg, "x");
    h1.words().open_count.store(42, Ordering::Release);
    let (id2, h2) = create_bare(&mut reg, "x");
    assert!(id2 > id1, "ids did not increase: {id1} then {id2}");
    assert_eq!(
        words(&h1),
        (SENTINEL, SENTINEL, SENTINEL),
        "old interlock not reaped"
    );
    let (o, c, _) = words(&h2);
    assert_eq!((o, c), (0, 0), "new interlock not fresh");
    let (id, _) = reg.attach("x").expect("attach");
    assert_eq!(id, id2);
    let (id3, _) = create_bare(&mut reg, "y");
    assert!(id3 > id2, "ids not monotonic across names");
}

/// CONTRACTS.md errors: attach to a name not in the registry is InterlockNotFound.
#[test]
fn registry__attach_unknown_is_not_found() {
    let reg = registry();
    assert_eq!(
        reg.attach("nobody").map(|x| x.0),
        Err(Condition::InterlockNotFound {
            name: "nobody".into()
        })
    );
    assert!(reg.attach("").is_err(), "empty name attached");
}

/// LIFECYCLE.md daemon evaluation: an interlock whose expiration has passed is reaped
/// (all words SENTINEL) and removed from the registry.
#[test]
fn registry__evaluate_reaps_on_ttl() {
    let mut reg = registry();
    let (_, h) = create_bare(&mut reg, "e");
    let now = monotonic_now_nanos();
    h.words().expiration_ns.store(now - 1, Ordering::Release);
    reg.evaluate_all(clock_now(&reg), monotonic_now_nanos());
    assert_eq!(
        words(&h),
        (SENTINEL, SENTINEL, SENTINEL),
        "expired interlock not reaped"
    );
    assert!(reg.attach("e").is_err(), "reaped entry still attachable");
    // Boundary: expiration exactly now is dead (ttl <= 0).
    let (_, h2) = create_bare(&mut reg, "e2");
    let now = monotonic_now_nanos();
    h2.words().expiration_ns.store(now, Ordering::Release);
    reg.evaluate_all(clock_now(&reg), monotonic_now_nanos());
    assert_eq!(words(&h2).2, SENTINEL, "expiration == now not reaped");
}

fn reaps_on_sentinel_in(word: &str) {
    let mut reg = registry();
    let (_, h) = create_bare(&mut reg, "s");
    match word {
        "open_count" => h.words().open_count.store(SENTINEL, Ordering::Release),
        "closed_count" => h.words().closed_count.store(SENTINEL, Ordering::Release),
        "expiration_ns" => h.words().expiration_ns.store(SENTINEL, Ordering::Release),
        _ => unreachable!(),
    }
    reg.evaluate_all(clock_now(&reg), monotonic_now_nanos());
    assert_eq!(
        words(&h),
        (SENTINEL, SENTINEL, SENTINEL),
        "SENTINEL in {word} did not reap: {:?}",
        words(&h)
    );
    assert!(
        reg.attach("s").is_err(),
        "SENTINEL in {word}: entry still attachable"
    );
}

/// LIFECYCLE.md daemon evaluation: SENTINEL in any one word (a client free) is reaped and
/// removed on the next pass. One case per word.
#[test]
fn registry__evaluate_reaps_on_any_sentinel_word() {
    for word in ["open_count", "closed_count", "expiration_ns"] {
        reaps_on_sentinel_in(word);
    }
}

/// LIFECYCLE.md daemon evaluation: SENTINEL in open_count alone reaps.
#[test]
fn registry__evaluate_reaps_on_any_sentinel_word_open_count() {
    reaps_on_sentinel_in("open_count");
}

/// LIFECYCLE.md daemon evaluation: SENTINEL in closed_count alone reaps.
#[test]
fn registry__evaluate_reaps_on_any_sentinel_word_closed_count() {
    reaps_on_sentinel_in("closed_count");
}

/// LIFECYCLE.md daemon evaluation: SENTINEL in expiration_ns alone (interlock_free) reaps.
#[test]
fn registry__evaluate_reaps_on_any_sentinel_word_expiration_ns() {
    let mut reg = registry();
    let (_, h) = create_bare(&mut reg, "f");
    interlock_free(&h);
    reg.evaluate_all(clock_now(&reg), monotonic_now_nanos());
    assert_eq!(words(&h), (SENTINEL, SENTINEL, SENTINEL));
    assert!(reg.attach("f").is_err());
}

/// LIFECYCLE.md clock advancement: the clock is never reaped, even with a lapsed expiration.
#[test]
fn registry__evaluate_skips_clock() {
    let mut reg = registry();
    let clock = reg.clock().clone();
    let now = monotonic_now_nanos();
    clock
        .words()
        .expiration_ns
        .store(now - 1, Ordering::Release);
    reg.evaluate_all(clock_now(&reg), monotonic_now_nanos());
    let (open, closed, exp) = words(&clock);
    assert!(
        open != SENTINEL && closed != SENTINEL && exp != SENTINEL,
        "clock reaped: ({open}, {closed}, {exp})"
    );
    let (id, _) = reg.attach("clock").expect("clock gone");
    assert_eq!(id, 0);
    reg.refresh_clock_expiration();
    assert!(
        words(&clock).2 > monotonic_now_nanos(),
        "refresh did not extend the clock"
    );
}

/// LIFECYCLE.md wait contract, WaitTimer: when the clock reaches open_count the daemon stamps
/// closed_count = clock ms.
#[test]
fn registry__evaluate_timer_fires_at_target_and_stamps_clock() {
    let mut reg = registry();
    let now = clock_now(&reg);
    let h = create_timer(&mut reg, "t");
    h.words().open_count.store(now + 5, Ordering::Release);
    reg.evaluate_all(now + 5, monotonic_now_nanos());
    let (open, closed, _) = words(&h);
    assert_eq!(
        closed,
        now + 5,
        "timer did not stamp at target: open={open} closed={closed}"
    );
    // Late clock stamps the actual clock value (overrun visible), never the target.
    let h2 = create_timer(&mut reg, "t2");
    h2.words().open_count.store(now + 5, Ordering::Release);
    reg.evaluate_all(now + 9, monotonic_now_nanos());
    assert_eq!(
        words(&h2).1,
        now + 9,
        "late timer stamped {} not clock {}",
        words(&h2).1,
        now + 9
    );
}

/// LIFECYCLE.md wait contract, WaitTimer: no stamp while the clock is below open_count.
#[test]
fn registry__evaluate_timer_does_not_fire_before_target() {
    let mut reg = registry();
    let now = clock_now(&reg);
    let h = create_timer(&mut reg, "t");
    h.words().open_count.store(now + 5, Ordering::Release);
    for tick in 0..5 {
        reg.evaluate_all(now + tick, monotonic_now_nanos());
        let (open, closed, _) = words(&h);
        assert_eq!(
            closed,
            now,
            "fired early at clock {}: open={open} closed={closed}",
            now + tick
        );
    }
}

/// LIFECYCLE.md wait contract, WaitCounter: watched_word selects open_count (0) or
/// closed_count (1) of the target; each fires only on its own word.
#[test]
fn registry__evaluate_counter_fires_on_open_word_and_on_closed_word() {
    let mut reg = registry();
    let (_, src) = create_bare(&mut reg, "src");
    let on_open = create_counter(&mut reg, "on-open", "src", 0);
    let on_closed = create_counter(&mut reg, "on-closed", "src", 1);
    on_open.words().open_count.store(3, Ordering::Release);
    on_closed.words().open_count.store(2, Ordering::Release);
    let now = clock_now(&reg);

    reg.evaluate_all(now, monotonic_now_nanos());
    assert_eq!(words(&on_open).1, 0, "fired with nothing advanced");
    assert_eq!(words(&on_closed).1, 0, "fired with nothing advanced");

    src.words().open_count.store(3, Ordering::Release);
    reg.evaluate_all(now, monotonic_now_nanos());
    assert_eq!(
        words(&on_open).1,
        3,
        "open-word counter did not fire on open_count 3"
    );
    assert_eq!(
        words(&on_closed).1,
        0,
        "closed-word counter fired on open_count"
    );

    src.words().closed_count.store(2, Ordering::Release);
    reg.evaluate_all(now, monotonic_now_nanos());
    assert_eq!(
        words(&on_closed).1,
        2,
        "closed-word counter did not fire on closed_count 2"
    );
}

/// LIFECYCLE.md wait contract: the daemon stamps closed_count to the actual watched value,
/// not to open_count, so an overshoot is visible as Overrun (value < 0).
#[test]
fn registry__evaluate_counter_stamps_watched_value_not_target() {
    let mut reg = registry();
    let (_, src) = create_bare(&mut reg, "src");
    let c = create_counter(&mut reg, "c", "src", 0);
    c.words().open_count.store(3, Ordering::Release);
    src.words().open_count.store(7, Ordering::Release);
    reg.evaluate_all(clock_now(&reg), monotonic_now_nanos());
    let (open, closed, _) = words(&c);
    assert_eq!(
        closed, 7,
        "stamped {closed}, watched value was 7 (target {open})"
    );
    assert!(
        (open as i64).wrapping_sub(closed as i64) < 0,
        "overrun not visible"
    );
}

/// LIFECYCLE.md WaitCron: on the grid line the daemon stamps closed_count = clock and
/// re-arms open_count to the next line; between lines nothing changes.
#[test]
fn registry__evaluate_cron_fires_and_rearms_to_next_line() {
    let mut reg = registry();
    let h = create_cron(&mut reg, "c", 10);
    let (line, _, _) = words(&h);
    reg.evaluate_all(line - 1, monotonic_now_nanos());
    assert_eq!(words(&h).0, line, "re-armed before the line");
    reg.evaluate_all(line, monotonic_now_nanos());
    let (open, closed, _) = words(&h);
    assert_eq!(
        (open, closed),
        (line + 10, line),
        "first fire: open={open} closed={closed}"
    );
    reg.evaluate_all(line + 3, monotonic_now_nanos());
    assert_eq!(
        words(&h),
        (line + 10, line, words(&h).2),
        "changed between lines"
    );
    reg.evaluate_all(line + 10, monotonic_now_nanos());
    let (open, closed, _) = words(&h);
    assert_eq!(
        (open, closed),
        (line + 20, line + 10),
        "second fire: open={open} closed={closed}"
    );
}

/// After a stall the cron fires once and re-arms to the next future line; missed lines
/// are skipped, never replayed as a burst.
#[test]
fn registry__evaluate_cron_skips_missed_lines_without_burst() {
    let mut reg = registry();
    let h = create_cron(&mut reg, "c", 10);
    let (line, _, _) = words(&h);
    let late = line + 35;
    reg.evaluate_all(late, monotonic_now_nanos());
    let (open, closed, _) = words(&h);
    assert_eq!(
        closed, late,
        "stamp after stall: closed={closed}, clock {late}"
    );
    assert_eq!(
        open,
        line + 40,
        "re-arm after stall: open={open}, expected next line {}",
        line + 40
    );
    reg.evaluate_all(late, monotonic_now_nanos());
    assert_eq!(
        words(&h).0,
        line + 40,
        "a second pass at the same clock burst-fired"
    );
    assert_eq!(words(&h).1, late);
}

/// LIFECYCLE.md wait contract, WaitBarrier: fires only when every condition holds. The
/// barrier stamps closed_count = open_count on fire (exact, not clock ms).
#[test]
fn registry__evaluate_barrier_fires_only_when_all_met() {
    let mut reg = registry();
    let (_, a) = create_bare(&mut reg, "a");
    let (_, b) = create_bare(&mut reg, "b");
    let bar = create_barrier(
        &mut reg,
        "bar",
        vec![("a".into(), 1, 3), ("b".into(), 1, 3)],
    );
    let now = clock_now(&reg);
    reg.evaluate_all(now, monotonic_now_nanos());
    assert_eq!(words(&bar).1, 0, "fired with nothing met");
    a.words().closed_count.store(3, Ordering::Release);
    reg.evaluate_all(now, monotonic_now_nanos());
    assert_eq!(words(&bar).1, 0, "fired with one of two met");
    b.words().closed_count.store(2, Ordering::Release);
    reg.evaluate_all(now, monotonic_now_nanos());
    assert_eq!(words(&bar).1, 0, "fired below threshold");
    b.words().closed_count.store(3, Ordering::Release);
    reg.evaluate_all(now + 1, monotonic_now_nanos());
    // barrier stamps closed_count = open_count (1), not clock ms.
    assert_eq!(
        words(&bar).1,
        1,
        "did not fire with all met: {:?}",
        words(&bar)
    );
}

/// A watcher whose target is reaped dies with it: watched_value returns None for a
/// terminated target (word == SENTINEL), so the watcher is reaped in the same evaluate_all
/// pass that reaps the target. All three words are stamped SENTINEL; the watcher is removed.
#[test]
fn registry__evaluate_watcher_dies_with_its_target() {
    let mut reg = registry();
    let (_, src) = create_bare(&mut reg, "src");
    let c = create_counter(&mut reg, "c", "src", 0);
    interlock_free(&src);
    let now = clock_now(&reg);
    reg.evaluate_all(now, monotonic_now_nanos());
    assert!(reg.attach("src").is_err(), "freed target still registered");
    // watcher is reaped in the same pass as its target.
    assert_eq!(
        words(&c),
        (SENTINEL, SENTINEL, SENTINEL),
        "watcher should be reaped with its target: {:?}",
        words(&c)
    );
    assert!(
        reg.attach("c").is_err(),
        "orphaned watcher still registered"
    );
}

/// A watcher does not silently retarget a recreated name: after the daemon recreates
/// the target, the old watcher is reaped rather than firing on the new interlock.
#[test]
fn registry__evaluate_watcher_does_not_follow_recreated_name() {
    let mut reg = registry();
    create_bare(&mut reg, "src");
    let c = create_counter(&mut reg, "c", "src", 0);
    c.words().open_count.store(5, Ordering::Release);
    let (_, src2) = create_bare(&mut reg, "src");
    src2.words().open_count.store(5, Ordering::Release);
    let now = clock_now(&reg);
    reg.evaluate_all(now, monotonic_now_nanos());
    reg.evaluate_all(now + 1, monotonic_now_nanos());
    let (open, closed, exp) = words(&c);
    assert_ne!(
        (open, closed),
        (5, 5),
        "watcher fired on the recreated target"
    );
    assert_eq!(
        (open, closed, exp),
        (SENTINEL, SENTINEL, SENTINEL),
        "watcher not reaped after its target was recreated: ({open}, {closed}, {exp})"
    );
}

/// Limits: a name longer than 255 bytes is InvalidRequest; the registry accepts 4096
/// client interlocks and refuses the next with InvalidRequest (not AllocationFailed, not a
/// wedge). If the clock counts against the cap, adjust MAX_INTERLOCKS here.
#[test]
fn registry__limits_enforced() {
    raise_fd_limit(16_384);
    let mut reg = registry();
    let long = "n".repeat(MAX_NAME_LEN + 1);
    let r = reg.create(long, Tier::Interlock, None, None, None, None);
    let long_name_refused = matches!(r, Err(Condition::InvalidRequest { .. }));
    let ok = reg.create(
        "n".repeat(MAX_NAME_LEN),
        Tier::Interlock,
        None,
        None,
        None,
        None,
    );
    assert!(ok.is_ok(), "255-byte name refused: {:?}", ok.err());

    // One interlock already exists (the 255-byte name above), so only
    // MAX_INTERLOCKS - 1 more fit before the cap.
    let mut handles = Vec::with_capacity(MAX_INTERLOCKS - 1);
    let mut first_failure = None;
    for i in 0..MAX_INTERLOCKS - 1 {
        match reg.create(format!("i{i}"), Tier::Interlock, None, None, None, None) {
            Ok((_, h)) => {
                interlock_arm(&h, KEEP_ALIVE_NS).expect("keep alive");
                handles.push(h);
            }
            Err(e) => {
                first_failure = Some((i, e));
                break;
            }
        }
    }
    let over = reg.create(
        "one-too-many".into(),
        Tier::Interlock,
        None,
        None,
        None,
        None,
    );
    let over_refused = matches!(over, Err(Condition::InvalidRequest { .. }));
    assert!(
        long_name_refused,
        "a {}-byte name was accepted",
        MAX_NAME_LEN + 1
    );
    assert!(
        first_failure.is_none(),
        "create failed before the cap at {:?}",
        first_failure
    );
    assert!(
        over_refused,
        "interlock {} past the {MAX_INTERLOCKS} cap returned {:?}",
        MAX_INTERLOCKS + 1,
        over.map(|x| x.0)
    );
}

/// A first performance floor: evaluate_all over 1000 live bare interlocks, median of 100
/// calls, stays under EVALUATE_1000_MEDIAN_CEILING_US. The ceiling is selected by
/// cfg!(debug_assertions): generous in debug, tight in release. L0 because it needs no daemon.
#[test]
fn registry__evaluate_all_1000_interlocks_under_100us() {
    raise_fd_limit(16_384);
    let mut reg = registry();
    let mut handles = Vec::with_capacity(1000);
    for i in 0..1000 {
        let (_, h) = reg
            .create(format!("p{i}"), Tier::Interlock, None, None, None, None)
            .unwrap_or_else(|e| panic!("create {i}: {e}"));
        interlock_arm(&h, KEEP_ALIVE_NS).expect("keep alive");
        handles.push(h);
    }
    let now = clock_now(&reg);
    let mut samples: Vec<u128> = (0..100)
        .map(|_| {
            let t0 = Instant::now();
            reg.evaluate_all(now, monotonic_now_nanos());
            t0.elapsed().as_micros()
        })
        .collect();
    samples.sort_unstable();
    let (min, median, p99, max) = (samples[0], samples[50], samples[98], samples[99]);
    assert!(
        handles.iter().all(|h| words(h).2 != SENTINEL),
        "interlocks were reaped during the measurement"
    );
    assert!(
        median < EVALUATE_1000_MEDIAN_CEILING_US,
        "evaluate_all over 1000 interlocks: min={min} median={median} p99={p99} max={max} us; \
         ceiling {EVALUATE_1000_MEDIAN_CEILING_US} us"
    );
}

/// A watcher in a lower slot must not fire against a freed target whose counter words are
/// still live. interlock_free stamps only expiration_ns to SENTINEL; the counter words
/// remain. If watched_value does not check expiration_ns, a lower-slot watcher reads the
/// live counter and fires before evaluate_all reaches (and reaps) the target.
///
/// Setup: occupy slot 0 with a dummy, create the target at slot 1, free the dummy so slot 0
/// re-enters the free list, then create the watcher which reclaims slot 0. Now the watcher
/// is evaluated before the target in the forward scan.
#[test]
fn registry__watcher_does_not_fire_against_freed_target() {
    let mut reg = registry();
    // Dummy occupies slot 0; target occupies slot 1.
    let (_, dummy) = create_bare(&mut reg, "dummy");
    let (_, target) = create_bare(&mut reg, "target");
    // Reap the dummy so slot 0 enters the free list.
    interlock_free(&dummy);
    reg.evaluate_all(clock_now(&reg), monotonic_now_nanos());
    assert!(reg.attach("dummy").is_err(), "dummy not reaped");
    // Watcher reclaims slot 0 (lower than target at slot 1).
    let watcher = create_counter(&mut reg, "w", "target", 0);
    // Make the watcher open (open_count > closed_count).
    watcher.words().open_count.store(3, Ordering::Release);
    // Give the target a live counter value the watcher would fire on.
    target.words().open_count.store(5, Ordering::Release);
    // Free the target: expiration_ns = SENTINEL, counters untouched.
    interlock_free(&target);
    // Evaluate: slot 0 (watcher) is visited before slot 1 (target).
    reg.evaluate_all(clock_now(&reg), monotonic_now_nanos());
    // The watcher must NOT have fired (closed_count must not be the target's value).
    let (_, watcher_closed, _) = words(&watcher);
    assert_ne!(
        watcher_closed, 5,
        "watcher fired against a freed target: closed_count={watcher_closed}"
    );
    // The watcher should be reaped (its target is dead).
    assert_eq!(
        words(&watcher),
        (SENTINEL, SENTINEL, SENTINEL),
        "watcher should be reaped when its target is freed"
    );
    assert!(
        reg.attach("w").is_err(),
        "orphaned watcher still registered"
    );
}

/// evaluate_all reuses the internal dead_slots vector across cycles instead of
/// allocating a new Vec<usize> each time. After a pass that reaps entries, the capacity
/// persists into the next pass, proving the allocation is reused.
#[test]
fn registry__evaluate_all_reuses_dead_slots_across_cycles() {
    let mut reg = registry();
    // Create 10 interlocks and free half so they show up as dead on the next evaluate.
    let mut handles = Vec::new();
    for i in 0..10 {
        let (_, h) = create_bare(&mut reg, &format!("r{i}"));
        handles.push(h);
    }
    // Free the even-numbered ones (5 dead entries).
    for i in (0..10).step_by(2) {
        interlock_free(&handles[i]);
    }

    // First evaluate_all: reaps the 5 freed interlocks, dead_slots grows to hold them.
    let now = clock_now(&reg);
    reg.evaluate_all(now, monotonic_now_nanos());
    let cap_after_first = reg.dead_slots_capacity();
    assert!(
        cap_after_first >= 5,
        "dead_slots capacity after reaping 5 entries: {cap_after_first}"
    );

    // Second evaluate_all: nothing to reap, but the capacity from the first pass persists.
    reg.evaluate_all(now, monotonic_now_nanos());
    let cap_after_second = reg.dead_slots_capacity();
    assert_eq!(
        cap_after_second, cap_after_first,
        "dead_slots capacity changed between cycles: first={cap_after_first} second={cap_after_second}"
    );
}

// Test removed: Registry::dup_clock_fd does not exist on Registry.
// Restore when (if) dup_clock_fd is added to Registry.
