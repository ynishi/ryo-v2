//! [`Clock`] adapters.

use crate::app::ports::Clock;
use std::time::Instant;

/// Milliseconds since construction, from the monotonic clock.
pub struct MonotonicClock {
    start: Instant,
}

impl MonotonicClock {
    /// Starts counting now.
    pub fn new() -> Self {
        Self {
            start: Instant::now(),
        }
    }
}

impl Default for MonotonicClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for MonotonicClock {
    fn now_ms(&self) -> u64 {
        u64::try_from(self.start.elapsed().as_millis()).unwrap_or(u64::MAX)
    }
}

/// test 用。常に同じ値。
pub struct FixedClock(pub u64);

impl Clock for FixedClock {
    fn now_ms(&self) -> u64 {
        self.0
    }
}
