//! Reading and writing the index manifest and per-file shards on disk.
//!
//! The index for a git root lives under a hash-derived directory, mirroring
//! how workspace state is persisted. Every write goes through a temp file
//! and a rename so a crash mid-write cannot leave a half-written index.

use crate::{host::FsHost, workspace::anchor_state_dir};
use codegraph::{decode_manifest, encode_manifest, FileEntry, Manifest, SCHEMA_VERSION};
use std::{
    hash::{Hash, Hasher},
    io,
    path::{Path, PathBuf},
};

const MANIFEST_FILE: &str = "manifest";
const SHARDS_DIR: &str = "shards";

/// Resolve the on-disk index directory for `git_root`.
///
/// The directory sits under the process state dir, keyed by a hash of the
/// canonical root path so each checkout gets its own index. Reads
/// `stoat_log::state_dir`, so it is environment-dependent and not pure.
pub(crate) fn index_dir_for(git_root: &Path, fs: &dyn FsHost) -> io::Result<PathBuf> {
    let index_root = stoat_log::state_dir()?.join("index");
    Ok(anchor_state_dir(&index_root, git_root, fs))
}

/// Write the index manifest under `index_dir`, replacing any existing one.
pub(crate) fn write_manifest(
    index_dir: &Path,
    manifest: &Manifest,
    fs: &dyn FsHost,
) -> io::Result<()> {
    write_atomic(
        &index_dir.join(MANIFEST_FILE),
        &encode_manifest(manifest),
        fs,
    )
}

/// Read and decode the index manifest under `index_dir`.
///
/// Returns [`io::ErrorKind::InvalidData`] when the bytes are not a manifest;
/// the caller still inspects [`Manifest::schema_version`] to decide whether
/// the index is current.
pub(crate) fn read_manifest(index_dir: &Path, fs: &dyn FsHost) -> io::Result<Manifest> {
    let bytes = read_bytes(&index_dir.join(MANIFEST_FILE), fs)?;
    decode_manifest(&bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))
}

/// Update or insert the manifest entry for `rel_path`, preserving the rest.
///
/// Reads the current manifest, replaces any entry for `rel_path` with one
/// carrying `content_hash`, and writes it back. Starts from an empty
/// manifest when none exists yet, and re-stamps the current schema version.
pub(crate) fn update_manifest_entry(
    index_dir: &Path,
    rel_path: &str,
    content_hash: [u8; 32],
    fs: &dyn FsHost,
) -> io::Result<()> {
    let mut manifest = read_manifest(index_dir, fs).unwrap_or_else(|_| Manifest {
        schema_version: SCHEMA_VERSION,
        files: Vec::new(),
    });
    manifest.schema_version = SCHEMA_VERSION;
    manifest.files.retain(|entry| entry.rel_path != rel_path);
    manifest.files.push(FileEntry {
        rel_path: rel_path.to_string(),
        content_hash,
    });
    write_manifest(index_dir, &manifest, fs)
}

/// Write a file's already-encoded shard bytes under `index_dir`.
pub(crate) fn write_shard(
    index_dir: &Path,
    rel_path: &str,
    bytes: &[u8],
    fs: &dyn FsHost,
) -> io::Result<()> {
    write_atomic(&shard_path(index_dir, rel_path), bytes, fs)
}

/// Read a file's encoded shard bytes under `index_dir`.
pub(crate) fn read_shard(index_dir: &Path, rel_path: &str, fs: &dyn FsHost) -> io::Result<Vec<u8>> {
    read_bytes(&shard_path(index_dir, rel_path), fs)
}

/// Delete a file's shard from `index_dir`, used when the source file is
/// gone so its stale shard does not linger.
pub(crate) fn delete_shard(index_dir: &Path, rel_path: &str, fs: &dyn FsHost) -> io::Result<()> {
    fs.remove_file(&shard_path(index_dir, rel_path))
}

/// The shard path for a workspace-relative file, named by a hash of the path
/// so it has a fixed, filesystem-safe form regardless of the source path.
fn shard_path(index_dir: &Path, rel_path: &str) -> PathBuf {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    rel_path.hash(&mut hasher);
    index_dir
        .join(SHARDS_DIR)
        .join(format!("{:016x}.shard", hasher.finish()))
}

fn write_atomic(path: &Path, data: &[u8], fs: &dyn FsHost) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs.create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    fs.write(&tmp, data)?;
    fs.rename(&tmp, path)
}

fn read_bytes(path: &Path, fs: &dyn FsHost) -> io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    fs.read(path, &mut buf)?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::{read_manifest, read_shard, update_manifest_entry, write_manifest, write_shard};
    use crate::{buffer_registry::fingerprint_bytes, host::FakeFs};
    use codegraph::{FileEntry, Manifest, SCHEMA_VERSION};
    use std::path::Path;

    fn manifest_for(content: &str) -> Manifest {
        Manifest {
            schema_version: SCHEMA_VERSION,
            files: vec![FileEntry {
                rel_path: "src/a.rs".to_string(),
                content_hash: fingerprint_bytes(content),
            }],
        }
    }

    #[test]
    fn manifest_round_trips_through_the_store() {
        let fs = FakeFs::new();
        let dir = Path::new("/idx");
        let manifest = manifest_for("alpha");
        write_manifest(dir, &manifest, &fs).unwrap();
        assert_eq!(read_manifest(dir, &fs).unwrap(), manifest);
    }

    #[test]
    fn shard_bytes_round_trip_through_the_store() {
        let fs = FakeFs::new();
        let dir = Path::new("/idx");
        let bytes = vec![9u8, 8, 7, 6, 5];
        write_shard(dir, "src/a.rs", &bytes, &fs).unwrap();
        assert_eq!(read_shard(dir, "src/a.rs", &fs).unwrap(), bytes);
    }

    #[test]
    fn content_hash_mismatch_is_detectable() {
        let fs = FakeFs::new();
        let dir = Path::new("/idx");
        write_manifest(dir, &manifest_for("v1"), &fs).unwrap();

        let stored = read_manifest(dir, &fs).unwrap();
        let entry = &stored.files[0];
        assert_eq!(entry.content_hash, fingerprint_bytes("v1"));
        assert_ne!(entry.content_hash, fingerprint_bytes("v2"));
    }

    #[test]
    fn update_manifest_entry_replaces_and_appends() {
        let fs = FakeFs::new();
        let dir = Path::new("/idx");

        update_manifest_entry(dir, "a.rs", [1u8; 32], &fs).unwrap();
        update_manifest_entry(dir, "b.rs", [2u8; 32], &fs).unwrap();
        update_manifest_entry(dir, "a.rs", [9u8; 32], &fs).unwrap();

        let mut files = read_manifest(dir, &fs).unwrap().files;
        files.sort_by(|x, y| x.rel_path.cmp(&y.rel_path));
        assert_eq!(
            files,
            vec![
                FileEntry {
                    rel_path: "a.rs".to_string(),
                    content_hash: [9u8; 32],
                },
                FileEntry {
                    rel_path: "b.rs".to_string(),
                    content_hash: [2u8; 32],
                },
            ]
        );
    }
}
