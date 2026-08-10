use compact_str::CompactString;
use ignore::{
    gitignore::{Gitignore, GitignoreBuilder},
    Match, WalkBuilder,
};
use std::{
    io,
    io::{Read, Write},
    ops::ControlFlow,
    path::{Path, PathBuf},
    time::SystemTime,
};
use tempfile::NamedTempFile;

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

    /// Clears `buf` and fills it with up to `limit` bytes from the start of the
    /// file at `path`. A file shorter than `limit` yields all of it.
    ///
    /// The default reads the whole file via [`Self::read`] and truncates. A host
    /// backed by a real filesystem should override this to stop reading once
    /// `limit` bytes are in hand, so previewing a huge file never pulls it fully
    /// into memory.
    fn read_prefix(&self, path: &Path, limit: usize, buf: &mut Vec<u8>) -> io::Result<()> {
        self.read(path, buf)?;
        buf.truncate(limit);
        Ok(())
    }

    /// Writes `data` to `path`, creating or truncating the file.
    fn write(&self, path: &Path, data: &[u8]) -> io::Result<()>;

    /// Atomically replaces `path` with `data`.
    ///
    /// On success `path` holds exactly `data`. On failure the file at
    /// `path` is left intact, never truncated or partially written.
    /// Preserves the existing file's permissions. When `path` is a
    /// symlink, the link's target is rewritten and the link itself is
    /// left in place rather than replaced by a regular file.
    ///
    /// The default delegates to [`Self::write`], which is not
    /// crash-atomic. Implementations backed by a real filesystem
    /// override it with a temp-file-plus-rename sequence.
    fn write_atomic(&self, path: &Path, data: &[u8]) -> io::Result<()> {
        self.write(path, data)
    }

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
    ///
    /// Returning [`ControlFlow::Break`] from `on_batch` stops the walk before
    /// the next batch, so a consumer whose receiver has gone can abandon a
    /// long scan promptly instead of enumerating the rest of the tree.
    fn walk_workspace_files_streaming(
        &self,
        root: &Path,
        on_batch: &mut dyn FnMut(Vec<PathBuf>) -> ControlFlow<()>,
    ) {
        let paths = self.walk_workspace_files(root);
        if !paths.is_empty() {
            let _ = on_batch(paths);
        }
    }

    /// Parallel counterpart to [`Self::walk_workspace_files_streaming`], which
    /// calls `on_batch` from several threads at once.
    ///
    /// The callback is shared rather than owned, which is what forces it to be
    /// `Fn + Sync` where the streaming one is `FnMut`. This exists for a caller
    /// doing real work per batch rather than collecting paths to work on
    /// later. The work lands on the walker's threads instead of queueing
    /// behind one consumer.
    ///
    /// Ordering is weaker than the streaming walk's, which is already
    /// unsorted. Batches from different threads interleave arbitrarily.
    ///
    /// Returning [`ControlFlow::Break`] stops the walk as before, though
    /// threads already inside a batch finish it.
    ///
    /// The default impl runs the streaming walk on the calling thread, so a
    /// host without a parallel walker still satisfies the contract.
    fn walk_workspace_files_parallel(
        &self,
        root: &Path,
        on_batch: &(dyn Fn(Vec<PathBuf>) -> ControlFlow<()> + Sync),
    ) {
        self.walk_workspace_files_streaming(root, &mut |batch| on_batch(batch));
    }
}

/// One parallel-walk visitor's in-flight batch.
///
/// Exists for its [`Drop`], which is where a thread's last partial batch gets
/// delivered. Without it those paths would be dropped with the visitor, and a
/// caller would silently miss up to a batch of files per walker thread.
struct BatchSink<'a> {
    buffer: Vec<PathBuf>,
    on_batch: &'a (dyn Fn(Vec<PathBuf>) -> ControlFlow<()> + Sync),
}

