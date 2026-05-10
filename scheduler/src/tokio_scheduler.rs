//! Production [`Scheduler`] impl backing
//! [`Executor`](crate::Executor) in the binary so Tokio-bound hosts
//! (LSP, Claude Code, fs watcher) share the same runtime as every
//! other async path routed through `stoat_scheduler::Executor`.
//! Tests substitute [`TestScheduler`](crate::TestScheduler).

use crate::{Clock, Executor, LocalClock, Runnable, Scheduler, Timer};
use futures::channel::oneshot;
use std::{sync::Arc, time::Duration};
use tokio::runtime::Handle;

pub struct TokioScheduler {
    handle: Handle,
    clock: LocalClock,
}

impl TokioScheduler {
    pub fn new(handle: Handle) -> Self {
        Self {
            handle,
            clock: LocalClock,
        }
    }

    pub fn executor(self: &Arc<Self>) -> Executor {
        Executor::new(self.clone())
    }
}

impl Scheduler for TokioScheduler {
    fn schedule(&self, runnable: Runnable) {
        self.handle.spawn(async move {
            runnable.run();
        });
    }

    fn timer(&self, duration: Duration) -> Timer {
        let (sender, receiver) = oneshot::channel();
        self.handle.spawn(async move {
            tokio::time::sleep(duration).await;
            let _ = sender.send(());
        });
        Timer(receiver)
    }

    fn clock(&self) -> &dyn Clock {
        &self.clock
    }

    fn schedule_blocking(&self, work: Box<dyn FnOnce() + Send + 'static>) {
        self.handle.spawn_blocking(work);
    }
}

#[cfg(test)]
mod tests {
    use super::TokioScheduler;
    use crate::Scheduler;
    use std::{
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        },
        time::{Duration, Instant},
    };
    use tokio::runtime::Handle;

    #[tokio::test]
    async fn schedule_executes_runnable() {
        let scheduler = Arc::new(TokioScheduler::new(Handle::current()));
        let executor = scheduler.executor();
        let ran = Arc::new(AtomicBool::new(false));

        let ran_inner = ran.clone();
        executor
            .spawn(async move {
                ran_inner.store(true, Ordering::SeqCst);
            })
            .await;

        assert!(ran.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn timer_fires_after_duration() {
        let scheduler = TokioScheduler::new(Handle::current());
        let started = Instant::now();
        scheduler.timer(Duration::from_millis(50)).await;
        assert!(started.elapsed() >= Duration::from_millis(50));
    }

    #[tokio::test]
    async fn clock_now_brackets_real_time() {
        let scheduler = TokioScheduler::new(Handle::current());
        let before = Instant::now();
        let from_clock = scheduler.clock().now();
        let after = Instant::now();
        assert!(before <= from_clock && from_clock <= after);
    }

    #[tokio::test]
    async fn spawn_blocking_does_not_block_runtime() {
        let scheduler = Arc::new(TokioScheduler::new(Handle::current()));
        let executor = scheduler.executor();
        let started = Arc::new(AtomicBool::new(false));
        let release = Arc::new(AtomicBool::new(false));

        let blocking_task = {
            let started = started.clone();
            let release = release.clone();
            executor.spawn_blocking(move || {
                started.store(true, Ordering::SeqCst);
                while !release.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(1));
                }
                123
            })
        };

        while !started.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
        release.store(true, Ordering::SeqCst);
        assert_eq!(blocking_task.await, 123);
    }
}
