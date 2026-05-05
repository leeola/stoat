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
}
