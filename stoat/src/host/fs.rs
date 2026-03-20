use compact_str::CompactString;
use std::{io, path::Path, time::SystemTime};

#[derive(Clone, Copy, Debug)]
pub struct FsMetadata {
    pub len: u64,
    pub modified: SystemTime,
    pub is_dir: bool,
    pub is_symlink: bool,
}

pub struct FsDirEntry {
    pub name: CompactString,
    pub is_dir: bool,
    pub is_symlink: bool,
}

#[allow(async_fn_in_trait)]
pub trait FsHost: Send + Sync {
    /// Clears `buf` and fills it with the file's contents.
    async fn read(&self, path: &Path, buf: &mut Vec<u8>) -> io::Result<()>;

    /// Writes `data` to `path`, creating or truncating the file.
    async fn write(&self, path: &Path, data: &[u8]) -> io::Result<()>;

    /// Returns metadata, or `None` if the path doesn't exist.
    /// Errors only on actual IO failures (permission denied, etc.), not NotFound.
    async fn metadata(&self, path: &Path) -> io::Result<Option<FsMetadata>>;

    /// Lists entries in `path`. Errors if the directory doesn't exist.
    async fn list_dir(&self, path: &Path) -> io::Result<Vec<FsDirEntry>>;

    /// Creates `path` and all missing parent directories.
    async fn create_dir_all(&self, path: &Path) -> io::Result<()>;

    /// Returns whether `path` exists.
    async fn exists(&self, path: &Path) -> bool {
        self.metadata(path).await.ok().flatten().is_some()
    }
}
