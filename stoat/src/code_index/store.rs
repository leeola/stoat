//! Reading and writing the index manifest and per-file shards on disk.
//!
//! The index for a git root lives under a hash-derived directory, mirroring
//! how workspace state is persisted. Every write goes through a temp file
//! and a rename so a crash mid-write cannot leave a half-written index.

use crate::{host::FsHost, workspace::anchor_state_dir};
use codegraph::{decode_manifest, encode_manifest, FileEntry, Manifest, SCHEMA_VERSION};
use std::{
    collections::HashSet,
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

/// Delete shard files under `index_dir` that no longer back a `manifest` entry,
/// returning the number removed.
///
/// A build only writes and rewrites shards. A source file that is renamed or
/// deleted leaves its shard behind, so the directory grows without bound. This
/// reconciles the directory against the manifest, removing every `.shard` file
/// whose name is not the [`shard_path`] of a current entry.
///
/// A missing shards directory yields zero removals rather than an error, so a
/// prune before the first shard write is a no-op.
pub(crate) fn prune_shards(
    index_dir: &Path,
    manifest: &Manifest,
    fs: &dyn FsHost,
) -> io::Result<usize> {
    let expected: HashSet<PathBuf> = manifest
        .files
        .iter()
        .map(|entry| shard_path(index_dir, &entry.rel_path))
        .collect();

    let shards_dir = index_dir.join(SHARDS_DIR);
    let entries = match fs.list_dir(&shards_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(err) => return Err(err),
    };

    let mut removed = 0;
    for entry in entries {
        if entry.is_dir || !entry.name.ends_with(".shard") {
            continue;
        }
        let path = shards_dir.join(entry.name.as_str());
        if !expected.contains(&path) {
            fs.remove_file(&path)?;
            removed += 1;
        }
    }
    Ok(removed)
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
///
/// Kept only as the reference a batched [`ManifestEdit::Set`] is measured
/// against, as [`remove_manifest_entry`] is for its counterpart. Every writer
/// now batches, a save included, and the comparison is only worth making
/// against the straightforward version.
#[cfg(test)]
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

/// One file's change to the manifest, as collected before being applied.
///
/// Exists so a drain's worth of changes can be applied together. Ordering is
/// significant, a path being set and then removed within one batch having to
/// end removed.
pub(crate) enum ManifestEdit {
    Set {
        rel_path: String,
        content_hash: [u8; 32],
    },
    Remove {
        rel_path: String,
    },
}

/// Apply `edits` to the manifest under `index_dir` in order, in one pass.
///
/// Equivalent to calling [`update_manifest_entry`] and [`remove_manifest_entry`]
/// for each edit in turn, but reading and writing the manifest once rather than
/// once per edit. A checkout touching N files costs one manifest cycle here and
/// N through the single-entry calls.
///
/// Starts from an empty manifest when none exists yet, and re-stamps the current
/// schema version. An empty `edits` writes nothing.
pub(crate) fn update_manifest_entries(
    index_dir: &Path,
    edits: &[ManifestEdit],
    fs: &dyn FsHost,
) -> io::Result<()> {
    if edits.is_empty() {
        return Ok(());
    }

    let manifest = read_manifest(index_dir, fs).unwrap_or_else(|_| Manifest {
        schema_version: SCHEMA_VERSION,
        files: Vec::new(),
    });
    write_manifest(index_dir, &apply_edits(manifest, edits), fs)
}

/// Fold `edits` into `manifest` in order, re-stamping the current schema
/// version.
fn apply_edits(mut manifest: Manifest, edits: &[ManifestEdit]) -> Manifest {
    manifest.schema_version = SCHEMA_VERSION;
    for edit in edits {
        match edit {
            ManifestEdit::Set {
                rel_path,
                content_hash,
            } => {
                manifest.files.retain(|entry| &entry.rel_path != rel_path);
                manifest.files.push(FileEntry {
                    rel_path: rel_path.clone(),
                    content_hash: *content_hash,
                });
            },
            ManifestEdit::Remove { rel_path } => {
                manifest.files.retain(|entry| &entry.rel_path != rel_path);
            },
        }
    }
    manifest
}

/// One index directory's worth of disk work, as gathered over a drain.
///
/// The event loop collects these while merging updates into the graph and hands
/// the lot to a blocking thread, so a drain covering hundreds of files performs
/// no writes itself.
#[derive(Default)]
pub(crate) struct IndexWrites {
    pub(crate) shards: Vec<(String, Vec<u8>)>,
    pub(crate) deleted_shards: Vec<String>,
    /// A finished build's manifest, which supersedes what is on disk rather than
    /// editing it, and licenses a prune of the shards it does not name.
    pub(crate) completed: Option<Manifest>,
    pub(crate) manifest_edits: Vec<ManifestEdit>,
}

/// Perform `writes` against the index under `index_dir`.
///
/// Shards land first, then the manifest is brought up to date in a single read
/// and write. A completed build's manifest is the base the edits apply over, so
/// a reindex that arrived in the same drain is not lost to the build's older
/// view of the tree.
///
/// Individual shard failures are skipped rather than aborting the rest, a
/// missing shard costing a re-extract on the next build. Returns the number of
/// stale shards pruned.
pub(crate) fn apply_index_writes(
    index_dir: &Path,
    writes: IndexWrites,
    fs: &dyn FsHost,
) -> io::Result<usize> {
    for (rel_path, bytes) in &writes.shards {
        let _ = write_shard(index_dir, rel_path, bytes, fs);
    }
    for rel_path in &writes.deleted_shards {
        let _ = delete_shard(index_dir, rel_path, fs);
    }

    let Some(built) = writes.completed else {
        update_manifest_entries(index_dir, &writes.manifest_edits, fs)?;
        return Ok(0);
    };

    let manifest = apply_edits(built, &writes.manifest_edits);
    write_manifest(index_dir, &manifest, fs)?;
    prune_shards(index_dir, &manifest, fs)
}

/// Drop the manifest entry for `rel_path`, preserving the rest.
///
/// A no-op when no manifest exists.
///
/// Kept only as the reference a batched [`ManifestEdit::Remove`] is measured
/// against. The drain that once removed entries one at a time now batches them,
/// and the comparison is only worth making against the straightforward version.
#[cfg(test)]
pub(crate) fn remove_manifest_entry(
    index_dir: &Path,
    rel_path: &str,
    fs: &dyn FsHost,
) -> io::Result<()> {
    let Ok(mut manifest) = read_manifest(index_dir, fs) else {
        return Ok(());
    };
    manifest.files.retain(|entry| entry.rel_path != rel_path);
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
    use super::{
        prune_shards, read_manifest, read_shard, remove_manifest_entry, update_manifest_entries,
        update_manifest_entry, write_manifest, write_shard, ManifestEdit,
    };
    use crate::{buffer_registry::fingerprint_bytes, host::FakeFs};
    use codegraph::{FileEntry, Manifest, SCHEMA_VERSION};
    use std::{io, path::Path};

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

    /// The batch exists only to spare N read-write cycles, so what it leaves on
    /// disk has to be what the single-entry calls would have left.
    ///
    /// The sequence sets a path twice, sets one it later removes, and removes
    /// one that was never there, because those are the orderings a drain
    /// covering a checkout actually produces and the ones a batch that merged
    /// its edits into a set would get wrong.
    #[test]
    fn a_batch_of_edits_lands_where_the_same_edits_one_at_a_time_would() {
        let edits = vec![
            ManifestEdit::Set {
                rel_path: "a.rs".to_string(),
                content_hash: [1u8; 32],
            },
            ManifestEdit::Set {
                rel_path: "b.rs".to_string(),
                content_hash: [2u8; 32],
            },
            ManifestEdit::Set {
                rel_path: "a.rs".to_string(),
                content_hash: [9u8; 32],
            },
            ManifestEdit::Remove {
                rel_path: "b.rs".to_string(),
            },
            ManifestEdit::Remove {
                rel_path: "never.rs".to_string(),
            },
            ManifestEdit::Set {
                rel_path: "c.rs".to_string(),
                content_hash: [3u8; 32],
            },
        ];

        let batched = FakeFs::new();
        update_manifest_entries(Path::new("/idx"), &edits, &batched).unwrap();

        let one_at_a_time = FakeFs::new();
        for edit in &edits {
            match edit {
                ManifestEdit::Set {
                    rel_path,
                    content_hash,
                } => update_manifest_entry(
                    Path::new("/idx"),
                    rel_path,
                    *content_hash,
                    &one_at_a_time,
                )
                .unwrap(),
                ManifestEdit::Remove { rel_path } => {
                    remove_manifest_entry(Path::new("/idx"), rel_path, &one_at_a_time).unwrap()
                },
            }
        }

        let sorted = |fs: &FakeFs| {
            let mut files = read_manifest(Path::new("/idx"), fs).unwrap().files;
            files.sort_by(|x, y| x.rel_path.cmp(&y.rel_path));
            files
        };
        assert_eq!(
            sorted(&batched),
            vec![
                FileEntry {
                    rel_path: "a.rs".to_string(),
                    content_hash: [9u8; 32],
                },
                FileEntry {
                    rel_path: "c.rs".to_string(),
                    content_hash: [3u8; 32],
                },
            ],
            "the later set wins and the removed path is gone",
        );
        assert_eq!(
            sorted(&batched),
            sorted(&one_at_a_time),
            "which is exactly where the single-entry calls land",
        );
    }

    #[test]
    fn an_empty_batch_leaves_no_manifest_behind() {
        let fs = FakeFs::new();
        update_manifest_entries(Path::new("/idx"), &[], &fs).unwrap();
        assert!(
            read_manifest(Path::new("/idx"), &fs).is_err(),
            "a drain that changed nothing writes nothing",
        );
    }

    #[test]
    fn prune_shards_removes_shards_absent_from_the_manifest() {
        let fs = FakeFs::new();
        let dir = Path::new("/idx");

        write_shard(dir, "a.rs", &[1u8], &fs).unwrap();
        write_shard(dir, "b.rs", &[2u8], &fs).unwrap();
        write_shard(dir, "c.rs", &[3u8], &fs).unwrap();

        let manifest = Manifest {
            schema_version: SCHEMA_VERSION,
            files: vec![FileEntry {
                rel_path: "a.rs".to_string(),
                content_hash: [1u8; 32],
            }],
        };

        assert_eq!(
            prune_shards(dir, &manifest, &fs).unwrap(),
            2,
            "the two shards absent from the manifest are pruned",
        );
        assert_eq!(
            read_shard(dir, "a.rs", &fs).unwrap(),
            vec![1u8],
            "the manifest's shard survives with its bytes intact",
        );
        assert_eq!(
            read_shard(dir, "b.rs", &fs).unwrap_err().kind(),
            io::ErrorKind::NotFound,
            "the b.rs shard is gone",
        );
        assert_eq!(
            read_shard(dir, "c.rs", &fs).unwrap_err().kind(),
            io::ErrorKind::NotFound,
            "the c.rs shard is gone",
        );
    }
}
