use crate::git::watcher::GitChangeKind;
use std::{
    any::Any,
    ffi::OsString,
    io,
    path::{Path, PathBuf},
    time::SystemTime,
};

pub struct FsMetadata {
    pub len: u64,
    pub is_dir: bool,
    pub is_file: bool,
    pub modified: Option<SystemTime>,
}

pub struct FsDirEntry {
    pub path: PathBuf,
    pub file_name: OsString,
    pub is_dir: bool,
    pub is_file: bool,
    pub len: u64,
}

pub trait Fs: Send + Sync {
    fn as_any(&self) -> &dyn Any;
    fn read_to_string(&self, path: &Path) -> io::Result<String>;
    fn read_bytes(&self, path: &Path, max_bytes: usize) -> io::Result<Vec<u8>>;
    fn write(&self, path: &Path, contents: &[u8]) -> io::Result<()>;
    fn atomic_write(&self, path: &Path, contents: &[u8]) -> io::Result<()>;
    fn metadata(&self, path: &Path) -> io::Result<FsMetadata>;
    fn read_dir(&self, path: &Path) -> io::Result<Vec<FsDirEntry>>;
    fn exists(&self, path: &Path) -> bool;
    fn is_dir(&self, path: &Path) -> bool;
    fn is_file(&self, path: &Path) -> bool;
    fn create_dir_all(&self, path: &Path) -> io::Result<()>;
    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf>;

    /// Start watching git-relevant paths (.git/index, HEAD, refs/).
    ///
    /// [`RealFs`] creates a [`notify::RecommendedWatcher`] + [`smol::channel`].
    /// [`FakeFs`] returns [`None`] (no OS-level events to watch).
    fn watch_git(
        &self,
        root: &Path,
    ) -> Option<(smol::channel::Receiver<GitChangeKind>, Box<dyn Any + Send>)>;
}

pub struct RealFs;

impl Fs for RealFs {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn read_to_string(&self, path: &Path) -> io::Result<String> {
        std::fs::read_to_string(path)
    }

    fn read_bytes(&self, path: &Path, max_bytes: usize) -> io::Result<Vec<u8>> {
        use std::io::Read;
        let mut file = std::fs::File::open(path)?;
        let mut buf = vec![0u8; max_bytes];
        let n = file.read(&mut buf)?;
        buf.truncate(n);
        Ok(buf)
    }

    fn write(&self, path: &Path, contents: &[u8]) -> io::Result<()> {
        std::fs::write(path, contents)
    }

    fn atomic_write(&self, path: &Path, contents: &[u8]) -> io::Result<()> {
        use std::io::Write;
        let dir = path.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "path has no parent directory")
        })?;
        let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
        tmp.write_all(contents)?;
        tmp.persist(path).map_err(|e| e.error)?;
        Ok(())
    }

    fn metadata(&self, path: &Path) -> io::Result<FsMetadata> {
        let m = std::fs::metadata(path)?;
        Ok(FsMetadata {
            len: m.len(),
            is_dir: m.is_dir(),
            is_file: m.is_file(),
            modified: m.modified().ok(),
        })
    }

    fn read_dir(&self, path: &Path) -> io::Result<Vec<FsDirEntry>> {
        let mut entries = Vec::new();
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            let metadata = entry.metadata()?;
            entries.push(FsDirEntry {
                path: entry.path(),
                file_name: entry.file_name(),
                is_dir: metadata.is_dir(),
                is_file: metadata.is_file(),
                len: metadata.len(),
            });
        }
        Ok(entries)
    }

    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }

    fn is_dir(&self, path: &Path) -> bool {
        path.is_dir()
    }

    fn is_file(&self, path: &Path) -> bool {
        path.is_file()
    }

    fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        std::fs::create_dir_all(path)
    }

    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        std::fs::canonicalize(path)
    }

    fn watch_git(
        &self,
        root: &Path,
    ) -> Option<(smol::channel::Receiver<GitChangeKind>, Box<dyn Any + Send>)> {
        let (sender, receiver) = smol::channel::bounded(64);
        let watcher = crate::git::watcher::start_watching(root, sender)?;
        Some((receiver, Box::new(watcher)))
    }
}

#[cfg(any(test, feature = "test-support"))]
use parking_lot::Mutex;

#[cfg(any(test, feature = "test-support"))]
pub struct FakeFs {
    state: Mutex<FakeFsState>,
}

