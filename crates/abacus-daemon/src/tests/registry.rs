//! Registry: the clock entry, create validation per tier, init values, name recreation, attach,
//! and evaluate_all (reap on sentinel and TTL, every wait tier, cron re-arm, barrier).
//! Constructed directly; no socket, no event loop.

use std::sync::atomic::Ordering;
use std::time::Instant;

use abacus_core::clock::monotonic_now_nanos;
use abacus_core::error::Condition;
use abacus_core::interlock::{
    interlock_arm, interlock_free, interlock_map, InterlockHandle, SENTINEL,
};

use crate::registry::{next_grid_line, CreateSpec, Registry, Tier};

use super::raise_fd_limit;

const MS: u64 = 1_000_000;
/// TTL long enough that no test interlock expires mid-test.
const KEEP_ALIVE_NS: u64 = 10_000 * MS;
/// Names over this many bytes are refused.
const MAX_NAME_LEN: usize = 255;
/// This many client interlocks are accepted; one more is refused.
const MAX_INTERLOCKS: usize = 4096;
/// Performance floor for evaluate_all over 1000 interlocks, median of 100 calls.
/// Debug is generous; release allows about 2x headroom over the observed floor.
/// The owned-interlock test shares this ceiling: each owned entry calls target_alive
/// once per tick for its owner, adding inherent per-entry overhead.
const EVALUATE_1000_MEDIAN_CEILING_US: u128 = if cfg!(debug_assertions) { 300 } else { 50 };

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
        .create(CreateSpec {
            name: name.into(),
            ..Default::default()
        })
        .unwrap_or_else(|e| panic!("create bare {name}: {e}"));
    interlock_arm(&r.1, KEEP_ALIVE_NS).expect("keep alive");
    r
}

fn create_counter(reg: &mut Registry, name: &str, watched: &str, word: u8) -> InterlockHandle {
    let (_, h) = reg
        .create(CreateSpec {
            name: name.into(),
            tier: Tier::WaitCounter,
            watched_name: Some(watched.into()),
            watched_word: Some(word),
            ..Default::default()
        })
        .unwrap_or_else(|e| panic!("create counter {name}: {e}"));
    interlock_arm(&h, KEEP_ALIVE_NS).expect("keep alive");
    h
}

fn create_timer(reg: &mut Registry, name: &str) -> InterlockHandle {
    let (_, h) = reg
        .create(CreateSpec {
            name: name.into(),
            tier: Tier::WaitTimer,
            ..Default::default()
        })
        .unwrap_or_else(|e| panic!("create timer {name}: {e}"));
    interlock_arm(&h, KEEP_ALIVE_NS).expect("keep alive");
    h
}

