use crate::{Clock, Executor, LocalClock, Runnable, Scheduler, Timer};
use std::{sync::Arc, time::Duration};

pub struct LocalScheduler {
    clock: LocalClock,
}

impl LocalScheduler {
    pub fn new() -> Self {
        Self { clock: LocalClock }
    }

    pub fn executor(self: &Arc<Self>) -> Executor {
        Executor::new(self.clone())
    }
}

impl Default for LocalScheduler {
    fn default() -> Self {
        Self::new()
    }
}

impl Scheduler for LocalScheduler {
    fn schedule(&self, _runnable: Runnable) {
        panic!(
            "LocalScheduler::schedule called -- LocalScheduler is for clock-only contexts; use TokioScheduler to spawn tasks"
        );
    }

    fn timer(&self, _duration: Duration) -> Timer {
        panic!(
            "LocalScheduler::timer called -- LocalScheduler is for clock-only contexts; use TokioScheduler for timers"
        );
    }

    fn clock(&self) -> &dyn Clock {
        &self.clock
    }

    fn schedule_blocking(&self, _work: Box<dyn FnOnce() + Send + 'static>) {
        panic!(
            "LocalScheduler::schedule_blocking called -- LocalScheduler is for clock-only contexts; use TokioScheduler for blocking work"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::LocalScheduler;
    use crate::Scheduler;
    use std::{
        sync::Arc,
        time::{Duration, Instant},
    };

    #[test]
    fn clock_returns_real_time() {
        let scheduler = LocalScheduler::new();
        let before = Instant::now();
        let from_clock = scheduler.clock().now();
        let after = Instant::now();
        assert!(before <= from_clock && from_clock <= after);
    }

    #[test]
    fn executor_now_brackets_real_time() {
        let scheduler = Arc::new(LocalScheduler::new());
        let executor = scheduler.executor();
        let before = Instant::now();
        let from_executor = executor.now();
        let after = Instant::now();
        assert!(before <= from_executor && from_executor <= after);
    }

    #[test]
    #[should_panic(expected = "LocalScheduler::schedule called")]
    fn schedule_panics() {
        let scheduler = Arc::new(LocalScheduler::new());
        let executor = scheduler.executor();
        executor.spawn(async {}).detach();
    }

    #[test]
    #[should_panic(expected = "LocalScheduler::timer called")]
    fn timer_panics() {
        let scheduler = Arc::new(LocalScheduler::new());
        let executor = scheduler.executor();
        let _timer = executor.timer(Duration::from_secs(1));
    }

    #[test]
    #[should_panic(expected = "LocalScheduler::schedule_blocking called")]
    fn schedule_blocking_panics() {
        let scheduler = Arc::new(LocalScheduler::new());
        let executor = scheduler.executor();
        executor.spawn_blocking(|| ()).detach();
    }
}
