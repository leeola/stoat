use crate::fs::{FsDirEntry, FsHost, FsMetadata};
use compact_str::CompactString;
use std::{
    collections::{BTreeMap, HashMap},
    io,
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, SystemTime},
};

enum FakeEntry {
    File { content: Vec<u8>, mtime: SystemTime },
    Dir { mtime: SystemTime },
    Symlink { target: PathBuf, mtime: SystemTime },
}

/// Recorded against the input path, before symlink resolution and
/// before error-injection checks, so the log captures every method
/// invocation -- including ones that the fake fails on purpose.
/// Write payloads are summarised as a byte length to keep the log
/// cheap; tests that need exact bytes should read the final state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FakeFsOp {
    Read { path: PathBuf },
    Write { path: PathBuf, len: usize },
    Metadata { path: PathBuf },
    ListDir { path: PathBuf },
    CreateDirAll { path: PathBuf },
    Canonicalize { path: PathBuf },
    RemoveFile { path: PathBuf },
    Rename { from: PathBuf, to: PathBuf },
}

/// Linux's `MAXSYMLINKS` limit. `ErrorKind::FilesystemLoop` would be
/// the natural kind to return after exceeding this, but it remains
/// unstable (rust-lang/rust#86442), so callers see [`io::ErrorKind::Other`]
/// with a `"too many symlinks"` message.
const MAX_SYMLINK_HOPS: usize = 40;

/// Walks the symlink chain starting at `path`. Returns the first
/// non-symlink path encountered, or `path` itself if the entry is
/// missing or already non-symlink. Relative targets resolve against
/// the symlink's parent directory. Errors with
/// `ErrorKind::Other` after [`MAX_SYMLINK_HOPS`] hops.
fn resolve_symlink(state: &FakeState, path: &Path) -> io::Result<PathBuf> {
    let mut current = path.to_path_buf();
    for _ in 0..MAX_SYMLINK_HOPS {
        match state.entries.get(&current) {
            Some(FakeEntry::Symlink { target, .. }) => {
                current = if target.is_absolute() {
                    target.clone()
                } else {
                    current.parent().unwrap_or(Path::new("/")).join(target)
                };
            },
            _ => return Ok(current),
        }
    }
    Err(io::Error::other(format!(
        "{}: too many symlinks",
        path.display()
    )))
}

/// Notification callback fired after a successful [`FsHost::write`]
/// against a [`FakeFs`]. Tests pair a [`FakeFs`] with a
/// [`crate::FakeFsWatcher`] by registering a hook that injects a
/// `Modified` event for watched paths; see
/// [`crate::FakeFsWatcher::install_on`].
pub type WriteHook = Box<dyn Fn(&Path) + Send + Sync>;

pub struct FakeFs {
    state: Mutex<FakeState>,
    write_hook: Mutex<Option<WriteHook>>,
}

struct FakeState {
    entries: BTreeMap<PathBuf, FakeEntry>,
    clock: u64,
    /// One-shot read failures keyed by input path. Drained on the next
    /// matching `read` call, irrespective of symlink resolution.
    read_failures: HashMap<PathBuf, io::ErrorKind>,
    /// One-shot mid-read partial failures keyed by input path. The
    /// usize is the number of bytes copied into the caller's buffer
    /// before the injected error fires; capped at the file's length.
    read_partial_failures: HashMap<PathBuf, (usize, io::ErrorKind)>,
    /// Sticky write failures keyed by input path. Returned from every
    /// matching `write` call until the entry is cleared; this commit
    /// has no removal API, matching the short-lived test fakes.
    write_failures: HashMap<PathBuf, io::ErrorKind>,
    /// One-shot metadata failures keyed by input path. Drained on the
    /// next matching `metadata` call, before symlink resolution.
    metadata_failures: HashMap<PathBuf, io::ErrorKind>,
    /// One-shot list_dir failures keyed by input path. Drained on the
    /// next matching `list_dir` call, before symlink resolution.
    list_dir_failures: HashMap<PathBuf, io::ErrorKind>,
    /// One-shot canonicalize failures keyed by input path. Drained on
    /// the next matching `canonicalize` call, before symlink
    /// resolution.
    canonicalize_failures: HashMap<PathBuf, io::ErrorKind>,
    /// One-shot create_dir_all failures keyed by input path. Drained
    /// on the next matching `create_dir_all` call, before symlink
    /// resolution.
    create_dir_all_failures: HashMap<PathBuf, io::ErrorKind>,
    /// One-shot remove_file failures keyed by input path. Drained on
    /// the next matching `remove_file` call, before symlink
    /// resolution.
    remove_file_failures: HashMap<PathBuf, io::ErrorKind>,
    /// One-shot rename failures keyed by the source path. Drained on
    /// the next matching `rename` call, before any state mutation;
    /// the destination path is not part of the match.
    rename_failures: HashMap<PathBuf, io::ErrorKind>,
    ops: Vec<FakeFsOp>,
}

impl FakeState {
    fn tick(&mut self) -> SystemTime {
        self.clock += 1;
        SystemTime::UNIX_EPOCH + Duration::from_secs(self.clock)
    }

