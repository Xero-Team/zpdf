//! Wall-clock access that degrades gracefully on `wasm32-unknown-unknown`.
//!
//! `std::time::Instant::now()` and `SystemTime::now()` *panic* on bare wasm32
//! (no OS clock). The anti-hang wall-clock budgets in the interpreter, the CPU
//! backend, and the SVG exporter are catch-all guards layered over the primary
//! deterministic budgets (operator counts, command counts, pixel budgets, mask
//! byte caps) — so on wasm they are simply disabled: [`Instant::now`] returns a
//! fixed epoch and a deadline built by `now() + budget` is never reached. The
//! deterministic budgets keep adversarial inputs bounded there.
//!
//! On every other target this is a zero-cost re-export of `std::time::Instant`.

#[cfg(not(target_arch = "wasm32"))]
pub use std::time::Instant;

#[cfg(target_arch = "wasm32")]
pub use wasm::Instant;

#[cfg(target_arch = "wasm32")]
mod wasm {
    use std::ops::Add;
    use std::time::Duration;

    /// Inert stand-in for `std::time::Instant` on bare wasm32: `now()` is a
    /// fixed origin and adding any budget yields an unreachable deadline, so
    /// `now() >= deadline` stays false and `elapsed()` reads zero.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub struct Instant(u64);

    impl Instant {
        pub fn now() -> Self {
            Instant(0)
        }

        pub fn elapsed(&self) -> Duration {
            Duration::ZERO
        }

        pub fn duration_since(&self, _earlier: Instant) -> Duration {
            Duration::ZERO
        }
    }

    impl Add<Duration> for Instant {
        type Output = Instant;

        fn add(self, _rhs: Duration) -> Instant {
            Instant(u64::MAX)
        }
    }
}

/// Seconds since the Unix epoch, or 0 on targets without a wall clock
/// (bare wasm32). Callers stamping dates (e.g. `/ModDate`) fall back to the
/// epoch rather than panicking.
pub fn unix_seconds() -> u64 {
    #[cfg(not(target_arch = "wasm32"))]
    {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }
    #[cfg(target_arch = "wasm32")]
    {
        0
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn deadline_pattern_works() {
        let deadline = super::Instant::now() + std::time::Duration::from_secs(8);
        assert!(super::Instant::now() < deadline);
    }
}
