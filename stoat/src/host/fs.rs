use compact_str::CompactString;
use std::{io, path::Path, time::SystemTime};

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

    /// Returns whether `path` exists.
    fn exists(&self, path: &Path) -> bool {
        self.metadata(path).ok().flatten().is_some()
    }
}
