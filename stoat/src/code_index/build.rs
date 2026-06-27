//! Cold-building the code index from a project scan.
//!
//! A single background job walks every indexable file under the workspace
//! root, extracts a [`FileShard`] from each, and streams the shards to the
//! event loop as [`IndexUpdate`] messages. The loop merges each shard into
//! the workspace graph and, on [`IndexUpdate::Complete`], resolves
//! cross-file references and writes the manifest.
//!
//! All parsing and extraction runs on the blocking pool. Only the cheap
//! merge happens on the main thread, off the paint path.

use crate::{
    buffer_registry::fingerprint_bytes, code_index::store, host::FsHost, workspace::WorkspaceId,
};
use codegraph::{
    build_shard, decode_shard, FileEntry, FileId, FileShard, Manifest, SCHEMA_VERSION,
};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
};
use stoat_language::{extract_references, extract_symbols, parse_rope, Language, LanguageRegistry};
use stoat_scheduler::{Executor, Task};
use stoat_text::Rope;
use tokio::sync::{mpsc::UnboundedSender, Notify};

/// A unit of index progress delivered from the build job to the event loop.
pub(crate) enum IndexUpdate {
    /// One file's shard, ready to merge into the graph. `persist` is true
    /// when the shard was freshly extracted and should be written to disk,
    /// false when it was loaded from an existing on-disk shard.
    Shard {
        workspace: WorkspaceId,
        rel_path: String,
        shard: FileShard,
        persist: bool,
    },
    /// The scan finished. Resolve cross-file references and persist the
    /// manifest listing every covered file.
    Complete {
        workspace: WorkspaceId,
        manifest: Manifest,
    },
    /// One edited buffer's freshly re-extracted shard. The drain evicts the
    /// file's prior symbols, inserts these, and re-resolves so callers of
    /// the changed file re-link. Not persisted until the buffer is saved.
    Reindex {
        workspace: WorkspaceId,
        file: FileId,
        shard: FileShard,
    },
}

/// The shared handles a build job captures while it runs. It holds the
/// filesystem, the language registry, the update channel, and the loop's
/// redraw signal.
pub(crate) struct IndexBuild {
    pub(crate) fs: Arc<dyn FsHost>,
    pub(crate) languages: Arc<LanguageRegistry>,
    pub(crate) tx: UnboundedSender<IndexUpdate>,
    pub(crate) redraw: Arc<Notify>,
}

/// Spawn the index build job for `workspace` rooted at `git_root`.
///
/// With `warm` set, each file whose manifest fingerprint still matches is
/// loaded from its on-disk shard rather than re-extracted, and shards for
/// files that have since vanished are deleted. Without it, every file is
/// extracted from scratch.
///
/// The returned [`Task`] must be kept alive for the build to run to
/// completion. Dropping it can cancel the in-flight scan. Progress is
/// reported through `tx`, not the task value.
pub(crate) fn build_index(
    executor: &Executor,
    handles: IndexBuild,
    git_root: PathBuf,
    workspace: WorkspaceId,
    warm: Option<(PathBuf, Manifest)>,
) -> Task<()> {
    let IndexBuild {
        fs,
        languages,
        tx,
        redraw,
    } = handles;
    executor.spawn_blocking(move || {
        let (index_dir, known) = match warm {
            Some((dir, manifest)) => {
                let known: HashMap<String, [u8; 32]> = manifest
                    .files
                    .into_iter()
                    .map(|entry| (entry.rel_path, entry.content_hash))
                    .collect();
                (Some(dir), known)
            },
            None => (None, HashMap::new()),
        };

        let mut entries = Vec::new();
        let mut seen = HashSet::new();
        fs.walk_workspace_files_streaming(&git_root, &mut |batch| {
            for path in batch {
                let Some((rel_path, shard, source)) = load_or_extract(
                    fs.as_ref(),
                    &languages,
                    &git_root,
                    index_dir.as_deref(),
                    &known,
                    &path,
                ) else {
                    continue;
                };
                seen.insert(rel_path.clone());
                entries.push(FileEntry {
                    rel_path: rel_path.clone(),
                    content_hash: shard.content_hash,
                });
                if tx
                    .send(IndexUpdate::Shard {
                        workspace,
                        rel_path,
                        shard,
                        persist: matches!(source, ShardSource::Extracted),
                    })
                    .is_err()
                {
                    return;
                }
                redraw.notify_one();
            }
        });

        if let Some(dir) = &index_dir {
            for rel_path in known.keys() {
                if !seen.contains(rel_path) {
                    let _ = store::delete_shard(dir, rel_path, fs.as_ref());
                }
            }
        }

        let manifest = Manifest {
            schema_version: SCHEMA_VERSION,
            files: entries,
        };
        let _ = tx.send(IndexUpdate::Complete {
            workspace,
            manifest,
        });
        redraw.notify_one();
    })
}

