use compact_str::CompactString;
use ignore::{
    gitignore::{Gitignore, GitignoreBuilder},
    Match, WalkBuilder,
};
use std::{
    io,
    io::Read,
    path::{Component, Path, PathBuf},
    time::SystemTime,
};

/// Baked-in default ignore patterns applied to every workspace's file
/// finder. Parsed with gitignore semantics; treated as an unconditional
/// hard filter so per-repo `.stoatignore` negations cannot re-introduce
/// the listed paths.
///
/// Sourced from `stoatignore` at the repo root (no leading dot) so the
/// developer can keep a personal `.stoatignore` in this checkout
/// without colliding with the build artifact.
pub(crate) const DEFAULT_STOATIGNORE: &str = include_str!("../../stoatignore");

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

    /// Recursively removes the directory at `path` and everything under
    /// it. Errors with `NotFound` if absent.
    fn remove_dir_all(&self, path: &Path) -> io::Result<()>;

    /// Renames `from` to `to`. Errors with `NotFound` if `from` is absent.
    /// Overwrites `to` if it already exists, matching `std::fs::rename`.
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;

    /// Returns whether `path` exists.
    fn exists(&self, path: &Path) -> bool {
        self.metadata(path).ok().flatten().is_some()
    }

    /// Enumerates every non-ignored file under `root`. Each implementation
    /// chooses how to honour the ignore stack: production [`LocalFs`]
    /// uses [`ignore::WalkBuilder`] so global `core.excludesFile` and
    /// `.git/info/exclude` apply; in-memory fakes walk through their
    /// own state via [`manual_walk`]. Output is sorted lexicographically.
    fn walk_workspace_files(&self, root: &Path) -> Vec<PathBuf>;

    /// Streaming counterpart to [`Self::walk_workspace_files`]. Calls
    /// `on_batch` repeatedly with chunks of paths as the walker discovers
    /// them, ending when the walk is exhausted. Lets long-running walks
    /// surface partial results to consumers that re-filter as data
    /// arrives. Batches arrive in walker order; unlike
    /// [`Self::walk_workspace_files`] the global output is not sorted,
    /// so callers must order results themselves if they need a stable
    /// presentation. The default impl emits the full sorted list as one
    /// batch so non-streaming hosts still satisfy the contract.
    fn walk_workspace_files_streaming(&self, root: &Path, on_batch: &mut dyn FnMut(Vec<PathBuf>)) {
        let paths = self.walk_workspace_files(root);
        if !paths.is_empty() {
            on_batch(paths);
        }
    }

    /// Returns whether `path` would be filtered out by the workspace
    /// ignore stack rooted at `workdir` (baked-in [`DEFAULT_STOATIGNORE`]
    /// plus per-directory `.gitignore` / `.stoatignore` accumulated from
    /// `workdir` down to `path.parent()`). Paths outside `workdir` are
    /// reported as non-ignored. The default impl re-reads ignore files
    /// on every call; callers that query at high rate should batch.
    fn is_ignored(&self, workdir: &Path, path: &Path) -> bool {
        if !path.starts_with(workdir) {
            return false;
        }
        let defaults = build_default_ignore(workdir);
        let mut stack: Vec<Gitignore> = Vec::new();
        let mut current = workdir.to_path_buf();
        push_dir_ignores(self, &current, &mut stack);

        if let Some(parent) = path.parent()
            && let Ok(rel) = parent.strip_prefix(workdir)
        {
            for component in rel.components() {
                let Component::Normal(c) = component else {
                    continue;
                };
                current.push(c);
                if path_is_ignored(&defaults, &stack, &current, true) {
                    return true;
                }
                push_dir_ignores(self, &current, &mut stack);
            }
        }

        let is_dir = self.metadata(path).ok().flatten().is_some_and(|m| m.is_dir);
        path_is_ignored(&defaults, &stack, path, is_dir)
    }
}

/// Batch size used by streaming walkers. Small enough that early
/// prefixes match against partial results within one or two render
/// ticks; large enough that channel + notify overhead does not
/// dominate per-path cost.
pub const WALK_BATCH_SIZE: usize = 256;

/// Recursive walker driven by [`FsHost::list_dir`] / [`FsHost::read`].
/// Used by every fake that has no notion of an underlying real filesystem
/// (notably [`crate::FakeFs`]). Honours per-directory `.gitignore` /
/// `.stoatignore` plus the baked-in [`DEFAULT_STOATIGNORE`] hard filter.
/// Does not consult global / `$HOME` ignore files; that is the
/// production walker's job.
pub fn manual_walk(fs: &dyn FsHost, root: &Path) -> Vec<PathBuf> {
    let defaults = build_default_ignore(root);
    let mut stack: Vec<Gitignore> = Vec::new();
    let mut out = Vec::new();
    walk_dir(fs, root, &defaults, &mut stack, &mut out);
    out.sort();
    out
}