fn create_cron(reg: &mut Registry, name: &str, interval_ms: u64) -> InterlockHandle {
    let (_, h) = reg
        .create(CreateSpec {
            name: name.into(),
            tier: Tier::WaitCron,
            interval_ns: Some(interval_ms * MS),
            ..Default::default()
        })
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
        .create(CreateSpec {
            name: name.into(),
            tier: Tier::WaitBarrier,
            barrier_conditions: Some(conds),
            ..Default::default()
        })
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
    let (id, _, h) = reg.attach("clock").expect("attach clock");
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
    create_bare(&mut reg, "t");
    let specs = vec![
        CreateSpec {
            name: "clock".into(),
            tier: Tier::Interlock,
            ..Default::default()
        },
        CreateSpec {
            name: "clock".into(),
            tier: Tier::WaitCounter,
            watched_name: Some("t".into()),
            watched_word: Some(0),
            ..Default::default()
        },
        CreateSpec {
            name: "clock".into(),
            tier: Tier::WaitTimer,
            ..Default::default()
        },
        CreateSpec {
            name: "clock".into(),
            tier: Tier::WaitCron,
            interval_ns: Some(10 * MS),
            ..Default::default()
        },
        CreateSpec {
            name: "clock".into(),
            tier: Tier::WaitBarrier,
            barrier_conditions: Some(vec![("t".into(), 0, 1)]),
            ..Default::default()
        },
    ];
    for spec in specs {
        let tier = spec.tier;
        let r = reg.create(spec);
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
    let (id, _, _) = reg.attach("clock").expect("clock still present");
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
        .create(CreateSpec {
            name: "il".into(),
            ..Default::default()
        })
        .expect("interlock");
    let after = monotonic_now_nanos();
    let (o, c, e) = words(&il);
    assert_eq!((o, c), (0, 0), "Interlock row: ({o}, {c})");
    assert_creation_ttl(e, before, after, "Interlock row");

    let before = monotonic_now_nanos();
    let (_, wc) = reg
        .create(CreateSpec {
            name: "wc".into(),
            tier: Tier::WaitCounter,
            watched_name: Some("src".into()),
            watched_word: Some(0),
            ..Default::default()
        })
        .expect("counter");
    let after = monotonic_now_nanos();
    let (o, c, e) = words(&wc);
    assert_eq!((o, c), (0, 0), "WaitCounter row: ({o}, {c})");
    assert_creation_ttl(e, before, after, "WaitCounter row");

    let before = monotonic_now_nanos();
    let (_, wt) = reg
        .create(CreateSpec {
            name: "wt".into(),
            tier: Tier::WaitTimer,
            ..Default::default()
        })
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
        .create(CreateSpec {
            name: "cr".into(),
            tier: Tier::WaitCron,
            interval_ns: Some(10 * MS),
            ..Default::default()
        })
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
        .create(CreateSpec {
            name: "wb".into(),
            tier: Tier::WaitBarrier,
            barrier_conditions: Some(vec![("src".into(), 1, 1)]),
            ..Default::default()
        })
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
    let ok = reg.create(CreateSpec {
        name: "ok".into(),
        tier: Tier::WaitCounter,
        watched_name: Some("src".into()),
        watched_word: Some(1),
        ..Default::default()
    });
    assert!(ok.is_ok(), "valid counter refused: {:?}", ok.err());
    let r = reg.create(CreateSpec {
        name: "no-name".into(),
        tier: Tier::WaitCounter,
        watched_word: Some(0),
        ..Default::default()
    });
    assert!(
        matches!(r, Err(Condition::InvalidRequest { .. })),
        "missing watched_name: {:?}",
        r.map(|x| x.0)
    );
    let r = reg.create(CreateSpec {
        name: "no-word".into(),
        tier: Tier::WaitCounter,
        watched_name: Some("src".into()),
        ..Default::default()
    });
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
    let r = reg.create(CreateSpec {
        name: "w".into(),
        tier: Tier::WaitCounter,
        watched_name: Some("ghost".into()),
        watched_word: Some(0),
        ..Default::default()
    });
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
    let r = reg.create(CreateSpec {
        name: "w".into(),
        tier: Tier::WaitCounter,
        watched_name: Some("src".into()),
        watched_word: Some(2),
        ..Default::default()
    });
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
        let r = reg.create(CreateSpec {
            name: format!("c{ival}"),
            tier: Tier::WaitCron,
            interval_ns: Some(ival),
            ..Default::default()
        });
        assert!(
            matches!(r, Err(Condition::InvalidRequest { .. })),
            "interval {ival} ns accepted: {:?}",
            r.map(|x| x.0)
        );
    }
    let r = reg.create(CreateSpec {
        name: "c1ms".into(),
        tier: Tier::WaitCron,
        interval_ns: Some(MS),
        ..Default::default()
    });
    assert!(r.is_ok(), "1 ms interval refused: {:?}", r.err());
    let r = reg.create(CreateSpec {
        name: "c-none".into(),
        tier: Tier::WaitCron,
        ..Default::default()
    });
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
    let r = reg.create(CreateSpec {
        name: "b".into(),
        tier: Tier::WaitBarrier,
        barrier_conditions: Some(vec![("a".into(), 0, 1), ("missing".into(), 1, 1)]),
        ..Default::default()
    });
    assert_eq!(
        r.map(|x| x.0),
        Err(Condition::InterlockNotFound {
            name: "missing".into()
        })
    );
    let r = reg.create(CreateSpec {
        name: "b2".into(),
        tier: Tier::WaitBarrier,
        barrier_conditions: Some(vec![("a".into(), 2, 1)]),
        ..Default::default()
    });
    assert!(
        matches!(r, Err(Condition::InvalidRequest { .. })),
        "watched_word 2 accepted"
    );
    let r = reg.create(CreateSpec {
        name: "b3".into(),
        tier: Tier::WaitBarrier,
        ..Default::default()
    });
    assert!(
        matches!(r, Err(Condition::InvalidRequest { .. })),
        "missing conditions accepted"
    );
    let ok = reg.create(CreateSpec {
        name: "b4".into(),
        tier: Tier::WaitBarrier,
        barrier_conditions: Some(vec![("a".into(), 1, 1)]),
        ..Default::default()
    });
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
    let (id, _, _) = reg.attach("x").expect("attach");
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
    // Boundary: expiration exactly now is dead (ttl <= 0). Evaluate with the same `now`
    // the expiration was stored as, not a later re-read, or this exercises `< now` instead.
    let (_, h2) = create_bare(&mut reg, "e2");
    let now = monotonic_now_nanos();
    h2.words().expiration_ns.store(now, Ordering::Release);
    reg.evaluate_all(clock_now(&reg), now);
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
    let (id, _, _) = reg.attach("clock").expect("clock gone");
    assert_eq!(id, 0);
    reg.refresh_clock_expiration(monotonic_now_nanos());
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
    let (_, _, exp_before) = words(&h);
    reg.evaluate_all(line + 3, monotonic_now_nanos());
    assert_eq!(
        words(&h),
        (line + 10, line, exp_before),
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
    let r = reg.create(CreateSpec {
        name: long,
        ..Default::default()
    });
    let long_name_refused = matches!(r, Err(Condition::InvalidRequest { .. }));
    let ok = reg.create(CreateSpec {
        name: "n".repeat(MAX_NAME_LEN),
        ..Default::default()
    });
    assert!(ok.is_ok(), "255-byte name refused: {:?}", ok.err());

    // One interlock already exists (the 255-byte name above), so only
    // MAX_INTERLOCKS - 1 more fit before the cap.
    let mut handles = Vec::with_capacity(MAX_INTERLOCKS - 1);
    let mut first_failure = None;
    for i in 0..MAX_INTERLOCKS - 1 {
        match reg.create(CreateSpec {
            name: format!("i{i}"),
            ..Default::default()
        }) {
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
    let over = reg.create(CreateSpec {
        name: "one-too-many".into(),
        ..Default::default()
    });
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
fn registry__evaluate_all_1000_interlocks_under_ceiling() {
    raise_fd_limit(16_384);
    let mut reg = registry();
    let mut handles = Vec::with_capacity(1000);
    for i in 0..1000 {
        let (_, h) = reg
            .create(CreateSpec {
                name: format!("p{i}"),
                ..Default::default()
            })
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
    assert_eq!(
        reg.slot_of("dummy"),
        Some(0),
        "test setup: dummy must be at slot 0"
    );
    assert_eq!(
        reg.slot_of("target"),
        Some(1),
        "test setup: target must be at slot 1"
    );
    // Reap the dummy so slot 0 enters the free list.
    interlock_free(&dummy);
    reg.evaluate_all(clock_now(&reg), monotonic_now_nanos());
    assert!(reg.attach("dummy").is_err(), "dummy not reaped");
    // Watcher reclaims slot 0 (lower than target at slot 1).
    let watcher = create_counter(&mut reg, "w", "target", 0);
    assert_eq!(
        reg.slot_of("w"),
        Some(0),
        "test setup: watcher must reclaim slot 0, lower than target's slot 1"
    );
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

/// An owned entry is reaped when its owner's TTL lapses.
#[test]
fn registry__owned_entry_is_reaped_when_owner_lapses() {
    let mut reg = registry();
    let (pc_id, pc) = create_bare(&mut reg, "pc");
    let (_, bus) = reg
        .create(CreateSpec {
            name: "bus".into(),
            owner: Some(("pc".into(), pc_id)),
            ..Default::default()
        })
        .unwrap();
    interlock_arm(&bus, KEEP_ALIVE_NS).expect("keep alive");
    pc.words().expiration_ns.store(1, Ordering::Release);
    reg.evaluate_all(clock_now(&reg), monotonic_now_nanos());
    assert_eq!(words(&bus), (SENTINEL, SENTINEL, SENTINEL));
    assert!(reg.attach("bus").is_err());
}

/// An owned entry is reaped when its owner is recreated under the same name.
#[test]
fn registry__owned_entry_is_reaped_when_owner_is_recreated() {
    let mut reg = registry();
    let (pc_id, _pc) = create_bare(&mut reg, "pc");
    let (_, bus) = reg
        .create(CreateSpec {
            name: "bus".into(),
            owner: Some(("pc".into(), pc_id)),
            ..Default::default()
        })
        .unwrap();
    interlock_arm(&bus, KEEP_ALIVE_NS).expect("keep alive");
    create_bare(&mut reg, "pc");
    reg.evaluate_all(clock_now(&reg), monotonic_now_nanos());
    assert_eq!(words(&bus), (SENTINEL, SENTINEL, SENTINEL));
    assert!(reg.attach("bus").is_err());
}

/// An owned entry is reaped when its owner is freed (both free paths).
#[test]
fn registry__owned_entry_is_reaped_when_owner_is_freed() {
    // Path 1: SENTINEL on expiration_ns (interlock_free)
    let mut reg = registry();
    let (pc_id, pc) = create_bare(&mut reg, "pc");
    let (_, bus) = reg
        .create(CreateSpec {
            name: "bus".into(),
            owner: Some(("pc".into(), pc_id)),
            ..Default::default()
        })
        .unwrap();
    interlock_arm(&bus, KEEP_ALIVE_NS).expect("keep alive");
    interlock_free(&pc);
    reg.evaluate_all(clock_now(&reg), monotonic_now_nanos());
    assert_eq!(words(&bus), (SENTINEL, SENTINEL, SENTINEL));

    // Path 2: SENTINEL on open_count
    let mut reg = registry();
    let (pc_id, pc) = create_bare(&mut reg, "pc2");
    let (_, bus) = reg
        .create(CreateSpec {
            name: "bus2".into(),
            owner: Some(("pc2".into(), pc_id)),
            ..Default::default()
        })
        .unwrap();
    interlock_arm(&bus, KEEP_ALIVE_NS).expect("keep alive");
    pc.words().open_count.store(SENTINEL, Ordering::Release);
    reg.evaluate_all(clock_now(&reg), monotonic_now_nanos());
    assert_eq!(words(&bus), (SENTINEL, SENTINEL, SENTINEL));
}

/// Every tier (0 through 4) dies with its owner.
#[test]
fn registry__every_tier_dies_with_its_owner() {
    let mut reg = registry();
    create_bare(&mut reg, "src");
    let (pc_id, pc) = create_bare(&mut reg, "pc");

    let (_, t0) = reg
        .create(CreateSpec {
            name: "t0".into(),
            owner: Some(("pc".into(), pc_id)),
            ..Default::default()
        })
        .unwrap();
    interlock_arm(&t0, KEEP_ALIVE_NS).expect("keep alive");

    let (_, t1) = reg
        .create(CreateSpec {
            name: "t1".into(),
            tier: Tier::WaitCounter,
            watched_name: Some("src".into()),
            watched_word: Some(0),
            owner: Some(("pc".into(), pc_id)),
            ..Default::default()
        })
        .unwrap();
    interlock_arm(&t1, KEEP_ALIVE_NS).expect("keep alive");

    let (_, t2) = reg
        .create(CreateSpec {
            name: "t2".into(),
            tier: Tier::WaitTimer,
            owner: Some(("pc".into(), pc_id)),
            ..Default::default()
        })
        .unwrap();
    interlock_arm(&t2, KEEP_ALIVE_NS).expect("keep alive");

    let (_, t3) = reg
        .create(CreateSpec {
            name: "t3".into(),
            tier: Tier::WaitCron,
            interval_ns: Some(10 * MS),
            owner: Some(("pc".into(), pc_id)),
            ..Default::default()
        })
        .unwrap();
    interlock_arm(&t3, KEEP_ALIVE_NS).expect("keep alive");

    let (_, t4) = reg
        .create(CreateSpec {
            name: "t4".into(),
            tier: Tier::WaitBarrier,
            barrier_conditions: Some(vec![("src".into(), 0, 1)]),
            owner: Some(("pc".into(), pc_id)),
            ..Default::default()
        })
        .unwrap();
    interlock_arm(&t4, KEEP_ALIVE_NS).expect("keep alive");

    interlock_free(&pc);
    reg.evaluate_all(clock_now(&reg), monotonic_now_nanos());

    for (name, h) in [
        ("t0", &t0),
        ("t1", &t1),
        ("t2", &t2),
        ("t3", &t3),
        ("t4", &t4),
    ] {
        assert_eq!(
            words(h),
            (SENTINEL, SENTINEL, SENTINEL),
            "{name} not reaped"
        );
        assert!(reg.attach(name).is_err(), "{name} still attachable");
    }
}

/// A stale owner id is refused, and the existing entry is not displaced.
#[test]
fn registry__stale_owner_id_is_refused_and_displaces_nothing() {
    let mut reg = registry();
    let (id_a, _) = create_bare(&mut reg, "pc");
    let (id_b, _) = create_bare(&mut reg, "pc");
    assert!(id_b > id_a);
    let (bus_id, bus) = reg
        .create(CreateSpec {
            name: "bus".into(),
            owner: Some(("pc".into(), id_b)),
            ..Default::default()
        })
        .unwrap();
    interlock_arm(&bus, KEEP_ALIVE_NS).expect("keep alive");
    let live_before = reg.live_count();

    let r = reg.create(CreateSpec {
        name: "bus".into(),
        owner: Some(("pc".into(), id_a)),
        ..Default::default()
    });
    assert!(
        matches!(r, Err(Condition::InterlockReaped)),
        "stale owner accepted: {r:?}"
    );
    assert_eq!(reg.attach("bus").unwrap().0, bus_id, "bus displaced");
    assert_eq!(reg.live_count(), live_before, "live_count changed");
}

/// An unknown owner is refused.
#[test]
fn registry__unknown_owner_is_refused() {
    let mut reg = registry();
    let r = reg.create(CreateSpec {
        name: "bus".into(),
        owner: Some(("ghost".into(), 999)),
        ..Default::default()
    });
    assert!(
        matches!(r, Err(Condition::InterlockReaped)),
        "unknown owner accepted: {r:?}"
    );
}

/// A missing dependency is InterlockNotFound.
#[test]
fn registry__missing_dependency_is_not_found() {
    let mut reg = registry();
    let r = reg.create(CreateSpec {
        name: "bus".into(),
        dependencies: vec!["ghost".into()],
        ..Default::default()
    });
    assert!(
        matches!(r, Err(Condition::InterlockNotFound { ref name }) if name == "ghost"),
        "missing dep accepted: {r:?}"
    );
}

/// Naming itself as owner or dependency is InvalidRequest.
#[test]
fn registry__naming_itself_as_owner_or_dependency_is_invalid() {
    let mut reg = registry();
    create_bare(&mut reg, "x");
    let r = reg.create(CreateSpec {
        name: "x".into(),
        owner: Some(("x".into(), 1)),
        ..Default::default()
    });
    assert!(
        matches!(r, Err(Condition::InvalidRequest { .. })),
        "self-owner: {r:?}"
    );
    let r = reg.create(CreateSpec {
        name: "y".into(),
        dependencies: vec!["y".into()],
        ..Default::default()
    });
    assert!(
        matches!(r, Err(Condition::InvalidRequest { .. })),
        "self-dep: {r:?}"
    );
}

/// A dependency on "clock" never reaps (the clock is always alive).
#[test]
fn registry__dependency_on_clock_never_reaps() {
    let mut reg = registry();
    let (_, bus) = reg
        .create(CreateSpec {
            name: "bus".into(),
            dependencies: vec!["clock".into()],
            ..Default::default()
        })
        .unwrap();
    interlock_arm(&bus, KEEP_ALIVE_NS).expect("keep alive");
    for _ in 0..10 {
        reg.evaluate_all(clock_now(&reg), monotonic_now_nanos());
    }
    assert_ne!(words(&bus).0, SENTINEL, "clock dependency reaped");
    assert!(reg.attach("bus").is_ok());
}

/// Chain A <- B <- C, C owns "x". In slot order one pass reaps all four. Inverted slot
/// order (B below its lapsed dependency A): target_alive now reads A's expiration
/// directly, so A and B are gone after the first pass instead of waiting a further pass
/// for A's own SENTINEL stamp; C follows on the second pass, x on the third.
#[test]
fn registry__cascade_reaches_three_levels_in_both_slot_orders() {
    // Forward slot order: A lowest slot.
    let mut reg = registry();
    let (_a_id, a) = create_bare(&mut reg, "A");
    let (_, b) = reg
        .create(CreateSpec {
            name: "B".into(),
            dependencies: vec!["A".into()],
            ..Default::default()
        })
        .unwrap();
    interlock_arm(&b, KEEP_ALIVE_NS).expect("keep alive");
    let (c_id, _) = {
        let (_, c) = reg
            .create(CreateSpec {
                name: "C".into(),
                dependencies: vec!["B".into()],
                ..Default::default()
            })
            .unwrap();
        interlock_arm(&c, KEEP_ALIVE_NS).expect("keep alive");
        let (cid, _, _) = reg.attach("C").unwrap();
        (cid, c)
    };
    let (_, x) = reg
        .create(CreateSpec {
            name: "x".into(),
            owner: Some(("C".into(), c_id)),
            ..Default::default()
        })
        .unwrap();
    interlock_arm(&x, KEEP_ALIVE_NS).expect("keep alive");

    a.words().expiration_ns.store(1, Ordering::Release);
    reg.evaluate_all(clock_now(&reg), monotonic_now_nanos());
    for name in ["A", "B", "C", "x"] {
        assert!(reg.attach(name).is_err(), "{name} survived (forward order)");
    }

    // Inverted slot order: free low slots so x, C, B, A land ascending.
    let mut reg = registry();
    let mut dummies = Vec::new();
    for i in 0..4 {
        let (_, d) = create_bare(&mut reg, &format!("d{i}"));
        dummies.push(d);
    }
    for d in &dummies {
        interlock_free(d);
    }
    reg.evaluate_all(clock_now(&reg), monotonic_now_nanos());

    let (_a_id, a) = create_bare(&mut reg, "A");
    let (_, b) = reg
        .create(CreateSpec {
            name: "B".into(),
            dependencies: vec!["A".into()],
            ..Default::default()
        })
        .unwrap();
    interlock_arm(&b, KEEP_ALIVE_NS).expect("keep alive");
    let (c_id, _) = {
        let (_, c) = reg
            .create(CreateSpec {
                name: "C".into(),
                dependencies: vec!["B".into()],
                ..Default::default()
            })
            .unwrap();
        interlock_arm(&c, KEEP_ALIVE_NS).expect("keep alive");
        let (cid, _, _) = reg.attach("C").unwrap();
        (cid, c)
    };
    let (_, x) = reg
        .create(CreateSpec {
            name: "x".into(),
            owner: Some(("C".into(), c_id)),
            ..Default::default()
        })
        .unwrap();
    interlock_arm(&x, KEEP_ALIVE_NS).expect("keep alive");

    a.words().expiration_ns.store(1, Ordering::Release);

    reg.evaluate_all(clock_now(&reg), monotonic_now_nanos());
    assert!(reg.attach("A").is_err(), "A survived pass 1");
    assert!(reg.attach("B").is_err(), "B survived pass 1");
    assert!(reg.attach("C").is_ok(), "C gone after pass 1");
    assert!(reg.attach("x").is_ok(), "x gone after pass 1");

    reg.evaluate_all(clock_now(&reg), monotonic_now_nanos());
    assert!(reg.attach("C").is_err(), "C survived pass 2");
    assert!(reg.attach("x").is_ok(), "x gone after pass 2");

    reg.evaluate_all(clock_now(&reg), monotonic_now_nanos());
    assert!(reg.attach("x").is_err(), "x survived pass 3");
}

/// Attach reports the tier.
#[test]
fn registry__attach_reports_the_tier() {
    let mut reg = registry();
    create_bare(&mut reg, "src");
    create_bare(&mut reg, "il");
    let (_, tier, _) = reg.attach("il").unwrap();
    assert_eq!(tier, Tier::Interlock);
    let (_, tier, _) = reg.attach("clock").unwrap();
    assert_eq!(tier, Tier::Interlock);

    reg.create(CreateSpec {
        name: "wc".into(),
        tier: Tier::WaitCounter,
        watched_name: Some("src".into()),
        watched_word: Some(0),
        ..Default::default()
    })
    .unwrap();
    let (_, tier, _) = reg.attach("wc").unwrap();
    assert_eq!(tier, Tier::WaitCounter);

    reg.create(CreateSpec {
        name: "wt".into(),
        tier: Tier::WaitTimer,
        ..Default::default()
    })
    .unwrap();
    let (_, tier, _) = reg.attach("wt").unwrap();
    assert_eq!(tier, Tier::WaitTimer);

    reg.create(CreateSpec {
        name: "wcr".into(),
        tier: Tier::WaitCron,
        interval_ns: Some(10 * MS),
        ..Default::default()
    })
    .unwrap();
    let (_, tier, _) = reg.attach("wcr").unwrap();
    assert_eq!(tier, Tier::WaitCron);

    reg.create(CreateSpec {
        name: "wb".into(),
        tier: Tier::WaitBarrier,
        barrier_conditions: Some(vec![("src".into(), 0, 1)]),
        ..Default::default()
    })
    .unwrap();
    let (_, tier, _) = reg.attach("wb").unwrap();
    assert_eq!(tier, Tier::WaitBarrier);
}

/// 1000 entries owned by one entry, same EVALUATE_1000_MEDIAN_CEILING_US bound as the bare
/// case.
#[test]
fn registry__evaluate_thousand_owned_interlocks_is_fast() {
    raise_fd_limit(16_384);
    let mut reg = registry();
    let (pc_id, _pc) = create_bare(&mut reg, "pc");
    let mut handles = Vec::with_capacity(1000);
    for i in 0..1000 {
        let (_, h) = reg
            .create(CreateSpec {
                name: format!("o{i}"),
                owner: Some(("pc".into(), pc_id)),
                ..Default::default()
            })
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
        "evaluate_all over 1000 owned interlocks: min={min} median={median} p99={p99} max={max} us; \
         ceiling {EVALUATE_1000_MEDIAN_CEILING_US} us"
    );
}

/// An owner whose expiration has lapsed, but is not yet reaped, is refused at create just
/// as a SENTINEL owner is: target_alive checks expiration_alive, not only SENTINEL.
#[test]
fn registry__lapsed_owner_is_refused_at_create() {
    let mut reg = registry();
    let (pc_id, pc) = create_bare(&mut reg, "pc");
    pc.words().expiration_ns.store(1, Ordering::Release);
    let r = reg.create(CreateSpec {
        name: "bus".into(),
        owner: Some(("pc".into(), pc_id)),
        ..Default::default()
    });
    assert!(
        matches!(r, Err(Condition::InterlockReaped)),
        "lapsed owner accepted: {r:?}"
    );
}

/// A dependency whose expiration has lapsed, but is not yet reaped, is InterlockNotFound
/// at create, matching the SENTINEL case.
#[test]
fn registry__lapsed_dependency_is_not_found_at_create() {
    let mut reg = registry();
    let (_, dep) = create_bare(&mut reg, "dep");
    dep.words().expiration_ns.store(1, Ordering::Release);
    let r = reg.create(CreateSpec {
        name: "bus".into(),
        dependencies: vec!["dep".into()],
        ..Default::default()
    });
    assert_eq!(
        r.map(|x| x.0),
        Err(Condition::InterlockNotFound { name: "dep".into() }),
        "lapsed dependency accepted"
    );
}

/// A WaitCounter or WaitBarrier watch target whose expiration has lapsed, but is not yet
/// reaped, is InterlockNotFound at create: the same liveness check the dependency path
/// already applied, now also applied to watched_name and every barrier condition name.
#[test]
fn registry__watcher_on_a_lapsed_target_is_not_found_at_create() {
    let mut reg = registry();
    let (_, src) = create_bare(&mut reg, "src");
    src.words().expiration_ns.store(1, Ordering::Release);

    let r = reg.create(CreateSpec {
        name: "wc".into(),
        tier: Tier::WaitCounter,
        watched_name: Some("src".into()),
        watched_word: Some(0),
        ..Default::default()
    });
    assert_eq!(
        r.map(|x| x.0),
        Err(Condition::InterlockNotFound { name: "src".into() }),
        "WaitCounter accepted a lapsed watched_name"
    );

    let r = reg.create(CreateSpec {
        name: "wb".into(),
        tier: Tier::WaitBarrier,
        barrier_conditions: Some(vec![("src".into(), 0, 1)]),
        ..Default::default()
    });
    assert_eq!(
        r.map(|x| x.0),
        Err(Condition::InterlockNotFound { name: "src".into() }),
        "WaitBarrier accepted a lapsed condition name"
    );
}

/// A dependent occupying a lower slot than its lapsed (but not yet SENTINEL-stamped)
/// dependency is reaped in the very evaluate_all pass that lapses the dependency:
/// target_alive reads the dependency's expiration directly from its shared words, so the
/// dependent does not have to wait for a later pass to see the dependency's own SENTINEL
/// stamp land.
#[test]
fn registry__dependent_of_a_lapsed_target_is_reaped_in_the_same_pass() {
    let mut reg = registry();
    // Fill and free slots 0 and 1 so the free list gives LIFO reuse: slot 1 first, then 0.
    let (_, d0) = create_bare(&mut reg, "d0");
    let (_, d1) = create_bare(&mut reg, "d1");
    interlock_free(&d0);
    interlock_free(&d1);
    reg.evaluate_all(clock_now(&reg), monotonic_now_nanos());

    // A reclaims slot 1; B (depends on A) reclaims slot 0, a lower slot than A.
    let (_a_id, a) = create_bare(&mut reg, "A");
    assert_eq!(
        reg.slot_of("A"),
        Some(1),
        "test setup: A must land on slot 1"
    );
    let (_, b) = reg
        .create(CreateSpec {
            name: "B".into(),
            dependencies: vec!["A".into()],
            ..Default::default()
        })
        .unwrap();
    interlock_arm(&b, KEEP_ALIVE_NS).expect("keep alive");
    assert_eq!(
        reg.slot_of("B"),
        Some(0),
        "test setup: B must land on a lower slot than A"
    );

    // Lapse A without reaping it: no word is SENTINEL yet, only its expiration is past.
    a.words().expiration_ns.store(1, Ordering::Release);
    reg.evaluate_all(clock_now(&reg), monotonic_now_nanos());

    assert!(reg.attach("A").is_err(), "A survived its own lapse");
    assert!(
        reg.attach("B").is_err(),
        "B (lower slot than its lapsed dependency A) survived the same pass"
    );
}

/// The cron re-arm's CAS never resurrects a target whose open_count already reads
/// SENTINEL: storing SENTINEL directly and evaluating once reaps the entry, and its
/// open_count still reads SENTINEL afterward, never overwritten with a re-armed line.
#[test]
fn registry__cron_rearm_never_erases_a_sentinel() {
    let mut reg = registry();
    let h = create_cron(&mut reg, "c", 10);
    h.words().open_count.store(SENTINEL, Ordering::Release);
    reg.evaluate_all(clock_now(&reg), monotonic_now_nanos());
    assert_eq!(
        words(&h),
        (SENTINEL, SENTINEL, SENTINEL),
        "cron re-arm overwrote a SENTINEL open_count: {:?}",
        words(&h)
    );
    assert!(reg.attach("c").is_err());
}

/// create_with_fd's returned descriptor maps the same interlock as its returned handle: a
/// write through the handle is visible through a fresh map of the fd.
#[test]
fn registry__create_with_fd_returns_a_descriptor_for_the_new_entry() {
    let mut reg = registry();
    let (_, handle, fd) = reg
        .create_with_fd(CreateSpec {
            name: "x".into(),
            ..Default::default()
        })
        .expect("create_with_fd");
    handle.words().open_count.store(42, Ordering::Release);
    let mapped = interlock_map(fd).expect("interlock_map the returned fd");
    assert_eq!(
        mapped.words().open_count.load(Ordering::Acquire),
        42,
        "the returned fd does not map the same interlock as the handle"
    );
}

/// A daemon stall longer than the clock TTL does not reap clock dependents: tick
/// refreshes the clock before evaluating.
#[test]
fn registry__stall_past_clock_ttl_keeps_clock_dependents() {
    let mut reg = registry();
    let (_, bus) = reg
        .create(CreateSpec {
            name: "bus".into(),
            dependencies: vec!["clock".into()],
            ..Default::default()
        })
        .unwrap();
    interlock_arm(&bus, KEEP_ALIVE_NS).expect("keep alive");
    reg.tick(monotonic_now_nanos());
    std::thread::sleep(std::time::Duration::from_millis(150));
    reg.tick(monotonic_now_nanos());
    assert_ne!(
        words(&bus).0,
        SENTINEL,
        "a stall past the clock TTL reaped a clock dependent"
    );
}

/// Attach validates the name: an oversize name is refused as an invalid request rather than
/// answered with a not-found reply too large for the frame cap.
#[test]
fn registry__attach_refuses_an_oversize_name() {
    let reg = registry();
    assert!(matches!(
        reg.attach(&"a".repeat(MAX_NAME_LEN + 1)),
        Err(Condition::InvalidRequest { .. })
    ));
}

/// next_grid_line returns `None` on a zero interval instead of dividing by zero.
#[test]
fn registry__next_grid_line_refuses_a_zero_interval() {
    assert_eq!(next_grid_line(5, 0), None);
    assert_eq!(next_grid_line(5, 10), Some(10));
}

/// A WaitCounter that watches its own name is refused, and the existing holder of that name
/// is left untouched.
#[test]
fn registry__counter_cannot_watch_its_own_name() {
    let mut reg = registry();
    let (_, h) = create_bare(&mut reg, "x");
    let r = reg.create(CreateSpec {
        name: "x".into(),
        tier: Tier::WaitCounter,
        watched_name: Some("x".into()),
        watched_word: Some(0),
        ..Default::default()
    });
    assert!(
        matches!(r, Err(Condition::InvalidRequest { .. })),
        "self-watching counter accepted"
    );
    assert_ne!(
        words(&h).0,
        SENTINEL,
        "a refused create displaced the holder"
    );
}

/// A WaitBarrier with a condition on its own name is refused, and the existing holder of
/// that name is left untouched.
#[test]
fn registry__barrier_cannot_watch_its_own_name() {
    let mut reg = registry();
    let (_, h) = create_bare(&mut reg, "x");
    let r = reg.create(CreateSpec {
        name: "x".into(),
        tier: Tier::WaitBarrier,
        barrier_conditions: Some(vec![("x".into(), 0, 1)]),
        ..Default::default()
    });
    assert!(
        matches!(r, Err(Condition::InvalidRequest { .. })),
        "self-watching barrier accepted"
    );
    assert_ne!(
        words(&h).0,
        SENTINEL,
        "a refused create displaced the holder"
    );
}
