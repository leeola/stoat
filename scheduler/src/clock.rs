use parking_lot::Mutex;
use std::time::{Duration, Instant, SystemTime};

pub trait Clock: Send + Sync {
    fn now(&self) -> Instant;

    /// Wall-clock time. Distinct from [`Clock::now`] which returns a
    /// monotonic [`Instant`]. Tests driven by
    /// [`TestScheduler`](crate::TestScheduler) advance both clocks in
    /// lock-step through `advance_clock`.
    fn system_now(&self) -> SystemTime;
}

pub struct LocalClock;

impl Clock for LocalClock {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn system_now(&self) -> SystemTime {
        SystemTime::now()
    }
}

struct TestClockState {
    instant: Instant,
    system: SystemTime,
}

pub struct TestClock {
    state: Mutex<TestClockState>,
}

impl TestClock {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(TestClockState {
                instant: Instant::now(),
                system: SystemTime::now(),
            }),
        }
    }

    pub fn advance(&self, duration: Duration) {
        let mut state = self.state.lock();
        state.instant += duration;
        state.system += duration;
    }
}

impl Default for TestClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for TestClock {
    fn now(&self) -> Instant {
        self.state.lock().instant
    }

    fn system_now(&self) -> SystemTime {
        self.state.lock().system
    }
}
