//! L1: Interlock contract (SURFACE.md "Interlock", "Background touch thread",
//! "Termination"; CONTRACTS.md TTL rules), daemon as a thread.

#![allow(non_snake_case)]

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use abacus_client::{InterlockState, SdkError};
use abacus_core::clock::{futex_wake, monotonic_now_nanos};
use abacus_core::interlock::SENTINEL;
use abacus_tests::{
    attach_words, interlock_words, on_thread, recv_within, wait_for, wait_for_value, ThreadDaemon,
};

const MS: u64 = 1_000_000;

/// SURFACE.md Interlock: open adds to open_count, close to closed_count, value is the
/// signed difference, state derives from value and ttl (LIFECYCLE.md states table).
/// CONTRACTS.md daemon contract: Overrun is informational, never reaped.
#[test]
fn interlock__open_close_value_peek_state() {
    let d = ThreadDaemon::start("il-ocvps");
    let mut client = d.client();
    let il = client.create_interlock("w").expect("create");
    assert_eq!(il.value(), 0);
    assert_eq!(
        il.state(),
        InterlockState::Closed,
        "fresh: peek={:?}",
        il.peek()
    );
    il.open(3).expect("open");
    assert_eq!(
        (il.value(), il.state()),
        (3, InterlockState::Open),
        "after open(3): peek={:?}",
        il.peek()
    );
    il.close(1).expect("close");
    assert_eq!(il.value(), 2, "after close(1): peek={:?}", il.peek());
    il.close(2).expect("close");
    assert_eq!(
        (il.value(), il.state()),
        (0, InterlockState::Closed),
        "after close(2): peek={:?}",
        il.peek()
    );
    assert_eq!(il.peek(), (3, 3));
    il.close(1).expect("close");
    assert_eq!(
        (il.value(), il.state()),
        (-1, InterlockState::Overrun),
        "after overshoot: peek={:?}",
        il.peek()
    );
    let reaped = wait_for(Duration::from_millis(20), Duration::from_millis(1), || {
        il.peek().0 == SENTINEL
    });
    assert!(
        reaped.is_err(),
        "daemon reaped an overrun interlock after {reaped:?}; peek={:?}",
        il.peek()
    );
    assert_eq!(il.peek(), (3, 4));
}

/// CONTRACTS.md TTL rules: touch sets expiration to max(current, now + ms) and never
/// decrements.
#[test]
fn interlock__touch_extends_and_never_shrinks() {
    let d = ThreadDaemon::start("il-touch");
    let mut client = d.client();
    let il = client.create_interlock("t").expect("create");
    let view = attach_words(d.socket_path(), "t");
    let before = monotonic_now_nanos();
    il.touch(500).expect("touch(500)");
    let after = monotonic_now_nanos();
    let (_, _, exp1) = interlock_words(&view);
    assert!(
        exp1 >= before + 500 * MS && exp1 <= after + 500 * MS,
        "touch(500): expiration={exp1} outside [{}, {}]",
        before + 500 * MS,
        after + 500 * MS
    );
    il.touch(10).expect("touch(10)");
    let (_, _, exp2) = interlock_words(&view);
    assert_eq!(
        exp2, exp1,
        "touch(10) after touch(500) moved expiration from {exp1} to {exp2}"
    );
}

/// SURFACE.md Interlock: wait_open/wait_close block until the word reaches the target; an
/// advance from another thread wakes them through the futex, not the 100 ms poll.
#[test]
fn interlock__wait_open_and_wait_close_wake_on_advance() {
    let d = ThreadDaemon::start("il-wake");
    let mut client = d.client();
    let il = Arc::new(client.create_interlock("w").expect("create"));
    for (which, target) in [("open", 5u64), ("close", 7u64)] {
        let waiter = il.clone();
        let rx = on_thread(move || {
            if which == "open" {
                waiter.wait_open(target)
            } else {
                waiter.wait_close(target)
            }
        });
        // Ordering only: give the waiter time to block before the advance.
        std::thread::sleep(Duration::from_millis(5));
        let t_advance = Instant::now();
        if which == "open" {
            il.open(target).expect("open");
        } else {
            il.close(target).expect("close");
        }
        let r = recv_within(&rx, Duration::from_millis(500)).unwrap_or_else(|e| {
            panic!(
                "wait_{which}({target}) never returned: {e}; peek={:?}",
                il.peek()
            )
        });
        let latency = t_advance.elapsed();
        match r {
            Ok(v) => assert!(
                v >= target,
                "wait_{which} returned {v} below target {target}"
            ),
            Err(e) => panic!("wait_{which}({target}) failed: {e}; peek={:?}", il.peek()),
        }
        assert!(
            latency < Duration::from_millis(50),
            "wait_{which} woke {latency:?} after the advance: poll cadence, not a futex wake"
        );
    }
}

