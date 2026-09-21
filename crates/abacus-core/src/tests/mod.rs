//! L0 unit tests for abacus-core. One claim per test; the name is the claim. Each doc
//! comment cites the CONTRACTS.md, LIFECYCLE.md, or SURFACE.md section the test guards.

#![allow(non_snake_case)]

mod clock;
mod error;
mod interlock;

/// errno as set by the last libc call on this thread. Read it immediately after the call.
fn last_errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// Seeded xorshift64 for contention tests. `ABACUS_TEST_SEED` overrides the default so a
/// failure can be replayed; every failure message prints the seed.
struct Xorshift {
    state: u64,
    seed: u64,
}

impl Xorshift {
    fn from_env(default: u64) -> Self {
        let seed = std::env::var("ABACUS_TEST_SEED")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(default);
        let seed = if seed == 0 {
            0x9E37_79B9_7F4A_7C15
        } else {
            seed
        };
        Self { state: seed, seed }
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x
    }

    /// Uniform in `0..n` (n > 0).
    fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }
}
