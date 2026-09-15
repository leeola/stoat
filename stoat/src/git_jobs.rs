//! The one queue every git write the run loop starts goes through.
//!
//! A git write on the blocking pool does not hold the frame, but two writes on
//! the pool race. If an amend runs while a walk checkout moves HEAD, the amend
//! goes to the wrong commit. So every write the loop starts waits its turn
//! here, one job at a time in press order.
//!
//! A job has three parts. Its start runs on the loop after every earlier job
//! lands, so it reads the state those jobs left. Its work runs on the blocking
//! pool. Its landing runs on the loop in [`pump`].

use crate::{app::Stoat, workspace::WorkspaceId};
use std::{
    collections::VecDeque,
    mem,
    sync::mpsc::{self, TryRecvError},
};
use stoat_scheduler::Task;

/// The change to the editor that a job's work hands back when the write is
/// done.
pub(crate) type GitLanding = Box<dyn FnOnce(&mut Stoat) + Send>;

/// A job's git work, which runs on the blocking pool.
pub(crate) type GitWork = Box<dyn FnOnce() -> GitLanding + Send>;

/// A job's start, which runs on the loop and returns `None` to refuse.
type GitStart = Box<dyn FnOnce(&mut Stoat) -> Option<GitWork>>;

/// One git write the loop started.
pub(crate) struct GitJob {
    key: Option<GitJobKey>,
    start: GitStart,
}

impl GitJob {
    /// A job whose `start` runs on the loop after every earlier job lands.
    ///
    /// `start` returns `None` to refuse, and the queue moves on to the next
    /// job. A job with a `key` takes the place of a queued job with the same
    /// key, since the later press makes the earlier one stale.
    pub(crate) fn new(
        key: Option<GitJobKey>,
        start: impl FnOnce(&mut Stoat) -> Option<GitWork> + 'static,
    ) -> Self {
        Self {
            key,
            start: Box::new(start),
        }
    }
}

/// The kinds of job a later press makes stale while it waits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GitJobKey {
    /// A review-walk step or a rebase edit-pause checkout. The later one names
    /// where the reader stands now.
    WalkLanding,
    /// A rebase step. The later one reads the state every earlier step left.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "reserved for the rebase stepper's queued steps")
    )]
    RebaseStep,
}

/// The git writes the loop started, held in press order until each one lands.
#[derive(Default)]
pub(crate) struct GitJobs {
    queue: VecDeque<GitJob>,
    running: Option<RunningGitJob>,
}

impl GitJobs {
    /// Whether a job with `key` runs or waits in the queue.
    pub(crate) fn holds(&self, key: GitJobKey) -> bool {
        self.running
            .as_ref()
            .is_some_and(|job| job.key == Some(key))
            || self.queue.iter().any(|job| job.key == Some(key))
    }

    /// Drop every queued job with `key`. A running one still lands.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "reserved for the rebase stepper's queued steps")
    )]
    pub(crate) fn drop_queued(&mut self, key: GitJobKey) {
        self.queue.retain(|job| job.key != Some(key));
    }
}

/// The job whose work is out on the pool.
struct RunningGitJob {
    key: Option<GitJobKey>,
    rx: mpsc::Receiver<GitLanding>,
    _task: Task<()>,
}

/// Queue `job` behind every git write the loop started before it.
///
/// A keyed job overwrites the first queued job with the same key in place, and
/// any other job goes to the back.
///
/// The job starts and lands in the workspace active at this call, even after
/// the reader goes to another workspace. If that workspace closes before the
/// job starts, the job does nothing. If it closes while the work runs, the git
/// write completes and the landing does nothing.
pub(crate) fn enqueue(stoat: &mut Stoat, job: GitJob) {
    let workspace = stoat.active_workspace;
    let GitJob { key, start } = job;
    let job = GitJob::new(key, move |stoat: &mut Stoat| {
        let work = in_workspace(stoat, workspace, start).flatten()?;
        Some(Box::new(move || {
            let landing = work();
            Box::new(move |stoat: &mut Stoat| {
                in_workspace(stoat, workspace, landing);
            }) as GitLanding
        }) as GitWork)
    });

    let queue = &mut stoat.git_jobs.queue;
    match key.and_then(|key| queue.iter_mut().find(|queued| queued.key == Some(key))) {
        Some(queued) => *queued = job,
        None => queue.push_back(job),
    }
    if stoat.git_jobs.running.is_none() {
        start_next(stoat);
    }
}

