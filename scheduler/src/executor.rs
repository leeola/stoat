use crate::{Scheduler, Timer};
use std::{
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};

#[derive(Clone)]
pub struct Executor {
    scheduler: Arc<dyn Scheduler>,
}

impl Executor {
    pub fn new(scheduler: Arc<dyn Scheduler>) -> Self {
        Self { scheduler }
    }

    pub fn spawn<F>(&self, future: F) -> Task<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let scheduler = Arc::clone(&self.scheduler);
        let (runnable, task) = async_task::spawn(future, move |runnable| {
            scheduler.schedule(runnable);
        });
        runnable.schedule();
        Task::Spawned(task)
    }

    pub fn timer(&self, duration: Duration) -> Timer {
        self.scheduler.timer(duration)
    }
}

#[must_use]
pub enum Task<T> {
    Ready(Option<T>),
    Spawned(async_task::Task<T>),
}

impl<T> Task<T> {
    pub fn ready(value: T) -> Self {
        Task::Ready(Some(value))
    }

    pub fn detach(self) {
        match self {
            Task::Ready(_) => {},
            Task::Spawned(task) => task.detach(),
        }
    }
}

impl<T> Future for Task<T> {
    type Output = T;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<T> {
        // SAFETY: we never move the inner async_task::Task out of the pin
        match unsafe { self.get_unchecked_mut() } {
            Task::Ready(val) => {
                Poll::Ready(val.take().expect("Task::Ready polled after completion"))
            },
            Task::Spawned(task) => Pin::new(task).poll(cx),
        }
    }
}