impl Drop for BatchSink<'_> {
    fn drop(&mut self) {
        if !self.buffer.is_empty() {
            let _ = (self.on_batch)(std::mem::take(&mut self.buffer));
        }
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
pub fn manual_walk_streaming(
    fs: &dyn FsHost,
    root: &Path,
    on_batch: &mut dyn FnMut(Vec<PathBuf>) -> ControlFlow<()>,
) {
    let defaults = build_default_ignore(root);
    let mut stack: Vec<Gitignore> = Vec::new();
    let mut buffer: Vec<PathBuf> = Vec::with_capacity(WALK_BATCH_SIZE);
    let flow = walk_dir_streaming(fs, root, &defaults, &mut stack, &mut buffer, on_batch);
    if flow.is_continue() && !buffer.is_empty() {
        let _ = on_batch(buffer);
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
    on_batch: &mut dyn FnMut(Vec<PathBuf>) -> ControlFlow<()>,
) -> ControlFlow<()> {
    let pushed = push_dir_ignores(fs, dir, stack);

    let entries = match fs.list_dir(dir) {
        Ok(e) => e,
        Err(_) => {
            stack.truncate(stack.len() - pushed);
            return ControlFlow::Continue(());
        },
    };

    let mut flow = ControlFlow::Continue(());
    for entry in entries {
        let path = dir.join(entry.name.as_str());
        if path_is_ignored(defaults, stack, &path, entry.is_dir) {
            continue;
        }
        if entry.is_dir {
            flow = walk_dir_streaming(fs, &path, defaults, stack, buffer, on_batch);
        } else {
            buffer.push(path);
            if buffer.len() >= WALK_BATCH_SIZE {
                let batch = std::mem::replace(buffer, Vec::with_capacity(WALK_BATCH_SIZE));
                flow = on_batch(batch);
            }
        }
        if flow.is_break() {
            break;
        }
    }

    stack.truncate(stack.len() - pushed);
    flow
}

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

    fn read_prefix(&self, path: &Path, limit: usize, buf: &mut Vec<u8>) -> io::Result<()> {
        buf.clear();
        std::fs::File::open(path)?
            .take(limit as u64)
            .read_to_end(buf)?;
        Ok(())
    }

    fn write(&self, path: &Path, data: &[u8]) -> io::Result<()> {
        std::fs::write(path, data)
    }

    fn write_atomic(&self, path: &Path, data: &[u8]) -> io::Result<()> {
        let dest = resolve_write_target(path);

        // Replacing the inode would leave every other name for this file
        // pointing at the old content, with nothing to show that it happened.
        // Keeping the inode costs the atomic replace for this one file, which
        // is the lesser harm.
        if has_siblings(&dest) {
            let mut file = std::fs::File::create(&dest)?;
            file.write_all(data)?;
            file.sync_all()?;
            return Ok(());
        }

        let dir = dest.parent().unwrap_or_else(|| Path::new("."));
        let mut tmp = NamedTempFile::new_in(dir)?;
        tmp.write_all(data)?;
        tmp.as_file().sync_all()?;

        match std::fs::metadata(&dest) {
            // Ownership first. An unprivileged chown clears the setuid and
            // setgid bits, so copying the mode afterwards is what puts them
            // back on a file that carried them.
            Ok(meta) => {
                copy_ownership(&meta, tmp.path());
                std::fs::set_permissions(tmp.path(), meta.permissions())?;
            },
            // A temp file is created private so its half-written contents are
            // not readable, which is not the mode a file the user just created
            // should keep.
            Err(_) => set_created_permissions(tmp.path())?,
        }
        tmp.persist(&dest).map_err(|e| e.error)?;
        Ok(())
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

    fn walk_workspace_files_streaming(
        &self,
        root: &Path,
        on_batch: &mut dyn FnMut(Vec<PathBuf>) -> ControlFlow<()>,
    ) {
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
                    if on_batch(batch).is_break() {
                        return;
                    }
                }
            }
        }
        if !buffer.is_empty() {
            let _ = on_batch(buffer);
        }
    }

    fn walk_workspace_files_parallel(
        &self,
        root: &Path,
        on_batch: &(dyn Fn(Vec<PathBuf>) -> ControlFlow<()> + Sync),
    ) {
        let defaults = build_default_ignore(root);
        let walker = WalkBuilder::new(root)
            .hidden(false)
            .require_git(false)
            .add_custom_ignore_filename(".stoatignore")
            .filter_entry(move |entry| {
                let is_dir = entry.file_type().is_some_and(|t| t.is_dir());
                !defaults.matched(entry.path(), is_dir).is_ignore()
            })
            .build_parallel();

        walker.run(|| {
            // Each visitor owns its buffer, so batches never cross threads and
            // the walker's threads never contend for one. The sink flushes what
            // is left when the visitor ends, which is the only thing that gets
            // a thread's final partial batch scanned.
            let mut sink = BatchSink {
                buffer: Vec::with_capacity(WALK_BATCH_SIZE),
                on_batch,
            };
            Box::new(move |entry| {
                let Ok(entry) = entry else {
                    return ignore::WalkState::Continue;
                };
                if !entry.file_type().is_some_and(|t| t.is_file()) {
                    return ignore::WalkState::Continue;
                }

                sink.buffer.push(entry.into_path());
                if sink.buffer.len() < WALK_BATCH_SIZE {
                    return ignore::WalkState::Continue;
                }

                let batch =
                    std::mem::replace(&mut sink.buffer, Vec::with_capacity(WALK_BATCH_SIZE));
                match on_batch(batch) {
                    // Quit rather than Skip, since a caller that has stopped
                    // reading wants the whole walk abandoned, not this
                    // directory alone.
                    ControlFlow::Break(()) => ignore::WalkState::Quit,
                    ControlFlow::Continue(()) => ignore::WalkState::Continue,
                }
            })
        });
    }
}

