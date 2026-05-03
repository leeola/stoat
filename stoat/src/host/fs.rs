use compact_str::CompactString;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
#[cfg(test)]
use ignore::Match;
use std::{
    io,
    path::{Path, PathBuf},
    time::SystemTime,
};

/// Baked-in default ignore patterns applied to every workspace's file
/// finder. Parsed with gitignore semantics; treated as an unconditional
/// hard filter so per-repo `.stoatignore` negations cannot re-introduce
/// the listed paths.
pub(crate) const DEFAULT_STOATIGNORE: &str = include_str!("../../../.stoatignore");

#[derive(Clone, Copy, Debug)]
pub struct FsMetadata {
    pub len: u64,
    pub modified: SystemTime,
    pub is_dir: bool,
    pub is_symlink: bool,
}

#[derive(Debug)]
pub struct FsDirEntry {
    pub name: CompactString,
    pub is_dir: bool,
    pub is_symlink: bool,
}

/// Filesystem operations, synchronous.
///
/// Callers in the TUI event loop invoke these directly; there is no
/// runtime-bridging layer. A future remote implementation that needs
/// async can wrap a sync [`FsHost`] call with its own blocking bridge
/// rather than forcing every UI call site to deal with futures.
pub trait FsHost: Send + Sync {
    /// Clears `buf` and fills it with the file's contents.
    fn read(&self, path: &Path, buf: &mut Vec<u8>) -> io::Result<()>;

    /// Writes `data` to `path`, creating or truncating the file.
    fn write(&self, path: &Path, data: &[u8]) -> io::Result<()>;

    /// Returns metadata, or `None` if the path doesn't exist. Errors
    /// only on real IO failures (permission denied, etc.), not NotFound.
    fn metadata(&self, path: &Path) -> io::Result<Option<FsMetadata>>;

    /// Lists entries in `path`. Errors if the directory doesn't exist.
    fn list_dir(&self, path: &Path) -> io::Result<Vec<FsDirEntry>>;

    /// Creates `path` and all missing parent directories.
    fn create_dir_all(&self, path: &Path) -> io::Result<()>;

    /// Resolves `path` to an absolute, symlink-free form. Errors with
    /// `NotFound` if the path doesn't exist (matches
    /// [`std::fs::canonicalize`] semantics).
    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf>;

    /// Removes the file at `path`. Errors with `NotFound` if absent.
    fn remove_file(&self, path: &Path) -> io::Result<()>;

    /// Renames `from` to `to`. Errors with `NotFound` if `from` is absent.
    /// Overwrites `to` if it already exists, matching `std::fs::rename`.
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;

    /// Returns whether `path` exists.
    fn exists(&self, path: &Path) -> bool {
        self.metadata(path).ok().flatten().is_some()
    }

    /// Enumerates every non-ignored file under `root`. Each implementation
    /// chooses how to honour the ignore stack: production [`super::LocalFs`]
    /// uses [`ignore::WalkBuilder`] so global `core.excludesFile` and
    /// `.git/info/exclude` apply; in-memory fakes walk through their
    /// own state via [`manual_walk`]. Output is sorted lexicographically.
    fn walk_workspace_files(&self, root: &Path) -> Vec<PathBuf>;
}

/// Recursive walker driven by [`FsHost::list_dir`] / [`FsHost::read`].
/// Used by every fake that has no notion of an underlying real filesystem
/// (notably [`super::FakeFs`]). Honours per-directory `.gitignore` /
/// `.stoatignore` plus the baked-in [`DEFAULT_STOATIGNORE`] hard filter.
/// Does not consult global / `$HOME` ignore files; that is the
/// production walker's job.
#[cfg(test)]
pub(crate) fn manual_walk(fs: &dyn FsHost, root: &Path) -> Vec<PathBuf> {
    let defaults = build_default_ignore(root);
    let mut stack: Vec<Gitignore> = Vec::new();
    let mut out = Vec::new();
    walk_dir(fs, root, &defaults, &mut stack, &mut out);
    out.sort();
    out
}

/// Build the baked-in ignore matcher from [`DEFAULT_STOATIGNORE`],
/// rooted at `root` so glob expansion treats the workspace as the
/// pattern base.
pub(crate) fn build_default_ignore(root: &Path) -> Gitignore {
    let mut builder = GitignoreBuilder::new(root);
    for line in DEFAULT_STOATIGNORE.lines() {
        builder
            .add_line(None, line)
            .expect("default .stoatignore parses");
    }
    builder.build().expect("default .stoatignore builds")
}

#[cfg(test)]
fn walk_dir(
    fs: &dyn FsHost,
    dir: &Path,
    defaults: &Gitignore,
    stack: &mut Vec<Gitignore>,
    out: &mut Vec<PathBuf>,
) {
    let pushed = push_dir_ignores(fs, dir, stack);

    let entries = match fs.list_dir(dir) {
        Ok(e) => e,
        Err(_) => {
            stack.truncate(stack.len() - pushed);
            return;
        },
    };

    for entry in entries {
        let path = dir.join(entry.name.as_str());
        if path_is_ignored(defaults, stack, &path, entry.is_dir) {
            continue;
        }
        if entry.is_dir {
            walk_dir(fs, &path, defaults, stack, out);
        } else {
            out.push(path);
        }
    }

    stack.truncate(stack.len() - pushed);
}

#[cfg(test)]
fn push_dir_ignores(fs: &dyn FsHost, dir: &Path, stack: &mut Vec<Gitignore>) -> usize {
    const NAMES: &[&str] = &[".gitignore", ".stoatignore"];
    let mut pushed = 0;
    for name in NAMES {
        if let Some(matcher) = read_ignore_file(fs, dir, name) {
            stack.push(matcher);
            pushed += 1;
        }
    }
    pushed
}

#[cfg(test)]
fn read_ignore_file(fs: &dyn FsHost, dir: &Path, name: &str) -> Option<Gitignore> {
    let path = dir.join(name);
    let mut buf = Vec::new();
    fs.read(&path, &mut buf).ok()?;
    let text = std::str::from_utf8(&buf).ok()?;
    let mut builder = GitignoreBuilder::new(dir);
    for line in text.lines() {
        let _ = builder.add_line(None, line);
    }
    builder.build().ok()
}

#[cfg(test)]
fn path_is_ignored(defaults: &Gitignore, stack: &[Gitignore], path: &Path, is_dir: bool) -> bool {
    if defaults.matched(path, is_dir).is_ignore() {
        return true;
    }
    let mut decision: Option<bool> = None;
    for matcher in stack {
        match matcher.matched(path, is_dir) {
            Match::Ignore(_) => decision = Some(true),
            Match::Whitelist(_) => decision = Some(false),
            Match::None => {},
        }
    }
    decision.unwrap_or(false)
}
