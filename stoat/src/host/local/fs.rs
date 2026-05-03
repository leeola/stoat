use crate::host::fs::{build_default_ignore, FsDirEntry, FsHost, FsMetadata};
use compact_str::CompactString;
use ignore::WalkBuilder;
use std::{
    io,
    io::Read,
    path::{Path, PathBuf},
};

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
}