/// Land the running job when its work is done, and start the next one.
///
/// Returns whether anything moved, which is what [`Stoat::drive_pumps`] reads
/// to decide whether another pass is worth making. Work that panicked lands
/// nothing, and the queue moves on.
pub(crate) fn pump(stoat: &mut Stoat) -> bool {
    let Some(running) = &stoat.git_jobs.running else {
        return false;
    };
    let landing = match running.rx.try_recv() {
        Ok(landing) => Some(landing),
        Err(TryRecvError::Empty) => return false,
        Err(TryRecvError::Disconnected) => None,
    };

    // The job's work is done, so the slot clears before its landing runs. A job
    // the landing queues then starts at once, before the landing returns.
    stoat.git_jobs.running = None;
    if let Some(landing) = landing {
        landing(stoat);
    }
    start_next(stoat);
    true
}

/// Start queued jobs until one hands work to the pool or the queue is empty.
fn start_next(stoat: &mut Stoat) {
    while stoat.git_jobs.running.is_none() {
        let Some(job) = stoat.git_jobs.queue.pop_front() else {
            return;
        };
        let Some(work) = (job.start)(stoat) else {
            continue;
        };

        let (tx, rx) = mpsc::channel();
        let redraw = stoat.redraw_notify.clone();
        let task = stoat.executor.spawn_blocking(move || {
            let _ = tx.send(work());
            redraw.notify_one();
        });
        // A job queued from inside a start starts at once, since the slot is
        // still empty. This job then overwrites it, and its landing never runs.
        debug_assert!(
            stoat.git_jobs.running.is_none(),
            "a git job's start queued another job"
        );
        stoat.git_jobs.running = Some(RunningGitJob {
            key: job.key,
            rx,
            _task: task,
        });
    }
}

/// Run `f` with `workspace` active, then put the active workspace back.
///
/// Returns `None` if `workspace` closed, since nothing is left for the job to
/// act on. The workspace that was active before the call stays active after
/// it, unless it closed during `f`.
fn in_workspace<R>(
    stoat: &mut Stoat,
    workspace: WorkspaceId,
    f: impl FnOnce(&mut Stoat) -> R,
) -> Option<R> {
    if !stoat.workspaces.contains_key(workspace) {
        return None;
    }
    let front = mem::replace(&mut stoat.active_workspace, workspace);
    let result = f(stoat);
    if stoat.workspaces.contains_key(front) {
        stoat.active_workspace = front;
    }
    Some(result)
}

/// A job whose work and landing do nothing, for a test that holds later jobs
/// behind a running one.
#[cfg(test)]
pub(crate) fn idle_job(key: Option<GitJobKey>) -> GitJob {
    GitJob::new(key, |_| {
        Some(Box::new(|| Box::new(|_: &mut Stoat| {}) as GitLanding) as GitWork)
    })
}

#[cfg(test)]
mod tests {
    use super::{enqueue, idle_job, GitJob, GitJobKey, GitLanding, GitWork};
    use crate::{app::Stoat, test_harness::TestHarness, workspace::WorkspaceId};
    use std::sync::{Arc, Mutex};

    type Log = Arc<Mutex<Vec<String>>>;