    fn ensure_ancestors(&mut self, path: &Path) {
        let mut ancestor = path.parent();
        while let Some(p) = ancestor {
            if p == Path::new("/") || p == Path::new("") {
                break;
            }
            if !self.entries.contains_key(p) {
                let mtime = self.tick();
                self.entries
                    .insert(p.to_path_buf(), FakeEntry::Dir { mtime });
            }
            ancestor = p.parent();
        }
    }
}

impl Default for FakeFs {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeFs {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(FakeState {
                entries: BTreeMap::new(),
                clock: 0,
                read_failures: HashMap::new(),
                read_partial_failures: HashMap::new(),
                write_failures: HashMap::new(),
                metadata_failures: HashMap::new(),
                list_dir_failures: HashMap::new(),
                canonicalize_failures: HashMap::new(),
                create_dir_all_failures: HashMap::new(),
                remove_file_failures: HashMap::new(),
                rename_failures: HashMap::new(),
                ops: Vec::new(),
            }),
            write_hook: Mutex::new(None),
        }
    }

    /// Install a callback fired after every successful
    /// [`FsHost::write`] against this fake. The hook runs after the
    /// entry mutation but before the [`FsHost::write`] return; sticky
    /// write failures skip both the mutation and the hook. Replaces
    /// any previously-installed hook. Decouples [`FakeFs`] from
    /// [`crate::FakeFsWatcher`] so the `fs` module does not depend on
    /// the watcher type.
    pub fn set_write_hook(&self, hook: WriteHook) {
        *self.write_hook.lock().expect("FakeFs lock poisoned") = Some(hook);
    }

    pub fn insert_file(&self, path: impl AsRef<Path>, content: impl AsRef<[u8]>) {
        let mut state = self.state.lock().expect("FakeFs lock poisoned");
        let path = path.as_ref();
        state.ensure_ancestors(path);
        let mtime = state.tick();
        state.entries.insert(
            path.to_path_buf(),
            FakeEntry::File {
                content: content.as_ref().to_vec(),
                mtime,
            },
        );
    }

    /// Bulk variant of [`Self::insert_file`]. Iteration order is preserved
    /// so timestamps match the input order, which matters for the few
    /// tests that compare modification times.
    pub fn insert_files<P, C, I>(&self, files: I)
    where
        P: AsRef<Path>,
        C: AsRef<[u8]>,
        I: IntoIterator<Item = (P, C)>,
    {
        for (path, content) in files {
            self.insert_file(path, content);
        }
    }

    pub fn insert_dir(&self, path: impl AsRef<Path>) {
        let mut state = self.state.lock().expect("FakeFs lock poisoned");
        let path = path.as_ref();
        state.ensure_ancestors(path);
        let mtime = state.tick();
        state
            .entries
            .insert(path.to_path_buf(), FakeEntry::Dir { mtime });
    }

    /// Insert a symlink at `path` pointing at `target`. The target may
    /// be absolute or relative; relative targets resolve against
    /// `path`'s parent during operations that follow symlinks. Targets
    /// are stored verbatim and may dangle.
    pub fn insert_symlink(&self, path: impl AsRef<Path>, target: impl AsRef<Path>) {
        let mut state = self.state.lock().expect("FakeFs lock poisoned");
        let path = path.as_ref();
        state.ensure_ancestors(path);
        let mtime = state.tick();
        state.entries.insert(
            path.to_path_buf(),
            FakeEntry::Symlink {
                target: target.as_ref().to_path_buf(),
                mtime,
            },
        );
    }

    /// Arm a one-shot read failure for `path`. The next [`FsHost::read`]
    /// call whose input path equals `path` returns
    /// `io::Error::new(kind, ...)` and clears the arm; subsequent reads
    /// behave normally. Matched before symlink resolution, so callers
    /// inject for the path they invoke with.
    pub fn fail_next_read(&self, path: impl AsRef<Path>, kind: io::ErrorKind) {
        let mut state = self.state.lock().expect("FakeFs lock poisoned");
        state
            .read_failures
            .insert(path.as_ref().to_path_buf(), kind);
    }

    /// Arm a one-shot mid-read partial failure for `path`. The next
    /// [`FsHost::read`] call whose input path equals `path` copies up
    /// to `n_bytes` from the resolved file's content into the
    /// caller's buffer (capped at the file's length) and then returns
    /// `io::Error::new(kind, ...)`. The arm clears after firing.
    /// `fail_next_read` takes precedence when both are armed for the
    /// same path. If the path resolves to a missing or non-file
    /// entry, the buffer is left empty and the injected error still
    /// fires.
    pub fn fail_next_read_after(
        &self,
        path: impl AsRef<Path>,
        n_bytes: usize,
        kind: io::ErrorKind,
    ) {
        let mut state = self.state.lock().expect("FakeFs lock poisoned");
        state
            .read_partial_failures
            .insert(path.as_ref().to_path_buf(), (n_bytes, kind));
    }

    /// Arm a sticky write failure for `path`. Every [`FsHost::write`]
    /// call whose input path equals `path` returns
    /// `io::Error::new(kind, ...)` until the entry is cleared. Matched
    /// before symlink resolution.
    pub fn fail_writes_to(&self, path: impl AsRef<Path>, kind: io::ErrorKind) {
        let mut state = self.state.lock().expect("FakeFs lock poisoned");
        state
            .write_failures
            .insert(path.as_ref().to_path_buf(), kind);
    }

    /// Arm a one-shot metadata failure for `path`. The next
    /// [`FsHost::metadata`] call whose input path equals `path` returns
    /// `io::Error::new(kind, ...)` and clears the arm; subsequent calls
    /// behave normally. Matched before symlink resolution, so callers
    /// inject for the path they invoke with.
    pub fn fail_next_metadata(&self, path: impl AsRef<Path>, kind: io::ErrorKind) {
        let mut state = self.state.lock().expect("FakeFs lock poisoned");
        state
            .metadata_failures
            .insert(path.as_ref().to_path_buf(), kind);
    }

    /// Arm a one-shot list_dir failure for `path`. The next
    /// [`FsHost::list_dir`] call whose input path equals `path` returns
    /// `io::Error::new(kind, ...)` and clears the arm; subsequent calls
    /// behave normally. Matched before symlink resolution, so callers
    /// inject for the path they invoke with.
    pub fn fail_next_list_dir(&self, path: impl AsRef<Path>, kind: io::ErrorKind) {
        let mut state = self.state.lock().expect("FakeFs lock poisoned");
        state
            .list_dir_failures
            .insert(path.as_ref().to_path_buf(), kind);
    }

    /// Arm a one-shot canonicalize failure for `path`. The next
    /// [`FsHost::canonicalize`] call whose input path equals `path`
    /// returns `io::Error::new(kind, ...)` and clears the arm;
    /// subsequent calls behave normally. Matched before symlink
    /// resolution, so callers inject for the path they invoke with.
    pub fn fail_next_canonicalize(&self, path: impl AsRef<Path>, kind: io::ErrorKind) {
        let mut state = self.state.lock().expect("FakeFs lock poisoned");
        state
            .canonicalize_failures
            .insert(path.as_ref().to_path_buf(), kind);
    }

    /// Arm a one-shot create_dir_all failure for `path`. The next
    /// [`FsHost::create_dir_all`] call whose input path equals `path`
    /// returns `io::Error::new(kind, ...)` and clears the arm;
    /// subsequent calls behave normally. Matched before symlink
    /// resolution, so callers inject for the path they invoke with.
    pub fn fail_next_create_dir_all(&self, path: impl AsRef<Path>, kind: io::ErrorKind) {
        let mut state = self.state.lock().expect("FakeFs lock poisoned");
        state
            .create_dir_all_failures
            .insert(path.as_ref().to_path_buf(), kind);
    }

    /// Arm a one-shot remove_file failure for `path`. The next
    /// [`FsHost::remove_file`] call whose input path equals `path`
    /// returns `io::Error::new(kind, ...)` and clears the arm;
    /// subsequent calls behave normally. Matched before symlink
    /// resolution, so callers inject for the path they invoke with.
    pub fn fail_next_remove_file(&self, path: impl AsRef<Path>, kind: io::ErrorKind) {
        let mut state = self.state.lock().expect("FakeFs lock poisoned");
        state
            .remove_file_failures
            .insert(path.as_ref().to_path_buf(), kind);
    }

    /// Arm a one-shot rename failure keyed by the source path. The
    /// next [`FsHost::rename`] call whose `from` equals `from`
    /// returns `io::Error::new(kind, ...)` and clears the arm; the
    /// destination is not part of the match. Source state is left
    /// intact since the failure fires before any mutation.
    pub fn fail_next_rename(&self, from: impl AsRef<Path>, kind: io::ErrorKind) {
        let mut state = self.state.lock().expect("FakeFs lock poisoned");
        state
            .rename_failures
            .insert(from.as_ref().to_path_buf(), kind);
    }

    /// Snapshot of every [`FsHost`] method invoked on this fake, in
    /// call order. Recorded against the input path before symlink
    /// resolution and before error-injection checks, so failed and
    /// successful calls both appear. Returns a clone so callers do
    /// not hold the internal mutex.
    pub fn ops(&self) -> Vec<FakeFsOp> {
        self.state.lock().expect("FakeFs lock poisoned").ops.clone()
    }
}

