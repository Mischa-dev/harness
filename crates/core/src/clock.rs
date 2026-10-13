//! All time goes through [`Clock`]. Only the daemon's wiring constructs a
//! [`SystemClock`]; everything else receives a `&dyn Clock` so the simulator
//! and tests can drive time with a [`FakeClock`].

use std::sync::Mutex;

use time::{Duration, OffsetDateTime};

pub trait Clock: Send + Sync {
    fn now(&self) -> OffsetDateTime;
}

/// The real clock. Construct it only in the daemon's wiring.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> OffsetDateTime {
        OffsetDateTime::now_utc()
    }
}

/// A clock that moves only when told to.
#[derive(Debug)]
pub struct FakeClock {
    now: Mutex<OffsetDateTime>,
}

impl FakeClock {
    pub fn new(start: OffsetDateTime) -> Self {
        Self {
            now: Mutex::new(start),
        }
    }

    /// Starts at 2026-01-05 09:00 UTC, a Monday, so scenarios read naturally.
    pub fn at_default_start() -> Self {
        Self::new(time::macros::datetime!(2026-01-05 09:00 UTC))
    }

    pub fn advance(&self, by: Duration) {
        let mut now = self.now.lock().expect("clock poisoned");
        *now += by;
    }

    pub fn set(&self, to: OffsetDateTime) {
        *self.now.lock().expect("clock poisoned") = to;
    }
}

impl Clock for FakeClock {
    fn now(&self) -> OffsetDateTime {
        *self.now.lock().expect("clock poisoned")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fake_clock_moves_only_when_advanced() {
        let clock = FakeClock::at_default_start();
        let start = clock.now();
        assert_eq!(clock.now(), start);
        clock.advance(Duration::hours(3));
        assert_eq!(clock.now(), start + Duration::hours(3));
    }
}