fn sentinel_word_test(label: &str, word: usize) {
    let d = ThreadDaemon::start(label);
    let mut client = d.client();
    let il = Arc::new(client.create_interlock("s").expect("create"));
    let view = attach_words(d.socket_path(), "s");
    let waiter = il.clone();
    let rx = on_thread(move || {
        if word == 1 {
            waiter.wait_close(1000)
        } else {
            waiter.wait_open(1000)
        }
    });
    // Ordering only: let the waiter block before the sentinel lands.
    std::thread::sleep(Duration::from_millis(5));
    let w = view.words();
    let target = match word {
        0 => &w.open_count,
        1 => &w.closed_count,
        _ => &w.expiration_ns,
    };
    target.store(SENTINEL, Ordering::Release);
    futex_wake(&w.open_count);
    futex_wake(&w.closed_count);
    let r = recv_within(&rx, Duration::from_millis(300)).unwrap_or_else(|e| {
        panic!(
            "waiter never returned after sentinel on word {word}: {e}; words={:?}",
            interlock_words(&view)
        )
    });
    assert!(
        matches!(r, Err(SdkError::InterlockReaped)),
        "sentinel on word {word}: expected Err(InterlockReaped), got {r:?}; words={:?}",
        interlock_words(&view)
    );
}

/// SURFACE.md Interlock: a word read as SENTINEL returns InterlockReaped immediately
/// (open_count, seen by wait_open).
#[test]
fn interlock__wait_returns_reaped_on_sentinel() {
    sentinel_word_test("il-sent-open", 0);
}

/// SURFACE.md Interlock: SENTINEL on closed_count returns InterlockReaped from wait_close.
#[test]
fn interlock__wait_returns_reaped_on_sentinel_closed_count() {
    sentinel_word_test("il-sent-closed", 1);
}

/// SURFACE.md Interlock: SENTINEL on expiration_ns is re-checked after every wake and
/// returns InterlockReaped.
#[test]
fn interlock__wait_returns_reaped_on_sentinel_expiration_ns() {
    sentinel_word_test("il-sent-exp", 2);
}

/// LIFECYCLE.md SDK state interpretation: a waiter blocked on a word wakes the moment reap
/// stamps it. With the touch thread stopped the TTL lapses, the daemon reaps, and the
/// waiter returns InterlockReaped.
#[test]
fn interlock__wait_returns_reaped_when_ttl_lapses_with_thread_stopped() {
    let d = ThreadDaemon::start("il-lapse-wait");
    let mut client = d.client();
    let mut il = client.create_interlock("l").expect("create");
    il.stop_touch_thread();
    let il = Arc::new(il);
    let waiter = il.clone();
    let t0 = Instant::now();
    let rx = on_thread(move || waiter.wait_open(1000));
    let r = recv_within(&rx, Duration::from_millis(300)).unwrap_or_else(|e| {
        panic!(
            "waiter never returned after TTL lapse: {e}; peek={:?}",
            il.peek()
        )
    });
    let elapsed = t0.elapsed();
    assert!(
        matches!(r, Err(SdkError::InterlockReaped)),
        "expected Err(InterlockReaped), got {r:?} after {elapsed:?}; peek={:?}",
        il.peek()
    );
    assert!(
        elapsed >= Duration::from_millis(50),
        "returned after {elapsed:?}, before any TTL could lapse"
    );
}

