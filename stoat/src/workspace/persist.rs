//! Per-workspace session state path resolution.
//!
//! Each workspace's persisted state lives at
//! `<stoat_log::workspace_state_dir()>/<git_root_hash>/<uid>.ron`. This
//! module resolves those paths and locates the most recent state for a
//! given git root or ancestor anchor. Multiple workspaces per git root
//! coexist as sibling files in the same directory.

use crate::{host::FsHost, workspace::WorkspaceUid};
use std::{
    io,
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

/// Resolve the per-git-root directory that holds every workspace persisted
/// against that root. One file per workspace sits in this directory, named
/// by the workspace's [`WorkspaceUid`]. Canonical form of `git_root` is
/// hashed with the stdlib's [`DefaultHasher`] (stable within a Rust release;
/// acceptable here because a hash mismatch just falls back to a fresh session).
pub fn workspace_dir_for(git_root: &Path, fs: &dyn FsHost) -> io::Result<PathBuf> {
    Ok(anchor_state_dir(
        &stoat_log::workspace_state_dir()?,
        git_root,
        fs,
    ))
}

/// Hash-derived state directory for a single anchor under `state_dir`.
/// Factored so callers (and tests) can supply a custom `state_dir`
/// rather than always going through `stoat_log::workspace_state_dir`.
fn anchor_state_dir(state_dir: &Path, anchor: &Path, fs: &dyn FsHost) -> PathBuf {
    use std::hash::{Hash, Hasher};
    let canon = fs
        .canonicalize(anchor)
        .unwrap_or_else(|_| anchor.to_path_buf());
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    canon.hash(&mut hasher);
    let name = format!("{:016x}", hasher.finish());
    state_dir.join(name)
}

/// Resolve the on-disk state file path for a specific workspace.
pub fn state_path_for(git_root: &Path, uid: WorkspaceUid, fs: &dyn FsHost) -> io::Result<PathBuf> {
    Ok(workspace_dir_for(git_root, fs)?.join(format!("{uid}.ron")))
}

/// List every persisted workspace file for a git root, newest first by
/// filesystem mtime. Returns an empty vec (not an error) if the directory
/// does not exist.
pub fn list_workspace_files(git_root: &Path, fs: &dyn FsHost) -> io::Result<Vec<PathBuf>> {
    list_ron_files_by_mtime_desc(&workspace_dir_for(git_root, fs)?, fs)
}

/// Walk ancestors of `cwd` (cwd itself first) for any directory whose
/// workspace state directory contains persisted `.ron` files, and return
/// the ancestor whose newest file has the highest mtime across all
/// candidates. Returns `None` when no ancestor has any persisted state.
///
/// Backs the binary's `--resume` flag: workspaces are tracked per anchor
/// directory, and `--resume` cascades up so a session run from
/// `~/foo/bar/baz/bang` reopens whichever ancestor's state is most
/// recent. cwd-first iteration means a tie at the same mtime resolves
/// to the deepest ancestor, which is the natural "most specific match"
/// when multiple state files were saved at the same instant.
pub fn find_resume_anchor(cwd: &Path, fs: &dyn FsHost) -> io::Result<Option<PathBuf>> {
    let state_dir = stoat_log::workspace_state_dir()?;
    find_resume_anchor_in(&state_dir, cwd, fs)
}

fn find_resume_anchor_in(
    state_dir: &Path,
    cwd: &Path,
    fs: &dyn FsHost,
) -> io::Result<Option<PathBuf>> {
    let mut best: Option<(PathBuf, std::time::SystemTime)> = None;
    for anc in cwd.ancestors() {
        let dir = anchor_state_dir(state_dir, anc, fs);
        if !fs.exists(&dir) {
            continue;
        }
        let mut newest: Option<std::time::SystemTime> = None;
        for entry in fs.list_dir(&dir)? {
            let path = dir.join(entry.name.as_str());
            if path.extension().and_then(|s| s.to_str()) != Some("ron") {
                continue;
            }
            let mtime = fs
                .metadata(&path)
                .ok()
                .flatten()
                .map(|m| m.modified)
                .unwrap_or(UNIX_EPOCH);
            newest = Some(newest.map_or(mtime, |prev| prev.max(mtime)));
        }
        if let Some(mtime) = newest {
            match &best {
                Some((_, prev_mtime)) if *prev_mtime >= mtime => {},
                _ => best = Some((anc.to_path_buf(), mtime)),
            }
        }
    }
    Ok(best.map(|(p, _)| p))
}

/// Underlying directory scan for [`list_workspace_files`]. Factored so tests
/// can exercise it against a tempdir without touching the real XDG path.
/// Entries whose metadata cannot be read are treated as unix-epoch-old so
/// they sort to the bottom rather than dropping out silently.
fn list_ron_files_by_mtime_desc(dir: &Path, fs: &dyn FsHost) -> io::Result<Vec<PathBuf>> {
    if !fs.exists(dir) {
        return Ok(Vec::new());
    }
    let mut entries: Vec<(PathBuf, std::time::SystemTime)> = Vec::new();
    for entry in fs.list_dir(dir)? {
        let path = dir.join(entry.name.as_str());
        if path.extension().and_then(|s| s.to_str()) != Some("ron") {
            continue;
        }
        let mtime = fs
            .metadata(&path)
            .ok()
            .flatten()
            .map(|m| m.modified)
            .unwrap_or(UNIX_EPOCH);
        entries.push((path, mtime));
    }
    entries.sort_by_key(|b| std::cmp::Reverse(b.1));
    Ok(entries.into_iter().map(|(p, _)| p).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::FakeFs;

    #[test]
    fn list_ron_files_sorts_newest_first() {
        let fake = FakeFs::new();
        let ws_dir = PathBuf::from("/test");
        let older = ws_dir.join("aaaa.ron");
        let newer = ws_dir.join("bbbb.ron");
        fake.insert_file(&older, "old");
        fake.insert_file(&newer, "new");

        let listed = list_ron_files_by_mtime_desc(&ws_dir, &fake).unwrap();
        assert_eq!(listed, vec![newer, older]);
    }

    #[test]
    fn list_ron_files_ignores_non_ron_entries() {
        let fake = FakeFs::new();
        let ws_dir = PathBuf::from("/test");
        fake.insert_file(ws_dir.join("ok.ron"), "");
        fake.insert_file(ws_dir.join("skip.txt"), "");
        fake.insert_dir(ws_dir.join("subdir"));

        let listed = list_ron_files_by_mtime_desc(&ws_dir, &fake).unwrap();
        assert_eq!(listed, vec![ws_dir.join("ok.ron")]);
    }

    #[test]
    fn list_ron_files_missing_dir_returns_empty() {
        let fake = FakeFs::new();
        let ws_dir = PathBuf::from("/test");
        let missing = ws_dir.join("nope");
        assert!(list_ron_files_by_mtime_desc(&missing, &fake)
            .unwrap()
            .is_empty());
    }

    fn write_anchor_state(state_dir: &Path, anchor: &Path, fake: &FakeFs, name: &str) -> PathBuf {
        let dir = anchor_state_dir(state_dir, anchor, fake);
        let path = dir.join(name);
        fake.insert_file(&path, "");
        path
    }

    #[test]
    fn find_resume_anchor_no_state_returns_none() {
        let fake = FakeFs::new();
        let state_dir = PathBuf::from("/state");
        let cwd = PathBuf::from("/foo/bar/baz");
        let result = find_resume_anchor_in(&state_dir, &cwd, &fake).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn find_resume_anchor_picks_only_ancestor_with_state() {
        let fake = FakeFs::new();
        let state_dir = PathBuf::from("/state");
        let cwd = PathBuf::from("/foo/bar/baz");
        let anchor = PathBuf::from("/foo");
        write_anchor_state(&state_dir, &anchor, &fake, "ws.ron");
        let result = find_resume_anchor_in(&state_dir, &cwd, &fake).unwrap();
        assert_eq!(result, Some(anchor));
    }

    #[test]
    fn find_resume_anchor_picks_cwd_when_only_cwd_has_state() {
        let fake = FakeFs::new();
        let state_dir = PathBuf::from("/state");
        let cwd = PathBuf::from("/foo/bar/baz");
        write_anchor_state(&state_dir, &cwd, &fake, "ws.ron");
        let result = find_resume_anchor_in(&state_dir, &cwd, &fake).unwrap();
        assert_eq!(result, Some(cwd));
    }

    #[test]
    fn find_resume_anchor_prefers_more_recent_anchor() {
        let fake = FakeFs::new();
        let state_dir = PathBuf::from("/state");
        let cwd = PathBuf::from("/foo/bar/baz");
        write_anchor_state(&state_dir, &PathBuf::from("/foo"), &fake, "old.ron");
        write_anchor_state(&state_dir, &cwd, &fake, "new.ron");
        let result = find_resume_anchor_in(&state_dir, &cwd, &fake).unwrap();
        assert_eq!(result, Some(cwd));
    }

    #[test]
    fn find_resume_anchor_prefers_parent_when_parent_newer() {
        let fake = FakeFs::new();
        let state_dir = PathBuf::from("/state");
        let cwd = PathBuf::from("/foo/bar/baz");
        write_anchor_state(&state_dir, &cwd, &fake, "old.ron");
        let parent = PathBuf::from("/foo");
        write_anchor_state(&state_dir, &parent, &fake, "new.ron");
        let result = find_resume_anchor_in(&state_dir, &cwd, &fake).unwrap();
        assert_eq!(result, Some(parent));
    }

    #[test]
    fn find_resume_anchor_skips_non_ron_files() {
        let fake = FakeFs::new();
        let state_dir = PathBuf::from("/state");
        let cwd = PathBuf::from("/foo/bar");
        let parent = PathBuf::from("/foo");
        let parent_dir = anchor_state_dir(&state_dir, &parent, &fake);
        fake.insert_file(parent_dir.join("notes.txt"), "");
        let result = find_resume_anchor_in(&state_dir, &cwd, &fake).unwrap();
        assert_eq!(
            result, None,
            "non-.ron files in an ancestor's state dir should be ignored"
        );
    }
}
