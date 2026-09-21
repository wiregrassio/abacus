//! Compile-fail doctests for the CONTRACTS.md permission table.
//!
//! Each "no method" cell of the table is one `compile_fail` doctest on a marker function
//! named after the row and cell, so `cargo test -p abacus-tests --doc` reports it under the
//! claim's name. Runtime cells (the r/w and r/o paths that exist) are tested in
//! `tests/permissions.rs`. The doctests live here, not on the SDK types, so the suite adds
//! no lines to product source files. Every snippet fails only because the method is absent
//! (E0599); a snippet that compiled would mean a permission leaked.

#![allow(non_snake_case)]

/// CONTRACTS.md permission model, row "Attacher (Interlock)", cell expiration_ns r/o: an
/// AttachedInterlock has no `touch`.
///
/// ```compile_fail,E0599
/// fn f(a: &abacus_client::AttachedInterlock) { let _ = a.touch(10); }
/// ```
pub fn attached__cannot_touch() {}

/// CONTRACTS.md permission model, row "Attacher (Interlock)": no touch thread either, since
/// there is nothing an attacher may keep alive.
///
/// ```compile_fail,E0599
/// fn f(a: &mut abacus_client::AttachedInterlock) { let _ = a.start_touch_thread(10); }
/// ```
pub fn attached__cannot_start_touch_thread() {}

/// CONTRACTS.md permission model, row "Attacher (WaitCounter)": read-only, no `open`.
///
/// ```compile_fail,E0599
/// fn f(a: &abacus_client::AttachedWaitCounter) { a.open(1); }
/// ```
pub fn attached_wait_counter__is_read_only() {}

/// Row "Attacher (WaitCounter)": no `close`.
///
/// ```compile_fail,E0599
/// fn f(a: &abacus_client::AttachedWaitCounter) { a.close(1); }
/// ```
pub fn attached_wait_counter__has_no_close() {}

/// Row "Attacher (WaitCounter)": no `touch`.
///
/// ```compile_fail,E0599
/// fn f(a: &abacus_client::AttachedWaitCounter) { let _ = a.touch(10); }
/// ```
pub fn attached_wait_counter__has_no_touch() {}

/// Row "Attacher (WaitCounter)": no `free` (no write access to any word).
///
/// ```compile_fail,E0599
/// fn f(a: &mut abacus_client::AttachedWaitCounter) { a.free(); }
/// ```
pub fn attached_wait_counter__has_no_free() {}

/// Row "Attacher (WaitCounter)": no `wait_until` (the creator owns the target).
///
/// ```compile_fail,E0599
/// fn f(a: &abacus_client::AttachedWaitCounter) { let _ = a.wait_until(1, 10); }
/// ```
pub fn attached_wait_counter__has_no_wait_until() {}

/// Row "Attacher (clock)": the clock is never freed by a client.
///
/// ```compile_fail,E0599
/// fn f(c: &mut abacus_client::ClockHandle) { c.free(); }
/// ```
pub fn clock__has_no_free() {}

/// Row "Attacher (clock)": open_count r/o.
///
/// ```compile_fail,E0599
/// fn f(c: &abacus_client::ClockHandle) { c.open(1); }
/// ```
pub fn clock__has_no_open() {}

/// Row "Attacher (clock)": closed_count r/o.
///
/// ```compile_fail,E0599
/// fn f(c: &abacus_client::ClockHandle) { c.close(1); }
/// ```
pub fn clock__has_no_close() {}

/// Row "Attacher (clock)": expiration_ns r/o.
///
/// ```compile_fail,E0599
/// fn f(c: &abacus_client::ClockHandle) { let _ = c.touch(10); }
/// ```
pub fn clock__has_no_touch() {}

/// Row "Creator (WaitCounter/WaitTimer)", cell closed_count r/o: the daemon writes it, so a
/// WaitCounter has no `close`.
///
/// ```compile_fail,E0599
/// fn f(c: &abacus_client::WaitCounter) { c.close(1); }
/// ```
pub fn wait_counter__has_no_close() {}

/// Row "Creator (WaitCounter/WaitTimer)", cell closed_count r/o: a WaitTimer has no `close`.
///
/// ```compile_fail,E0599
/// fn f(t: &abacus_client::WaitTimer) { t.close(1); }
/// ```
pub fn wait_timer__has_no_close() {}

/// Row "WaitTimer: not attachable": the client exposes no attach path for timers. (The wire
/// carries no tier on attach, so attach by name is not tier-checked; deferred to wire v2.)
///
/// ```compile_fail,E0599
/// fn f(c: &mut abacus_client::AbacusClient) { let _ = c.attach_wait_timer("t"); }
/// ```
pub fn client__has_no_attach_wait_timer() {}
