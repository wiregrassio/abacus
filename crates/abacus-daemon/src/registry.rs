//! The interlock registry: a slab of live entries, a name index used only on create and
//! attach, the clock, and the per-cycle evaluation pass.

use std::collections::HashMap;
use std::sync::atomic::Ordering;

use abacus_core::clock::{expiration_alive, futex_wake, monotonic_now_nanos, NANOS_PER_MS};
use abacus_core::error::{Condition, Result};
use abacus_core::interlock::{
    interlock_arm, interlock_create, interlock_create_clock, interlock_reap, Interlock,
    InterlockHandle, SENTINEL,
};

/// The reserved name of the daemon-owned clock interlock.
pub const CLOCK_NAME: &str = "clock";
/// The registry id of the clock.
pub const CLOCK_ID: u64 = 0;
/// Longest accepted interlock name, in bytes.
pub const MAX_NAME_LEN: usize = 255;
/// Default cap on live interlocks, excluding the clock. Each costs one fd and one mapping.
pub const DEFAULT_MAX_INTERLOCKS: usize = 4096;

const CLOCK_TTL_NANOS: u64 = 100_000_000;

// -- Tier --

/// What the daemon does with an interlock each cycle beyond reaping it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// Reap only.
    Interlock,
    /// Watch one word of another interlock; stamp and wake when it reaches the target.
    WaitCounter,
    /// Watch the clock; stamp and wake when it reaches the target.
    WaitTimer,
    /// Watch the clock; stamp, wake, and re-arm to the next grid line.
    WaitCron,
    /// Watch N conditions; stamp and wake when all are met.
    WaitBarrier,
}

impl Tier {
    /// The wire discriminant.
    pub fn from_wire(tier: u8) -> Option<Self> {
        match tier {
            0 => Some(Self::Interlock),
            1 => Some(Self::WaitCounter),
            2 => Some(Self::WaitTimer),
            3 => Some(Self::WaitCron),
            4 => Some(Self::WaitBarrier),
            _ => None,
        }
    }
}

/// Which word of a watched interlock to compare.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchedWord {
    /// Word 0.
    OpenCount,
    /// Word 1.
    ClosedCount,
}

impl WatchedWord {
    fn from_wire(word: u8) -> Result<Self> {
        match word {
            0 => Ok(Self::OpenCount),
            1 => Ok(Self::ClosedCount),
            _ => Err(Condition::InvalidRequest {
                message: format!("invalid watched_word: {word}"),
            }),
        }
    }
}

/// A watched interlock, resolved at create time to a slot plus the id that slot held then.
/// A later id mismatch means the target was reaped or recreated; the watcher dies with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Target {
    Clock,
    Slot { slot: usize, id: u64 },
}

/// One live interlock.
struct Entry {
    id: u64,
    name: String,
    handle: InterlockHandle,
    tier: Tier,
    /// WaitCounter: the watched target and word. WaitTimer and WaitCron watch the clock.
    watch: Option<(Target, WatchedWord)>,
    /// WaitCron: grid interval in milliseconds.
    interval_ms: Option<u64>,
    /// WaitBarrier: (target, word, threshold) per condition.
    conditions: Option<Vec<(Target, WatchedWord, u64)>>,
}

/// The registry.
pub struct Registry {
    next_id: u64,
    clock: InterlockHandle,
    slots: Vec<Option<Entry>>,
    free: Vec<usize>,
    index: HashMap<String, usize>,
    live: usize,
    max_interlocks: usize,
    /// Reusable scratch buffer for slots reaped during evaluate_all.
    /// Capacity persists across cycles; only cleared (not deallocated) each pass.
    dead_slots: Vec<usize>,
}

impl Registry {
    /// A registry holding only the clock, with the default interlock cap.
    pub fn new() -> Result<Self> {
        Self::with_limit(DEFAULT_MAX_INTERLOCKS)
    }

