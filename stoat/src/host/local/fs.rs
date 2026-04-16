use crate::host::fs::{FsDirEntry, FsHost, FsMetadata};
use compact_str::CompactString;
use std::{io, io::Read, path::Path};

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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hello.txt");
        std::fs::write(&path, b"hello world").unwrap();

        let fs = LocalFs;
        let mut buf = Vec::new();
        fs.read(&path, &mut buf).unwrap();
        assert_eq!(buf, b"hello world");
    }

    #[test]
    fn write_read_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.bin");

        let fs = LocalFs;
        fs.write(&path, b"round trip").unwrap();

        let mut buf = Vec::new();
        fs.read(&path, &mut buf).unwrap();
        assert_eq!(buf, b"round trip");
    }

    #[test]
    fn metadata_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, b"abc").unwrap();

        let fs = LocalFs;
        let m = fs.metadata(&path).unwrap().unwrap();
        assert_eq!(m.len, 3);
        assert!(!m.is_dir);
        assert!(!m.is_symlink);
    }

    #[test]
    fn metadata_nonexistent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nope");

        let fs = LocalFs;
        assert!(fs.metadata(&path).unwrap().is_none());
    }

    #[test]
    fn list_dir_entries() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();

        let fs = LocalFs;
        let mut entries = fs.list_dir(dir.path()).unwrap();
        entries.sort_by(|a, b| a.name.cmp(&b.name));

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name.as_str(), "a.txt");
        assert!(!entries[0].is_dir);
        assert_eq!(entries[1].name.as_str(), "sub");
        assert!(entries[1].is_dir);
    }

    #[test]
    fn exists_true_and_false() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("yes");
        std::fs::write(&path, b"").unwrap();

        let fs = LocalFs;
        assert!(fs.exists(&path));
        assert!(!fs.exists(&dir.path().join("no")));
    }
}