/// Inputs for re-indexing one edited buffer off the main thread.
pub(crate) struct ReindexTarget {
    pub(crate) git_root: PathBuf,
    pub(crate) workspace: WorkspaceId,
    pub(crate) language: Arc<Language>,
    pub(crate) path: PathBuf,
    pub(crate) text: String,
}

/// Spawn a job to re-extract one edited buffer and deliver its
/// [`IndexUpdate::Reindex`].
///
/// Extraction runs on the blocking pool from the buffer's in-memory text,
/// so it never touches disk or stalls the parse tick. The returned [`Task`]
/// must be kept alive until the job runs. Progress is reported through `tx`.
pub(crate) fn reindex_buffer(
    executor: &Executor,
    tx: UnboundedSender<IndexUpdate>,
    redraw: Arc<Notify>,
    target: ReindexTarget,
) -> Task<()> {
    executor.spawn_blocking(move || {
        let ReindexTarget {
            git_root,
            workspace,
            language,
            path,
            text,
        } = target;
        if let Some((rel_path, shard)) = extract_shard(&language, &git_root, &path, &text) {
            let file = file_id(&rel_path);
            let _ = tx.send(IndexUpdate::Reindex {
                workspace,
                file,
                shard,
            });
            redraw.notify_one();
        }
    })
}

/// Whether a file's shard was loaded from disk or freshly extracted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShardSource {
    Loaded,
    Extracted,
}

/// Load a file's shard from disk when its manifest fingerprint still
/// matches, otherwise extract it fresh.
///
/// A file loads only when `index_dir` is present, `known` holds its
/// rel-path, the current content fingerprint equals the stored one, and the
/// on-disk shard decodes. Any miss falls back to extraction.
fn load_or_extract(
    fs: &dyn FsHost,
    languages: &LanguageRegistry,
    git_root: &Path,
    index_dir: Option<&Path>,
    known: &HashMap<String, [u8; 32]>,
    path: &Path,
) -> Option<(String, FileShard, ShardSource)> {
    if let Some(dir) = index_dir
        && let Some(rel_path) = relpath(git_root, path)
        && let Some(&known_hash) = known.get(&rel_path)
        && current_fingerprint(fs, path) == Some(known_hash)
        && let Ok(bytes) = store::read_shard(dir, &rel_path, fs)
        && let Ok(shard) = decode_shard(&bytes)
    {
        return Some((rel_path, shard, ShardSource::Loaded));
    }

    let (rel_path, shard) = index_file(fs, languages, git_root, path)?;
    Some((rel_path, shard, ShardSource::Extracted))
}

/// A path's workspace-relative form, or `None` when it is not under `root`.
fn relpath(git_root: &Path, path: &Path) -> Option<String> {
    Some(
        path.strip_prefix(git_root)
            .ok()?
            .to_string_lossy()
            .into_owned(),
    )
}

/// A file's stable in-graph id, derived from its workspace-relative path.
///
/// Deriving from the path (rather than scan order) keeps a file's id
/// constant across a cold build, a warm load, and a live re-extract, so a
/// re-extract evicts and replaces exactly that file's symbols.
fn file_id(rel_path: &str) -> FileId {
    let digest = blake3::hash(rel_path.as_bytes());
    let mut bytes = [0u8; 4];
    bytes.copy_from_slice(&digest.as_bytes()[..4]);
    FileId(u32::from_le_bytes(bytes))
}

/// The content fingerprint of a readable UTF-8 file, or `None` otherwise.
fn current_fingerprint(fs: &dyn FsHost, path: &Path) -> Option<[u8; 32]> {
    let mut bytes = Vec::new();
    fs.read(path, &mut bytes).ok()?;
    let text = String::from_utf8(bytes).ok()?;
    Some(fingerprint_bytes(&text))
}

/// Extract one file's shard, or `None` when the file is not an indexable
/// language, cannot be read, or is not valid UTF-8.
///
/// Returns the file's workspace-relative path alongside the shard. The
/// shard's `content_hash` fingerprints the source for staleness checks.
fn index_file(
    fs: &dyn FsHost,
    languages: &LanguageRegistry,
    git_root: &Path,
    path: &Path,
) -> Option<(String, FileShard)> {
    let language = languages.for_path(path)?;
    let mut bytes = Vec::new();
    fs.read(path, &mut bytes).ok()?;
    let text = String::from_utf8(bytes).ok()?;
    extract_shard(&language, git_root, path, &text)
}