    /// A registry holding only the clock, capped at `max_interlocks` live entries.
    pub fn with_limit(max_interlocks: usize) -> Result<Self> {
        let clock = interlock_create_clock()?;
        interlock_arm(&clock, CLOCK_TTL_NANOS).expect("fresh clock cannot be reaped");
        // closed_count = daemon start ms (set once). open_count = current ms (each cycle).
        let start_ms = monotonic_now_nanos() / NANOS_PER_MS;
        clock
            .words()
            .closed_count
            .store(start_ms, Ordering::Release);
        clock.words().open_count.store(start_ms, Ordering::Release);
        Ok(Self {
            next_id: CLOCK_ID + 1,
            clock,
            slots: Vec::new(),
            free: Vec::new(),
            index: HashMap::new(),
            live: 0,
            max_interlocks,
            dead_slots: Vec::new(),
        })
    }

    /// The clock interlock.
    pub fn clock(&self) -> &InterlockHandle {
        &self.clock
    }

    /// Live interlocks, excluding the clock.
    pub fn live_count(&self) -> usize {
        self.live
    }

    /// The interlock cap.
    pub fn max_interlocks(&self) -> usize {
        self.max_interlocks
    }

    /// Current heap capacity of the reusable dead-slots scratch buffer.
    /// Exposed for tests that verify allocation reuse across evaluate_all cycles.
    pub fn dead_slots_capacity(&self) -> usize {
        self.dead_slots.capacity()
    }

    /// Create a named interlock. An existing name is reaped and replaced.
    /// Returns the new id and a handle sharing the daemon's mapping.
    pub fn create(
        &mut self,
        name: String,
        tier: Tier,
        watched_name: Option<String>,
        watched_word: Option<u8>,
        interval_ns: Option<u64>,
        barrier_conditions: Option<Vec<(String, u8, u64)>>,
    ) -> Result<(u64, InterlockHandle)> {
        validate_name(&name)?;
        if name == CLOCK_NAME {
            return Err(Condition::InvalidRequest {
                message: format!("\"{CLOCK_NAME}\" is a reserved name"),
            });
        }
        if !self.index.contains_key(&name) && self.live >= self.max_interlocks {
            return Err(Condition::InvalidRequest {
                message: format!("interlock limit reached: {}", self.max_interlocks),
            });
        }

        let mut watch = None;
        let mut interval_ms = None;
        let mut conditions = None;

        match tier {
            Tier::WaitCounter => {
                let wn = watched_name.ok_or_else(|| Condition::InvalidRequest {
                    message: "WaitCounter requires watched_name".to_string(),
                })?;
                let ww = watched_word.ok_or_else(|| Condition::InvalidRequest {
                    message: "WaitCounter requires watched_word".to_string(),
                })?;
                let target = self.resolve(&wn)?;
                watch = Some((target, WatchedWord::from_wire(ww)?));
            }
            Tier::WaitTimer => {
                watch = Some((Target::Clock, WatchedWord::OpenCount));
            }
            Tier::WaitCron => {
                let ival_ns = interval_ns.ok_or_else(|| Condition::InvalidRequest {
                    message: "WaitCron requires interval_ns".to_string(),
                })?;
                if ival_ns == 0 {
                    return Err(Condition::InvalidRequest {
                        message: "WaitCron interval_ns must be > 0".to_string(),
                    });
                }
                let ival_ms = ival_ns / NANOS_PER_MS;
                if ival_ms == 0 {
                    return Err(Condition::InvalidRequest {
                        message: format!("WaitCron interval_ns ({ival_ns}) is less than 1ms"),
                    });
                }
                interval_ms = Some(ival_ms);
                watch = Some((Target::Clock, WatchedWord::OpenCount));
            }
            Tier::WaitBarrier => {
                let conds = barrier_conditions.ok_or_else(|| Condition::InvalidRequest {
                    message: "WaitBarrier requires conditions".to_string(),
                })?;
                if conds.is_empty() {
                    return Err(Condition::InvalidRequest {
                        message: "WaitBarrier requires at least one condition".to_string(),
                    });
                }
                let mut parsed = Vec::with_capacity(conds.len());
                for (wn, ww, threshold) in conds {
                    let target = self.resolve(&wn)?;
                    parsed.push((target, WatchedWord::from_wire(ww)?, threshold));
                }
                conditions = Some(parsed);
            }
            Tier::Interlock => {}
        }

        let handle = interlock_create()?;
        let id = self.next_id;
        self.next_id += 1;

        // Tier-specific initialization per CONTRACTS.md.
        let clock_now_ms = self.clock.words().open_count.load(Ordering::Acquire);
        match tier {
            Tier::WaitTimer => {
                handle
                    .words()
                    .open_count
                    .store(clock_now_ms, Ordering::Release);
                handle
                    .words()
                    .closed_count
                    .store(clock_now_ms, Ordering::Release);
            }
            Tier::WaitCron => {
                if clock_now_ms == SENTINEL {
                    return Err(Condition::InvalidRequest {
                        message: "clock word is SENTINEL".to_string(),
                    });
                }
                let next_line =
                    next_grid_line(clock_now_ms, interval_ms.unwrap_or(1)).ok_or_else(|| {
                        Condition::InvalidRequest {
                            message: "WaitCron grid line overflow".to_string(),
                        }
                    })?;
                handle
                    .words()
                    .open_count
                    .store(next_line, Ordering::Release);
                handle
                    .words()
                    .closed_count
                    .store(clock_now_ms, Ordering::Release);
            }
            Tier::WaitBarrier => {
                // open_count = 1 makes the barrier Open so the daemon evaluates it.
                handle.words().open_count.store(1, Ordering::Release);
            }
            Tier::Interlock | Tier::WaitCounter => {}
        }

        // An existing name is reaped and replaced.
        if let Some(old_slot) = self.index.get(&name).copied() {
            self.remove_slot(old_slot);
        }

        let slot = match self.free.pop() {
            Some(slot) => slot,
            None => {
                self.slots.push(None);
                self.slots.len() - 1
            }
        };
        self.slots[slot] = Some(Entry {
            id,
            name: name.clone(),
            handle: handle.clone(),
            tier,
            watch,
            interval_ms,
            conditions,
        });
        self.index.insert(name, slot);
        self.live += 1;

        Ok((id, handle))
    }