/// CONTRACTS.md TTL rules: keepalive registration arms the interlock to 200 ms TTL (the
/// keepalive TTL floor). Without touches the daemon reaps once that TTL lapses. Debug builds run
/// slower, so the deadline is wider.
#[test]
fn interlock__reaped_when_not_touched_within_150ms() {
    let d = ThreadDaemon::start("il-150");
    let mut client = d.client();
    let mut il = client.create_interlock("e").expect("create");
    let t0 = Instant::now();
    il.stop_touch_thread();
    // The keepalive's synchronous first arm sets the TTL to 200 ms (MIN_TOUCH_TTL_MS).
    // Allow enough headroom beyond that for daemon evaluation latency.
    let reap_deadline = if cfg!(debug_assertions) { 400 } else { 300 };
    let elapsed = wait_for(
        Duration::from_millis(reap_deadline),
        Duration::from_millis(1),
        || il.peek() == (SENTINEL, SENTINEL),
    )
    .unwrap_or_else(|e| {
        panic!(
            "not reaped within {reap_deadline} ms: {e}; peek={:?} state={:?}",
            il.peek(),
            il.state()
        )
    });
    assert!(
        t0.elapsed() >= Duration::from_millis(50),
        "reaped after only {:?} (elapsed since stop {elapsed:?})",
        t0.elapsed()
    );
    let touch = il.touch(100);
    assert!(
        matches!(touch, Err(SdkError::InterlockReaped)),
        "touch after reap returned {touch:?}"
    );
    assert_eq!(il.state(), InterlockState::Expired);
}

/// SURFACE.md Background touch thread: the default touch thread keeps the interlock alive
/// without the owner touching. 2 s is 20 creation TTLs.
#[test]
fn interlock__touch_thread_keeps_alive_for_2s() {
    let d = ThreadDaemon::start("il-2s");
    let mut client = d.client();
    let il = client.create_interlock("k").expect("create");
    let view = attach_words(d.socket_path(), "k");
    let died = wait_for(Duration::from_secs(2), Duration::from_millis(50), || {
        let (o, _, e) = interlock_words(&view);
        o == SENTINEL || e == SENTINEL
    });
    assert!(
        died.is_err(),
        "interlock died after {died:?} with the touch thread running; words={:?}",
        interlock_words(&view)
    );
    assert!(
        il.touch(10).is_ok(),
        "touch failed after 2 s: peek={:?}",
        il.peek()
    );
    assert_ne!(il.state(), InterlockState::Expired);
}

/// SURFACE.md Background touch thread: on stop, the heartbeat lapses and the daemon reaps.
/// Window covers today's 80 ms steady-state TTL and the 200 ms keepalive floor.
#[test]
fn interlock__stop_touch_thread_then_reaped() {
    let d = ThreadDaemon::start("il-stop");
    let mut client = d.client();
    let mut il = client.create_interlock("s").expect("create");
    let view = attach_words(d.socket_path(), "s");
    let (_, _, exp0) = interlock_words(&view);
    wait_for(Duration::from_millis(300), Duration::from_millis(5), || {
        interlock_words(&view).2 > exp0
    })
    .expect("touch thread never armed past the creation TTL");
    il.stop_touch_thread();
    let t0 = Instant::now();
    let elapsed = wait_for(Duration::from_millis(400), Duration::from_millis(1), || {
        il.peek() == (SENTINEL, SENTINEL)
    })
    .unwrap_or_else(|e| {
        panic!(
            "not reaped after stop: {e}; words={:?}",
            interlock_words(&view)
        )
    });
    assert!(
        t0.elapsed() >= Duration::from_millis(30),
        "reaped {elapsed:?} after stop: sooner than any TTL"
    );
}

/// SURFACE.md Background touch thread: start_touch_thread replaces the running thread. A
/// 400 ms thread arms far ahead; after replacement by a 10 ms thread the old arm decays and
/// the expiration settles within the short thread's TTL, so the old thread is gone.
#[test]
fn interlock__start_touch_thread_replaces_previous() {
    let d = ThreadDaemon::start("il-replace");
    let mut client = d.client();
    let mut il = client.create_interlock("r").expect("create");
    let view = attach_words(d.socket_path(), "r");
    il.start_touch_thread(400, 2000)
        .expect("start_touch_thread");
    wait_for(Duration::from_millis(200), Duration::from_millis(2), || {
        let (_, _, e) = interlock_words(&view);
        e > monotonic_now_nanos() + 600 * MS
    })
    .unwrap_or_else(|e| {
        panic!(
            "400 ms thread never armed: {e}; words={:?}",
            interlock_words(&view)
        )
    });
    il.start_touch_thread(10, 200).expect("start_touch_thread");
    let (ahead_ms, settled) =
        wait_for_value(Duration::from_secs(3), Duration::from_millis(5), || {
            let (o, _, e) = interlock_words(&view);
            assert_ne!(o, SENTINEL, "interlock reaped during replacement");
            let now = monotonic_now_nanos();
            if e > now && (e - now) < 300 * MS {
                Some((e - now) / MS)
            } else {
                None
            }
        })
        .unwrap_or_else(|e| {
            panic!(
                "expiration never settled under the short thread's TTL: {e}; words={:?}",
                interlock_words(&view)
            )
        });
    assert_ne!(
        il.peek().0,
        SENTINEL,
        "interlock died: ahead={ahead_ms} ms settled after {settled:?}"
    );
    assert!(il.touch(5).is_ok());
}

