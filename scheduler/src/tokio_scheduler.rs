use crate::{Clock, Executor, LocalClock, Runnable, Scheduler, Timer};
use std::{panic::AssertUnwindSafe, sync::Arc, time::Duration};
use tokio::{runtime::Handle, sync::mpsc};

pub struct TokioScheduler {
    handle: Handle,
    clock: LocalClock,
    wakes: mpsc::UnboundedSender<Runnable>,
}

impl TokioScheduler {
    /// Build a scheduler driving `handle`, starting the task that polls
    /// everything it schedules.
    ///
    /// The pump runs until the scheduler is dropped or `handle`'s runtime shuts
    /// down. It expects a current-thread runtime, where polls serialize anyway,
    /// so draining wakes one at a time costs no parallelism. On a multi-thread
    /// runtime it would serialize polls that could otherwise overlap.
    pub fn new(handle: Handle) -> Self {
        let (wakes, mut pending) = mpsc::unbounded_channel::<Runnable>();

        handle.spawn(async move {
            while let Some(runnable) = pending.recv().await {
                // Polling a task must not be able to stop the pump, or one
                // panicking task would strand every future wake. Catching here
                // keeps a panic to the task that raised it, as giving each wake
                // its own tokio task used to. The panic hook has already run and
                // printed by this point.
                let _ = std::panic::catch_unwind(AssertUnwindSafe(|| runnable.run()));
            }
        });

        Self {
            handle,
            clock: LocalClock,
            wakes,
        }
    }

    pub fn executor(self: &Arc<Self>) -> Executor {
        Executor::new(self.clone())
    }
}

impl Scheduler for TokioScheduler {
    fn schedule(&self, runnable: Runnable) {
        // A closed channel means the runtime went away, so dropping the
        // runnable cancels its task, which is what dropping an unrun spawn did.
        let _ = self.wakes.send(runnable);
    }

    fn timer(&self, duration: Duration) -> Timer {
        Timer::waiting(async move { tokio::time::sleep(duration).await })
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
            Arc, Mutex,
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
    async fn a_burst_of_wakes_adds_no_tokio_tasks() {
        let handle = Handle::current();
        let scheduler = Arc::new(TokioScheduler::new(handle.clone()));
        let executor = scheduler.executor();

        // Let the pump reach its first await, so it counts as alive in the
        // reading taken next.
        tokio::task::yield_now().await;
        let alive_before = handle.metrics().num_alive_tasks();

        // Nothing is awaited here, so the runtime cannot drain the wakes these
        // queue. Every one of them used to be a tokio task of its own.
        let spawned: Vec<_> = (0..64).map(|i| executor.spawn(async move { i })).collect();

        assert_eq!(
            handle.metrics().num_alive_tasks(),
            alive_before,
            "64 pending wakes cost no tokio tasks",
        );

        let mut ran = Vec::new();
        for task in spawned {
            ran.push(task.await);
        }
        assert_eq!(ran, (0..64).collect::<Vec<_>>(), "all of them still run");
    }

    #[tokio::test]
    async fn wakes_run_in_the_order_they_were_scheduled() {
        let scheduler = Arc::new(TokioScheduler::new(Handle::current()));
        let executor = scheduler.executor();
        let order = Arc::new(Mutex::new(Vec::new()));

        let spawned: Vec<_> = (0..16)
            .map(|i| {
                let order = order.clone();
                executor.spawn(async move {
                    order.lock().expect("order poisoned").push(i);
                })
            })
            .collect();
        for task in spawned {
            task.await;
        }

        let ran = order.lock().expect("order poisoned").clone();
        assert_eq!(
            ran,
            (0..16).collect::<Vec<_>>(),
            "one pump drains wakes in send order",
        );
    }

    #[tokio::test]
    async fn a_panicking_task_leaves_the_scheduler_running() {
        let scheduler = Arc::new(TokioScheduler::new(Handle::current()));
        let executor = scheduler.executor();

        let hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        executor.spawn(async { panic!("task panics") }).detach();
        tokio::task::yield_now().await;
        std::panic::set_hook(hook);

        let ran = Arc::new(AtomicBool::new(false));
        executor
            .spawn({
                let ran = ran.clone();
                async move {
                    ran.store(true, Ordering::SeqCst);
                }
            })
            .await;

        assert!(
            ran.load(Ordering::SeqCst),
            "a task scheduled after a panicking one still runs",
        );
    }

    #[tokio::test]
    async fn a_dropped_timer_leaves_nothing_sleeping() {
        let handle = Handle::current();
        let scheduler = TokioScheduler::new(handle.clone());

        // Let the pump reach its first await, so it counts as alive below.
        tokio::task::yield_now().await;
        let alive_before = handle.metrics().num_alive_tasks();

        // A debounce does this on every keystroke. It takes a timer, then
        // drops it unfired when the next keystroke re-arms.
        for _ in 0..32 {
            drop(scheduler.timer(Duration::from_secs(3600)));
        }

        assert_eq!(
            handle.metrics().num_alive_tasks(),
            alive_before,
            "dropping a timer cancels its sleep instead of orphaning it",
        );
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