/// Symlink hops [`resolve_write_target`] follows before giving up.
///
/// A cycle of links would otherwise never resolve. The bound only has to exceed
/// what a real path uses, which is one or two hops.
const MAX_SYMLINK_HOPS: usize = 16;

/// The file a write aimed at `path` should actually land on.
///
/// Follows symlinks to the end of the chain. The last hop is returned even when
/// nothing is there, because a link pointing at a file that does not exist yet
/// still names where that file belongs. Resolving the same thing by
/// canonicalizing would refuse, having nothing to canonicalize against.
///
/// A relative target is read against the directory holding the link that names
/// it, not the process's working directory.
fn resolve_write_target(path: &Path) -> PathBuf {
    let mut current = path.to_path_buf();
    for _ in 0..MAX_SYMLINK_HOPS {
        let Ok(target) = std::fs::read_link(&current) else {
            return current;
        };
        current = match target.is_relative() {
            true => current.parent().unwrap_or(Path::new(".")).join(target),
            false => target,
        };
    }
    current
}

/// Whether `dest` is reachable under more than one name.
///
/// Metadata that cannot be read counts as more than one, since detaching links
/// that turn out to exist is worse than losing atomicity on a file that has
/// none. A destination that does not exist is not that case: there is no inode
/// yet, so nothing can be pointing at it.
#[cfg(unix)]
fn has_siblings(dest: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;

    match std::fs::metadata(dest) {
        Ok(meta) => meta.nlink() > 1,
        Err(e) if e.kind() == io::ErrorKind::NotFound => false,
        Err(_) => true,
    }
}

#[cfg(not(unix))]
fn has_siblings(_dest: &Path) -> bool {
    false
}