    /// Look up an interlock by name.
    pub fn attach(&self, name: &str) -> Result<(u64, InterlockHandle)> {
        if name == CLOCK_NAME {
            return Ok((CLOCK_ID, self.clock.clone()));
        }
        match self
            .index
            .get(name)
            .and_then(|&slot| self.slots[slot].as_ref())
        {
            Some(entry) => Ok((entry.id, entry.handle.clone())),
            None => Err(Condition::InterlockNotFound {
                name: name.to_string(),
            }),
        }
    }

    /// One daemon cycle at `now_ns`: advance the clock, evaluate every interlock, wake clock
    /// watchers, refresh the clock's own expiration.
    pub fn tick(&mut self, now_ns: u64) {
        let current_ms = now_ns / NANOS_PER_MS;
        self.clock
            .words()
            .open_count
            .store(current_ms, Ordering::Release);
        self.evaluate_all(current_ms, now_ns);
        futex_wake(&self.clock.words().open_count);
        self.refresh_clock_expiration();
    }

    /// Evaluate every interlock once. For each: reap on any SENTINEL word, reap on expired
    /// TTL, else if Open evaluate the tier's wait contract. Watchers whose target is gone or
    /// recreated are reaped. Dead entries are removed after the pass.
    pub fn evaluate_all(&mut self, clock_now_ms: u64, now_ns: u64) {
        self.dead_slots.clear();

        for slot in 0..self.slots.len() {
            let Some(entry) = self.slots[slot].as_ref() else {
                continue;
            };
            let words = entry.handle.words();
            let open_count = words.open_count.load(Ordering::Acquire);
            let closed_count = words.closed_count.load(Ordering::Acquire);
            let exp = words.expiration_ns.load(Ordering::Acquire);

            if open_count == SENTINEL || closed_count == SENTINEL || exp == SENTINEL {
                interlock_reap(&entry.handle);
                self.dead_slots.push(slot);
                continue;
            }
            if !expiration_alive(exp, now_ns) {
                interlock_reap(&entry.handle);
                self.dead_slots.push(slot);
                continue;
            }

            // Open (value > 0) is the firing gate. Target liveness is checked regardless, so
            // an idle watcher still dies with its target.
            let value = (open_count as i64).wrapping_sub(closed_count as i64);
            let open = value > 0;

            match entry.tier {
                Tier::Interlock => {}
                Tier::WaitTimer => {
                    if open && clock_now_ms >= open_count {
                        words.closed_count.store(clock_now_ms, Ordering::Release);
                        futex_wake(&words.closed_count);
                    }
                }
                Tier::WaitCron => {
                    if open && clock_now_ms >= open_count {
                        words.closed_count.store(clock_now_ms, Ordering::Release);
                        futex_wake(&words.closed_count);
                        // Re-arm to the next future grid line. Missed lines are skipped,
                        // never burst. Overflow means the grid is exhausted; reap.
                        match next_grid_line(clock_now_ms, entry.interval_ms.unwrap_or(1)) {
                            Some(next_line) => {
                                words.open_count.store(next_line, Ordering::Release);
                            }
                            None => {
                                interlock_reap(&entry.handle);
                                self.dead_slots.push(slot);
                            }
                        }
                    }
                }
                Tier::WaitCounter => {
                    let Some((target, word)) = entry.watch else {
                        continue;
                    };
                    match self.watched_value(target, word) {
                        None => {
                            // Target reaped or recreated: the watcher dies with it.
                            interlock_reap(&entry.handle);
                            self.dead_slots.push(slot);
                        }
                        Some(watched) if open && watched >= open_count => {
                            words.closed_count.store(watched, Ordering::Release);
                            futex_wake(&words.closed_count);
                        }
                        Some(_) => {}
                    }
                }
                Tier::WaitBarrier => {
                    let Some(conditions) = entry.conditions.as_ref() else {
                        continue;
                    };
                    let mut all_met = true;
                    let mut target_gone = false;
                    for (target, word, threshold) in conditions {
                        match self.watched_value(*target, *word) {
                            Some(watched) => {
                                if watched < *threshold {
                                    all_met = false;
                                }
                            }
                            None => {
                                target_gone = true;
                                break;
                            }
                        }
                    }
                    if target_gone {
                        interlock_reap(&entry.handle);
                        self.dead_slots.push(slot);
                    } else if open && all_met {
                        // Fire exactly: closed_count = open_count. The SDK reads the clock
                        // for completed_at; rearm is open_count += 1 by the creator.
                        words.closed_count.store(open_count, Ordering::Release);
                        futex_wake(&words.closed_count);
                    }
                }
            }
        }

        for i in 0..self.dead_slots.len() {
            let slot = self.dead_slots[i];
            self.remove_slot(slot);
        }
        self.dead_slots.clear();
    }

