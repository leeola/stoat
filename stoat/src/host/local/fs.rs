use crate::host::fs::{FsDirEntry, FsHost, FsMetadata};
use async_trait::async_trait;
use compact_str::CompactString;
use std::{io, path::Path};
use tokio::io::AsyncReadExt;

pub struct LocalFs;

#[async_trait]
impl FsHost for LocalFs {
    async fn read(&self, path: &Path, buf: &mut Vec<u8>) -> io::Result<()> {
        buf.clear();
        let mut file = tokio::fs::File::open(path).await?;
        file.read_to_end(buf).await?;
        Ok(())
    }

    async fn write(&self, path: &Path, data: &[u8]) -> io::Result<()> {
        tokio::fs::write(path, data).await
    }

    async fn metadata(&self, path: &Path) -> io::Result<Option<FsMetadata>> {
        match tokio::fs::symlink_metadata(path).await {
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

    async fn list_dir(&self, path: &Path) -> io::Result<Vec<FsDirEntry>> {
        let mut entries = Vec::new();
        let mut rd = tokio::fs::read_dir(path).await?;
        while let Some(entry) = rd.next_entry().await? {
            let Some(name) = entry.file_name().to_str().map(CompactString::from) else {
                continue;
            };
            let ft = entry.file_type().await?;
            entries.push(FsDirEntry {
                name,
                is_dir: ft.is_dir(),
                is_symlink: ft.is_symlink(),
            });
        }
        Ok(entries)
    }

    async fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        tokio::fs::create_dir_all(path).await
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
    fn read_file() {
        let rt = rt();
        rt.block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("hello.txt");
            std::fs::write(&path, b"hello world").unwrap();

            let fs = LocalFs;
            let mut buf = Vec::new();
            fs.read(&path, &mut buf).await.unwrap();
            assert_eq!(buf, b"hello world");
        });
    }

    #[test]
    fn write_read_roundtrip() {
        let rt = rt();
        rt.block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("data.bin");

            let fs = LocalFs;
            fs.write(&path, b"round trip").await.unwrap();

            let mut buf = Vec::new();
            fs.read(&path, &mut buf).await.unwrap();
            assert_eq!(buf, b"round trip");
        });
    }

    #[test]
    fn metadata_existing_file() {
        let rt = rt();
        rt.block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("f.txt");
            std::fs::write(&path, b"abc").unwrap();

            let fs = LocalFs;
            let m = fs.metadata(&path).await.unwrap().unwrap();
            assert_eq!(m.len, 3);
            assert!(!m.is_dir);
            assert!(!m.is_symlink);
        });
    }

    #[test]
    fn metadata_nonexistent() {
        let rt = rt();
        rt.block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("nope");

            let fs = LocalFs;
            assert!(fs.metadata(&path).await.unwrap().is_none());
        });
    }

    #[test]
    fn list_dir_entries() {
        let rt = rt();
        rt.block_on(async {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("a.txt"), b"").unwrap();
            std::fs::create_dir(dir.path().join("sub")).unwrap();

            let fs = LocalFs;
            let mut entries = fs.list_dir(dir.path()).await.unwrap();
            entries.sort_by(|a, b| a.name.cmp(&b.name));

            assert_eq!(entries.len(), 2);
            assert_eq!(entries[0].name.as_str(), "a.txt");
            assert!(!entries[0].is_dir);
            assert_eq!(entries[1].name.as_str(), "sub");
            assert!(entries[1].is_dir);
        });
    }

    #[test]
    fn exists_true_and_false() {
        let rt = rt();
        rt.block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("yes");
            std::fs::write(&path, b"").unwrap();

            let fs = LocalFs;
            assert!(fs.exists(&path).await);
            assert!(!fs.exists(&dir.path().join("no")).await);
        });
    }
}