/// Give `tmp` the permissions a file the user has just created should carry.
///
/// A newly created regular file is granted read and write to everyone, minus
/// whatever the umask withholds. That is what the kernel would have done had
/// the file been opened directly rather than assembled under a temporary name.
#[cfg(unix)]
fn set_created_permissions(tmp: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mode = 0o666 & !process_umask();
    std::fs::set_permissions(tmp, std::fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn set_created_permissions(_tmp: &Path) -> io::Result<()> {
    Ok(())
}

/// The permission bits this process withholds from files it creates.
///
/// There is no call that reads a umask, only one that replaces it and hands
/// back what was there, so the value is recovered by setting it and putting it
/// straight back. That leaves a window where the process umask is the probe
/// value. It is read once and nothing else here sets a umask, so the window is
/// accepted rather than worked around.
#[cfg(unix)]
fn process_umask() -> u32 {
    static UMASK: std::sync::OnceLock<u32> = std::sync::OnceLock::new();

    *UMASK.get_or_init(|| unsafe {
        // SAFETY: umask takes and returns a mode by value and touches no
        // memory. It cannot fail.
        let previous = libc::umask(0o022);
        libc::umask(previous);
        previous as u32
    })
}

/// Give `tmp` the owner and group recorded in `meta`.
///
/// Failure is ignored, leaving the file owned by whoever is running the editor.
/// Handing a file to another user takes a privilege the editor usually lacks,
/// and refusing the save over it would be worse than saving it under the wrong
/// name.
///
/// The owner and group go separately because one chown carrying both is
/// all-or-nothing. Bundled, an unprivileged process that cannot set the owner
/// would lose the group with it, which is the part it could have kept.
#[cfg(unix)]
fn copy_ownership(meta: &std::fs::Metadata, tmp: &Path) {
    use std::os::unix::fs::MetadataExt;

    let _ = std::os::unix::fs::chown(tmp, None, Some(meta.gid()));
    let _ = std::os::unix::fs::chown(tmp, Some(meta.uid()), None);
}

#[cfg(not(unix))]
fn copy_ownership(_meta: &std::fs::Metadata, _tmp: &Path) {}

#[cfg(all(test, unix))]
mod tests {
    use super::{FsHost, LocalFs};
    use std::{
        fs,
        os::unix::fs::{MetadataExt, PermissionsExt},
        path::Path,
    };
    use tempfile::TempDir;

    fn read(path: &Path) -> Vec<u8> {
        fs::read(path).expect("readable")
    }

    #[test]
    fn a_plain_file_is_replaced() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("a.txt");
        fs::write(&path, b"old").expect("seed");

        LocalFs.write_atomic(&path, b"new").expect("write");

        assert_eq!(read(&path), b"new");
    }

    #[test]
    fn a_new_file_is_created() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("fresh.txt");

        LocalFs.write_atomic(&path, b"new").expect("write");

        assert_eq!(read(&path), b"new");
    }

    /// A group the running user belongs to that is not `current`, or `None`
    /// when they belong to no other. Moving a file to such a group is what
    /// puts something behind the claim that its group survived a save.
    fn other_group(current: u32) -> Option<u32> {
        let mut buf = [0u32; 64];
        // SAFETY: getgroups writes at most the count it is given, which is the
        // buffer's own length, and reports how many it wrote.
        let found = unsafe { libc::getgroups(buf.len() as i32, buf.as_mut_ptr()) };
        buf.iter()
            .take(found.max(0) as usize)
            .copied()
            .find(|gid| *gid != current)
    }

    #[test]
    fn a_new_file_lands_with_umask_permissions() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("fresh.txt");

        LocalFs.write_atomic(&path, b"new").expect("write");

        let umask = unsafe {
            let previous = libc::umask(0o022);
            libc::umask(previous);
            previous
        };
        assert_eq!(
            fs::metadata(&path).expect("meta").mode() & 0o777,
            0o666 & !(umask as u32),
            "a created file gets what the umask allows, not the temp file's mode"
        );
    }

    #[test]
    fn a_replaced_file_keeps_its_mode() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("a.txt");
        fs::write(&path, b"old").expect("seed");
        fs::set_permissions(&path, PermissionsExt::from_mode(0o640)).expect("chmod");

        LocalFs.write_atomic(&path, b"new").expect("write");

        assert_eq!(fs::metadata(&path).expect("meta").mode() & 0o777, 0o640);
    }

    #[test]
    fn a_replaced_file_keeps_its_setgid_bit() {
        // An unprivileged chown that moves a group-executable file to another
        // group clears setgid, so this only holds while the ownership is copied
        // before the mode rather than after. Both halves of that are needed:
        // the group has to actually change, and the file has to be executable
        // by it, or the kernel leaves the bit alone regardless of order.
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("a.sh");
        fs::write(&path, b"old").expect("seed");

        let seeded = fs::metadata(&path).expect("meta").gid();
        if let Some(gid) = other_group(seeded) {
            std::os::unix::fs::chown(&path, None, Some(gid)).expect("chgrp");
        }
        fs::set_permissions(&path, PermissionsExt::from_mode(0o2750)).expect("chmod");

        LocalFs.write_atomic(&path, b"new").expect("write");

        assert_eq!(fs::metadata(&path).expect("meta").mode() & 0o7777, 0o2750);
    }

    #[test]
    fn a_replaced_file_keeps_its_group() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("a.txt");
        fs::write(&path, b"old").expect("seed");

        let seeded = fs::metadata(&path).expect("meta").gid();
        let expected = match other_group(seeded) {
            Some(gid) => {
                std::os::unix::fs::chown(&path, None, Some(gid)).expect("chgrp");
                gid
            },
            None => seeded,
        };

        LocalFs.write_atomic(&path, b"new").expect("write");

        assert_eq!(fs::metadata(&path).expect("meta").gid(), expected);
    }

    #[test]
    fn a_hardlinked_file_keeps_its_siblings_in_step() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("a.txt");
        let sibling = dir.path().join("b.txt");
        fs::write(&path, b"old").expect("seed");
        fs::hard_link(&path, &sibling).expect("link");

        LocalFs.write_atomic(&path, b"new").expect("write");

        assert_eq!(read(&path), b"new");
        assert_eq!(read(&sibling), b"new", "the sibling link must follow");
        assert_eq!(
            fs::metadata(&path).expect("meta").ino(),
            fs::metadata(&sibling).expect("meta").ino(),
            "both names must still be the same file"
        );
    }

    #[test]
    fn a_dangling_symlink_gets_its_target_created() {
        let dir = TempDir::new().expect("tempdir");
        let target = dir.path().join("target.txt");
        let link = dir.path().join("link.txt");
        std::os::unix::fs::symlink(&target, &link).expect("symlink");

        LocalFs.write_atomic(&link, b"new").expect("write");

        assert_eq!(read(&target), b"new");
        assert!(
            fs::symlink_metadata(&link)
                .expect("meta")
                .file_type()
                .is_symlink(),
            "the link itself must survive"
        );
    }

    #[test]
    fn a_relative_dangling_symlink_resolves_against_its_own_parent() {
        let dir = TempDir::new().expect("tempdir");
        let nested = dir.path().join("nested");
        fs::create_dir(&nested).expect("mkdir");
        let link = nested.join("link.txt");
        std::os::unix::fs::symlink(Path::new("target.txt"), &link).expect("symlink");

        LocalFs.write_atomic(&link, b"new").expect("write");

        assert_eq!(read(&nested.join("target.txt")), b"new");
    }

    #[test]
    fn a_live_symlink_writes_through_to_its_target() {
        let dir = TempDir::new().expect("tempdir");
        let target = dir.path().join("target.txt");
        let link = dir.path().join("link.txt");
        fs::write(&target, b"old").expect("seed");
        std::os::unix::fs::symlink(&target, &link).expect("symlink");

        LocalFs.write_atomic(&link, b"new").expect("write");

        assert_eq!(read(&target), b"new");
        assert!(
            fs::symlink_metadata(&link)
                .expect("meta")
                .file_type()
                .is_symlink(),
            "the link must not be replaced by a regular file"
        );
    }

    #[test]
    fn a_chain_of_symlinks_reaches_the_file_at_its_end() {
        let dir = TempDir::new().expect("tempdir");
        let target = dir.path().join("target.txt");
        let middle = dir.path().join("middle.txt");
        let outer = dir.path().join("outer.txt");
        fs::write(&target, b"old").expect("seed");
        std::os::unix::fs::symlink(&target, &middle).expect("symlink");
        std::os::unix::fs::symlink(&middle, &outer).expect("symlink");

        LocalFs.write_atomic(&outer, b"new").expect("write");

        assert_eq!(read(&target), b"new");
        assert!(
            fs::symlink_metadata(&middle)
                .expect("meta")
                .file_type()
                .is_symlink(),
            "the intermediate link must not be clobbered"
        );
    }
}