#[cfg(any(test, feature = "test-support"))]
struct FakeFsState {
    files: std::collections::HashMap<PathBuf, Vec<u8>>,
    dirs: std::collections::HashSet<PathBuf>,
    next_mtime: SystemTime,
}

#[cfg(any(test, feature = "test-support"))]
impl FakeFs {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(FakeFsState {
                files: std::collections::HashMap::new(),
                dirs: std::collections::HashSet::new(),
                next_mtime: SystemTime::UNIX_EPOCH,
            }),
        }
    }

    pub fn insert_file(&self, path: impl Into<PathBuf>, content: impl AsRef<[u8]>) {
        let path = path.into();
        let mut state = self.state.lock();
        if let Some(parent) = path.parent() {
            let mut p = parent.to_path_buf();
            while p != Path::new("") && p != Path::new("/") {
                state.dirs.insert(p.clone());
                match p.parent() {
                    Some(pp) => p = pp.to_path_buf(),
                    None => break,
                }
            }
        }
        state.files.insert(path, content.as_ref().to_vec());
        state.next_mtime += std::time::Duration::from_secs(1);
    }

    pub fn files(&self) -> Vec<PathBuf> {
        self.state.lock().files.keys().cloned().collect()
    }
}

#[cfg(any(test, feature = "test-support"))]
impl Fs for FakeFs {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn read_to_string(&self, path: &Path) -> io::Result<String> {
        let state = self.state.lock();
        state
            .files
            .get(path)
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("{path:?} not found")))
    }

    fn read_bytes(&self, path: &Path, max_bytes: usize) -> io::Result<Vec<u8>> {
        let state = self.state.lock();
        state
            .files
            .get(path)
            .map(|b| b[..b.len().min(max_bytes)].to_vec())
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("{path:?} not found")))
    }

    fn write(&self, path: &Path, contents: &[u8]) -> io::Result<()> {
        self.insert_file(path, contents);
        Ok(())
    }

    fn atomic_write(&self, path: &Path, contents: &[u8]) -> io::Result<()> {
        self.write(path, contents)
    }

    fn metadata(&self, path: &Path) -> io::Result<FsMetadata> {
        let state = self.state.lock();
        if state.dirs.contains(path) {
            Ok(FsMetadata {
                len: 0,
                is_dir: true,
                is_file: false,
                modified: Some(state.next_mtime),
            })
        } else if let Some(content) = state.files.get(path) {
            Ok(FsMetadata {
                len: content.len() as u64,
                is_dir: false,
                is_file: true,
                modified: Some(state.next_mtime),
            })
        } else {
            Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("{path:?} not found"),
            ))
        }
    }

    fn read_dir(&self, path: &Path) -> io::Result<Vec<FsDirEntry>> {
        let state = self.state.lock();
        let mut entries = Vec::new();

        for (file_path, content) in &state.files {
            if file_path.parent() == Some(path) {
                entries.push(FsDirEntry {
                    path: file_path.clone(),
                    file_name: file_path.file_name().unwrap_or_default().to_os_string(),
                    is_dir: false,
                    is_file: true,
                    len: content.len() as u64,
                });
            }
        }

        for dir_path in &state.dirs {
            if dir_path.parent() == Some(path) && *dir_path != *path {
                entries.push(FsDirEntry {
                    path: dir_path.clone(),
                    file_name: dir_path.file_name().unwrap_or_default().to_os_string(),
                    is_dir: true,
                    is_file: false,
                    len: 0,
                });
            }
        }

        Ok(entries)
    }

    fn exists(&self, path: &Path) -> bool {
        let state = self.state.lock();
        state.files.contains_key(path) || state.dirs.contains(path)
    }

    fn is_dir(&self, path: &Path) -> bool {
        self.state.lock().dirs.contains(path)
    }

    fn is_file(&self, path: &Path) -> bool {
        self.state.lock().files.contains_key(path)
    }

    fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        let mut state = self.state.lock();
        let mut p = path.to_path_buf();
        while p != Path::new("") && p != Path::new("/") {
            state.dirs.insert(p.clone());
            match p.parent() {
                Some(pp) => p = pp.to_path_buf(),
                None => break,
            }
        }
        Ok(())
    }

    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        Ok(path.to_path_buf())
    }

    fn watch_git(
        &self,
        _root: &Path,
    ) -> Option<(smol::channel::Receiver<GitChangeKind>, Box<dyn Any + Send>)> {
        None
    }
}
