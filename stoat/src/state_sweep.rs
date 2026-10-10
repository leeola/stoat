//! The sweep a start of stoat runs over its state directory.
//!
//! A server removes its hook socket when it drops, and a fresh workspace writes
//! no session file. A killed process does neither, so its socket stays, and
//! every run in a temporary repository leaves a session directory behind. Left
//! alone, the state directory grows by thousands of entries. The sweep removes
//! what has gone unused for longer than the retention period.
//!
//! The clock, the retention, and socket liveness come from the caller, so the
//! sweep runs on the in-memory filesystem in tests.

use crate::{
    host::{FsDirEntry, FsHost},
    workspace::registry,
};
use std::{
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

/// Days of retention for a config that leaves `session.retention_days` unset.
/// The shipped config states the same number.
pub(crate) const DEFAULT_RETENTION_DAYS: u32 = 14;

/// What one [`sweep`] removed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct SweepReport {
    pub(crate) removed_files: usize,
    pub(crate) removed_dirs: usize,
}

/// The filesystem, the clock, and the running count that one sweep shares.
struct Sweeper<'a> {
    fs: &'a dyn FsHost,
    now: SystemTime,
    retention: Duration,
    report: SweepReport,
}

/// Remove the stale hook sockets in `socket_dirs` and the stale session files
/// under `workspaces_dir`.
///
/// A path is stale when it was last modified more than `retention` before
/// `now`. A modification time later than `now` reads as fresh.
///
/// A stale socket stays while `socket_is_live` reports a server on it. The
/// session file at `keep` stays whatever its age, for a restore that reads it
/// off the main thread. A `.ron` session file goes with its `.meta` sidecar.
/// Any other file goes on its own age once no `.ron` beside it names it, and a
/// session directory left empty goes too.
///
/// A failed list, metadata read, or remove is logged and skipped, so a start
/// never aborts on the sweep.
pub(crate) fn sweep(
    fs: &dyn FsHost,
    socket_dirs: &[PathBuf],
    workspaces_dir: &Path,
    now: SystemTime,
    retention: Duration,
    keep: Option<&Path>,
    socket_is_live: &dyn Fn(&Path) -> bool,
) -> SweepReport {
    let mut sweeper = Sweeper {
        fs,
        now,
        retention,
        report: SweepReport::default(),
    };

    let mut swept: Vec<&Path> = Vec::new();
    for dir in socket_dirs {
        if swept.contains(&dir.as_path()) || !fs.exists(dir) {
            continue;
        }
        swept.push(dir);
        sweeper.sweep_sockets(dir, socket_is_live);
    }

    if fs.exists(workspaces_dir) {
        for entry in sweeper.list(workspaces_dir).unwrap_or_default() {
            if entry.is_dir {
                sweeper.sweep_session_dir(&workspaces_dir.join(entry.name.as_str()), keep);
            }
        }
    }
    sweeper.report
}

impl Sweeper<'_> {
    /// Remove each stale `.sock` file in `dir` that no server answers on.
    ///
    /// The age check runs first, so a fresh socket is never probed.
    fn sweep_sockets(&mut self, dir: &Path, socket_is_live: &dyn Fn(&Path) -> bool) {
        for entry in self.list(dir).unwrap_or_default() {
            if entry.is_dir || !entry.name.ends_with(".sock") {
                continue;
            }
            let path = dir.join(entry.name.as_str());
            if self.is_stale(&path) && !socket_is_live(&path) {
                self.remove_file(&path);
            }
        }
    }

    /// Remove the stale session files in one anchor's directory, then the
    /// directory once it is empty.
    fn sweep_session_dir(&mut self, dir: &Path, keep: Option<&Path>) {
        let Some(entries) = self.list(dir) else {
            return;
        };
        for entry in entries {
            let path = dir.join(entry.name.as_str());
            if entry.is_dir || !is_session_file(&path) || keep == Some(path.as_path()) {
                continue;
            }
            if self.is_stale(&path) {
                self.remove_file(&path);
                let meta = registry::meta_path_for(&path);
                if self.fs.exists(&meta) {
                    self.remove_file(&meta);
                }
            }
        }

        // A file no session file names goes on its own age. That takes a
        // sidecar whose session file is gone, and the temporary half of a
        // sidecar write that never finished, whose `.ron` twin never exists.
        let Some(left) = self.list(dir) else {
            return;
        };
        for entry in left {
            let path = dir.join(entry.name.as_str());
            if entry.is_dir || is_session_file(&path) || self.fs.exists(&path.with_extension("ron"))
            {
                continue;
            }
            if self.is_stale(&path) {
                self.remove_file(&path);
            }
        }

        if self.list(dir).is_some_and(|left| left.is_empty()) {
            self.remove_dir(dir);
        }
    }

    /// The entries of `dir`, or `None` when the listing fails.
    fn list(&self, dir: &Path) -> Option<Vec<FsDirEntry>> {
        match self.fs.list_dir(dir) {
            Ok(entries) => Some(entries),
            Err(err) => {
                tracing::warn!(
                    target: "stoat::state_sweep",
                    %err,
                    path = ?dir,
                    "state directory listing failed",
                );
                None
            },
        }
    }

    /// Whether `path` was last modified more than the retention before now.
    ///
    /// A path whose metadata fails to read counts as fresh, so the sweep keeps
    /// a file of unknown age.
    fn is_stale(&self, path: &Path) -> bool {
        match self.fs.metadata(path) {
            Ok(Some(meta)) => self
                .now
                .duration_since(meta.modified)
                .is_ok_and(|age| age > self.retention),
            Ok(None) => false,
            Err(err) => {
                tracing::warn!(
                    target: "stoat::state_sweep",
                    %err,
                    ?path,
                    "state file metadata read failed",
                );
                false
            },
        }
    }

    fn remove_file(&mut self, path: &Path) {
        match self.fs.remove_file(path) {
            Ok(()) => self.report.removed_files += 1,
            Err(err) => tracing::warn!(
                target: "stoat::state_sweep",
                %err,
                ?path,
                "stale state file removal failed",
            ),
        }
    }

    fn remove_dir(&mut self, path: &Path) {
        match self.fs.remove_dir(path) {
            Ok(()) => self.report.removed_dirs += 1,
            Err(err) => tracing::warn!(
                target: "stoat::state_sweep",
                %err,
                ?path,
                "empty session directory removal failed",
            ),
        }
    }
}