/// Parse `text` as `language` and build the file's shard, or `None` when
/// `path` is not under `git_root`.
///
/// Takes the source as a string so an open buffer can be re-indexed from
/// its in-memory contents without a disk read.
fn extract_shard(
    language: &Language,
    git_root: &Path,
    path: &Path,
    text: &str,
) -> Option<(String, FileShard)> {
    let rel_path = relpath(git_root, path)?;

    let rope = Rope::from(text);
    let tree = parse_rope(language, &rope, None)?;
    let root = tree.root_node();

    let defs = language
        .outline_query
        .as_ref()
        .map(|query| extract_symbols(query, root, &rope))
        .unwrap_or_default();
    let refs = language
        .tags_query
        .as_ref()
        .map(|query| extract_references(query, root, &rope))
        .unwrap_or_default();

    let shard = build_shard(
        file_id(&rel_path),
        &rel_path,
        fingerprint_bytes(text),
        text,
        defs,
        refs,
    );
    Some((rel_path, shard))
}

#[cfg(test)]
mod tests {
    use super::{index_file, load_or_extract, ShardSource};
    use crate::{
        buffer_registry::fingerprint_bytes,
        code_index::store,
        host::{FakeFs, FsHost},
    };
    use std::{collections::HashMap, path::Path};
    use stoat_language::LanguageRegistry;

    #[test]
    fn index_file_extracts_a_rust_shard() {
        let fs = FakeFs::new();
        let source = "fn helper() {}\n\nfn main() {\n    helper();\n}\n";
        fs.write(Path::new("/repo/src/a.rs"), source.as_bytes())
            .unwrap();

        let registry = LanguageRegistry::standard();
        let (rel_path, shard) = index_file(
            &fs,
            &registry,
            Path::new("/repo"),
            Path::new("/repo/src/a.rs"),
        )
        .unwrap();

        assert_eq!(rel_path, "src/a.rs");
        assert_eq!(shard.content_hash, fingerprint_bytes(source));

        let mut names: Vec<&str> = shard.symbols.iter().map(|s| s.name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, vec!["helper", "main"]);

        let calls = shard
            .edges
            .iter()
            .filter(|e| e.kind == codegraph::EdgeKind::Calls)
            .count();
        assert_eq!(calls, 1);
    }

    #[test]
    fn index_file_skips_non_language_files() {
        let fs = FakeFs::new();
        fs.write(Path::new("/repo/notes.xyz"), b"not code").unwrap();
        let registry = LanguageRegistry::standard();
        assert!(index_file(
            &fs,
            &registry,
            Path::new("/repo"),
            Path::new("/repo/notes.xyz")
        )
        .is_none());
    }

    #[test]
    fn load_or_extract_loads_unchanged_reextracts_otherwise() {
        let fs = FakeFs::new();
        let git_root = Path::new("/repo");
        let index_dir = Path::new("/idx");
        let path = Path::new("/repo/src/a.rs");
        let registry = LanguageRegistry::standard();

        let original = "fn helper() {}\n";
        fs.write(path, original.as_bytes()).unwrap();

        let (rel_path, original_shard) = index_file(&fs, &registry, git_root, path).unwrap();
        store::write_shard(
            index_dir,
            &rel_path,
            &codegraph::encode_shard(&original_shard),
            &fs,
        )
        .unwrap();
        let mut known = HashMap::new();
        known.insert(rel_path.clone(), fingerprint_bytes(original));

        let (_, loaded, source) =
            load_or_extract(&fs, &registry, git_root, Some(index_dir), &known, path).unwrap();
        assert_eq!(source, ShardSource::Loaded);
        assert_eq!(loaded, original_shard);

        fs.write(path, b"fn helper() {}\nfn added() {}\n").unwrap();
        let (_, _, source) =
            load_or_extract(&fs, &registry, git_root, Some(index_dir), &known, path).unwrap();
        assert_eq!(source, ShardSource::Extracted);

        let (_, _, source) = load_or_extract(
            &fs,
            &registry,
            git_root,
            Some(index_dir),
            &HashMap::new(),
            path,
        )
        .unwrap();
        assert_eq!(source, ShardSource::Extracted);
    }
}
