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

use crate::{buffer_registry::fingerprint_bytes, host::FsHost, workspace::WorkspaceId};
use codegraph::{build_shard, FileEntry, FileId, FileShard, Manifest, SCHEMA_VERSION};
use std::{path::Path, sync::Arc};
use stoat_language::{extract_references, extract_symbols, parse_rope, LanguageRegistry};
use stoat_scheduler::{Executor, Task};
use stoat_text::Rope;
use tokio::sync::{mpsc::UnboundedSender, Notify};

/// A unit of index progress delivered from the build job to the event loop.
pub(crate) enum IndexUpdate {
    /// One file's freshly extracted shard, ready to merge into the graph.
    Shard {
        workspace: WorkspaceId,
        rel_path: String,
        shard: FileShard,
    },
    /// The scan finished. Resolve cross-file references and persist the
    /// manifest listing every covered file.
    Complete {
        workspace: WorkspaceId,
        manifest: Manifest,
    },
}

/// Spawn the cold-build job for `workspace` rooted at `git_root`.
///
/// The returned [`Task`] must be kept alive for the build to run to
/// completion. Dropping it can cancel the in-flight scan. Progress is
/// reported through `tx`, not the task value.
pub(crate) fn build_index(
    executor: &Executor,
    fs: Arc<dyn FsHost>,
    languages: Arc<LanguageRegistry>,
    tx: UnboundedSender<IndexUpdate>,
    redraw: Arc<Notify>,
    git_root: std::path::PathBuf,
    workspace: WorkspaceId,
) -> Task<()> {
    executor.spawn_blocking(move || {
        let mut entries = Vec::new();
        let mut next_file = 0u32;
        fs.walk_workspace_files_streaming(&git_root, &mut |batch| {
            for path in batch {
                let Some((rel_path, shard)) =
                    index_file(fs.as_ref(), &languages, &git_root, &path, FileId(next_file))
                else {
                    continue;
                };
                next_file += 1;
                entries.push(FileEntry {
                    rel_path: rel_path.clone(),
                    content_hash: shard.content_hash,
                });
                if tx
                    .send(IndexUpdate::Shard {
                        workspace,
                        rel_path,
                        shard,
                    })
                    .is_err()
                {
                    return;
                }
                redraw.notify_one();
            }
        });

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
    file: FileId,
) -> Option<(String, FileShard)> {
    let language = languages.for_path(path)?;
    let rel_path = path
        .strip_prefix(git_root)
        .ok()?
        .to_string_lossy()
        .into_owned();

    let mut bytes = Vec::new();
    fs.read(path, &mut bytes).ok()?;
    let text = String::from_utf8(bytes).ok()?;

    let rope = Rope::from(text.as_str());
    let tree = parse_rope(&language, &rope, None)?;
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

    let shard = build_shard(file, &rel_path, fingerprint_bytes(&text), &text, defs, refs);
    Some((rel_path, shard))
}

#[cfg(test)]
mod tests {
    use super::index_file;
    use crate::{
        buffer_registry::fingerprint_bytes,
        host::{FakeFs, FsHost},
    };
    use codegraph::FileId;
    use std::path::Path;
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
            FileId(0),
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
            Path::new("/repo/notes.xyz"),
            FileId(0),
        )
        .is_none());
    }
}