    /// Keep the clock alive. It is never reaped.
    pub fn refresh_clock_expiration(&self) {
        let _ = interlock_arm(&self.clock, CLOCK_TTL_NANOS);
    }

    // -- internals --

    fn resolve(&self, name: &str) -> Result<Target> {
        if name == CLOCK_NAME {
            return Ok(Target::Clock);
        }
        match self
            .index
            .get(name)
            .and_then(|&slot| self.slots[slot].as_ref().map(|e| (slot, e.id)))
        {
            Some((slot, id)) => Ok(Target::Slot { slot, id }),
            None => Err(Condition::InterlockNotFound {
                name: name.to_string(),
            }),
        }
    }

    /// The watched word's value, or `None` if the target is gone, recreated under the same
    /// name, or itself terminated.
    fn watched_value(&self, target: Target, word: WatchedWord) -> Option<u64> {
        let words: &Interlock = match target {
            Target::Clock => self.clock.words(),
            Target::Slot { slot, id } => {
                let entry = self.slots.get(slot)?.as_ref()?;
                if entry.id != id {
                    return None;
                }
                entry.handle.words()
            }
        };
        // A freed interlock has expiration_ns stamped to SENTINEL while its counter words
        // remain live. Check all three words so a watcher in a lower slot (already past
        // evaluate_all's forward scan of the target) does not read the stale counters.
        if words.open_count.load(Ordering::Acquire) == SENTINEL
            || words.closed_count.load(Ordering::Acquire) == SENTINEL
            || words.expiration_ns.load(Ordering::Acquire) == SENTINEL
        {
            return None;
        }
        let value = match word {
            WatchedWord::OpenCount => words.open_count.load(Ordering::Acquire),
            WatchedWord::ClosedCount => words.closed_count.load(Ordering::Acquire),
        };
        if value == SENTINEL {
            None
        } else {
            Some(value)
        }
    }