/// Streaming counterpart to [`manual_walk`]. Calls `on_batch` whenever
/// the in-flight buffer reaches [`WALK_BATCH_SIZE`] paths and once more
/// for any remainder. Does not sort; batches arrive in walker order.
pub fn manual_walk_streaming(fs: &dyn FsHost, root: &Path, on_batch: &mut dyn FnMut(Vec<PathBuf>)) {
    let defaults = build_default_ignore(root);
    let mut stack: Vec<Gitignore> = Vec::new();
    let mut buffer: Vec<PathBuf> = Vec::with_capacity(WALK_BATCH_SIZE);
    walk_dir_streaming(fs, root, &defaults, &mut stack, &mut buffer, on_batch);
    if !buffer.is_empty() {
        on_batch(buffer);
    }
}

/// Build the baked-in ignore matcher from [`DEFAULT_STOATIGNORE`],
/// rooted at `root` so glob expansion treats the workspace as the
/// pattern base.
pub(crate) fn build_default_ignore(root: &Path) -> Gitignore {
    let mut builder = GitignoreBuilder::new(root);
    for line in DEFAULT_STOATIGNORE.lines() {
        builder
            .add_line(None, line)
            .expect("default stoatignore parses");
    }
    builder.build().expect("default stoatignore builds")
}

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

fn walk_dir_streaming(
    fs: &dyn FsHost,
    dir: &Path,
    defaults: &Gitignore,
    stack: &mut Vec<Gitignore>,
    buffer: &mut Vec<PathBuf>,
    on_batch: &mut dyn FnMut(Vec<PathBuf>),
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
            walk_dir_streaming(fs, &path, defaults, stack, buffer, on_batch);
        } else {
            buffer.push(path);
            if buffer.len() >= WALK_BATCH_SIZE {
                let batch = std::mem::replace(buffer, Vec::with_capacity(WALK_BATCH_SIZE));
                on_batch(batch);
            }
        }
    }

    stack.truncate(stack.len() - pushed);
}

fn push_dir_ignores<F: FsHost + ?Sized>(fs: &F, dir: &Path, stack: &mut Vec<Gitignore>) -> usize {
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

fn read_ignore_file<F: FsHost + ?Sized>(fs: &F, dir: &Path, name: &str) -> Option<Gitignore> {
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

pub struct LocalFs;

impl FsHost for LocalFs {
    fn read(&self, path: &Path, buf: &mut Vec<u8>) -> io::Result<()> {
        buf.clear();
        let mut file = std::fs::File::open(path)?;
        file.read_to_end(buf)?;
        Ok(())
    }

    fn write(&self, path: &Path, data: &[u8]) -> io::Result<()> {
        std::fs::write(path, data)
    }

    fn metadata(&self, path: &Path) -> io::Result<Option<FsMetadata>> {
        match std::fs::symlink_metadata(path) {
            Ok(m) => Ok(Some(FsMetadata {
                len: m.len(),
                modified: m.modified()?,
                is_dir: m.is_dir(),
                is_symlink: m.file_type().is_symlink(),
            })),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn list_dir(&self, path: &Path) -> io::Result<Vec<FsDirEntry>> {
        let mut entries = Vec::new();
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            let Some(name) = entry.file_name().to_str().map(CompactString::from) else {
                continue;
            };
            let ft = entry.file_type()?;
            entries.push(FsDirEntry {
                name,
                is_dir: ft.is_dir(),
                is_symlink: ft.is_symlink(),
            });
        }
        Ok(entries)
    }

    fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        std::fs::create_dir_all(path)
    }

    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        std::fs::canonicalize(path)
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        std::fs::remove_file(path)
    }

    fn remove_dir_all(&self, path: &Path) -> io::Result<()> {
        std::fs::remove_dir_all(path)
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        std::fs::rename(from, to)
    }

    fn walk_workspace_files(&self, root: &Path) -> Vec<PathBuf> {
        let defaults = build_default_ignore(root);
        let walker = WalkBuilder::new(root)
            .hidden(false)
            .require_git(false)
            .add_custom_ignore_filename(".stoatignore")
            .filter_entry(move |entry| {
                let is_dir = entry.file_type().is_some_and(|t| t.is_dir());
                !defaults.matched(entry.path(), is_dir).is_ignore()
            })
            .build();

        let mut out = Vec::new();
        for entry in walker.flatten() {
            if entry.file_type().is_some_and(|t| t.is_file()) {
                out.push(entry.into_path());
            }
        }
        out.sort();
        out
    }

    fn walk_workspace_files_streaming(&self, root: &Path, on_batch: &mut dyn FnMut(Vec<PathBuf>)) {
        let defaults = build_default_ignore(root);
        let walker = WalkBuilder::new(root)
            .hidden(false)
            .require_git(false)
            .add_custom_ignore_filename(".stoatignore")
            .filter_entry(move |entry| {
                let is_dir = entry.file_type().is_some_and(|t| t.is_dir());
                !defaults.matched(entry.path(), is_dir).is_ignore()
            })
            .build();

        let mut buffer: Vec<PathBuf> = Vec::with_capacity(WALK_BATCH_SIZE);
        for entry in walker.flatten() {
            if entry.file_type().is_some_and(|t| t.is_file()) {
                buffer.push(entry.into_path());
                if buffer.len() >= WALK_BATCH_SIZE {
                    let batch = std::mem::replace(&mut buffer, Vec::with_capacity(WALK_BATCH_SIZE));
                    on_batch(batch);
                }
            }
        }
        if !buffer.is_empty() {
            on_batch(buffer);
        }
    }
}
