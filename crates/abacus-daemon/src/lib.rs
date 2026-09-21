//! Abacus RTS daemon: a 1 ms loop that reaps expired interlocks, wakes watchers whose
//! condition is met, advances the clock interlock, and hands out interlock fds over UDS.

#![warn(missing_docs)]

pub mod daemon;
pub mod registry;
#[cfg(test)]
mod tests;
pub mod transport;