    fn remove_slot(&mut self, slot: usize) {
        if let Some(entry) = self.slots[slot].take() {
            interlock_reap(&entry.handle);
            if self.index.get(&entry.name) == Some(&slot) {
                self.index.remove(&entry.name);
            }
            self.free.push(slot);
            self.live -= 1;
        }
    }
}

fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(Condition::InvalidRequest {
            message: "name must not be empty".to_string(),
        });
    }
    if name.len() > MAX_NAME_LEN {
        return Err(Condition::InvalidRequest {
            message: format!("name too long: {} bytes, max {MAX_NAME_LEN}", name.len()),
        });
    }
    Ok(())
}

/// The first grid line strictly after `now_ms`, or `None` on overflow.
pub fn next_grid_line(now_ms: u64, interval_ms: u64) -> Option<u64> {
    (now_ms / interval_ms)
        .checked_add(1)?
        .checked_mul(interval_ms)
}

#[cfg(test)]
mod tests {
    use super::*;
    use abacus_core::interlock::interlock_read_expiration;

    fn registry() -> Registry {
        Registry::new().unwrap()
    }

    fn create_bare(r: &mut Registry, name: &str) -> (u64, InterlockHandle) {
        r.create(name.into(), Tier::Interlock, None, None, None, None)
            .unwrap()
    }

    fn create_counter(
        r: &mut Registry,
        name: &str,
        watched: &str,
        word: u8,
    ) -> Result<(u64, InterlockHandle)> {
        r.create(
            name.into(),
            Tier::WaitCounter,
            Some(watched.into()),
            Some(word),
            None,
            None,
        )
    }

    fn now_ms() -> u64 {
        monotonic_now_nanos() / NANOS_PER_MS
    }

    #[test]
    fn clock_has_id_zero_and_is_attachable() {
        let r = registry();
        let (id, handle) = r.attach(CLOCK_NAME).unwrap();
        assert_eq!(id, CLOCK_ID);
        assert_eq!(handle.as_raw_fd(), r.clock().as_raw_fd());
        assert!(interlock_read_expiration(r.clock()) > monotonic_now_nanos());
    }

    #[test]
    fn create_clock_name_rejected() {
        let mut r = registry();
        let err = r
            .create(CLOCK_NAME.into(), Tier::Interlock, None, None, None, None)
            .unwrap_err();
        assert!(matches!(err, Condition::InvalidRequest { .. }));
    }

    #[test]
    fn name_limits() {
        let mut r = registry();
        assert!(matches!(
            r.create(String::new(), Tier::Interlock, None, None, None, None),
            Err(Condition::InvalidRequest { .. })
        ));
        assert!(create_bare(&mut r, &"n".repeat(MAX_NAME_LEN)).0 > 0);
        assert!(matches!(
            r.create(
                "n".repeat(MAX_NAME_LEN + 1),
                Tier::Interlock,
                None,
                None,
                None,
                None
            ),
            Err(Condition::InvalidRequest { .. })
        ));
    }

    #[test]
    fn interlock_limit_enforced_and_recreate_does_not_count() {
        let mut r = Registry::with_limit(2).unwrap();
        create_bare(&mut r, "a");
        create_bare(&mut r, "b");
        assert_eq!(r.live_count(), 2);
        let err = r
            .create("c".into(), Tier::Interlock, None, None, None, None)
            .unwrap_err();
        assert!(
            matches!(err, Condition::InvalidRequest { ref message } if message.contains("limit"))
        );
        // Recreating an existing name replaces it and stays at the cap.
        create_bare(&mut r, "a");
        assert_eq!(r.live_count(), 2);
        assert_eq!(r.max_interlocks(), 2);
    }

    #[test]
    fn recreate_reaps_old_and_ids_increase() {
        let mut r = registry();
        let (id1, h1) = create_bare(&mut r, "x");
        let (id2, h2) = create_bare(&mut r, "x");
        assert!(id2 > id1);
        assert!(abacus_core::interlock::interlock_is_terminated(&h1));
        assert!(!abacus_core::interlock::interlock_is_terminated(&h2));
        assert_eq!(r.live_count(), 1);
        assert_eq!(r.attach("x").unwrap().0, id2);
    }

