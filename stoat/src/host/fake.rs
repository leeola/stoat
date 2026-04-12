mod claude_code;
mod lsp;
pub mod terminal;

pub use self::{
    claude_code::FakeClaudeCode,
    lsp::{
        change_params, completion_params, definition_params, document_highlight_params,
        hover_params, inlay_hint_params, open_params, reference_params, workspace_symbol_params,
        FakeLsp,
    },
};
use crate::host::fs::{FsDirEntry, FsHost, FsMetadata};
use async_trait::async_trait;
use compact_str::CompactString;
use std::{
    collections::BTreeMap,
    io,
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, SystemTime},
};

enum FakeEntry {
    File { content: Vec<u8>, mtime: SystemTime },
    Dir { mtime: SystemTime },
}

pub struct FakeFs {
    state: Mutex<FakeState>,
}

struct FakeState {
    entries: BTreeMap<PathBuf, FakeEntry>,
    clock: u64,
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
            }),
        }
    }

    pub fn insert_file(&self, path: impl AsRef<Path>, content: impl AsRef<[u8]>) {
        let mut state = self.state.lock().unwrap();
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

    pub fn insert_dir(&self, path: impl AsRef<Path>) {
        let mut state = self.state.lock().unwrap();
        let path = path.as_ref();
        state.ensure_ancestors(path);
        let mtime = state.tick();
        state
            .entries
            .insert(path.to_path_buf(), FakeEntry::Dir { mtime });
    }
}

#[async_trait]
impl FsHost for FakeFs {
    async fn read(&self, path: &Path, buf: &mut Vec<u8>) -> io::Result<()> {
        let state = self.state.lock().unwrap();
        match state.entries.get(path) {
            Some(FakeEntry::File { content, .. }) => {
                buf.clear();
                buf.extend_from_slice(content);
                Ok(())
            },
            Some(FakeEntry::Dir { .. }) => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "is a directory",
            )),
            None => Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("{}: not found", path.display()),
            )),
        }
    }

    async fn write(&self, path: &Path, data: &[u8]) -> io::Result<()> {
        let mut state = self.state.lock().unwrap();
        state.ensure_ancestors(path);
        let mtime = state.tick();
        state.entries.insert(
            path.to_path_buf(),
            FakeEntry::File {
                content: data.to_vec(),
                mtime,
            },
        );
        Ok(())
    }

    async fn metadata(&self, path: &Path) -> io::Result<Option<FsMetadata>> {
        let state = self.state.lock().unwrap();
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
        }))
    }

    async fn list_dir(&self, path: &Path) -> io::Result<Vec<FsDirEntry>> {
        let state = self.state.lock().unwrap();
        match state.entries.get(path) {
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
                    format!("{}: not found", path.display()),
                ))
            },
        }

        let depth = path.components().count() + 1;
        let entries = state
            .entries
            .range(path.to_path_buf()..)
            .skip(1) // skip the directory itself
            .take_while(|(k, _)| k.starts_with(path))
            .filter(|(k, _)| k.components().count() == depth)
            .map(|(k, entry)| {
                let name = k
                    .file_name()
                    .expect("entry must have a file name")
                    .to_string_lossy();
                FsDirEntry {
                    name: CompactString::from(name.as_ref()),
                    is_dir: matches!(entry, FakeEntry::Dir { .. }),
                    is_symlink: false,
                }
            })
            .collect();

        Ok(entries)
    }

    async fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        let mut state = self.state.lock().unwrap();
        state.ensure_ancestors(path);
        if !state.entries.contains_key(path) {
            let mtime = state.tick();
            state
                .entries
                .insert(path.to_path_buf(), FakeEntry::Dir { mtime });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    #[test]
    fn write_read_roundtrip() {
        rt().block_on(async {
            let fs = FakeFs::new();
            fs.write(Path::new("/a/b.txt"), b"hello").await.unwrap();
            let mut buf = Vec::new();
            fs.read(Path::new("/a/b.txt"), &mut buf).await.unwrap();
            assert_eq!(buf, b"hello");
        });
    }

    #[test]
    fn read_nonexistent() {
        rt().block_on(async {
            let fs = FakeFs::new();
            let mut buf = Vec::new();
            let err = fs.read(Path::new("/nope"), &mut buf).await.unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::NotFound);
        });
    }

    #[test]
    fn write_auto_creates_parents() {
        rt().block_on(async {
            let fs = FakeFs::new();
            fs.write(Path::new("/a/b/c/d.txt"), b"deep").await.unwrap();
            assert!(fs.exists(Path::new("/a")).await);
            assert!(fs.exists(Path::new("/a/b")).await);
            assert!(fs.exists(Path::new("/a/b/c")).await);
            assert!(fs.exists(Path::new("/a/b/c/d.txt")).await);
        });
    }

    #[test]
    fn metadata_existing_file() {
        rt().block_on(async {
            let fs = FakeFs::new();
            fs.insert_file("/x.txt", "abc");
            let m = fs.metadata(Path::new("/x.txt")).await.unwrap().unwrap();
            assert_eq!(m.len, 3);
            assert!(!m.is_dir);
            assert!(!m.is_symlink);
        });
    }

    #[test]
    fn metadata_nonexistent() {
        rt().block_on(async {
            let fs = FakeFs::new();
            assert!(fs.metadata(Path::new("/nope")).await.unwrap().is_none());
        });
    }

    #[test]
    fn list_dir_entries() {
        rt().block_on(async {
            let fs = FakeFs::new();
            fs.insert_dir("/root");
            fs.insert_file("/root/a.txt", "");
            fs.insert_dir("/root/sub");
            fs.insert_file("/root/sub/nested.txt", "");

            let entries = fs.list_dir(Path::new("/root")).await.unwrap();
            let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
            assert_eq!(names, ["a.txt", "sub"]);
            assert!(!entries[0].is_dir);
            assert!(entries[1].is_dir);
        });
    }

    #[test]
    fn list_dir_nonexistent() {
        rt().block_on(async {
            let fs = FakeFs::new();
            let err = fs.list_dir(Path::new("/nope")).await.unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::NotFound);
        });
    }

    #[test]
    fn create_dir_all_creates_ancestors() {
        rt().block_on(async {
            let fs = FakeFs::new();
            fs.create_dir_all(Path::new("/a/b/c")).await.unwrap();
            for p in ["/a", "/a/b", "/a/b/c"] {
                let m = fs.metadata(Path::new(p)).await.unwrap().unwrap();
                assert!(m.is_dir);
            }
        });
    }

    #[test]
    fn exists_true_and_false() {
        rt().block_on(async {
            let fs = FakeFs::new();
            fs.insert_file("/yes", "");
            assert!(fs.exists(Path::new("/yes")).await);
            assert!(!fs.exists(Path::new("/no")).await);
        });
    }
}
