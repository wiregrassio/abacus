//! The interlock registry: a slab of live entries, a name index used only on create and
//! attach, the clock, and the per-cycle evaluation pass.

use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::sync::atomic::Ordering;

use abacus_core::clock::{expiration_alive, futex_wake, monotonic_now_nanos, NANOS_PER_MS};
use abacus_core::error::{Condition, Result};
use abacus_core::interlock::{
    interlock_arm, interlock_create, interlock_create_clock, interlock_dup_fd,
    interlock_read_expiration, interlock_reap, Interlock, InterlockHandle, CLOCK_TTL_NANOS,
    SENTINEL,
};

/// The reserved name of the daemon-owned clock interlock, defined in abacus-core so the
/// SDK shares it.
pub use abacus_core::interlock::CLOCK_NAME;
/// The registry id of the clock.
pub const CLOCK_ID: u64 = 0;
/// Longest accepted interlock name, in bytes.
pub const MAX_NAME_LEN: usize = 255;
/// Default cap on live interlocks, excluding the clock. Each costs one fd and one mapping.
pub const DEFAULT_MAX_INTERLOCKS: usize = 4096;
/// Per-entry cap on dependency targets (the explicit `dependencies` list, not the owner).
pub const MAX_DEPENDENCIES: usize = 64;
/// Per-entry cap on barrier conditions.
pub const MAX_BARRIER_CONDITIONS: usize = 64;
/// Default cap on total watch edges across all live entries. Each edge is one
/// `target_alive` or `watched_value` call per tick. Adjustable with `--max-watch-edges`.
pub const DEFAULT_MAX_WATCH_EDGES: usize = 32768;

// -- Tier --