    #[test]
    fn attach_unknown_is_not_found() {
        let r = registry();
        assert!(matches!(
            r.attach("nope"),
            Err(Condition::InterlockNotFound { .. })
        ));
    }

    #[test]
    fn wait_counter_validation() {
        let mut r = registry();
        create_bare(&mut r, "src");
        assert!(matches!(
            r.create("w".into(), Tier::WaitCounter, None, Some(0), None, None),
            Err(Condition::InvalidRequest { .. })
        ));
        assert!(matches!(
            r.create(
                "w".into(),
                Tier::WaitCounter,
                Some("src".into()),
                None,
                None,
                None
            ),
            Err(Condition::InvalidRequest { .. })
        ));
        assert!(matches!(
            create_counter(&mut r, "w", "missing", 0),
            Err(Condition::InterlockNotFound { .. })
        ));
        assert!(matches!(
            create_counter(&mut r, "w", "src", 2),
            Err(Condition::InvalidRequest { .. })
        ));
        assert!(create_counter(&mut r, "w", "src", 1).is_ok());
        assert!(create_counter(&mut r, "w2", CLOCK_NAME, 0).is_ok());
    }

    #[test]
    fn cron_validation_and_first_target() {
        let mut r = registry();
        assert!(matches!(
            r.create("c".into(), Tier::WaitCron, None, None, Some(0), None),
            Err(Condition::InvalidRequest { .. })
        ));
        assert!(matches!(
            r.create("c".into(), Tier::WaitCron, None, None, Some(500_000), None),
            Err(Condition::InvalidRequest { .. })
        ));
        let (_, h) = r
            .create(
                "c".into(),
                Tier::WaitCron,
                None,
                None,
                Some(10_000_000),
                None,
            )
            .unwrap();
        let clock_ms = r.clock().words().open_count.load(Ordering::Acquire);
        let open = h.words().open_count.load(Ordering::Acquire);
        assert_eq!(open, next_grid_line(clock_ms, 10).unwrap());
        assert_eq!(open % 10, 0);
        assert!(open > clock_ms);
    }

    #[test]
    fn barrier_validation() {
        let mut r = registry();
        create_bare(&mut r, "a");
        assert!(matches!(
            r.create("b".into(), Tier::WaitBarrier, None, None, None, None),
            Err(Condition::InvalidRequest { .. })
        ));
        assert!(matches!(
            r.create(
                "b".into(),
                Tier::WaitBarrier,
                None,
                None,
                None,
                Some(vec![])
            ),
            Err(Condition::InvalidRequest { .. })
        ));
        assert!(matches!(
            r.create(
                "b".into(),
                Tier::WaitBarrier,
                None,
                None,
                None,
                Some(vec![("a".into(), 0, 1), ("zz".into(), 0, 1)])
            ),
            Err(Condition::InterlockNotFound { .. })
        ));
        let (_, h) = r
            .create(
                "b".into(),
                Tier::WaitBarrier,
                None,
                None,
                None,
                Some(vec![("a".into(), 1, 3)]),
            )
            .unwrap();
        assert_eq!(h.words().open_count.load(Ordering::Acquire), 1);
        assert_eq!(h.words().closed_count.load(Ordering::Acquire), 0);
    }

    #[test]
    fn evaluate_reaps_on_ttl_and_on_each_sentinel_word() {
        for word in 0..4 {
            let mut r = registry();
            let (_, h) = create_bare(&mut r, "x");
            match word {
                0 => h.words().open_count.store(SENTINEL, Ordering::Release),
                1 => h.words().closed_count.store(SENTINEL, Ordering::Release),
                2 => h.words().expiration_ns.store(SENTINEL, Ordering::Release),
                _ => h.words().expiration_ns.store(1, Ordering::Release),
            }
            r.evaluate_all(now_ms(), monotonic_now_nanos());
            assert!(
                abacus_core::interlock::interlock_is_terminated(&h),
                "word {word}"
            );
            assert_eq!(r.live_count(), 0, "word {word}");
            assert!(r.attach("x").is_err(), "word {word}");
        }
    }

