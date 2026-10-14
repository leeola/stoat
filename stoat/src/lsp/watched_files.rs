//! The file watchers a language server registers with the editor.
//!
//! A server must learn about files that change on disk outside the editor: a
//! new source file, a deleted one, or a manifest edit. Stoat offers dynamic
//! registration of `workspace/didChangeWatchedFiles`, and a server that uses it
//! turns its own watcher off. From then on, the server learns about the disk
//! only through the notifications the editor sends. A registration that the
//! editor holds and never serves leaves the server blind to the disk after
//! startup.
//!
//! A registration's globs compile once, when the server sends them. Each
//! changed path is then tested against the compiled form.
//!
//! The editor also records the files on disk that each server's delete
//! watchers name. A directory that moves away reaches the editor as one event
//! on the directory, and no event names the files it held. The record is what
//! lets the editor report each of those files deleted.
//!
//! See also:
//! - [`crate::lsp::drain`] for the requests that hold and drop registrations.
//! - [`crate::debounce::drain_fs_watch_events`] for the changes sent to them.

use crate::{host::FsHost, lsp::util};
use globset::{Glob, GlobMatcher};
use lsp_types::{
    notification::{DidChangeWatchedFiles, Notification},
    DidChangeWatchedFilesRegistrationOptions, FileChangeType, FileSystemWatcher, GlobPattern,
    OneOf, Registration, WatchKind,
};
use serde::Deserialize;
use std::{
    collections::BTreeSet,
    ops::Bound,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use stoat_scheduler::{Executor, Task};
use tokio::sync::Notify;

/// One `workspace/didChangeWatchedFiles` registration of a language server,
/// with its globs compiled.
#[derive(Clone)]
pub(crate) struct WatchedFilesRegistration {
    /// The id the server registered under, which its unregistration names.
    pub(crate) id: String,
    watchers: Vec<CompiledWatcher>,
}

/// One glob of a registration and the change kinds the server wants for it.
#[derive(Clone)]
struct CompiledWatcher {
    matcher: GlobMatcher,
    /// The folder a relative pattern matches below, or `None` for a pattern
    /// that matches the whole absolute path.
    base: Option<PathBuf>,
    kind: WatchKind,
}

impl WatchedFilesRegistration {
    /// Whether the server asked to hear about a `change` to `path`.
    pub(crate) fn matches(&self, path: &Path, change: FileChangeType) -> bool {
        let wanted = watch_kind(change);
        self.watchers
            .iter()
            .any(|watcher| watcher.kind.contains(wanted) && watcher.matches(path))
    }
}

impl CompiledWatcher {
    fn matches(&self, path: &Path) -> bool {
        match &self.base {
            Some(base) => path
                .strip_prefix(base)
                .is_ok_and(|relative| self.matcher.is_match(relative)),
            None => self.matcher.is_match(path),
        }
    }
}

/// One language server's file watchers, and the files on disk they name.
///
/// The record holds the files below the language-server root that a delete
/// watcher names, as the editor last saw the disk. A walk fills it when the
/// server registers watchers, and the fs-watch drain keeps it current after
/// that. A directory that leaves the disk takes the recorded files below it,
/// and that is how they reach the server as deletions.
#[derive(Default)]
pub(crate) struct FileWatchers {
    pub(crate) registrations: Vec<WatchedFilesRegistration>,
    files: WatchedFiles,
}

impl FileWatchers {
    /// Whether a registration asks to hear about a `change` to `path`.
    pub(crate) fn matches(&self, path: &Path, change: FileChangeType) -> bool {
        self.registrations
            .iter()
            .any(|registration| registration.matches(path, change))
    }

    /// Record the file that arrived at `path`, when a delete watcher names it.
    pub(crate) fn note_arrival(&mut self, path: &Path) {
        if self.matches(path, FileChangeType::DELETED) {
            self.files.note(path, true);
        }
    }

    /// Record that `path` left the disk, as a file or as a directory.
    pub(crate) fn note_departure(&mut self, path: &Path) {
        self.files.note(path, false);
    }

    /// Take the recorded files below `dir`, which left the disk, that a delete
    /// watcher names.
    ///
    /// Empty while a walk is out. Its landing reports them instead, together
    /// with the files the walk found there.
    pub(crate) fn take_departed_below(&mut self, dir: &Path) -> Vec<PathBuf> {
        if self.files.walk.is_some() {
            return Vec::new();
        }
        let mut below = take_below(&mut self.files.paths, dir);
        below.retain(|path| self.matches(path, FileChangeType::DELETED));
        below
    }

    /// Refill the record from a walk of `root` on the blocking pool, which
    /// notifies `wake` when it finishes.
    ///
    /// A walk still out is dropped. The changes the drain kept for it carry
    /// over to this one, because the record has not taken them yet.
    pub(crate) fn walk(
        &mut self,
        executor: &Executor,
        fs: Arc<dyn FsHost>,
        root: PathBuf,
        wake: Arc<Notify>,
    ) {
        let walked = Arc::new(Mutex::new(None));
        let task = executor.spawn_blocking({
            let walked = walked.clone();
            let registrations = self.registrations.clone();
            move || {
                let files = list_watched_files(&*fs, &root, |path| {
                    registrations
                        .iter()
                        .any(|registration| registration.matches(path, FileChangeType::DELETED))
                });
                *walked.lock().expect("watched files walk mutex") = Some(files);
                wake.notify_one();
            }
        });

        let changes = self
            .files
            .walk
            .take()
            .map(|walk| walk.changes)
            .unwrap_or_default();
        self.files.walk = Some(FileWalk {
            _task: task,
            walked,
            changes,
        });
    }

    /// Land a finished walk, and return the files that left the disk while it
    /// was out and that a delete watcher names.
    ///
    /// The drain reports none of those, because the record it reads waits on
    /// the walk. `None` while no walk has finished.
    pub(crate) fn land_walk(&mut self) -> Option<Vec<PathBuf>> {
        let walked = self
            .files
            .walk
            .as_ref()?
            .walked
            .lock()
            .expect("watched files walk mutex")
            .take()?;
        let changes = self
            .files
            .walk
            .take()
            .map(|walk| walk.changes)
            .unwrap_or_default();

        let mut gone = self.files.settle(walked, &changes);
        gone.retain(|path| self.matches(path, FileChangeType::DELETED));
        Some(gone)
    }
}

/// The record of a [`FileWatchers`], with the walk that refills it.
#[derive(Default)]
struct WatchedFiles {
    /// Ordered, so the files below a directory are one range.
    paths: BTreeSet<PathBuf>,
    /// A walk still out. Until it lands, the drain's changes wait in it and
    /// not in `paths`.
    walk: Option<FileWalk>,
}

impl WatchedFiles {
    /// Apply a file that arrived at `path` or a path that left the disk, or
    /// keep the change for the walk that is out.
    fn note(&mut self, path: &Path, arrived: bool) {
        if let Some(walk) = &mut self.walk {
            walk.changes.push((path.to_path_buf(), arrived));
        } else if arrived {
            self.paths.insert(path.to_path_buf());
        } else {
            self.paths.remove(path);
        }
    }

    /// Replace the record with what a walk read, with `changes` laid over it in
    /// order, and return the files that a departed directory took and that no
    /// event reported.
    ///
    /// A departed file comes from the walk or from the record it replaces. A
    /// walk that reads a directory after the directory left finds none of its
    /// files, and the record still holds them. A later change to the file takes
    /// it back out of the return. An arrival brings the file back, and its own
    /// departure went to the server as that event.
    fn settle(
        &mut self,
        mut walked: BTreeSet<PathBuf>,
        changes: &[(PathBuf, bool)],
    ) -> Vec<PathBuf> {
        let mut gone = BTreeSet::new();
        for (path, arrived) in changes {
            gone.remove(path);
            if *arrived {
                walked.insert(path.clone());
                continue;
            }
            walked.remove(path);
            self.paths.remove(path);
            gone.extend(take_below(&mut walked, path));
            gone.extend(take_below(&mut self.paths, path));
        }

        self.paths = walked;
        gone.into_iter().collect()
    }
}

/// A walk of the language-server root on the blocking pool.
struct FileWalk {
    _task: Task<()>,
    walked: Arc<Mutex<Option<BTreeSet<PathBuf>>>>,
    /// Each path the drain saw arrive (`true`) or leave (`false`) since the
    /// walk started, in order.
    ///
    /// The walk reads each directory at some moment in that window, so its
    /// result does not show which of these came before that read.
    changes: Vec<(PathBuf, bool)>,
}

/// Compiles `registration` when it registers file watchers.
///
/// Returns `None` for a registration of any other method, and for options
/// that do not decode. A glob that does not compile, or a base that is not a
/// `file:` URI, is logged and skipped. The other watchers of the registration
/// stay in force.
pub(crate) fn compile(registration: &Registration) -> Option<WatchedFilesRegistration> {
    if registration.method != DidChangeWatchedFiles::METHOD {
        return None;
    }
    let options = DidChangeWatchedFilesRegistrationOptions::deserialize(
        registration.register_options.as_ref()?,
    )
    .ok()?;

    let watchers = options
        .watchers
        .into_iter()
        .filter_map(compile_watcher)
        .collect();
    Some(WatchedFilesRegistration {
        id: registration.id.clone(),
        watchers,
    })
}

fn compile_watcher(watcher: FileSystemWatcher) -> Option<CompiledWatcher> {
    let (pattern, base) = match watcher.glob_pattern {
        GlobPattern::String(pattern) => (pattern, None),
        GlobPattern::Relative(relative) => {
            let base_uri = match relative.base_uri {
                OneOf::Left(folder) => folder.uri,
                OneOf::Right(uri) => uri,
            };
            let Some(base) = util::lsp_uri_to_path(&base_uri) else {
                tracing::warn!(
                    target: "stoat::lsp",
                    base = base_uri.as_str(),
                    "file watcher base is not a file URI, skipping"
                );
                return None;
            };
            (relative.pattern, Some(base))
        },
    };

    let matcher = match Glob::new(&pattern) {
        Ok(glob) => glob.compile_matcher(),
        Err(err) => {
            tracing::warn!(
                target: "stoat::lsp",
                glob = %pattern,
                %err,
                "invalid file watcher glob, skipping"
            );
            return None;
        },
    };
    Some(CompiledWatcher {
        matcher,
        base,
        kind: watcher.kind.unwrap_or(WatchKind::all()),
    })
}

/// The [`WatchKind`] flag a watcher sets to hear about `change`.
fn watch_kind(change: FileChangeType) -> WatchKind {
    match change {
        FileChangeType::CREATED => WatchKind::Create,
        FileChangeType::CHANGED => WatchKind::Change,
        _ => WatchKind::Delete,
    }
}

/// Remove the paths strictly below `dir` from `paths`, and return them.
fn take_below(paths: &mut BTreeSet<PathBuf>, dir: &Path) -> Vec<PathBuf> {
    let below: Vec<PathBuf> = paths
        .range::<Path, _>((Bound::Excluded(dir), Bound::Unbounded))
        .take_while(|path| path.starts_with(dir))
        .cloned()
        .collect();
    for path in &below {
        paths.remove(path);
    }
    below
}

/// The files that `keep` accepts, directly in each directory that the
/// workspace walk keeps below `root`.
///
/// Reads directories and not a walk of files, because the editor watches
/// exactly those directories. A file that only a per-file ignore rule drops
/// still raises events there, so it belongs in the record too.
fn list_watched_files(
    fs: &dyn FsHost,
    root: &Path,
    keep: impl Fn(&Path) -> bool,
) -> BTreeSet<PathBuf> {
    let mut files = BTreeSet::new();
    for dir in fs.walk_workspace_dirs(root) {
        let Ok(entries) = fs.list_dir(&dir) else {
            continue;
        };
        files.extend(
            entries
                .into_iter()
                .filter(|entry| !entry.is_dir && !entry.is_symlink)
                .map(|entry| dir.join(entry.name.as_str()))
                .filter(|path| keep(path)),
        );
    }
    files
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_fixture;
    use lsp_types::{RelativePattern, Uri};
    use std::str::FromStr;

    /// A string pattern matches the whole absolute path, at any depth.
    #[test]
    fn a_string_glob_matches_absolute_paths() {
        let rust = compiled(GlobPattern::String("**/*.rs".into()), None);

        assert_eq!(
            hits(
                &rust,
                FileChangeType::CREATED,
                &["/repo/src/a.rs", "/repo/Cargo.toml"]
            ),
            [true, false],
        );
    }

    /// A relative pattern matches only the paths below its base.
    #[test]
    fn a_relative_glob_matches_below_its_base() {
        let base_uri = OneOf::Right(Uri::from_str("file:///repo/sub").expect("base uri"));
        let manifests = compiled(
            GlobPattern::Relative(RelativePattern {
                base_uri,
                pattern: "*.toml".into(),
            }),
            None,
        );

        assert_eq!(
            hits(
                &manifests,
                FileChangeType::CHANGED,
                &["/repo/sub/Cargo.toml", "/repo/Cargo.toml"]
            ),
            [true, false],
        );
    }

    /// A watcher hears only the change kinds it asked for.
    #[test]
    fn a_create_only_watcher_ignores_changes() {
        let created = compiled(
            GlobPattern::String("**/*.rs".into()),
            Some(WatchKind::Create),
        );
        let path = Path::new("/repo/src/a.rs");

        assert_eq!(
            (
                created.matches(path, FileChangeType::CREATED),
                created.matches(path, FileChangeType::CHANGED),
            ),
            (true, false),
        );
    }

    /// The options of every other capability mean something else, so only a
    /// watched-files registration compiles.
    #[test]
    fn another_method_compiles_to_none() {
        let mut formatting = test_fixture::watched_files_registration(
            "w",
            GlobPattern::String("**/*.rs".into()),
            None,
        );
        formatting.method = "textDocument/formatting".into();

        assert!(compile(&formatting).is_none());
    }

    fn compiled(glob_pattern: GlobPattern, kind: Option<WatchKind>) -> WatchedFilesRegistration {
        let registration = test_fixture::watched_files_registration("w", glob_pattern, kind);
        compile(&registration).expect("a watched-files registration compiles")
    }

    /// Whether `registration` asks for `change` on each of `paths`.
    fn hits(
        registration: &WatchedFilesRegistration,
        change: FileChangeType,
        paths: &[&str],
    ) -> Vec<bool> {
        paths
            .iter()
            .map(|path| registration.matches(Path::new(path), change))
            .collect()
    }
}
