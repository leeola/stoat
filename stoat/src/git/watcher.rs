//! Filesystem watcher for git-relevant paths.
//!
//! Monitors `.git/index`, `.git/HEAD`, and `.git/refs/` for changes caused by
//! external git operations (add, commit, checkout, stash, etc.) and sends
//! debounce-friendly events through a channel.

use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use smol::channel::Sender;
use std::path::Path;

/// Classification of a git filesystem event.
#[derive(Debug, Clone, Copy)]
pub enum GitChangeKind {
    /// `.git/index` changed (git add, reset, commit, stash)
    Index,
    /// `.git/HEAD` or refs changed (checkout, commit, branch)
    Head,
}

/// Start watching git-relevant paths. Sends events to `sender`.
///
/// Returns the watcher handle which must be kept alive for the duration of monitoring.
/// Returns [`None`] if the `.git` directory doesn't exist or the watcher can't be created.
pub fn start_watching(root: &Path, sender: Sender<GitChangeKind>) -> Option<RecommendedWatcher> {
    let git_dir = root.join(".git");
    if !git_dir.exists() {
        return None;
    }

    let git_dir_clone = git_dir.clone();
    let mut watcher = notify::recommended_watcher(move |event: Result<notify::Event, _>| {
        let Ok(event) = event else { return };
        if let Some(kind) = classify_event(&event, &git_dir_clone) {
            let _ = sender.try_send(kind);
        }
    })
    .ok()?;

    let _ = watcher.watch(&git_dir.join("index"), RecursiveMode::NonRecursive);
    let _ = watcher.watch(&git_dir.join("HEAD"), RecursiveMode::NonRecursive);
    let _ = watcher.watch(&git_dir.join("refs"), RecursiveMode::Recursive);

    Some(watcher)
}

fn classify_event(event: &notify::Event, git_dir: &Path) -> Option<GitChangeKind> {
    for path in &event.paths {
        if path.starts_with(git_dir.join("refs")) || *path == git_dir.join("HEAD") {
            return Some(GitChangeKind::Head);
        }
        if *path == git_dir.join("index") {
            return Some(GitChangeKind::Index);
        }
    }
    None
}