    #[test]
    fn timer_fires_at_target_not_before() {
        let mut r = registry();
        let (_, h) = r
            .create("t".into(), Tier::WaitTimer, None, None, None, None)
            .unwrap();
        let base = h.words().open_count.load(Ordering::Acquire);
        h.words().open_count.store(base + 5, Ordering::Release);
        r.evaluate_all(base + 4, monotonic_now_nanos());
        assert_eq!(h.words().closed_count.load(Ordering::Acquire), base);
        r.evaluate_all(base + 5, monotonic_now_nanos());
        assert_eq!(h.words().closed_count.load(Ordering::Acquire), base + 5);
    }

    #[test]
    fn counter_fires_on_either_word_and_stamps_watched_value() {
        let mut r = registry();
        let (_, src) = create_bare(&mut r, "src");
        let (_, on_open) = create_counter(&mut r, "wo", "src", 0).unwrap();
        let (_, on_closed) = create_counter(&mut r, "wc", "src", 1).unwrap();
        on_open.words().open_count.store(3, Ordering::Release);
        on_closed.words().open_count.store(3, Ordering::Release);
        src.words().open_count.store(4, Ordering::Release);
        r.evaluate_all(now_ms(), monotonic_now_nanos());
        assert_eq!(
            on_open.words().closed_count.load(Ordering::Acquire),
            4,
            "overshoot stamped"
        );
        assert_eq!(on_closed.words().closed_count.load(Ordering::Acquire), 0);
        src.words().closed_count.store(3, Ordering::Release);
        r.evaluate_all(now_ms(), monotonic_now_nanos());
        assert_eq!(on_closed.words().closed_count.load(Ordering::Acquire), 3);
    }

    #[test]
    fn watcher_dies_with_its_target_and_does_not_follow_a_recreated_name() {
        let mut r = registry();
        let (_, src) = create_bare(&mut r, "src");
        let (_, w) = create_counter(&mut r, "w", "src", 1).unwrap();
        w.words().open_count.store(10, Ordering::Release);
        // Recreate the target under the same name: the watcher must not retarget.
        let (_, src2) = create_bare(&mut r, "src");
        src2.words().closed_count.store(10, Ordering::Release);
        r.evaluate_all(now_ms(), monotonic_now_nanos());
        assert!(abacus_core::interlock::interlock_is_terminated(&w));
        assert!(r.attach("w").is_err());
        assert!(abacus_core::interlock::interlock_is_terminated(&src));

        // Free the target: the watcher is reaped on the next pass.
        let (_, src3) = create_bare(&mut r, "src3");
        let (_, w3) = create_counter(&mut r, "w3", "src3", 0).unwrap();
        w3.words().open_count.store(1, Ordering::Release);
        abacus_core::interlock::interlock_free(&src3);
        r.evaluate_all(now_ms(), monotonic_now_nanos());
        assert!(abacus_core::interlock::interlock_is_terminated(&w3));
        assert!(r.attach("w3").is_err());
        assert!(r.attach("src3").is_err());
        // Only src2 (the recreated "src") is left.
        assert_eq!(r.live_count(), 1);
    }

    #[test]
    fn cron_fires_rearms_and_skips_missed_lines() {
        let mut r = registry();
        let (_, h) = r
            .create(
                "c".into(),
                Tier::WaitCron,
                None,
                None,
                Some(10_000_000),
                None,
            )
            .unwrap();
        let first = h.words().open_count.load(Ordering::Acquire);
        let created_at = h.words().closed_count.load(Ordering::Acquire);
        r.evaluate_all(first - 1, monotonic_now_nanos());
        assert_eq!(h.words().closed_count.load(Ordering::Acquire), created_at);
        assert_eq!(h.words().open_count.load(Ordering::Acquire), first);
        r.evaluate_all(first, monotonic_now_nanos());
        assert_eq!(h.words().closed_count.load(Ordering::Acquire), first);
        assert_eq!(h.words().open_count.load(Ordering::Acquire), first + 10);
        // A 35 ms stall: one fire, re-armed to the next future line, no burst.
        r.evaluate_all(first + 45, monotonic_now_nanos());
        assert_eq!(h.words().closed_count.load(Ordering::Acquire), first + 45);
        assert_eq!(h.words().open_count.load(Ordering::Acquire), first + 50);
    }

