//! Abacus RTS core: the interlock primitive, the monotonic clock and futex helpers, and the
//! error vocabulary shared by the daemon and the client SDK. Depends on libc only.

#![warn(missing_docs)]

#[cfg(not(target_os = "linux"))]
compile_error!("abacus-rts is Linux-only: memfd, futex, SCM_RIGHTS");

pub mod clock;
pub mod error;
pub mod interlock;
#[cfg(test)]
mod tests;
