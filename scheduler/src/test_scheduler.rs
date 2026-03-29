use crate::{Clock, Executor, Runnable, Scheduler, TestClock, Timer};
use futures::channel::oneshot;
use parking_lot::Mutex;
use std::{
    collections::VecDeque,
    sync::Arc,
    task::{Context, Poll},
    time::{Duration, Instant},
};

struct ScheduledTimer {
    expiration: Instant,
    _sender: oneshot::Sender<()>,
}

struct SchedulerState {
    runnables: VecDeque<Runnable>,
    timers: Vec<ScheduledTimer>,
}

pub struct TestScheduler {
    clock: TestClock,
    state: Mutex<SchedulerState>,
}

impl TestScheduler {
    pub fn new() -> Self {
        Self {
            clock: TestClock::new(),
            state: Mutex::new(SchedulerState {
                runnables: VecDeque::new(),
                timers: Vec::new(),
            }),
        }
    }

    pub fn test_clock(&self) -> &TestClock {
        &self.clock
    }

    /// Execute one unit of work: expire timers or run one task.
    /// Returns true if any work was done.
    pub fn tick(&self) -> bool {
        // Phase 1: expire timers whose time has come
        {
            let mut state = self.state.lock();
            let now = self.clock.now();
            let partition = state.timers.partition_point(|t| t.expiration <= now);
            if partition > 0 {
                let expired: Vec<_> = state.timers.drain(..partition).collect();
                drop(state);
                drop(expired);
                return true;
            }
        }

        // Phase 2: run one task
        let runnable = self.state.lock().runnables.pop_front();
        if let Some(runnable) = runnable {
            runnable.run();
            return true;
        }

        false
    }

    /// Drain all ready work until no more progress can be made.
    pub fn run_until_parked(&self) {
        while self.tick() {}
    }

    /// Alias for [`run_until_parked`](Self::run_until_parked).
    pub fn run(&self) {
        self.run_until_parked();
    }

    /// Run all tasks and advance through all pending timers until
    /// absolutely nothing remains. Convenience for tests that don't
    /// need to observe intermediate states.
    pub fn settle(&self) {
        loop {
            self.run_until_parked();
            if !self.advance_clock_to_next_timer() {
                break;
            }
        }
    }

    /// Advance the clock by `duration`, stepping through any timers
    /// that fall within the window and running tasks between each.
    pub fn advance_clock(&self, duration: Duration) {
        let target = self.clock.now() + duration;
        loop {
            self.run_until_parked();
            let next_timer = self.state.lock().timers.first().map(|t| t.expiration);
            if let Some(exp) = next_timer {
                if exp <= target {
                    self.clock.advance(exp - self.clock.now());
                    continue;
                }
            }
            break;
        }
        let remaining = target - self.clock.now();
        if remaining > Duration::ZERO {
            self.clock.advance(remaining);
        }
        self.run_until_parked();
    }

    pub fn has_pending_work(&self) -> bool {
        let state = self.state.lock();
        !state.runnables.is_empty() || !state.timers.is_empty()
    }

    /// Create an [`Executor`] backed by this scheduler.
    pub fn executor(self: &Arc<Self>) -> Executor {
        Executor::new(self.clone())
    }

    fn advance_clock_to_next_timer(&self) -> bool {
        let next = self.state.lock().timers.first().map(|t| t.expiration);
        if let Some(expiration) = next {
            let now = self.clock.now();
            if expiration > now {
                self.clock.advance(expiration - now);
            }
            true
        } else {
            false
        }
    }

    /// Poll `future` to completion, stepping the scheduler between polls.
    /// Panics if the future cannot make progress (deadlock).
    pub fn block_on<F: std::future::Future>(&self, future: F) -> F::Output {
        let mut future = std::pin::pin!(future);
        let waker = futures::task::noop_waker();
        let mut cx = Context::from_waker(&waker);

        loop {
            if let Poll::Ready(output) = future.as_mut().poll(&mut cx) {
                return output;
            }

            if self.tick() {
                continue;
            }

            if self.advance_clock_to_next_timer() {
                continue;
            }

            panic!(
                "block_on: future is not ready and no tasks or timers remain. \
                 This is a deadlock."
            );
        }
    }
}

impl Default for TestScheduler {
    fn default() -> Self {
        Self::new()
    }
}

impl Scheduler for TestScheduler {
    fn schedule(&self, runnable: Runnable) {
        self.state.lock().runnables.push_back(runnable);
    }

    fn timer(&self, duration: Duration) -> Timer {
        let (sender, receiver) = oneshot::channel();
        let mut state = self.state.lock();
        let expiration = self.clock.now() + duration;
        state.timers.push(ScheduledTimer {
            expiration,
            _sender: sender,
        });
        state.timers.sort_by_key(|t| t.expiration);
        Timer(receiver)
    }

    fn clock(&self) -> &dyn Clock {
        &self.clock
    }
}