fn is_session_file(path: &Path) -> bool {
    path.extension().is_some_and(|ext| ext == "ron")
}

#[cfg(test)]
mod tests {
    use super::{sweep, SweepReport};
    use crate::host::{FakeFs, FsHost};
    use std::{
        cell::RefCell,
        io,
        path::{Path, PathBuf},
        time::{Duration, SystemTime},
    };

    const STATE: &str = "/state";
    const WORKSPACES: &str = "/state/workspaces";
    const RETENTION: Duration = Duration::from_secs(1000);

    /// Insert the `stale` files, then the `fresh` ones, and return the `now`
    /// that splits them.
    ///
    /// The fake clock ticks a second per insert, so the first fresh file is
    /// exactly [`RETENTION`] old at that `now` and every stale file is older.
    fn seed(fs: &FakeFs, stale: &[&str], fresh: &[&str]) -> SystemTime {
        for path in stale {
            fs.insert_file(path, "");
        }
        let (first, rest) = fresh.split_first().expect("a fresh file marks the split");
        fs.insert_file(first, "");
        for path in rest {
            fs.insert_file(path, "");
        }
        let split = fs.metadata(Path::new(first)).unwrap().unwrap().modified;
        split + RETENTION
    }

    /// Every file and directory under `root`, sorted.
    fn tree(fs: &FakeFs, root: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for entry in fs.list_dir(root).unwrap_or_default() {
            let path = root.join(entry.name.as_str());
            if entry.is_dir {
                out.extend(tree(fs, &path));
            }
            out.push(path);
        }
        out.sort();
        out
    }

    fn paths(list: &[&str]) -> Vec<PathBuf> {
        list.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn sweep_removes_stale_unused_state_and_keeps_the_rest() {
        let fs = FakeFs::new();
        let now = seed(
            &fs,
            &[
                "/state/agent-dead.sock",
                "/state/agent-live.sock",
                "/state/workspaces/aaa/1.ron",
                "/state/workspaces/aaa/1.meta",
                "/state/workspaces/ccc/3.ron",
                "/state/workspaces/ccc/3.meta",
                "/state/workspaces/ddd/4.meta.tmp",
            ],
            &[
                "/state/agent-fresh.sock",
                "/state/workspaces/bbb/2.ron",
                "/state/workspaces/bbb/2.meta",
            ],
        );
        let probed = RefCell::new(Vec::new());
        let probe = |path: &Path| {
            probed.borrow_mut().push(path.to_path_buf());
            path.ends_with("agent-live.sock")
        };

        let report = sweep(
            &fs,
            &[PathBuf::from(STATE), PathBuf::from(STATE)],
            Path::new(WORKSPACES),
            now,
            RETENTION,
            Some(Path::new("/state/workspaces/ccc/3.ron")),
            &probe,
        );

        assert_eq!(
            (tree(&fs, Path::new(STATE)), report, probed.into_inner()),
            (
                paths(&[
                    "/state/agent-fresh.sock",
                    "/state/agent-live.sock",
                    "/state/workspaces",
                    "/state/workspaces/bbb",
                    "/state/workspaces/bbb/2.meta",
                    "/state/workspaces/bbb/2.ron",
                    "/state/workspaces/ccc",
                    "/state/workspaces/ccc/3.meta",
                    "/state/workspaces/ccc/3.ron",
                ]),
                SweepReport {
                    removed_files: 4,
                    removed_dirs: 2,
                },
                paths(&["/state/agent-dead.sock", "/state/agent-live.sock"]),
            ),
            "the stale and unused go, each socket directory is swept once, and \
             only stale sockets are probed",
        );
    }

    #[test]
    fn a_failed_remove_leaves_the_rest_of_the_sweep_to_run() {
        let fs = FakeFs::new();
        let now = seed(
            &fs,
            &[
                "/state/agent-a.sock",
                "/state/agent-b.sock",
                "/state/workspaces/aaa/1.ron",
            ],
            &["/state/agent-fresh.sock"],
        );
        fs.fail_next_remove_file("/state/agent-a.sock", io::ErrorKind::PermissionDenied);

        let report = sweep(
            &fs,
            &[PathBuf::from(STATE)],
            Path::new(WORKSPACES),
            now,
            RETENTION,
            None,
            &|_| false,
        );

        assert_eq!(
            (tree(&fs, Path::new(STATE)), report),
            (
                paths(&[
                    "/state/agent-a.sock",
                    "/state/agent-fresh.sock",
                    "/state/workspaces",
                ]),
                SweepReport {
                    removed_files: 2,
                    removed_dirs: 1,
                },
            ),
        );
    }
}