    /// A job whose work and landing each append their name to `log`.
    fn recorded(log: &Log, name: &'static str, key: Option<GitJobKey>) -> GitJob {
        let log = Arc::clone(log);
        GitJob::new(key, move |_| {
            Some(Box::new(move || {
                log.lock().unwrap().push(format!("{name} work"));
                Box::new(move |_: &mut Stoat| {
                    log.lock().unwrap().push(format!("{name} landing"));
                }) as GitLanding
            }) as GitWork)
        })
    }

    #[test]
    fn jobs_run_one_at_a_time_in_press_order() {
        let mut h = TestHarness::with_size(80, 24);
        let log = Log::default();

        enqueue(&mut h.stoat, recorded(&log, "first", None));
        let refused = Arc::clone(&log);
        enqueue(
            &mut h.stoat,
            GitJob::new(None, move |_| {
                refused.lock().unwrap().push("refused start".to_string());
                None
            }),
        );
        enqueue(&mut h.stoat, recorded(&log, "second", None));
        h.settle();

        assert_eq!(
            *log.lock().unwrap(),
            [
                "first work",
                "first landing",
                "refused start",
                "second work",
                "second landing"
            ]
        );
    }

    #[test]
    fn a_queued_keyed_job_gives_its_place_to_a_later_one() {
        let mut h = TestHarness::with_size(80, 24);
        let log = Log::default();
        let walk = Some(GitJobKey::WalkLanding);

        enqueue(&mut h.stoat, recorded(&log, "running", walk));
        enqueue(&mut h.stoat, recorded(&log, "stale", walk));
        enqueue(&mut h.stoat, recorded(&log, "unkeyed", None));
        enqueue(&mut h.stoat, recorded(&log, "fresh", walk));
        h.settle();

        assert_eq!(
            *log.lock().unwrap(),
            [
                "running work",
                "running landing",
                "fresh work",
                "fresh landing",
                "unkeyed work",
                "unkeyed landing"
            ],
            "the fresh job waits where the stale one did, and the running one lands"
        );
    }

    #[test]
    fn holds_answers_for_running_and_queued_jobs() {
        let mut h = TestHarness::with_size(80, 24);
        let held = |h: &TestHarness| {
            (
                h.stoat.git_jobs.holds(GitJobKey::RebaseStep),
                h.stoat.git_jobs.holds(GitJobKey::WalkLanding),
            )
        };
        let mut seen = Vec::new();

        enqueue(&mut h.stoat, idle_job(Some(GitJobKey::RebaseStep)));
        seen.push(held(&h));
        enqueue(&mut h.stoat, idle_job(Some(GitJobKey::WalkLanding)));
        seen.push(held(&h));
        h.stoat.git_jobs.drop_queued(GitJobKey::WalkLanding);
        seen.push(held(&h));
        h.settle();
        seen.push(held(&h));

        assert_eq!(
            seen,
            [(true, false), (true, true), (true, false), (false, false)]
        );
    }

    #[test]
    fn a_job_starts_and_lands_in_the_workspace_that_queued_it() {
        let mut h = TestHarness::with_size(80, 24);
        let queued_in = h.stoat.active_workspace;
        let other = h.create_workspace();
        let records: Arc<Mutex<Vec<(&'static str, WorkspaceId)>>> = Arc::default();

        enqueue(&mut h.stoat, idle_job(None));
        let job = {
            let records = Arc::clone(&records);
            GitJob::new(None, move |stoat| {
                records
                    .lock()
                    .unwrap()
                    .push(("start", stoat.active_workspace));
                Some(Box::new(move || {
                    Box::new(move |stoat: &mut Stoat| {
                        records
                            .lock()
                            .unwrap()
                            .push(("landing", stoat.active_workspace));
                    }) as GitLanding
                }) as GitWork)
            })
        };
        enqueue(&mut h.stoat, job);
        h.set_active_workspace(other);
        h.settle();

        assert_eq!(
            (records.lock().unwrap().clone(), h.stoat.active_workspace),
            (vec![("start", queued_in), ("landing", queued_in)], other),
            "the job acts on its own workspace and leaves the reader where they went"
        );
    }
}
