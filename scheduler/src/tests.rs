use crate::TestScheduler;
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

fn scheduler() -> Arc<TestScheduler> {
    Arc::new(TestScheduler::new())
}

#[test]
fn spawn_and_await() {
    let s = scheduler();
    let exec = s.executor();
    let result = s.block_on(async { exec.spawn(async { 42 }).await });
    assert_eq!(result, 42);
}

#[test]
fn tick_single_step() {
    let s = scheduler();
    let exec = s.executor();
    let order = Arc::new(std::sync::Mutex::new(Vec::new()));

    for i in 0..3 {
        let order = order.clone();
        exec.spawn(async move {
            order.lock().expect("poisoned").push(i);
        })
        .detach();
    }

    assert!(s.tick());
    assert!(s.tick());
    assert!(s.tick());
    assert!(!s.tick());

    assert_eq!(*order.lock().expect("poisoned"), vec![0, 1, 2]);
}

#[test]
fn run_until_parked_drains_all() {
    let s = scheduler();
    let exec = s.executor();
    let count = Arc::new(AtomicUsize::new(0));

    for _ in 0..5 {
        let count = count.clone();
        exec.spawn(async move {
            count.fetch_add(1, Ordering::SeqCst);
        })
        .detach();
    }

    s.run_until_parked();
    assert_eq!(count.load(Ordering::SeqCst), 5);
    assert!(!s.has_pending_work());
}

#[test]
fn cascading_spawns() {
    let s = scheduler();
    let exec = s.executor();
    let flag = Arc::new(AtomicUsize::new(0));

    {
        let inner_exec = exec.clone();
        let flag = flag.clone();
        exec.spawn(async move {
            flag.fetch_add(1, Ordering::SeqCst);
            let flag2 = flag.clone();
            inner_exec
                .spawn(async move {
                    flag2.fetch_add(10, Ordering::SeqCst);
                })
                .detach();
        })
        .detach();
    }

    s.settle();
    assert_eq!(flag.load(Ordering::SeqCst), 11);
}

#[test]
fn timer_fires_after_advance() {
    let s = scheduler();
    let exec = s.executor();
    let fired = Arc::new(AtomicUsize::new(0));

    {
        let fired = fired.clone();
        let timer = exec.timer(Duration::from_millis(100));
        exec.spawn(async move {
            timer.await;
            fired.store(1, Ordering::SeqCst);
        })
        .detach();
    }

    s.run_until_parked();
    assert_eq!(fired.load(Ordering::SeqCst), 0);

    s.advance_clock(Duration::from_millis(100));
    assert_eq!(fired.load(Ordering::SeqCst), 1);
}

#[test]
fn timer_ordering() {
    let s = scheduler();
    let exec = s.executor();
    let order = Arc::new(std::sync::Mutex::new(Vec::new()));

    for (i, ms) in [50u64, 150, 100].iter().enumerate() {
        let order = order.clone();
        let timer = exec.timer(Duration::from_millis(*ms));
        exec.spawn(async move {
            timer.await;
            order.lock().expect("poisoned").push(i);
        })
        .detach();
    }

    s.settle();
    assert_eq!(*order.lock().expect("poisoned"), vec![0, 2, 1]);
}

#[test]
fn settle_timer_chain() {
    let s = scheduler();
    let exec = s.executor();
    let flag = Arc::new(AtomicUsize::new(0));

    {
        let inner_exec = exec.clone();
        let flag = flag.clone();
        let timer1 = exec.timer(Duration::from_millis(50));
        exec.spawn(async move {
            timer1.await;
            let flag2 = flag.clone();
            let timer2 = inner_exec.timer(Duration::from_millis(50));
            inner_exec
                .spawn(async move {
                    timer2.await;
                    flag2.store(1, Ordering::SeqCst);
                })
                .detach();
        })
        .detach();
    }

    s.settle();
    assert_eq!(flag.load(Ordering::SeqCst), 1);
}

#[test]
fn block_on_awaits_spawned_task() {
    let s = scheduler();
    let exec = s.executor();
    let task = exec.spawn(async { 99 });
    let result = s.block_on(task);
    assert_eq!(result, 99);
}

#[test]
#[should_panic(expected = "deadlock")]
fn block_on_deadlock() {
    let s = scheduler();
    s.block_on(futures::future::pending::<()>());
}

#[test]
fn channel_communication() {
    let s = scheduler();
    let exec = s.executor();
    let (tx, rx) = futures::channel::oneshot::channel::<Vec<i32>>();

    exec.spawn(async move {
        tx.send(vec![0, 1, 2]).ok();
    })
    .detach();

    let result = s.block_on(async { rx.await.expect("channel closed") });

    assert_eq!(result, vec![0, 1, 2]);
}

#[test]
fn task_detach() {
    let s = scheduler();
    let exec = s.executor();
    let flag = Arc::new(AtomicUsize::new(0));

    let flag2 = flag.clone();
    exec.spawn(async move {
        flag2.store(1, Ordering::SeqCst);
    })
    .detach();

    s.settle();
    assert_eq!(flag.load(Ordering::SeqCst), 1);
}

#[test]
fn task_ready() {
    let s = scheduler();
    let result = s.block_on(async { crate::Task::ready(42).await });
    assert_eq!(result, 42);
}

#[test]
fn advance_clock_partial() {
    let s = scheduler();
    let exec = s.executor();
    let fired = Arc::new(AtomicUsize::new(0));

    {
        let fired = fired.clone();
        let timer = exec.timer(Duration::from_millis(100));
        exec.spawn(async move {
            timer.await;
            fired.store(1, Ordering::SeqCst);
        })
        .detach();
    }

    s.run_until_parked();
    s.advance_clock(Duration::from_millis(50));
    assert_eq!(fired.load(Ordering::SeqCst), 0);

    s.advance_clock(Duration::from_millis(50));
    assert_eq!(fired.load(Ordering::SeqCst), 1);
}

#[test]
fn has_pending_work_lifecycle() {
    let s = scheduler();
    assert!(!s.has_pending_work());

    let exec = s.executor();
    exec.spawn(async {}).detach();
    assert!(s.has_pending_work());

    s.settle();
    assert!(!s.has_pending_work());
}