impl FsHost for FakeFs {
    fn read(&self, path: &Path, buf: &mut Vec<u8>) -> io::Result<()> {
        let mut state = self.state.lock().expect("FakeFs lock poisoned");
        state.ops.push(FakeFsOp::Read {
            path: path.to_path_buf(),
        });
        if let Some(kind) = state.read_failures.remove(path) {
            return Err(io::Error::new(
                kind,
                format!("{}: injected read failure", path.display()),
            ));
        }
        if let Some((n_bytes, kind)) = state.read_partial_failures.remove(path) {
            buf.clear();
            let resolved = resolve_symlink(&state, path)?;
            if let Some(FakeEntry::File { content, .. }) = state.entries.get(&resolved) {
                let cap = n_bytes.min(content.len());
                buf.extend_from_slice(&content[..cap]);
            }
            return Err(io::Error::new(
                kind,
                format!(
                    "{}: injected partial read failure after {} bytes",
                    path.display(),
                    n_bytes,
                ),
            ));
        }
        let resolved = resolve_symlink(&state, path)?;
        match state.entries.get(&resolved) {
            Some(FakeEntry::File { content, .. }) => {
                buf.clear();
                buf.extend_from_slice(content);
                Ok(())
            },
            Some(FakeEntry::Dir { .. }) => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "is a directory",
            )),
            Some(FakeEntry::Symlink { .. }) => unreachable!("resolve_symlink yields non-symlink"),
            None => Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("{}: not found", resolved.display()),
            )),
        }
    }

    fn write(&self, path: &Path, data: &[u8]) -> io::Result<()> {
        let mut state = self.state.lock().expect("FakeFs lock poisoned");
        state.ops.push(FakeFsOp::Write {
            path: path.to_path_buf(),
            len: data.len(),
        });
        if let Some(kind) = state.write_failures.get(path) {
            return Err(io::Error::new(
                *kind,
                format!("{}: injected write failure", path.display()),
            ));
        }
        let resolved = resolve_symlink(&state, path)?;
        state.ensure_ancestors(&resolved);
        let mtime = state.tick();
        state.entries.insert(
            resolved,
            FakeEntry::File {
                content: data.to_vec(),
                mtime,
            },
        );
        drop(state);
        if let Some(hook) = self
            .write_hook
            .lock()
            .expect("FakeFs lock poisoned")
            .as_ref()
        {
            hook(path);
        }
        Ok(())
    }

    fn metadata(&self, path: &Path) -> io::Result<Option<FsMetadata>> {
        let mut state = self.state.lock().expect("FakeFs lock poisoned");
        state.ops.push(FakeFsOp::Metadata {
            path: path.to_path_buf(),
        });
        if let Some(kind) = state.metadata_failures.remove(path) {
            return Err(io::Error::new(
                kind,
                format!("{}: injected metadata failure", path.display()),
            ));
        }
        Ok(state.entries.get(path).map(|entry| match entry {
            FakeEntry::File { content, mtime } => FsMetadata {
                len: content.len() as u64,
                modified: *mtime,
                is_dir: false,
                is_symlink: false,
            },
            FakeEntry::Dir { mtime } => FsMetadata {
                len: 0,
                modified: *mtime,
                is_dir: true,
                is_symlink: false,
            },
            FakeEntry::Symlink { target, mtime } => FsMetadata {
                len: target.as_os_str().len() as u64,
                modified: *mtime,
                is_dir: false,
                is_symlink: true,
            },
        }))
    }

    fn list_dir(&self, path: &Path) -> io::Result<Vec<FsDirEntry>> {
        let mut state = self.state.lock().expect("FakeFs lock poisoned");
        state.ops.push(FakeFsOp::ListDir {
            path: path.to_path_buf(),
        });
        if let Some(kind) = state.list_dir_failures.remove(path) {
            return Err(io::Error::new(
                kind,
                format!("{}: injected list_dir failure", path.display()),
            ));
        }
        let resolved = resolve_symlink(&state, path)?;
        match state.entries.get(&resolved) {
            Some(FakeEntry::Dir { .. }) => {},
            Some(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "not a directory",
                ))
            },
            None => {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("{}: not found", resolved.display()),
                ))
            },
        }

        let depth = resolved.components().count() + 1;
        let entries = state
            .entries
            .range(resolved.clone()..)
            .skip(1)
            .take_while(|(k, _)| k.starts_with(&resolved))
            .filter(|(k, _)| k.components().count() == depth)
            .map(|(k, entry)| {
                let name = k
                    .file_name()
                    .expect("entry must have a file name")
                    .to_string_lossy();
                FsDirEntry {
                    name: CompactString::from(name.as_ref()),
                    is_dir: matches!(entry, FakeEntry::Dir { .. }),
                    is_symlink: matches!(entry, FakeEntry::Symlink { .. }),
                }
            })
            .collect();

        Ok(entries)
    }

    fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        let mut state = self.state.lock().expect("FakeFs lock poisoned");
        state.ops.push(FakeFsOp::CreateDirAll {
            path: path.to_path_buf(),
        });
        if let Some(kind) = state.create_dir_all_failures.remove(path) {
            return Err(io::Error::new(
                kind,
                format!("{}: injected create_dir_all failure", path.display()),
            ));
        }
        let resolved = resolve_symlink(&state, path)?;
        state.ensure_ancestors(&resolved);
        if !state.entries.contains_key(&resolved) {
            let mtime = state.tick();
            state.entries.insert(resolved, FakeEntry::Dir { mtime });
        }
        Ok(())
    }

    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        let mut state = self.state.lock().expect("FakeFs lock poisoned");
        state.ops.push(FakeFsOp::Canonicalize {
            path: path.to_path_buf(),
        });
        if let Some(kind) = state.canonicalize_failures.remove(path) {
            return Err(io::Error::new(
                kind,
                format!("{}: injected canonicalize failure", path.display()),
            ));
        }
        let resolved = resolve_symlink(&state, path)?;
        if state.entries.contains_key(&resolved) {
            Ok(resolved)
        } else {
            Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("{}: not found", resolved.display()),
            ))
        }
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        let mut state = self.state.lock().expect("FakeFs lock poisoned");
        state.ops.push(FakeFsOp::RemoveFile {
            path: path.to_path_buf(),
        });
        if let Some(kind) = state.remove_file_failures.remove(path) {
            return Err(io::Error::new(
                kind,
                format!("{}: injected remove_file failure", path.display()),
            ));
        }
        match state.entries.get(path) {
            Some(FakeEntry::File { .. } | FakeEntry::Symlink { .. }) => {
                state.entries.remove(path);
                Ok(())
            },
            Some(FakeEntry::Dir { .. }) => Err(io::Error::new(
                io::ErrorKind::IsADirectory,
                "is a directory",
            )),
            None => Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("{}: not found", path.display()),
            )),
        }
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        let mut state = self.state.lock().expect("FakeFs lock poisoned");
        state.ops.push(FakeFsOp::Rename {
            from: from.to_path_buf(),
            to: to.to_path_buf(),
        });
        if let Some(kind) = state.rename_failures.remove(from) {
            return Err(io::Error::new(
                kind,
                format!(
                    "{} -> {}: injected rename failure",
                    from.display(),
                    to.display()
                ),
            ));
        }
        let Some(entry) = state.entries.remove(from) else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("{}: not found", from.display()),
            ));
        };
        state.ensure_ancestors(to);
        let new_mtime = state.tick();
        let entry = match entry {
            FakeEntry::File { content, .. } => FakeEntry::File {
                content,
                mtime: new_mtime,
            },
            FakeEntry::Dir { .. } => FakeEntry::Dir { mtime: new_mtime },
            FakeEntry::Symlink { target, .. } => FakeEntry::Symlink {
                target,
                mtime: new_mtime,
            },
        };
        state.entries.insert(to.to_path_buf(), entry);
        Ok(())
    }

    fn walk_workspace_files(&self, root: &Path) -> Vec<PathBuf> {
        crate::fs::manual_walk(self, root)
    }

    fn walk_workspace_files_streaming(&self, root: &Path, on_batch: &mut dyn FnMut(Vec<PathBuf>)) {
        crate::fs::manual_walk_streaming(self, root, on_batch);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_read_roundtrip() {
        let fs = FakeFs::new();
        fs.write(Path::new("/a/b.txt"), b"hello").unwrap();
        let mut buf = Vec::new();
        fs.read(Path::new("/a/b.txt"), &mut buf).unwrap();
        assert_eq!(buf, b"hello");
    }

    #[test]
    fn read_nonexistent() {
        let fs = FakeFs::new();
        let mut buf = Vec::new();
        let err = fs.read(Path::new("/nope"), &mut buf).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn write_auto_creates_parents() {
        let fs = FakeFs::new();
        fs.write(Path::new("/a/b/c/d.txt"), b"deep").unwrap();
        assert!(fs.exists(Path::new("/a")));
        assert!(fs.exists(Path::new("/a/b")));
        assert!(fs.exists(Path::new("/a/b/c")));
        assert!(fs.exists(Path::new("/a/b/c/d.txt")));
    }

    #[test]
    fn metadata_existing_file() {
        let fs = FakeFs::new();
        fs.insert_file("/x.txt", "abc");
        let m = fs.metadata(Path::new("/x.txt")).unwrap().unwrap();
        assert_eq!(m.len, 3);
        assert!(!m.is_dir);
        assert!(!m.is_symlink);
    }

    #[test]
    fn metadata_nonexistent() {
        let fs = FakeFs::new();
        assert!(fs.metadata(Path::new("/nope")).unwrap().is_none());
    }

    #[test]
    fn rename_moves_file() {
        let fs = FakeFs::new();
        fs.insert_file("/from.txt", "payload");
        fs.rename(Path::new("/from.txt"), Path::new("/to.txt"))
            .unwrap();
        assert!(!fs.exists(Path::new("/from.txt")));
        let mut buf = Vec::new();
        fs.read(Path::new("/to.txt"), &mut buf).unwrap();
        assert_eq!(buf, b"payload");
    }

    #[test]
    fn rename_missing_source_errors() {
        let fs = FakeFs::new();
        let err = fs
            .rename(Path::new("/nope"), Path::new("/somewhere"))
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn list_dir_entries() {
        let fs = FakeFs::new();
        fs.insert_dir("/root");
        fs.insert_file("/root/a.txt", "");
        fs.insert_dir("/root/sub");
        fs.insert_file("/root/sub/nested.txt", "");

        let entries = fs.list_dir(Path::new("/root")).unwrap();
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["a.txt", "sub"]);
        assert!(!entries[0].is_dir);
        assert!(entries[1].is_dir);
    }

    #[test]
    fn list_dir_nonexistent() {
        let fs = FakeFs::new();
        let err = fs.list_dir(Path::new("/nope")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn create_dir_all_creates_ancestors() {
        let fs = FakeFs::new();
        fs.create_dir_all(Path::new("/a/b/c")).unwrap();
        for p in ["/a", "/a/b", "/a/b/c"] {
            let m = fs.metadata(Path::new(p)).unwrap().unwrap();
            assert!(m.is_dir);
        }
    }

    #[test]
    fn exists_true_and_false() {
        let fs = FakeFs::new();
        fs.insert_file("/yes", "");
        assert!(fs.exists(Path::new("/yes")));
        assert!(!fs.exists(Path::new("/no")));
    }

    #[test]
    fn insert_symlink_metadata_reports_is_symlink() {
        let fs = FakeFs::new();
        fs.insert_file("/target.txt", "hello");
        fs.insert_symlink("/link", "/target.txt");
        let m = fs.metadata(Path::new("/link")).unwrap().unwrap();
        assert!(m.is_symlink);
        assert!(!m.is_dir);
    }

    #[test]
    fn read_through_symlink() {
        let fs = FakeFs::new();
        fs.insert_file("/target.txt", "hello");
        fs.insert_symlink("/link", "/target.txt");
        let mut buf = Vec::new();
        fs.read(Path::new("/link"), &mut buf).unwrap();
        assert_eq!(buf, b"hello");
    }

    #[test]
    fn canonicalize_follows_chain() {
        let fs = FakeFs::new();
        fs.insert_file("/target.txt", "");
        fs.insert_symlink("/a", "/target.txt");
        fs.insert_symlink("/b", "/a");
        fs.insert_symlink("/c", "/b");
        let canon = fs.canonicalize(Path::new("/c")).unwrap();
        assert_eq!(canon, Path::new("/target.txt"));
    }

    #[test]
    fn canonicalize_loop_returns_too_many_symlinks() {
        let fs = FakeFs::new();
        fs.insert_symlink("/a", "/b");
        fs.insert_symlink("/b", "/a");
        let err = fs.canonicalize(Path::new("/a")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Other);
        assert!(err.to_string().contains("too many symlinks"));
    }

    #[test]
    fn canonicalize_broken_returns_not_found() {
        let fs = FakeFs::new();
        fs.insert_symlink("/link", "/missing");
        let err = fs.canonicalize(Path::new("/link")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn list_dir_reports_symlink_flag() {
        let fs = FakeFs::new();
        fs.insert_dir("/root");
        fs.insert_file("/root/file", "");
        fs.insert_file("/target", "");
        fs.insert_symlink("/root/link", "/target");
        let entries = fs.list_dir(Path::new("/root")).unwrap();
        let summary: Vec<(&str, bool, bool)> = entries
            .iter()
            .map(|e| (e.name.as_str(), e.is_dir, e.is_symlink))
            .collect();
        assert_eq!(summary, [("file", false, false), ("link", false, true)]);
    }

    #[test]
    fn remove_file_on_symlink_removes_only_link() {
        let fs = FakeFs::new();
        fs.insert_file("/target", "keep me");
        fs.insert_symlink("/link", "/target");
        fs.remove_file(Path::new("/link")).unwrap();
        assert!(!fs.exists(Path::new("/link")));
        assert!(fs.exists(Path::new("/target")));
    }

    #[test]
    fn fail_next_read_fires_once() {
        let fs = FakeFs::new();
        fs.insert_file("/x.txt", "hi");
        fs.fail_next_read("/x.txt", io::ErrorKind::PermissionDenied);
        let mut buf = Vec::new();
        let err = fs.read(Path::new("/x.txt"), &mut buf).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        fs.read(Path::new("/x.txt"), &mut buf).unwrap();
        assert_eq!(buf, b"hi");
    }

    #[test]
    fn fail_next_read_distinct_paths() {
        let fs = FakeFs::new();
        fs.insert_file("/a", "aa");
        fs.insert_file("/b", "bb");
        fs.fail_next_read("/a", io::ErrorKind::PermissionDenied);
        let mut buf = Vec::new();
        fs.read(Path::new("/b"), &mut buf).unwrap();
        assert_eq!(buf, b"bb");
        let err = fs.read(Path::new("/a"), &mut buf).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn fail_next_read_after_yields_partial_then_errors() {
        let fs = FakeFs::new();
        fs.insert_file("/x.txt", "hello");
        fs.fail_next_read_after("/x.txt", 2, io::ErrorKind::Interrupted);
        let mut buf = Vec::new();
        let err = fs.read(Path::new("/x.txt"), &mut buf).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Interrupted);
        assert_eq!(buf, b"he");
    }

    #[test]
    fn fail_next_read_after_caps_at_content_length() {
        let fs = FakeFs::new();
        fs.insert_file("/x.txt", "hi");
        fs.fail_next_read_after("/x.txt", 50, io::ErrorKind::Interrupted);
        let mut buf = Vec::new();
        let err = fs.read(Path::new("/x.txt"), &mut buf).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Interrupted);
        assert_eq!(buf, b"hi");
    }

    #[test]
    fn fail_next_read_after_fires_once() {
        let fs = FakeFs::new();
        fs.insert_file("/x.txt", "hello");
        fs.fail_next_read_after("/x.txt", 2, io::ErrorKind::Interrupted);
        let mut buf = Vec::new();
        let _ = fs.read(Path::new("/x.txt"), &mut buf);
        fs.read(Path::new("/x.txt"), &mut buf).unwrap();
        assert_eq!(buf, b"hello");
    }

    #[test]
    fn fail_next_read_after_records_op_on_failure() {
        let fs = FakeFs::new();
        fs.insert_file("/x", "ok");
        fs.fail_next_read_after("/x", 1, io::ErrorKind::Interrupted);
        let _ = fs.read(Path::new("/x"), &mut Vec::new());
        assert_eq!(
            fs.ops(),
            [FakeFsOp::Read {
                path: PathBuf::from("/x"),
            }]
        );
    }

    #[test]
    fn fail_writes_to_is_sticky() {
        let fs = FakeFs::new();
        fs.fail_writes_to("/locked", io::ErrorKind::PermissionDenied);
        let first = fs.write(Path::new("/locked"), b"a").unwrap_err();
        let second = fs.write(Path::new("/locked"), b"b").unwrap_err();
        assert_eq!(first.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(second.kind(), io::ErrorKind::PermissionDenied);
        assert!(!fs.exists(Path::new("/locked")));
    }

    #[test]
    fn fail_next_metadata_fires_once() {
        let fs = FakeFs::new();
        fs.insert_file("/x.txt", "hi");
        fs.fail_next_metadata("/x.txt", io::ErrorKind::PermissionDenied);
        let err = fs.metadata(Path::new("/x.txt")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        let m = fs.metadata(Path::new("/x.txt")).unwrap().unwrap();
        assert_eq!(m.len, 2);
    }

    #[test]
    fn fail_next_metadata_records_op_on_failure() {
        let fs = FakeFs::new();
        fs.insert_file("/x", "ok");
        fs.fail_next_metadata("/x", io::ErrorKind::PermissionDenied);
        let _ = fs.metadata(Path::new("/x"));
        assert_eq!(
            fs.ops(),
            [FakeFsOp::Metadata {
                path: PathBuf::from("/x"),
            }]
        );
    }

    #[test]
    fn fail_next_list_dir_fires_once() {
        let fs = FakeFs::new();
        fs.insert_dir("/d");
        fs.insert_file("/d/a", "");
        fs.fail_next_list_dir("/d", io::ErrorKind::PermissionDenied);
        let err = fs.list_dir(Path::new("/d")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        let entries = fs.list_dir(Path::new("/d")).unwrap();
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["a"]);
    }

    #[test]
    fn fail_next_list_dir_records_op_on_failure() {
        let fs = FakeFs::new();
        fs.insert_dir("/d");
        fs.fail_next_list_dir("/d", io::ErrorKind::PermissionDenied);
        let _ = fs.list_dir(Path::new("/d"));
        assert_eq!(
            fs.ops(),
            [FakeFsOp::ListDir {
                path: PathBuf::from("/d"),
            }]
        );
    }

    #[test]
    fn fail_next_canonicalize_fires_once() {
        let fs = FakeFs::new();
        fs.insert_file("/target", "");
        fs.insert_symlink("/link", "/target");
        fs.fail_next_canonicalize("/link", io::ErrorKind::PermissionDenied);
        let err = fs.canonicalize(Path::new("/link")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        let canon = fs.canonicalize(Path::new("/link")).unwrap();
        assert_eq!(canon, Path::new("/target"));
    }

    #[test]
    fn fail_next_canonicalize_records_op_on_failure() {
        let fs = FakeFs::new();
        fs.insert_file("/x", "");
        fs.fail_next_canonicalize("/x", io::ErrorKind::PermissionDenied);
        let _ = fs.canonicalize(Path::new("/x"));
        assert_eq!(
            fs.ops(),
            [FakeFsOp::Canonicalize {
                path: PathBuf::from("/x"),
            }]
        );
    }

    #[test]
    fn fail_next_create_dir_all_fires_once() {
        let fs = FakeFs::new();
        fs.fail_next_create_dir_all("/d", io::ErrorKind::PermissionDenied);
        let err = fs.create_dir_all(Path::new("/d")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert!(!fs.exists(Path::new("/d")));
        fs.create_dir_all(Path::new("/d")).unwrap();
        let m = fs.metadata(Path::new("/d")).unwrap().unwrap();
        assert!(m.is_dir);
    }

    #[test]
    fn fail_next_create_dir_all_records_op_on_failure() {
        let fs = FakeFs::new();
        fs.fail_next_create_dir_all("/d", io::ErrorKind::PermissionDenied);
        let _ = fs.create_dir_all(Path::new("/d"));
        assert_eq!(
            fs.ops(),
            [FakeFsOp::CreateDirAll {
                path: PathBuf::from("/d"),
            }]
        );
    }

    #[test]
    fn fail_next_remove_file_fires_once() {
        let fs = FakeFs::new();
        fs.insert_file("/x.txt", "hi");
        fs.fail_next_remove_file("/x.txt", io::ErrorKind::PermissionDenied);
        let err = fs.remove_file(Path::new("/x.txt")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert!(fs.exists(Path::new("/x.txt")));
        fs.remove_file(Path::new("/x.txt")).unwrap();
        assert!(!fs.exists(Path::new("/x.txt")));
    }

    #[test]
    fn fail_next_remove_file_records_op_on_failure() {
        let fs = FakeFs::new();
        fs.insert_file("/x", "");
        fs.fail_next_remove_file("/x", io::ErrorKind::PermissionDenied);
        let _ = fs.remove_file(Path::new("/x"));
        assert_eq!(
            fs.ops(),
            [FakeFsOp::RemoveFile {
                path: PathBuf::from("/x"),
            }]
        );
    }

    #[test]
    fn fail_next_rename_fires_once() {
        let fs = FakeFs::new();
        fs.insert_file("/from.txt", "payload");
        fs.fail_next_rename("/from.txt", io::ErrorKind::PermissionDenied);
        let err = fs
            .rename(Path::new("/from.txt"), Path::new("/to.txt"))
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert!(fs.exists(Path::new("/from.txt")));
        assert!(!fs.exists(Path::new("/to.txt")));
        fs.rename(Path::new("/from.txt"), Path::new("/to.txt"))
            .unwrap();
        assert!(!fs.exists(Path::new("/from.txt")));
        let mut buf = Vec::new();
        fs.read(Path::new("/to.txt"), &mut buf).unwrap();
        assert_eq!(buf, b"payload");
    }

    #[test]
    fn fail_next_rename_records_op_on_failure() {
        let fs = FakeFs::new();
        fs.insert_file("/a", "");
        fs.fail_next_rename("/a", io::ErrorKind::PermissionDenied);
        let _ = fs.rename(Path::new("/a"), Path::new("/b"));
        assert_eq!(
            fs.ops(),
            [FakeFsOp::Rename {
                from: PathBuf::from("/a"),
                to: PathBuf::from("/b"),
            }]
        );
    }

    #[test]
    fn ops_records_call_sequence() {
        let fs = FakeFs::new();
        fs.write(Path::new("/a"), b"hi").unwrap();
        let mut buf = Vec::new();
        fs.read(Path::new("/a"), &mut buf).unwrap();
        fs.rename(Path::new("/a"), Path::new("/b")).unwrap();
        assert_eq!(
            fs.ops(),
            [
                FakeFsOp::Write {
                    path: PathBuf::from("/a"),
                    len: 2,
                },
                FakeFsOp::Read {
                    path: PathBuf::from("/a"),
                },
                FakeFsOp::Rename {
                    from: PathBuf::from("/a"),
                    to: PathBuf::from("/b"),
                },
            ]
        );
    }

    #[test]
    fn ops_captures_atomic_write_then_rename() {
        let fs = FakeFs::new();
        fs.write(Path::new("/data.tmp"), b"payload").unwrap();
        fs.rename(Path::new("/data.tmp"), Path::new("/data"))
            .unwrap();
        assert_eq!(
            fs.ops(),
            [
                FakeFsOp::Write {
                    path: PathBuf::from("/data.tmp"),
                    len: 7,
                },
                FakeFsOp::Rename {
                    from: PathBuf::from("/data.tmp"),
                    to: PathBuf::from("/data"),
                },
            ]
        );
    }

    #[test]
    fn ops_records_injected_read_failure_attempt() {
        let fs = FakeFs::new();
        fs.insert_file("/x", "ok");
        fs.fail_next_read("/x", io::ErrorKind::PermissionDenied);
        let _ = fs.read(Path::new("/x"), &mut Vec::new());
        assert_eq!(
            fs.ops(),
            [FakeFsOp::Read {
                path: PathBuf::from("/x"),
            }]
        );
    }

    #[test]
    fn walk_workspace_files_streaming_emits_multiple_batches() {
        let fs = FakeFs::new();
        let root = PathBuf::from("/repo");
        let count = crate::fs::WALK_BATCH_SIZE * 2 + 5;
        fs.insert_files(
            (0..count).map(|i| (root.join(format!("file_{i:05}.rs")), b"x".as_slice())),
        );

        let mut batches: Vec<Vec<PathBuf>> = Vec::new();
        fs.walk_workspace_files_streaming(&root, &mut |batch| batches.push(batch));

        assert!(
            batches.len() > 1,
            "expected multiple batches for {count} files (batch size {}); got {}",
            crate::fs::WALK_BATCH_SIZE,
            batches.len(),
        );
        let total: usize = batches.iter().map(|b| b.len()).sum();
        assert_eq!(total, count, "every path should reach a batch exactly once");

        let mut combined: Vec<PathBuf> = batches.into_iter().flatten().collect();
        combined.sort();
        let expected = fs.walk_workspace_files(&root);
        assert_eq!(combined, expected);
    }

    #[test]
    fn is_ignored_default_stoatignore_filters_target_dir() {
        let fs = FakeFs::new();
        fs.insert_file("/repo/src/main.rs", "fn main() {}");
        fs.insert_file("/repo/target/debug/foo", "bin");
        let repo = Path::new("/repo");

        assert!(fs.is_ignored(repo, Path::new("/repo/target/debug/foo")));
        assert!(!fs.is_ignored(repo, Path::new("/repo/src/main.rs")));
    }

    #[test]
    fn is_ignored_honours_per_dir_gitignore() {
        let fs = FakeFs::new();
        fs.insert_file("/repo/.gitignore", "secrets.txt\n");
        fs.insert_file("/repo/secrets.txt", "shh");
        fs.insert_file("/repo/src/secrets.txt", "shh-nested");
        fs.insert_file("/repo/src/main.rs", "fn main() {}");
        let repo = Path::new("/repo");

        assert!(fs.is_ignored(repo, Path::new("/repo/secrets.txt")));
        assert!(fs.is_ignored(repo, Path::new("/repo/src/secrets.txt")));
        assert!(!fs.is_ignored(repo, Path::new("/repo/src/main.rs")));
    }

    #[test]
    fn is_ignored_nested_stoatignore_negation_reincludes() {
        let fs = FakeFs::new();
        fs.insert_file("/repo/.gitignore", "*.log\n");
        fs.insert_file("/repo/sub/.stoatignore", "!keep.log\n");
        fs.insert_file("/repo/sub/keep.log", "important");
        fs.insert_file("/repo/sub/drop.log", "noise");
        let repo = Path::new("/repo");

        assert!(!fs.is_ignored(repo, Path::new("/repo/sub/keep.log")));
        assert!(fs.is_ignored(repo, Path::new("/repo/sub/drop.log")));
    }

    #[test]
    fn is_ignored_path_outside_workdir_returns_false() {
        let fs = FakeFs::new();
        fs.insert_file("/repo/.gitignore", "*\n");
        let repo = Path::new("/repo");

        assert!(!fs.is_ignored(repo, Path::new("/other/main.rs")));
    }
}