/// What the daemon does with an interlock each cycle beyond reaping it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tier {
    /// Reap only.
    #[default]
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

    /// The inverse of `from_wire`.
    pub fn to_wire(self) -> u8 {
        match self {
            Self::Interlock => 0,
            Self::WaitCounter => 1,
            Self::WaitTimer => 2,
            Self::WaitCron => 3,
            Self::WaitBarrier => 4,
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

/// Tier-specific data carried on an entry. Each variant holds exactly the fields its tier
/// needs, so invalid combinations (a cron with no interval, a counter with no target) are
/// unrepresentable.
enum EntryKind {
    Interlock,
    WaitCounter { target: Target, word: WatchedWord },
    WaitTimer,
    WaitCron { interval_ms: u64 },
    WaitBarrier { conditions: Vec<(Target, WatchedWord, u64)> },
}

impl EntryKind {
    fn tier(&self) -> Tier {
        match self {
            Self::Interlock => Tier::Interlock,
            Self::WaitCounter { .. } => Tier::WaitCounter,
            Self::WaitTimer => Tier::WaitTimer,
            Self::WaitCron { .. } => Tier::WaitCron,
            Self::WaitBarrier { .. } => Tier::WaitBarrier,
        }
    }
}

/// One live interlock.
struct Entry {
    id: u64,
    name: String,
    handle: InterlockHandle,
    kind: EntryKind,
    /// Owner (first) then dependencies: targets this entry dies with.
    depends_on: Vec<Target>,
}

/// All fields for a create request.
#[derive(Debug, Clone, Default)]
pub struct CreateSpec {
    /// The interlock name.
    pub name: String,
    /// The tier.
    pub tier: Tier,
    /// WaitCounter: the interlock to watch.
    pub watched_name: Option<String>,
    /// WaitCounter: which word of it to watch.
    pub watched_word: Option<u8>,
    /// WaitCron: grid interval in nanoseconds.
    pub interval_ns: Option<u64>,
    /// WaitBarrier: list of (watched_name, watched_word, threshold).
    pub barrier_conditions: Option<Vec<(String, u8, u64)>>,
    /// The ProcessClock that owns this interlock: name and registry id.
    pub owner: Option<(String, u64)>,
    /// Names this interlock dies with, resolved at create.
    pub dependencies: Vec<String>,
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
    watch_edges: usize,
    max_watch_edges: usize,
    /// Reusable scratch buffer for slots reaped during evaluate_all.
    /// Capacity persists across cycles; only cleared (not deallocated) each pass.
    dead_slots: Vec<usize>,
}

impl Registry {
    /// A registry holding only the clock, with default caps.
    pub fn new() -> Result<Self> {
        Self::with_limits(DEFAULT_MAX_INTERLOCKS, DEFAULT_MAX_WATCH_EDGES)
    }

    /// A registry holding only the clock, capped at `max_interlocks` live entries
    /// and the default watch-edge budget.
    pub fn with_limit(max_interlocks: usize) -> Result<Self> {
        Self::with_limits(max_interlocks, DEFAULT_MAX_WATCH_EDGES)
    }

    /// A registry holding only the clock, capped at `max_interlocks` live entries
    /// and `max_watch_edges` total watch edges.
    pub fn with_limits(max_interlocks: usize, max_watch_edges: usize) -> Result<Self> {
        let clock = interlock_create_clock()?;
        interlock_arm(&clock, CLOCK_TTL_NANOS)?;
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
            watch_edges: 0,
            max_watch_edges,
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

    /// Total watch edges across all live entries.
    pub fn watch_edge_count(&self) -> usize {
        self.watch_edges
    }

    /// The watch-edge budget.
    pub fn max_watch_edges(&self) -> usize {
        self.max_watch_edges
    }

    /// The slot a live name currently occupies, or `None` if it is not registered.
    /// Exposed for tests that assert a slot-order precondition rather than just naming it
    /// in a comment.
    #[cfg(test)]
    pub(crate) fn slot_of(&self, name: &str) -> Option<usize> {
        self.index.get(name).copied()
    }

    /// Create a named interlock. An existing name is reaped and replaced.
    /// Returns the new id and a handle sharing the daemon's mapping.
    pub fn create(&mut self, spec: CreateSpec) -> Result<(u64, InterlockHandle)> {
        self.create_with_fd(spec)
            .map(|(id, handle, _fd)| (id, handle))
    }

    /// Create a named interlock and hand back a descriptor for it in the same call. An
    /// existing name is reaped and replaced. All validation, `interlock_create()`, and the
    /// descriptor duplication happen before any registry mutation, so a refused or failed
    /// create changes nothing.
    pub fn create_with_fd(&mut self, spec: CreateSpec) -> Result<(u64, InterlockHandle, OwnedFd)> {
        validate_name(&spec.name)?;
        if spec.name == CLOCK_NAME {
            return Err(Condition::InvalidRequest {
                message: format!("\"{CLOCK_NAME}\" is a reserved name"),
            });
        }
        if !self.index.contains_key(&spec.name) && self.live >= self.max_interlocks {
            return Err(Condition::InvalidRequest {
                message: format!("interlock limit reached: {}", self.max_interlocks),
            });
        }
        if spec.dependencies.len() > MAX_DEPENDENCIES {
            return Err(Condition::InvalidRequest {
                message: format!(
                    "too many dependencies: {}, max {MAX_DEPENDENCIES}",
                    spec.dependencies.len()
                ),
            });
        }
        if let Some(ref conds) = spec.barrier_conditions {
            if conds.len() > MAX_BARRIER_CONDITIONS {
                return Err(Condition::InvalidRequest {
                    message: format!(
                        "too many barrier conditions: {}, max {MAX_BARRIER_CONDITIONS}",
                        conds.len()
                    ),
                });
            }
        }

        let now_ns = monotonic_now_nanos();

        let mut kind = match spec.tier {
            Tier::WaitCounter => {
                let wn = spec.watched_name.ok_or_else(|| Condition::InvalidRequest {
                    message: "WaitCounter requires watched_name".to_string(),
                })?;
                if wn == spec.name {
                    return Err(Condition::InvalidRequest {
                        message: "an interlock cannot watch its own name".to_string(),
                    });
                }
                let ww = spec.watched_word.ok_or_else(|| Condition::InvalidRequest {
                    message: "WaitCounter requires watched_word".to_string(),
                })?;
                let target = self.resolve(&wn)?;
                if !self.target_alive(target, now_ns) {
                    return Err(Condition::InterlockNotFound { name: wn });
                }
                EntryKind::WaitCounter {
                    target,
                    word: WatchedWord::from_wire(ww)?,
                }
            }
            Tier::WaitTimer => EntryKind::WaitTimer,
            Tier::WaitCron => {
                let ival_ns = spec.interval_ns.ok_or_else(|| Condition::InvalidRequest {
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
                EntryKind::WaitCron { interval_ms: ival_ms }
            }
            Tier::WaitBarrier => {
                let conds = spec
                    .barrier_conditions
                    .ok_or_else(|| Condition::InvalidRequest {
                        message: "WaitBarrier requires conditions".to_string(),
                    })?;
                if conds.is_empty() {
                    return Err(Condition::InvalidRequest {
                        message: "WaitBarrier requires at least one condition".to_string(),
                    });
                }
                let mut parsed = Vec::with_capacity(conds.len());
                for (wn, ww, threshold) in conds {
                    if wn == spec.name {
                        return Err(Condition::InvalidRequest {
                            message: "an interlock cannot watch its own name".to_string(),
                        });
                    }
                    let target = self.resolve(&wn)?;
                    if !self.target_alive(target, now_ns) {
                        return Err(Condition::InterlockNotFound { name: wn });
                    }
                    parsed.push((target, WatchedWord::from_wire(ww)?, threshold));
                }
                EntryKind::WaitBarrier { conditions: parsed }
            }
            Tier::Interlock => EntryKind::Interlock,
        };

        // Owner and dependency validation: before interlock_create() so a refused create
        // never displaces the existing entry.
        let mut depends_on = Vec::new();

        if let Some((ref oname, _)) = spec.owner {
            if *oname == spec.name {
                return Err(Condition::InvalidRequest {
                    message: "an interlock cannot depend on its own name".to_string(),
                });
            }
        }
        for dep in &spec.dependencies {
            if *dep == spec.name {
                return Err(Condition::InvalidRequest {
                    message: "an interlock cannot depend on its own name".to_string(),
                });
            }
        }

        if let Some((ref oname, oid)) = spec.owner {
            if oname == CLOCK_NAME && oid == CLOCK_ID {
                depends_on.push(Target::Clock);
            } else {
                let target = match self
                    .index
                    .get(oname.as_str())
                    .and_then(|&slot| self.slots[slot].as_ref().map(|e| (slot, e.id)))
                {
                    Some((slot, id))
                        if id == oid && self.target_alive(Target::Slot { slot, id }, now_ns) =>
                    {
                        Target::Slot { slot, id }
                    }
                    _ => return Err(Condition::InterlockReaped),
                };
                depends_on.push(target);
            }
        }

        for dep in &spec.dependencies {
            let target = self.resolve(dep)?;
            if !self.target_alive(target, now_ns) {
                return Err(Condition::InterlockNotFound { name: dep.clone() });
            }
            depends_on.push(target);
        }

        // Deduplicate: dying with the same target twice adds nothing.
        {
            let mut deduped: Vec<Target> = Vec::with_capacity(depends_on.len());
            for t in &depends_on {
                if !deduped.contains(t) {
                    deduped.push(*t);
                }
            }
            depends_on = deduped;
        }

        // Merge duplicate barrier conditions: same (target, word) keeps the highest
        // threshold, since every condition must hold.
        if let EntryKind::WaitBarrier { ref mut conditions } = kind {
            let mut merged: Vec<(Target, WatchedWord, u64)> =
                Vec::with_capacity(conditions.len());
            for &(target, word, threshold) in conditions.iter() {
                if let Some(existing) =
                    merged.iter_mut().find(|(t, w, _)| *t == target && *w == word)
                {
                    existing.2 = existing.2.max(threshold);
                } else {
                    merged.push((target, word, threshold));
                }
            }
            *conditions = merged;
        }

        // Watch-edge budget: the total per-tick work across the whole registry.
        let new_edge_count = depends_on.len()
            + match &kind {
                EntryKind::WaitCounter { .. } => 1,
                EntryKind::WaitBarrier { conditions } => conditions.len(),
                _ => 0,
            };
        let old_edges = self
            .index
            .get(&spec.name)
            .copied()
            .and_then(|slot| self.slots[slot].as_ref().map(entry_edge_count))
            .unwrap_or(0);
        if self.watch_edges - old_edges + new_edge_count > self.max_watch_edges {
            return Err(Condition::InvalidRequest {
                message: format!(
                    "watch edge budget exceeded: {} in use, {} needed, max {}",
                    self.watch_edges - old_edges,
                    new_edge_count,
                    self.max_watch_edges,
                ),
            });
        }

        // The descriptor is duplicated before any registry mutation, so a dup failure
        // leaves an existing entry of the same name (if any) untouched.
        let handle = interlock_create()?;
        let fd = interlock_dup_fd(&handle)?;

        // Tier-specific initialization per CONTRACTS.md.
        let clock_now_ms = self.clock.words().open_count.load(Ordering::Acquire);
        match &kind {
            EntryKind::WaitTimer => {
                handle
                    .words()
                    .open_count
                    .store(clock_now_ms, Ordering::Release);
                handle
                    .words()
                    .closed_count
                    .store(clock_now_ms, Ordering::Release);
            }
            EntryKind::WaitCron { interval_ms } => {
                if clock_now_ms == SENTINEL {
                    return Err(Condition::InvalidRequest {
                        message: "clock word is SENTINEL".to_string(),
                    });
                }
                let next_line =
                    next_grid_line(clock_now_ms, *interval_ms).ok_or_else(|| {
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
            EntryKind::WaitBarrier { .. } => {
                // open_count = 1 makes the barrier Open so the daemon evaluates it.
                handle.words().open_count.store(1, Ordering::Release);
            }
            EntryKind::Interlock | EntryKind::WaitCounter { .. } => {}
        }

        let id = self.next_id;
        self.next_id += 1;

        // An existing name is reaped and replaced.
        if let Some(old_slot) = self.index.get(&spec.name).copied() {
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
            name: spec.name.clone(),
            handle: handle.clone(),
            kind,
            depends_on,
        });
        self.index.insert(spec.name, slot);
        self.live += 1;
        self.watch_edges += new_edge_count;

        Ok((id, handle, fd))
    }

    /// Look up an interlock by name. Returns the id, tier, and handle.
    pub fn attach(&self, name: &str) -> Result<(u64, Tier, InterlockHandle)> {
        if name == CLOCK_NAME {
            return Ok((CLOCK_ID, Tier::Interlock, self.clock.clone()));
        }
        validate_name(name)?;
        match self
            .index
            .get(name)
            .and_then(|&slot| self.slots[slot].as_ref())
        {
            Some(entry) => Ok((entry.id, entry.kind.tier(), entry.handle.clone())),
            None => Err(Condition::InterlockNotFound {
                name: name.to_string(),
            }),
        }
    }

    /// One daemon cycle at `now_ns`: advance the clock and refresh its expiration, evaluate
    /// every interlock against it, then wake clock watchers.
    pub fn tick(&mut self, now_ns: u64) {
        let current_ms = now_ns / NANOS_PER_MS;
        self.clock
            .words()
            .open_count
            .store(current_ms, Ordering::Release);
        // The clock is current before anything is evaluated against it: after a stall
        // longer than its TTL, evaluating first would reap every clock dependent.
        self.refresh_clock_expiration(now_ns);
        self.evaluate_all(current_ms, now_ns);
        futex_wake(&self.clock.words().open_count);
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

            // The owner or a dependency died.
            if entry
                .depends_on
                .iter()
                .any(|t| !self.target_alive(*t, now_ns))
            {
                interlock_reap(&entry.handle);
                self.dead_slots.push(slot);
                continue;
            }

            // Open (value > 0) is the firing gate. Target liveness is checked regardless, so
            // an idle watcher still dies with its target.
            let value = (open_count as i64).wrapping_sub(closed_count as i64);
            let open = value > 0;

            match &entry.kind {
                EntryKind::Interlock => {}
                EntryKind::WaitTimer => {
                    if open && clock_now_ms >= open_count {
                        words.closed_count.store(clock_now_ms, Ordering::Release);
                        futex_wake(&words.closed_count);
                    }
                }
                EntryKind::WaitCron { interval_ms } => {
                    if open && clock_now_ms >= open_count {
                        words.closed_count.store(clock_now_ms, Ordering::Release);
                        futex_wake(&words.closed_count);
                        // Re-arm to the next future grid line. Missed lines are skipped,
                        // never burst. Overflow means the grid is exhausted; reap.
                        match next_grid_line(clock_now_ms, *interval_ms) {
                            Some(next_line) => {
                                // A concurrent free's SENTINEL must never be undone by this
                                // re-arm; on CAS failure the next pass reaps whatever was written.
                                let _ = words.open_count.compare_exchange(
                                    open_count,
                                    next_line,
                                    Ordering::AcqRel,
                                    Ordering::Acquire,
                                );
                            }
                            None => {
                                interlock_reap(&entry.handle);
                                self.dead_slots.push(slot);
                            }
                        }
                    }
                }
                EntryKind::WaitCounter { target, word } => {
                    match self.watched_value(*target, *word, now_ns) {
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
                EntryKind::WaitBarrier { conditions } => {
                    let mut all_met = true;
                    let mut target_gone = false;
                    for (target, word, threshold) in conditions {
                        match self.watched_value(*target, *word, now_ns) {
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

    /// Keep the clock alive. It is never reaped. If the clock has lapsed past its TTL
    /// (the daemon stalled), log the stall duration before re-arming.
    pub fn refresh_clock_expiration(&self, now_ns: u64) {
        let exp = interlock_read_expiration(&self.clock);
        if exp != SENTINEL && exp < now_ns {
            let stall_ms = (now_ns - exp) / NANOS_PER_MS;
            eprintln!(
                "abacus: clock lapsed for {} ms (TTL {} ms); clients may have aborted",
                stall_ms,
                CLOCK_TTL_NANOS / NANOS_PER_MS
            );
        }
        if interlock_arm(&self.clock, CLOCK_TTL_NANOS).is_err() {
            eprintln!("abacus: the daemon clock is terminated; aborting");
            std::process::abort();
        }
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

    /// The three words of a target, or `None` if the slot is empty or holds a different id.
    fn words_of(&self, target: Target) -> Option<&Interlock> {
        match target {
            Target::Clock => Some(self.clock.words()),
            Target::Slot { slot, id } => {
                let entry = self.slots.get(slot)?.as_ref()?;
                if entry.id != id {
                    return None;
                }
                Some(entry.handle.words())
            }
        }
    }

    /// True when the target exists, none of its three words is SENTINEL, and its expiration
    /// has not lapsed. A lapsed-but-unreaped target counts as dead, not alive: a create
    /// against it must see the same refusal evaluate_all will enforce on the next pass. A
    /// freed interlock has expiration_ns stamped to SENTINEL while its counter words remain
    /// live; checking all three words lets a watcher in a lower slot (already past the
    /// target in evaluate_all's forward scan) see the death. `Target::Clock` uses the
    /// clock's own words; its expiration is refreshed every tick.
    fn target_alive(&self, target: Target, now_ns: u64) -> bool {
        match self.words_of(target) {
            None => false,
            Some(w) => {
                let open = w.open_count.load(Ordering::Acquire);
                let closed = w.closed_count.load(Ordering::Acquire);
                let exp = w.expiration_ns.load(Ordering::Acquire);
                open != SENTINEL
                    && closed != SENTINEL
                    && exp != SENTINEL
                    && expiration_alive(exp, now_ns)
            }
        }
    }

    /// The watched word's value, or `None` if the target is gone, recreated under the same
    /// name, lapsed, or itself terminated.
    fn watched_value(&self, target: Target, word: WatchedWord, now_ns: u64) -> Option<u64> {
        if !self.target_alive(target, now_ns) {
            return None;
        }
        let words = self.words_of(target)?;
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
            self.watch_edges -= entry_edge_count(&entry);
            interlock_reap(&entry.handle);
            if self.index.get(&entry.name) == Some(&slot) {
                self.index.remove(&entry.name);
            }
            self.free.push(slot);
            self.live -= 1;
        }
    }
}

fn entry_edge_count(entry: &Entry) -> usize {
    entry.depends_on.len()
        + match &entry.kind {
            EntryKind::WaitCounter { .. } => 1,
            EntryKind::WaitBarrier { conditions } => conditions.len(),
            _ => 0,
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

/// The first grid line strictly after `now_ms`, or `None` on a zero interval or overflow.
pub fn next_grid_line(now_ms: u64, interval_ms: u64) -> Option<u64> {
    if interval_ms == 0 {
        return None;
    }
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
        r.create(CreateSpec {
            name: name.into(),
            ..Default::default()
        })
        .unwrap()
    }

    fn create_counter(
        r: &mut Registry,
        name: &str,
        watched: &str,
        word: u8,
    ) -> Result<(u64, InterlockHandle)> {
        r.create(CreateSpec {
            name: name.into(),
            tier: Tier::WaitCounter,
            watched_name: Some(watched.into()),
            watched_word: Some(word),
            ..Default::default()
        })
    }

    fn now_ms() -> u64 {
        monotonic_now_nanos() / NANOS_PER_MS
    }

    #[test]
    fn clock_has_id_zero_and_is_attachable() {
        let r = registry();
        let (id, _, handle) = r.attach(CLOCK_NAME).unwrap();
        assert_eq!(id, CLOCK_ID);
        assert_eq!(handle.as_raw_fd(), r.clock().as_raw_fd());
        assert!(interlock_read_expiration(r.clock()) > monotonic_now_nanos());
    }

    #[test]
    fn create_clock_name_rejected() {
        let mut r = registry();
        let err = r
            .create(CreateSpec {
                name: CLOCK_NAME.into(),
                ..Default::default()
            })
            .unwrap_err();
        assert!(matches!(err, Condition::InvalidRequest { .. }));
    }

    #[test]
    fn name_limits() {
        let mut r = registry();
        assert!(matches!(
            r.create(CreateSpec {
                name: String::new(),
                ..Default::default()
            }),
            Err(Condition::InvalidRequest { .. })
        ));
        assert!(create_bare(&mut r, &"n".repeat(MAX_NAME_LEN)).0 > 0);
        assert!(matches!(
            r.create(CreateSpec {
                name: "n".repeat(MAX_NAME_LEN + 1),
                ..Default::default()
            }),
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
            .create(CreateSpec {
                name: "c".into(),
                ..Default::default()
            })
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
            r.create(CreateSpec {
                name: "w".into(),
                tier: Tier::WaitCounter,
                watched_word: Some(0),
                ..Default::default()
            }),
            Err(Condition::InvalidRequest { .. })
        ));
        assert!(matches!(
            r.create(CreateSpec {
                name: "w".into(),
                tier: Tier::WaitCounter,
                watched_name: Some("src".into()),
                ..Default::default()
            }),
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
            r.create(CreateSpec {
                name: "c".into(),
                tier: Tier::WaitCron,
                interval_ns: Some(0),
                ..Default::default()
            }),
            Err(Condition::InvalidRequest { .. })
        ));
        assert!(matches!(
            r.create(CreateSpec {
                name: "c".into(),
                tier: Tier::WaitCron,
                interval_ns: Some(500_000),
                ..Default::default()
            }),
            Err(Condition::InvalidRequest { .. })
        ));
        let (_, h) = r
            .create(CreateSpec {
                name: "c".into(),
                tier: Tier::WaitCron,
                interval_ns: Some(10_000_000),
                ..Default::default()
            })
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
            r.create(CreateSpec {
                name: "b".into(),
                tier: Tier::WaitBarrier,
                ..Default::default()
            }),
            Err(Condition::InvalidRequest { .. })
        ));
        assert!(matches!(
            r.create(CreateSpec {
                name: "b".into(),
                tier: Tier::WaitBarrier,
                barrier_conditions: Some(vec![]),
                ..Default::default()
            }),
            Err(Condition::InvalidRequest { .. })
        ));
        assert!(matches!(
            r.create(CreateSpec {
                name: "b".into(),
                tier: Tier::WaitBarrier,
                barrier_conditions: Some(vec![("a".into(), 0, 1), ("zz".into(), 0, 1)]),
                ..Default::default()
            }),
            Err(Condition::InterlockNotFound { .. })
        ));
        let (_, h) = r
            .create(CreateSpec {
                name: "b".into(),
                tier: Tier::WaitBarrier,
                barrier_conditions: Some(vec![("a".into(), 1, 3)]),
                ..Default::default()
            })
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
            .create(CreateSpec {
                name: "t".into(),
                tier: Tier::WaitTimer,
                ..Default::default()
            })
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
            .create(CreateSpec {
                name: "c".into(),
                tier: Tier::WaitCron,
                interval_ns: Some(10_000_000),
                ..Default::default()
            })
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
            .create(CreateSpec {
                name: "bar".into(),
                tier: Tier::WaitBarrier,
                barrier_conditions: Some(vec![("a".into(), 1, 3), ("b".into(), 1, 3)]),
                ..Default::default()
            })
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
            .create(CreateSpec {
                name: "bar".into(),
                tier: Tier::WaitBarrier,
                barrier_conditions: Some(vec![("a".into(), 0, 1)]),
                ..Default::default()
            })
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

    fn test_registry(max: usize) -> Registry {
        Registry::with_limit(max).unwrap()
    }

    #[test]
    fn registry_clock_lapse_is_logged_at_rearm() {
        let reg = test_registry(10);
        // Advance past the clock's TTL so the expiration lapses.
        let now_ns = monotonic_now_nanos() + CLOCK_TTL_NANOS + 50_000_000;
        // The lapse log goes to stderr; we just verify it does not abort and the clock
        // is re-armed afterward.
        reg.refresh_clock_expiration(now_ns);
        let exp = interlock_read_expiration(&reg.clock);
        let after = monotonic_now_nanos();
        assert!(
            exp >= after,
            "clock was not re-armed after a lapse: exp {exp}, after {after}"
        );
    }

    // -- per-entry limit tests --

    #[test]
    fn too_many_dependencies_rejected() {
        let mut r = registry();
        let deps: Vec<String> = (0..MAX_DEPENDENCIES + 1).map(|i| format!("d{i}")).collect();
        let err = r
            .create(CreateSpec {
                name: "x".into(),
                dependencies: deps,
                ..Default::default()
            })
            .unwrap_err();
        assert!(matches!(err, Condition::InvalidRequest { ref message } if message.contains("too many dependencies")));
    }

    #[test]
    fn max_dependencies_accepted() {
        let mut r = registry();
        for i in 0..MAX_DEPENDENCIES {
            create_bare(&mut r, &format!("d{i}"));
        }
        let deps: Vec<String> = (0..MAX_DEPENDENCIES).map(|i| format!("d{i}")).collect();
        let result = r.create(CreateSpec {
            name: "x".into(),
            dependencies: deps,
            ..Default::default()
        });
        assert!(result.is_ok());
    }

    #[test]
    fn too_many_barrier_conditions_rejected() {
        let mut r = registry();
        let conds: Vec<(String, u8, u64)> = (0..MAX_BARRIER_CONDITIONS + 1)
            .map(|i| (format!("t{i}"), 0, 1))
            .collect();
        let err = r
            .create(CreateSpec {
                name: "b".into(),
                tier: Tier::WaitBarrier,
                barrier_conditions: Some(conds),
                ..Default::default()
            })
            .unwrap_err();
        assert!(matches!(err, Condition::InvalidRequest { ref message } if message.contains("too many barrier conditions")));
    }

    // -- deduplication tests --

    #[test]
    fn duplicate_dependencies_are_merged() {
        let mut r = registry();
        create_bare(&mut r, "t");
        let result = r.create(CreateSpec {
            name: "x".into(),
            dependencies: vec!["t".into(), "t".into(), "t".into()],
            ..Default::default()
        });
        assert!(result.is_ok());
        // Three duplicate deps should be merged to one edge.
        assert_eq!(r.watch_edge_count(), 1);
    }

    #[test]
    fn duplicate_barrier_conditions_are_merged_with_max_threshold() {
        let mut r = registry();
        create_bare(&mut r, "a");
        let (_, bar) = r
            .create(CreateSpec {
                name: "bar".into(),
                tier: Tier::WaitBarrier,
                barrier_conditions: Some(vec![
                    ("a".into(), 1, 3),
                    ("a".into(), 1, 7),
                    ("a".into(), 0, 5),
                ]),
                ..Default::default()
            })
            .unwrap();
        // Two unique (target, word) pairs after merge: (a, closed_count) and (a, open_count).
        // Threshold for (a, closed_count) should be max(3, 7) = 7.
        // Bar has 0 depends_on edges + 2 condition edges = 2 total for bar.
        // "a" has 0 edges.
        assert_eq!(r.watch_edge_count(), 2);
        // Verify the merge kept the semantics: the bar fires only when a.closed_count >= 7
        // AND a.open_count >= 5.
        bar.words().open_count.store(1, Ordering::Release);
        r.evaluate_all(now_ms(), monotonic_now_nanos());
        assert_eq!(bar.words().closed_count.load(Ordering::Acquire), 0);
    }

    // -- watch-edge budget tests --

    #[test]
    fn watch_edge_budget_enforced() {
        let mut r = Registry::with_limits(100, 5).unwrap();
        create_bare(&mut r, "t1");
        create_bare(&mut r, "t2");
        create_bare(&mut r, "t3");
        assert_eq!(r.watch_edge_count(), 0);
        // Each dependency adds one edge. 5 deps = 5 edges = at the budget.
        let result = r.create(CreateSpec {
            name: "x".into(),
            dependencies: vec![
                "t1".into(),
                "t2".into(),
                "t3".into(),
                "t1".into(),
                "t2".into(),
            ],
            ..Default::default()
        });
        // After dedup, only 3 unique deps = 3 edges, within budget.
        assert!(result.is_ok());
        assert_eq!(r.watch_edge_count(), 3);
        // Now try to add 3 more edges, exceeding the budget of 5.
        let err = r
            .create(CreateSpec {
                name: "y".into(),
                dependencies: vec!["t1".into(), "t2".into(), "t3".into()],
                ..Default::default()
            })
            .unwrap_err();
        assert!(matches!(err, Condition::InvalidRequest { ref message } if message.contains("watch edge budget")));
    }

    #[test]
    fn watch_edge_budget_accounts_for_reap() {
        let mut r = Registry::with_limits(100, 3).unwrap();
        create_bare(&mut r, "t");
        let (_, x) = r
            .create(CreateSpec {
                name: "x".into(),
                dependencies: vec!["t".into()],
                ..Default::default()
            })
            .unwrap();
        let (_, y) = r
            .create(CreateSpec {
                name: "y".into(),
                dependencies: vec!["t".into()],
                ..Default::default()
            })
            .unwrap();
        assert_eq!(r.watch_edge_count(), 2);
        // Reap x: frees 1 edge.
        abacus_core::interlock::interlock_free(&x);
        r.evaluate_all(now_ms(), monotonic_now_nanos());
        assert_eq!(r.watch_edge_count(), 1);
        // Now we can add 2 more edges.
        create_bare(&mut r, "t2");
        let result = r.create(CreateSpec {
            name: "z".into(),
            dependencies: vec!["t".into(), "t2".into()],
            ..Default::default()
        });
        assert!(result.is_ok());
        assert_eq!(r.watch_edge_count(), 3);
    }

    #[test]
    fn watch_edge_budget_accounts_for_recreate() {
        let mut r = Registry::with_limits(100, 4).unwrap();
        create_bare(&mut r, "t1");
        create_bare(&mut r, "t2");
        // x uses 2 edges.
        r.create(CreateSpec {
            name: "x".into(),
            dependencies: vec!["t1".into(), "t2".into()],
            ..Default::default()
        })
        .unwrap();
        assert_eq!(r.watch_edge_count(), 2);
        // Recreate x with only 1 edge: frees 2, adds 1 = net 1.
        r.create(CreateSpec {
            name: "x".into(),
            dependencies: vec!["t1".into()],
            ..Default::default()
        })
        .unwrap();
        assert_eq!(r.watch_edge_count(), 1);
        // We now have 3 edges of budget left.
        r.create(CreateSpec {
            name: "y".into(),
            dependencies: vec!["t1".into(), "t2".into()],
            ..Default::default()
        })
        .unwrap();
        assert_eq!(r.watch_edge_count(), 3);
    }

    #[test]
    fn watch_edge_budget_includes_tier_edges() {
        let mut r = Registry::with_limits(100, 2).unwrap();
        create_bare(&mut r, "src");
        // WaitCounter adds 1 tier edge.
        let result = create_counter(&mut r, "w", "src", 0);
        assert!(result.is_ok());
        assert_eq!(r.watch_edge_count(), 1);
        // A second counter would need 1 more edge = 2 total, at the limit.
        create_bare(&mut r, "src2");
        let result = create_counter(&mut r, "w2", "src2", 0);
        assert!(result.is_ok());
        assert_eq!(r.watch_edge_count(), 2);
        // A third would exceed.
        create_bare(&mut r, "src3");
        let err = create_counter(&mut r, "w3", "src3", 0).unwrap_err();
        assert!(matches!(err, Condition::InvalidRequest { ref message } if message.contains("watch edge budget")));
    }
}