/// SURFACE.md Termination: free() writes SENTINEL synchronously (touch fails at once), the
/// daemon removes the entry on its next pass, and the name can be created again fresh.
#[test]
fn interlock__free_is_immediate_and_name_is_reusable() {
    let d = ThreadDaemon::start("il-free");
    let mut client = d.client();
    let mut il = client.create_interlock("f").expect("create");
    il.open(1).expect("open");
    il.free();
    let touch = il.touch(100);
    assert!(
        matches!(touch, Err(SdkError::InterlockReaped)),
        "touch right after free returned {touch:?}"
    );
    assert_eq!(il.state(), InterlockState::Expired);
    // Removal latency is attached__free_then_daemon_removes_entry_within_5ms's claim; here
    // the deadline only bounds the test.
    wait_for(
        Duration::from_millis(200),
        Duration::from_micros(200),
        || {
            matches!(
                client.attach_interlock("f"),
                Err(SdkError::InterlockNotFound { .. })
            )
        },
    )
    .unwrap_or_else(|e| panic!("entry still attachable after free: {e}"));
    let il2 = client.create_interlock("f").expect("recreate after free");
    assert_eq!(il2.peek(), (0, 0), "recreated interlock is not fresh");
    assert_eq!(il2.state(), InterlockState::Closed);
}

/// SURFACE.md Termination: dropping the handle only stops the touch thread; the interlock
/// stays live until its TTL lapses, then the daemon reaps it.
#[test]
fn interlock__drop_does_not_free_but_lapses() {
    let d = ThreadDaemon::start("il-drop");
    let mut client = d.client();
    let il = client.create_interlock("d").expect("create");
    let attached = client.attach_interlock("d").expect("attach");
    drop(il);
    let t_drop = Instant::now();
    assert_ne!(
        attached.state(),
        InterlockState::Expired,
        "drop terminated the interlock at once: peek={:?}",
        attached.peek()
    );
    assert_ne!(attached.peek().0, SENTINEL);
    let elapsed = wait_for(Duration::from_millis(400), Duration::from_millis(1), || {
        attached.peek().0 == SENTINEL
    })
    .unwrap_or_else(|e| panic!("never reaped after drop: {e}; peek={:?}", attached.peek()));
    assert!(
        t_drop.elapsed() >= Duration::from_millis(30),
        "reaped {elapsed:?} after drop: a drop must not free"
    );
}

/// CONTRACTS.md UDS surface: create over an existing name reaps the old
/// interlock; the old handle sees InterlockReaped, the new one starts at zero.
#[test]
fn interlock__recreate_old_handle_sees_reaped_new_is_fresh() {
    let d = ThreadDaemon::start("il-recreate");
    let mut client = d.client();
    let il1 = client.create_interlock("contested").expect("create 1");
    il1.open(42).expect("open");
    let il2 = client.create_interlock("contested").expect("create 2");
    wait_for(Duration::from_millis(20), Duration::from_millis(1), || {
        il1.peek() == (SENTINEL, SENTINEL)
    })
    .unwrap_or_else(|e| panic!("old handle not reaped: {e}; peek={:?}", il1.peek()));
    let touch = il1.touch(100);
    assert!(
        matches!(touch, Err(SdkError::InterlockReaped)),
        "old handle touch returned {touch:?}"
    );
    assert_eq!(il1.state(), InterlockState::Expired);
    assert_eq!(il2.peek(), (0, 0), "new interlock is not fresh");
    assert_eq!(il2.state(), InterlockState::Closed);
}