    #[test]
    fn barrier_fires_only_when_all_met_and_stamps_open_count() {
        let mut r = registry();
        let (_, a) = create_bare(&mut r, "a");
        let (_, b) = create_bare(&mut r, "b");
        let (_, bar) = r
            .create(
                "bar".into(),
                Tier::WaitBarrier,
                None,
                None,
                None,
                Some(vec![("a".into(), 1, 3), ("b".into(), 1, 3)]),
            )
            .unwrap();
        a.words().closed_count.store(3, Ordering::Release);
        r.evaluate_all(now_ms(), monotonic_now_nanos());
        assert_eq!(bar.words().closed_count.load(Ordering::Acquire), 0);
        b.words().closed_count.store(5, Ordering::Release);
        r.evaluate_all(now_ms(), monotonic_now_nanos());
        assert_eq!(bar.words().closed_count.load(Ordering::Acquire), 1);
        assert_eq!(bar.words().open_count.load(Ordering::Acquire), 1);
        // Rearm: open_count += 1, fires again on the next pass since conditions still hold.
        bar.words().open_count.fetch_add(1, Ordering::Release);
        r.evaluate_all(now_ms(), monotonic_now_nanos());
        assert_eq!(bar.words().closed_count.load(Ordering::Acquire), 2);
    }

    #[test]
    fn idle_watcher_dies_with_its_target() {
        let mut r = registry();
        let (_, src) = create_bare(&mut r, "src");
        let (_, w) = create_counter(&mut r, "w", "src", 0).unwrap();
        // No target set: value is 0, the watcher is idle.
        assert_eq!(w.words().open_count.load(Ordering::Acquire), 0);
        abacus_core::interlock::interlock_free(&src);
        r.evaluate_all(now_ms(), monotonic_now_nanos());
        assert!(abacus_core::interlock::interlock_is_terminated(&w));
        assert_eq!(r.live_count(), 0);
    }

    #[test]
    fn barrier_dies_when_a_target_dies() {
        let mut r = registry();
        let (_, a) = create_bare(&mut r, "a");
        let (_, bar) = r
            .create(
                "bar".into(),
                Tier::WaitBarrier,
                None,
                None,
                None,
                Some(vec![("a".into(), 0, 1)]),
            )
            .unwrap();
        abacus_core::interlock::interlock_free(&a);
        r.evaluate_all(now_ms(), monotonic_now_nanos());
        assert!(abacus_core::interlock::interlock_is_terminated(&bar));
    }

    #[test]
    fn slots_are_reused_and_index_stays_consistent() {
        let mut r = registry();
        let (_, a) = create_bare(&mut r, "a");
        create_bare(&mut r, "b");
        abacus_core::interlock::interlock_free(&a);
        r.evaluate_all(now_ms(), monotonic_now_nanos());
        let (id_c, _) = create_bare(&mut r, "c");
        assert_eq!(r.attach("c").unwrap().0, id_c);
        assert!(r.attach("a").is_err());
        assert!(r.attach("b").is_ok());
        assert_eq!(r.live_count(), 2);
        assert_eq!(r.slots.len(), 2, "freed slot reused");
    }

    #[test]
    fn tick_advances_clock_and_keeps_it_alive() {
        let mut r = registry();
        let before = r.clock().words().open_count.load(Ordering::Acquire);
        let now = monotonic_now_nanos() + 5 * NANOS_PER_MS;
        r.tick(now);
        assert_eq!(
            r.clock().words().open_count.load(Ordering::Acquire),
            now / NANOS_PER_MS
        );
        assert!(r.clock().words().open_count.load(Ordering::Acquire) >= before);
        assert!(interlock_read_expiration(r.clock()) >= now);
    }

    #[test]
    fn evaluate_thousand_interlocks_is_fast() {
        let mut r = registry();
        for i in 0..1000 {
            create_bare(&mut r, &format!("i{i}"));
        }
        let t0 = std::time::Instant::now();
        for _ in 0..100 {
            r.evaluate_all(now_ms(), monotonic_now_nanos());
        }
        let per_pass = t0.elapsed() / 100;
        assert!(
            per_pass < std::time::Duration::from_micros(500),
            "per pass {per_pass:?}"
        );
    }
}
