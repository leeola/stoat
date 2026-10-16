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
    buffer::{BufferId, TextBufferSnapshot},
    buffer_registry::fingerprint_bytes,
    code_index::store,
    host::FsHost,
    paths,
    picker::DisplayCache,
    syntax_parse,
    workspace::WorkspaceId,
};
use codegraph::{
    build_shard, decode_shard, FileEntry, FileId, FileShard, Manifest, SCHEMA_VERSION,
};
use std::{
    cmp::Reverse,
    collections::{HashMap, HashSet},
    ops::{ControlFlow, Range},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Instant,
};
use stoat_language::{
    edit_tree, enclosing_symbol_item, extract_references, extract_symbols, grammar_yields_symbols,
    parse_rope, Language, LanguageRegistry, Node, RefSite, SymbolDef, Tree,
};
use stoat_scheduler::{Executor, Task};
use stoat_text::{patch::Patch, LineEnding, Rope};
use tokio::sync::{mpsc::UnboundedSender, Notify};

/// A unit of index progress delivered from the build job to the event loop.
pub(crate) enum IndexUpdate {
    /// One file's shard, ready to merge into the graph.
    ///
    /// The build job has already written it to disk when it was freshly
    /// extracted, so nothing here needs persisting.
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
        /// Every file the build's walk yielded, which is the same tree the
        /// file finder walks on its first open.
        ///
        /// Wider than the manifest, which holds only the files a language
        /// extracted. The finder lists what the workspace contains.
        walked: Vec<PathBuf>,
        /// The `finder_path_epoch` this build spawned under, which stamps the
        /// cache `walked` becomes. A tree that moved while the build ran leaves
        /// the stamp behind the epoch, and the next finder open walks instead.
        walk_epoch: u64,
        /// The finder's display rows and order for `walked`, derived with the
        /// build on the pool, so the first finder open derives none on the loop.
        display: Option<DisplayCache>,
    },
    /// One file's freshly re-extracted shard. The drain evicts the file's
    /// prior symbols, inserts these, and re-resolves so callers of the
    /// changed file re-link.
    ///
    /// `persist` writes the shard and manifest entry now, used for changes
    /// that arrive without a later save (external edits); a buffer edit
    /// sends `false` and defers persistence to the save path.
    Reindex {
        workspace: WorkspaceId,
        file: FileId,
        rel_path: String,
        shard: FileShard,
        persist: bool,
        /// What a buffer reindex extracted, which the drain stores for the
        /// buffer's next reindex to start from. A reindex from disk has none.
        extraction: Option<Arc<Extraction>>,
    },
    /// A file that vanished from disk. The drain evicts its symbols, deletes
    /// its shard, drops its manifest entry, and re-resolves.
    Remove {
        workspace: WorkspaceId,
        file: FileId,
        rel_path: String,
    },
}

/// The shared handles a build job captures while it runs. It holds the
/// filesystem, the language registry, the update channel, and the loop's two
/// wake signals.
pub(crate) struct IndexBuild {
    pub(crate) fs: Arc<dyn FsHost>,
    pub(crate) languages: Arc<LanguageRegistry>,
    pub(crate) tx: UnboundedSender<IndexUpdate>,
    /// The wake for an update something on screen reads, which asks for a
    /// frame of its own.
    pub(crate) redraw: Arc<Notify>,
    /// The wake for an update nothing on screen reads, which asks the loop to
    /// merge it and paints only if the merge turns out to change something.
    ///
    /// A streamed shard is the case in point. It reaches the graph and no
    /// pane, so waking on the redraw signal repaints the whole screen once per
    /// file of the tree while the pool parses.
    pub(crate) drain: Arc<Notify>,
}

/// Spawn the index build job for `workspace` rooted at `git_root`.
///
/// `index_dir` is where the index persists, or `None` when persistence is off,
/// in which case nothing is read or written and every file is extracted fresh.
/// Given a directory, the job reads its manifest and loads each file whose
/// fingerprint still matches from its shard rather than re-extracting it,
/// deleting the shards of files that have since vanished. Reading the manifest
/// here rather than at the call site keeps a decode of one entry per indexed
/// file off the thread that paints the first frame.
///
/// The returned [`Task`] must be kept alive for the build to run to
/// completion. Dropping it can cancel the in-flight scan. Progress is
/// reported through `tx`, not the task value.
pub(crate) fn build_index(
    executor: &Executor,
    handles: IndexBuild,
    git_root: PathBuf,
    workspace: WorkspaceId,
    index_dir: Option<PathBuf>,
    walk_epoch: u64,
) -> Task<()> {
    let IndexBuild {
        fs,
        languages,
        tx,
        redraw,
        drain,
    } = handles;
    executor.spawn_blocking(move || {
        let started = Instant::now();
        let known: HashMap<String, [u8; 32]> = index_dir
            .as_deref()
            .and_then(|dir| store::read_manifest(dir, fs.as_ref()).ok())
            .filter(|manifest| manifest.schema_version == SCHEMA_VERSION)
            .map(|manifest| {
                manifest
                    .files
                    .into_iter()
                    .map(|entry| (entry.rel_path, entry.content_hash))
                    .collect()
            })
            .unwrap_or_default();
        tracing::info!(
            target: "stoat::code_index",
            root = %git_root.display(),
            mode = if known.is_empty() { "cold" } else { "warm" },
            "index build starting",
        );

        // Extraction is a read, a tree-sitter parse, two query walks, and an
        // encode per file, none of which touch shared state. Fanning the walk
        // across cores is what keeps a cold build of a large repo from being
        // one core's worth of work.
        let entries: Mutex<Vec<FileEntry>> = Mutex::new(Vec::new());

        // Every path the walk yielded, not only the ones a language extracted.
        // This walk is the same one the file finder makes on its first open, so
        // recording it here spares that one entirely.
        let walked: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
        let cancelled = AtomicBool::new(false);
        fs.walk_workspace_files_parallel(&git_root, &|batch| {
            for path in &batch {
                // The receiver drops on quit. Stop the walk instead of scanning
                // the rest of the tree while the runtime blocks on shutdown.
                if tx.is_closed() {
                    cancelled.store(true, Ordering::Relaxed);
                    return ControlFlow::Break(());
                }
                let Some((rel_path, shard, source)) = load_or_extract(
                    fs.as_ref(),
                    &languages,
                    &git_root,
                    index_dir.as_deref(),
                    &known,
                    path,
                ) else {
                    continue;
                };
                // Written here rather than by the drain, so a build streaming
                // hundreds of files does not hand the loop hundreds of
                // temp-file-and-rename pairs to perform between frames.
                if let Some(dir) = &index_dir
                    && matches!(source, ShardSource::Extracted)
                {
                    let _ = store::write_shard(
                        dir,
                        &rel_path,
                        &codegraph::encode_shard(&shard),
                        fs.as_ref(),
                    );
                }
                entries
                    .lock()
                    .expect("index entries poisoned")
                    .push(FileEntry {
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
                    cancelled.store(true, Ordering::Relaxed);
                    return ControlFlow::Break(());
                }
                // The shard reaches the graph and no pane, so the loop merges
                // it on a wake that earns its frame rather than assuming one.
                drain.notify_one();
            }

            // Past the early returns, so a walk the shutdown cut short records
            // nothing. Its list covers part of the tree, and a finder seeded
            // from that silently loses whatever the walk never reached.
            walked.lock().expect("walked paths poisoned").extend(batch);
            ControlFlow::Continue(())
        });

        // A cancelled build must not prune shards or send Complete. Its receiver
        // is gone, and pruning against its partial entries deletes live shards.
        if cancelled.load(Ordering::Relaxed) {
            return;
        }

        // Sorted because the walk hands files back in whatever order its
        // threads finish them, and a manifest that reorders itself between
        // builds of an unchanged tree defeats every comparison made against it.
        let mut entries = entries.into_inner().expect("index entries poisoned");
        entries.sort_unstable_by(|a, b| a.rel_path.cmp(&b.rel_path));

        if let Some(dir) = &index_dir {
            let seen: HashSet<&str> = entries.iter().map(|e| e.rel_path.as_str()).collect();
            for rel_path in known.keys() {
                if !seen.contains(rel_path.as_str()) {
                    let _ = store::delete_shard(dir, rel_path, fs.as_ref());
                }
            }
        }

        let manifest = Manifest {
            schema_version: SCHEMA_VERSION,
            files: entries,
        };
        let file_count = manifest.files.len();
        let walked = walked.into_inner().expect("walked paths poisoned");
        let display = DisplayCache::derive(&walked, &git_root, None, paths::home_dir());
        let _ = tx.send(IndexUpdate::Complete {
            workspace,
            manifest,
            walked,
            walk_epoch,
            display: Some(display),
        });
        redraw.notify_one();
        tracing::info!(
            target: "stoat::code_index",
            files = file_count,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "index build complete",
        );
    })
}

/// Inputs for re-indexing one edited buffer off the main thread.
pub(crate) struct ReindexTarget {
    pub(crate) git_root: PathBuf,
    pub(crate) workspace: WorkspaceId,
    pub(crate) language: Arc<Language>,
    pub(crate) path: PathBuf,
    /// The buffer as it stands. Its edit history since `previous` names the
    /// ranges to extract again.
    pub(crate) snapshot: TextBufferSnapshot,
    /// The tree the parse pipeline already built for exactly the snapshot's
    /// text, when it has one. Extraction reuses it rather than re-parsing the
    /// file, which is the dominant cost of a reindex. `None` means the caller
    /// found no stored tree that describes this text.
    pub(crate) tree: Option<Tree>,
    /// The buffer's last extraction, when one is stored.
    ///
    /// With it and a `tree`, the queries run only over the items that the
    /// edits since reach, and every other entry carries over.
    pub(crate) previous: Option<Arc<Extraction>>,
    /// Whether the resulting [`IndexUpdate::Reindex`] writes the shard and
    /// manifest entry as well as updating the graph.
    ///
    /// A save sets it, because the file on disk now matches the buffer and a
    /// later open warm-loads what is written. A plain edit leaves it clear and
    /// defers to whichever save follows.
    pub(crate) persist: bool,
}

/// What one reindex of an open buffer extracted, kept so the next reindex of
/// the buffer re-runs the queries only where the text changed.
///
/// The text and the tree are the buffer's at `version`. A later reindex edits a
/// copy of the tree to the later text and asks tree-sitter which ranges changed
/// structure. For example, a closing brace that completes an open block in one
/// function ends the impl around it early. The items after the brace then move
/// out of the impl, far from the edit.
pub(crate) struct Extraction {
    /// The buffer whose edit history `version` counts in. A version means
    /// nothing against the history of another buffer.
    pub(crate) buffer: BufferId,
    pub(crate) version: u64,
    language: &'static str,
    text: Rope,
    tree: Tree,
    /// The definitions and references of `text`, in the order that
    /// [`canonical_order`] gives them.
    defs: Vec<SymbolDef>,
    refs: Vec<RefSite>,
}

impl Extraction {
    /// Whether `snapshot` is this extraction's text or a later state of it, in
    /// the same language, so the buffer's history holds the edits between them.
    fn precedes(&self, snapshot: &TextBufferSnapshot, language: &Language) -> bool {
        self.buffer == snapshot.buffer_id
            && self.version <= snapshot.version
            && self.language == language.name
    }
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
            snapshot,
            tree,
            previous,
            persist,
        } = target;
        if let Some((rel_path, shard, extraction)) = extract_buffer(
            &language,
            &git_root,
            &path,
            snapshot,
            tree,
            previous.as_deref(),
        ) {
            let file = file_id(&rel_path);
            let _ = tx.send(IndexUpdate::Reindex {
                workspace,
                file,
                rel_path,
                shard,
                persist,
                extraction: Some(Arc::new(extraction)),
            });
            redraw.notify_one();
        }
    })
}

/// Inputs for re-indexing one file from disk after a change outside the editor.
pub(crate) struct ExternalReindex {
    pub(crate) git_root: PathBuf,
    pub(crate) workspace: WorkspaceId,
    pub(crate) path: PathBuf,
    /// `path` relative to `git_root`, and the id derived from it. Both are
    /// resolved by the caller, which needs them anyway to look up `expected`.
    pub(crate) rel_path: String,
    pub(crate) file: FileId,
    /// The content hash the graph already holds for this file, when it holds
    /// one. The editor's own save indexes what it wrote, so the watch event it
    /// then produces names a file the graph is already current on.
    pub(crate) expected: Option<[u8; 32]>,
}

/// Spawn a job re-indexing one file from disk, delivering a persisting
/// [`IndexUpdate::Reindex`].
///
/// Mirrors [`reindex_buffer`] but reads the file's current bytes from
/// disk, for a change that arrived outside the editor. The returned
/// [`Task`] must be kept alive until the job runs.
///
/// The staleness gate runs here rather than at the call site, so the run loop
/// never reads the file. One read serves the fingerprint and the extraction
/// both. A file that no longer reads as UTF-8 text, whether it vanished or
/// turned binary, sends [`IndexUpdate::Remove`] instead.
pub(crate) fn reindex_path(
    executor: &Executor,
    handles: IndexBuild,
    target: ExternalReindex,
) -> Task<()> {
    // One file the user is looking at, rather than a stream of them, so its
    // update takes the wake that asks for a frame.
    let IndexBuild {
        fs,
        languages,
        tx,
        redraw,
        ..
    } = handles;
    executor.spawn_blocking(move || {
        let ExternalReindex {
            git_root,
            workspace,
            path,
            rel_path,
            file,
            expected,
        } = target;

        // Resolved before the read, so a file no query touches costs the watch
        // event nothing. Such a file has no shard in the graph to update.
        let Some(language) = languages.for_path(&path).filter(|l| indexable(l)) else {
            return;
        };

        let Some(text) = read_utf8(fs.as_ref(), &path) else {
            let _ = tx.send(IndexUpdate::Remove {
                workspace,
                file,
                rel_path,
            });
            redraw.notify_one();
            return;
        };
        if expected == Some(fingerprint_bytes(&text)) {
            return;
        }

        // The caller's rel_path and the one extraction derives are the same
        // string from the same pair of paths, so the caller's is kept and the
        // second is dropped rather than allocated into the update.
        if let Some((_, shard)) =
            extract_shard(&language, &git_root, &path, &Rope::from(text.as_str()))
        {
            let _ = tx.send(IndexUpdate::Reindex {
                workspace,
                file,
                rel_path,
                shard,
                persist: true,
                extraction: None,
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
///
/// The language resolves ahead of the fingerprint, which is itself a whole
/// read. A manifest written before a language lost its queries still lists
/// entries this takes nothing from, and those are not read for either.
fn load_or_extract(
    fs: &dyn FsHost,
    languages: &LanguageRegistry,
    git_root: &Path,
    index_dir: Option<&Path>,
    known: &HashMap<String, [u8; 32]>,
    path: &Path,
) -> Option<(String, FileShard, ShardSource)> {
    if !languages.for_path(path).is_some_and(|l| indexable(&l)) {
        return None;
    }

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
pub(crate) fn relpath(git_root: &Path, path: &Path) -> Option<String> {
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
pub(crate) fn file_id(rel_path: &str) -> FileId {
    let digest = blake3::hash(rel_path.as_bytes());
    let mut bytes = [0u8; 4];
    bytes.copy_from_slice(&digest.as_bytes()[..4]);
    FileId(u32::from_le_bytes(bytes))
}

/// The content fingerprint of a readable UTF-8 file, or `None` otherwise.
pub(crate) fn current_fingerprint(fs: &dyn FsHost, path: &Path) -> Option<[u8; 32]> {
    Some(fingerprint_bytes(&read_utf8(fs, path)?))
}

/// A file's contents with line terminators normalized to bare `\n`, or `None`
/// when the read fails or the bytes are not UTF-8.
///
/// The two failures are deliberately one answer. A caller deciding whether a
/// file is still indexable treats a binary file exactly as it treats a missing
/// one.
///
/// Normalizing here puts extracted symbol offsets in the same coordinate space
/// as an open buffer, which holds its text normalized. A CRLF file otherwise
/// indexes at offsets one byte per preceding line ahead of the buffer's.
fn read_utf8(fs: &dyn FsHost, path: &Path) -> Option<String> {
    let mut bytes = Vec::new();
    fs.read(path, &mut bytes).ok()?;
    let text = String::from_utf8(bytes).ok()?;
    Some(LineEnding::normalize(&text).into_owned())
}

/// Whether the index takes anything from a language.
///
/// A file extracts an empty shard when its language has no tags query and its
/// outline query matches nothing a symbol comes from. Reading and parsing such
/// a file is work with no result. `toml` has neither query. `json` and
/// `markdown` have outline queries, but their grammars name no node kind a
/// symbol comes from.
///
/// Both queries compile on the first call and cache per grammar, so asking
/// costs nothing on the paths that go on to parse.
pub(crate) fn indexable(language: &Language) -> bool {
    language.tags_query().is_some()
        || (language.outline_query().is_some() && grammar_yields_symbols(&language.grammar))
}

/// Extract one file's shard, or `None` when the index takes nothing from the
/// file, the file cannot be read, or it is not valid UTF-8.
///
/// Returns the file's workspace-relative path alongside the shard. The
/// shard's `content_hash` fingerprints the source for staleness checks.
///
/// The language resolves before the read. A tree holds far more files no query
/// touches than files one does, and for those the read is the whole cost.
fn index_file(
    fs: &dyn FsHost,
    languages: &LanguageRegistry,
    git_root: &Path,
    path: &Path,
) -> Option<(String, FileShard)> {
    let language = languages.for_path(path)?;
    if !indexable(&language) {
        return None;
    }

    let text = read_utf8(fs, path)?;
    extract_shard(&language, git_root, path, &Rope::from(text.as_str()))
}

/// Parse `rope` as `language` and build the file's shard, or `None` when
/// `path` is not under `git_root` or the text does not parse.
///
/// The cold build and a reindex from disk extract through this. An open buffer
/// goes through [`extract_buffer`] instead, which reuses the buffer's parse
/// tree and its last extraction.
pub(crate) fn extract_shard(
    language: &Language,
    git_root: &Path,
    path: &Path,
    rope: &Rope,
) -> Option<(String, FileShard)> {
    let rel_path = relpath(git_root, path)?;
    let tree = parse_rope(language, rope, None)?;
    let (defs, refs) = extract_whole(language, tree.root_node(), rope);
    let shard = shard_of(&rel_path, rope, defs, refs);
    Some((rel_path, shard))
}

/// Build an open buffer's shard, beside the [`Extraction`] that the next
/// reindex of the buffer starts from.
///
/// Returns `None` when `path` is not under `git_root` or the text does not
/// parse. With a `tree` and a `previous` extraction that the snapshot
/// continues, the queries run only where the edits since `previous` reach.
/// The shard equals a whole-file extraction's either way.
fn extract_buffer(
    language: &Language,
    git_root: &Path,
    path: &Path,
    snapshot: TextBufferSnapshot,
    tree: Option<Tree>,
    previous: Option<&Extraction>,
) -> Option<(String, FileShard, Extraction)> {
    let rel_path = relpath(git_root, path)?;
    let rope = snapshot.visible_text.clone();

    let (tree, (defs, refs)) = match (tree, previous) {
        (Some(tree), Some(prev)) if prev.precedes(&snapshot, language) => {
            let extracted = extract_since(language, prev, &snapshot, &tree);
            (tree, extracted)
        },
        (tree, _) => {
            let tree = match tree {
                Some(tree) => tree,
                None => parse_rope(language, &rope, None)?,
            };
            let extracted = extract_whole(language, tree.root_node(), &rope);
            (tree, extracted)
        },
    };

    let shard = shard_of(&rel_path, &rope, defs.clone(), refs.clone());
    let extraction = Extraction {
        buffer: snapshot.buffer_id,
        version: snapshot.version,
        language: language.name,
        text: rope,
        tree,
        defs,
        refs,
    };
    Some((rel_path, shard, extraction))
}

/// The snapshot's definitions and references, with the queries run only over
/// the window that the edits since `prev` reach.
///
/// An entry of `prev` that no edit meets and that lies outside the window
/// carries over, shifted through the edits. The query output is cut to the
/// window. A match that meets the window comes back whole, and its captures
/// outside the window are among the entries that carry over.
fn extract_since(
    language: &Language,
    prev: &Extraction,
    snapshot: &TextBufferSnapshot,
    tree: &Tree,
) -> (Vec<SymbolDef>, Vec<RefSite>) {
    let patch = snapshot.edits_since(prev.version);
    if patch.is_empty() {
        return (prev.defs.clone(), prev.refs.clone());
    }

    let rope = &snapshot.visible_text;
    let window = edit_window(prev, &patch, rope, tree);

    let mut defs: Vec<SymbolDef> = prev
        .defs
        .iter()
        .filter_map(|def| {
            let def_range = carry(&patch, &window, &def.def_range)?;
            Some(SymbolDef {
                name: def.name.clone(),
                kind: def.kind,
                def_range,
                name_range: shift(&patch, &def.name_range),
                container: def.container.clone(),
            })
        })
        .collect();
    let mut refs: Vec<RefSite> = prev
        .refs
        .iter()
        .filter_map(|site| {
            let site_range = carry(&patch, &window, &site.site_range)?;
            Some(RefSite {
                name: site.name.clone(),
                kind: site.kind,
                site_range,
            })
        })
        .collect();

    let hull = window[0].start..window[window.len() - 1].end;
    let (fresh_defs, fresh_refs) = run_queries(language, tree.root_node(), rope, Some(hull));
    defs.extend(
        fresh_defs
            .into_iter()
            .filter(|def| meets_any(&window, &def.def_range, |range| range)),
    );
    refs.extend(
        fresh_refs
            .into_iter()
            .filter(|site| meets_any(&window, &site.site_range, |range| range)),
    );

    canonical_order(&mut defs, &mut refs);
    (defs, refs)
}

/// Every definition and reference in the tree at `root`, in canonical order.
fn extract_whole(
    language: &Language,
    root: Node<'_>,
    rope: &Rope,
) -> (Vec<SymbolDef>, Vec<RefSite>) {
    let (mut defs, mut refs) = run_queries(language, root, rope, None);
    canonical_order(&mut defs, &mut refs);
    (defs, refs)
}

/// Run the outline and tags queries over the matches that meet `range`, or
/// over the whole tree when it is `None`.
fn run_queries(
    language: &Language,
    root: Node<'_>,
    rope: &Rope,
    range: Option<Range<usize>>,
) -> (Vec<SymbolDef>, Vec<RefSite>) {
    let defs = language
        .outline_query()
        .map(|query| extract_symbols(query, root, rope, range.clone()))
        .unwrap_or_default();
    let refs = language
        .tags_query()
        .map(|query| extract_references(query, root, rope, range))
        .unwrap_or_default();
    (defs, refs)
}

/// Sort extracted entries into one order, whatever path extracted them.
///
/// The outline query yields an impl after its methods in places, so query
/// order does not survive a merge of carried and fresh entries. The sorts are
/// stable, so entries at one range keep their query order.
fn canonical_order(defs: &mut [SymbolDef], refs: &mut [RefSite]) {
    defs.sort_by_key(|def| (def.def_range.start, Reverse(def.def_range.end)));
    refs.sort_by_key(|site| site.site_range.start);
}

/// The shard of the file at `rel_path`, holding `defs` and `refs`.
fn shard_of(rel_path: &str, rope: &Rope, defs: Vec<SymbolDef>, refs: Vec<RefSite>) -> FileShard {
    // The shard hashes the whole file and each symbol's body by byte range, so
    // this one materialization is what those need and is not avoidable here.
    let text = rope.to_string();
    build_shard(
        file_id(rel_path),
        rel_path,
        fingerprint_bytes(&text),
        &text,
        defs,
        refs,
    )
}

/// The sorted, disjoint ranges of `rope` whose extraction the edits of `patch`
/// since `prev` reach, where `tree` is the parse of `rope`.
///
/// The edits name the text that changed. Tree-sitter's changed ranges between
/// the edited tree of `prev` and `tree` name the regions whose structure
/// changed, which sometimes lie far from the edit. Each range widens to the
/// symbol item that holds it, because an item's kind and container come from
/// the items around it. A range that no item holds widens to the whole text.
///
/// The widening looks in both trees. Tree-sitter compares the trees node by
/// node, so a function that moves from one module into the next reads as
/// unchanged, and only the boundary between the modules changes. When an edit
/// makes an item of the new tree swallow that boundary, that item holds
/// neither module. In the old tree the item around the boundary holds both.
fn edit_window(
    prev: &Extraction,
    patch: &Patch<usize>,
    rope: &Rope,
    tree: &Tree,
) -> Vec<Range<usize>> {
    let mut edited = prev.tree.clone();
    edit_tree(&mut edited, patch.edits(), &prev.text, rope);

    let len = rope.len();
    let reached: Vec<Range<usize>> = patch
        .edits()
        .iter()
        .map(|edit| edit.new.clone())
        .chain(
            edited
                .changed_ranges(tree)
                .map(|range| range.start_byte..range.end_byte),
        )
        .collect();
    let mut window: Vec<Range<usize>> = reached
        .iter()
        .flat_map(|range| {
            let start = range.start.min(len);
            let end = range.end.clamp(start, len);
            [tree.root_node(), edited.root_node()].map(|root| {
                root.descendant_for_byte_range(start, end)
                    .and_then(enclosing_symbol_item)
                    .map_or(0..len, |item| {
                        let item = item.byte_range();
                        item.start.min(len)..item.end.min(len)
                    })
            })
        })
        .collect();
    window.sort_unstable_by_key(|range| range.start);
    syntax_parse::merge_ranges(&mut window);
    window
}

/// `range` shifted through `patch`, or `None` when the entry at `range` is
/// extracted again, because an edit meets it or its shifted range meets
/// `window`.
fn carry(
    patch: &Patch<usize>,
    window: &[Range<usize>],
    range: &Range<usize>,
) -> Option<Range<usize>> {
    if meets_any(patch.edits(), range, |edit| &edit.old) {
        return None;
    }
    let shifted = shift(patch, range);
    (!meets_any(window, &shifted, |range| range)).then_some(shifted)
}

/// `range` moved through the edits of `patch`, for a range that no edit meets.
///
/// An end at an insertion point stays before the inserted text, where
/// [`Patch::old_to_new`] carries it past. No edit meets the range, so the
/// inserted text lies outside it.
fn shift(patch: &Patch<usize>, range: &Range<usize>) -> Range<usize> {
    let edits = patch.edits();
    let end = match edits.partition_point(|edit| edit.old.start < range.end) {
        0 => range.end,
        ix => {
            let edit = &edits[ix - 1];
            if range.end >= edit.old.end {
                edit.new.end + (range.end - edit.old.end)
            } else {
                edit.new.start
            }
        },
    };
    patch.old_to_new(range.start)..end
}

/// Whether `range` meets the span of one of `items`, whose spans are sorted
/// and disjoint.
///
/// Two ranges meet when each starts before the other ends. Thus an empty span
/// meets `range` only strictly inside it, as an insertion inside an entry
/// does, and ranges that only touch do not meet. A query cursor applies the
/// same test to its byte range.
fn meets_any<T>(items: &[T], range: &Range<usize>, span: impl Fn(&T) -> &Range<usize>) -> bool {
    let ix = items.partition_point(|item| span(item).end <= range.start);
    items
        .get(ix)
        .is_some_and(|item| span(item).start < range.end)
}

#[cfg(test)]
mod tests {
    use super::{
        build_index, carry, edit_window, extract_buffer, extract_shard, index_file, indexable,
        load_or_extract, Extraction, IndexBuild, IndexUpdate, ShardSource,
    };
    use crate::{
        buffer::{BufferId, TextBuffer},
        buffer_registry::fingerprint_bytes,
        code_index::store,
        host::{FakeFs, FakeFsOp, FsHost},
        workspace::WorkspaceId,
    };
    use codegraph::{FileEntry, Manifest, SCHEMA_VERSION};
    use futures::FutureExt;
    use std::{
        collections::HashMap,
        ops::Range,
        path::{Path, PathBuf},
        sync::Arc,
    };
    use stoat_language::{edit_tree, parse_rope, Language, LanguageRegistry};
    use stoat_scheduler::TestScheduler;
    use stoat_text::{
        patch::{Edit, Patch},
        Rope,
    };
    use tokio::sync::Notify;

    /// Run a build to completion over `fs`, returning the updates it streamed.
    fn run_build(fs: Arc<FakeFs>, index_dir: Option<PathBuf>) -> Vec<IndexUpdate> {
        run_build_watched(fs, index_dir).0
    }

    /// The same, beside the two wake signals the build fired.
    ///
    /// A signal holds one permit however often it is notified, so what a caller
    /// reads back is whether the build touched it at all.
    fn run_build_watched(
        fs: Arc<FakeFs>,
        index_dir: Option<PathBuf>,
    ) -> (Vec<IndexUpdate>, Arc<Notify>, Arc<Notify>) {
        let scheduler = Arc::new(TestScheduler::new());
        let (redraw, drain) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let task = build_index(
            &scheduler.executor(),
            IndexBuild {
                fs,
                languages: Arc::new(LanguageRegistry::standard()),
                tx,
                redraw: redraw.clone(),
                drain: drain.clone(),
            },
            PathBuf::from("/repo"),
            WorkspaceId::default(),
            index_dir,
            0,
        );
        scheduler.run_until_parked();
        drop(task);

        let mut updates = Vec::new();
        while let Ok(update) = rx.try_recv() {
            updates.push(update);
        }
        (updates, redraw, drain)
    }

    /// A streamed shard reaches the graph and no pane, so it wakes the loop to
    /// merge rather than to paint. Waking the redraw signal repaints the whole
    /// screen once per file of the tree while the pool parses, and every key
    /// pressed during the build queues behind those turns.
    ///
    /// The two builds separate the shard's wake from the build's own. A
    /// completed build asks for its frame either way, and a signal holds one
    /// permit however often it is notified, so a tree with a file cannot show
    /// on its own which of the two fired the redraw.
    #[test]
    fn a_streamed_shard_wakes_the_loop_to_merge_and_not_to_paint() {
        let fs = Arc::new(FakeFs::new());
        fs.write(Path::new("/repo/a.rs"), b"fn helper() {}\n")
            .unwrap();
        let (_, _, drain) = run_build_watched(fs, None);

        assert!(
            drain.notified().now_or_never().is_some(),
            "the shard woke the loop to merge it",
        );

        let (_, redraw, drain) = run_build_watched(Arc::new(FakeFs::new()), None);

        assert!(
            drain.notified().now_or_never().is_none(),
            "a build with no shard to stream wakes no merge",
        );
        assert!(
            redraw.notified().now_or_never().is_some(),
            "and the completed build still asks for its frame",
        );
    }

    /// The build writes each shard it extracted, rather than handing the bytes
    /// to the event loop to write between frames.
    #[test]
    fn a_build_writes_the_shards_it_extracted() {
        let fs = Arc::new(FakeFs::new());
        fs.write(Path::new("/repo/a.rs"), b"fn helper() {}\n")
            .unwrap();

        let updates = run_build(fs.clone(), Some(PathBuf::from("/idx")));

        assert!(
            updates
                .iter()
                .any(|u| matches!(u, IndexUpdate::Shard { rel_path, .. } if rel_path == "a.rs")),
            "the shard still reaches the graph",
        );
        let stored = store::read_shard(Path::new("/idx"), "a.rs", fs.as_ref())
            .expect("the build wrote the shard itself");
        assert_eq!(
            codegraph::decode_shard(&stored).unwrap().content_hash,
            fingerprint_bytes("fn helper() {}\n"),
            "and wrote the bytes for the text it indexed",
        );
    }

    /// The walk hands files back in whatever order its threads finish them, so
    /// the manifest is sorted rather than accumulated in arrival order. A
    /// manifest that reshuffles between builds of an unchanged tree defeats the
    /// warm-build comparison and every diff taken against the file.
    ///
    /// Shard updates carry no such promise. They stream as they are extracted,
    /// which is the whole point of streaming them.
    #[test]
    fn the_completed_manifest_lists_every_file_in_sorted_order() {
        let fs = Arc::new(FakeFs::new());
        for name in ["z.rs", "a.rs", "m.rs", "nested/b.rs"] {
            fs.write(&PathBuf::from("/repo").join(name), b"fn helper() {}\n")
                .unwrap();
        }

        let updates = run_build(fs, Some(PathBuf::from("/idx")));

        let manifest = updates
            .iter()
            .find_map(|u| match u {
                IndexUpdate::Complete { manifest, .. } => Some(manifest),
                _ => None,
            })
            .expect("the build completed");
        let listed: Vec<&str> = manifest
            .files
            .iter()
            .map(|entry| entry.rel_path.as_str())
            .collect();

        assert_eq!(
            listed,
            ["a.rs", "m.rs", "nested/b.rs", "z.rs"],
            "every indexed file, in an order that does not depend on the walk",
        );
    }

    /// Without a directory the build persists nothing at all, which is what a
    /// persistence-disabled session asks for.
    #[test]
    fn a_build_with_no_index_dir_writes_nothing() {
        let fs = Arc::new(FakeFs::new());
        fs.write(Path::new("/repo/a.rs"), b"fn helper() {}\n")
            .unwrap();

        let updates = run_build(fs.clone(), None);

        assert!(!updates.is_empty(), "the graph is still populated");
        assert!(
            store::read_shard(Path::new("/idx"), "a.rs", fs.as_ref()).is_err(),
            "but nothing was written",
        );
    }

    /// The build reads the manifest itself, so a matching fingerprint loads the
    /// stored shard instead of re-extracting the file.
    ///
    /// The stored shard carries a symbol the source does not, which is what
    /// makes the two paths tell apart at all.
    #[test]
    fn a_build_reads_its_own_manifest_to_go_warm() {
        let fs = Arc::new(FakeFs::new());
        let source = "fn helper() {}\n";
        fs.write(Path::new("/repo/a.rs"), source.as_bytes())
            .unwrap();

        let dir = Path::new("/idx");
        let stored = codegraph::FileShard {
            content_hash: fingerprint_bytes(source),
            symbols: Vec::new(),
            edges: Vec::new(),
        };
        store::write_shard(dir, "a.rs", &codegraph::encode_shard(&stored), fs.as_ref()).unwrap();
        store::write_manifest(
            dir,
            &Manifest {
                schema_version: SCHEMA_VERSION,
                files: vec![FileEntry {
                    rel_path: "a.rs".to_string(),
                    content_hash: fingerprint_bytes(source),
                }],
            },
            fs.as_ref(),
        )
        .unwrap();

        let updates = run_build(fs, Some(dir.to_path_buf()));

        let shard = updates
            .iter()
            .find_map(|u| match u {
                IndexUpdate::Shard {
                    rel_path, shard, ..
                } if rel_path == "a.rs" => Some(shard),
                _ => None,
            })
            .expect("a.rs was indexed");
        assert!(
            shard.symbols.is_empty(),
            "the empty stored shard was loaded, not the source re-extracted",
        );
    }

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

    /// A buffer holds its text with the carriage returns stripped. A shard
    /// extracted from the raw disk bytes instead places every symbol past the
    /// first line one byte per preceding line ahead of the buffer's offset.
    #[test]
    fn an_indexed_crlf_file_carries_offsets_into_the_normalized_text() {
        let fs = FakeFs::new();
        let source = "fn helper() {}\r\n\r\nfn main() {\r\n    helper();\r\n}\r\n";
        fs.write(Path::new("/repo/src/a.rs"), source.as_bytes())
            .unwrap();

        let registry = LanguageRegistry::standard();
        let (_rel_path, shard) = index_file(
            &fs,
            &registry,
            Path::new("/repo"),
            Path::new("/repo/src/a.rs"),
        )
        .unwrap();

        let normalized = "fn helper() {}\n\nfn main() {\n    helper();\n}\n";
        let mut located: Vec<(&str, &str)> = shard
            .symbols
            .iter()
            .map(|s| (s.name.as_str(), &normalized[s.name_range.clone()]))
            .collect();
        located.sort_unstable();
        assert_eq!(
            located,
            vec![("helper", "helper"), ("main", "main")],
            "each symbol's range slices its own name out of the normalized text",
        );
        assert_eq!(
            shard.content_hash,
            fingerprint_bytes(normalized),
            "and the staleness fingerprint covers the text a buffer would hold",
        );
    }

    /// Handing extraction the tree the parse pipeline already built produces the
    /// same shard as parsing from scratch.
    ///
    /// The reindex path takes this shortcut on every quiet window, so a shard
    /// that differed would put the code graph's symbols and call edges somewhere
    /// the from-scratch path never would, and only for buffers that had been open
    /// long enough to be parsed.
    #[test]
    fn a_supplied_tree_extracts_the_same_shard_as_parsing() {
        let source = "fn helper() {}\n\nfn main() {\n    helper();\n}\n";
        let language = rust();
        let fresh = extract_shard(
            &language,
            Path::new("/repo"),
            Path::new(RUST_PATH),
            &Rope::from(source),
        )
        .unwrap();

        let (supplied, _) = extract(
            &language,
            &TextBuffer::with_text(BufferId::new(0), source),
            None,
        );

        assert_eq!(supplied, fresh.1);
        assert!(
            !supplied.symbols.is_empty(),
            "the fixture must extract symbols, or the comparison proves nothing",
        );
    }

    const RUST_PATH: &str = "/repo/src/a.rs";

    const TRAIT_IMPL: &str = "\
impl Greet for Point {
    fn hello(&self) {
        first();
    }

    fn bye(&self) {
        second();
    }
}

fn free() {
    third();
}
";

    fn rust() -> Arc<Language> {
        LanguageRegistry::standard()
            .for_path(Path::new(RUST_PATH))
            .unwrap()
    }

    /// The offset just past the first `needle` in `text`.
    fn after(text: &str, needle: &str) -> usize {
        text.find(needle).unwrap() + needle.len()
    }

    /// The shard and extraction of `buffer` as it stands, from a tree parsed the
    /// way the parse pipeline parses it, incrementally from `previous` when one
    /// is given.
    fn extract(
        language: &Language,
        buffer: &TextBuffer,
        previous: Option<&Extraction>,
    ) -> (codegraph::FileShard, Extraction) {
        let snapshot = buffer.snapshot.clone();
        let old_tree = previous.map(|prev| {
            let mut tree = prev.tree.clone();
            let patch = snapshot.edits_since(prev.version);
            edit_tree(&mut tree, patch.edits(), &prev.text, &snapshot.visible_text);
            tree
        });
        let tree = parse_rope(language, &snapshot.visible_text, old_tree.as_ref()).unwrap();
        let (_, shard, extraction) = extract_buffer(
            language,
            Path::new("/repo"),
            Path::new(RUST_PATH),
            snapshot,
            Some(tree),
            previous,
        )
        .unwrap();
        (shard, extraction)
    }

    /// Extract `source`, apply `edits` in order, and extract again from the
    /// first extraction. The second extraction equals a whole-file extraction
    /// of the edited text.
    ///
    /// Each edit's range is in the text that the edits before it leave.
    fn assert_windowed_matches_whole(source: &str, edits: &[(Range<usize>, &str)]) {
        let language = rust();
        let mut buffer = TextBuffer::with_text(BufferId::new(0), source);
        let (_, first) = extract(&language, &buffer, None);
        for (range, text) in edits {
            buffer.edit(range.clone(), text);
        }
        assert!(
            first.precedes(&buffer.snapshot, &language),
            "the second extraction takes the windowed path",
        );

        let (windowed_shard, windowed) = extract(&language, &buffer, Some(&first));
        let (whole_shard, whole) = extract(&language, &buffer, None);
        assert_eq!((&windowed.defs, &windowed.refs), (&whole.defs, &whole.refs));
        assert_eq!(windowed_shard, whole_shard);
    }

    /// The impl's match comes back with the method's, and it brings the
    /// references in the impl header with it. Those carry over from the first
    /// extraction, so the window cuts them out of the query's output.
    #[test]
    fn a_line_typed_in_a_method_extracts_as_the_whole_file_does() {
        let at = after(TRAIT_IMPL, "first();\n");
        assert_windowed_matches_whole(TRAIT_IMPL, &[(at..at, "        let p: Point = added();\n")]);
    }

    /// A settle re-runs the queries over the method that holds the edit and
    /// nothing around it, which keeps a settle in a large impl cheap.
    #[test]
    fn a_line_typed_in_a_method_re_runs_the_queries_over_that_method_alone() {
        let language = rust();
        let mut buffer = TextBuffer::with_text(BufferId::new(0), TRAIT_IMPL);
        let (_, first) = extract(&language, &buffer, None);
        let at = after(TRAIT_IMPL, "first();\n");
        buffer.edit(at..at, "        added();\n");
        let (_, second) = extract(&language, &buffer, Some(&first));

        let rope = &buffer.snapshot.visible_text;
        let patch = buffer.snapshot.edits_since(first.version);
        let text = rope.to_string();
        let hello = text.find("fn hello").unwrap()..text.find("\n\n    fn bye").unwrap();
        assert_eq!(edit_window(&first, &patch, rope, &second.tree), [hello]);
    }

    /// The impl's type is the container of every method in it, so the rename
    /// re-extracts the whole impl rather than the renamed word.
    #[test]
    fn a_renamed_impl_type_extracts_as_the_whole_file_does() {
        let at = TRAIT_IMPL.find("Point").unwrap();
        assert_windowed_matches_whole(TRAIT_IMPL, &[(at..at + "Point".len(), "Spot")]);
    }

    /// The brace completes an open block in `f`, which ends the impl before `g`
    /// and moves `g` out of it. The edit sits in `f`, so only tree-sitter's
    /// changed ranges reach `g`.
    #[test]
    fn a_brace_that_ends_an_impl_early_extracts_as_the_whole_file_does() {
        let source = "impl X {\n    fn f() {\n        {\n    }\n}\nfn g() {}\n";
        let at = after(source, "        {");
        assert_windowed_matches_whole(source, &[(at..at, "}")]);
    }

    /// Two edits make two window ranges. The one query over their hull also
    /// finds the method between them, which carries over and does not come back
    /// twice.
    #[test]
    fn edits_in_two_functions_extract_as_the_whole_file_does() {
        let (hello, free) = (
            after(TRAIT_IMPL, "first();\n"),
            after(TRAIT_IMPL, "third();\n"),
        );
        assert_windowed_matches_whole(
            TRAIT_IMPL,
            &[
                (free..free, "    fourth();\n"),
                (hello..hello, "        added();\n"),
            ],
        );
    }

    /// An item typed flush between two others touches both. The item before it
    /// keeps its end at the insertion point, and the item after it moves past
    /// the inserted text.
    #[test]
    fn an_item_typed_flush_between_two_items_extracts_as_the_whole_file_does() {
        let source = "fn a() { x(); }fn c() { z(); }\n";
        let at = after(source, "x(); }");
        assert_windowed_matches_whole(source, &[(at..at, "fn b() { y(); }")]);
    }

    /// A quote typed in `f` pairs with every quote after it, which turns the
    /// end of `a` and the header of `b` into string tokens inside `f`. Then `h`
    /// moves into `a` with its own subtree unchanged. In the new tree the
    /// change sits inside `f`, so only the item around it in the old tree
    /// reaches `h`.
    #[test]
    fn a_quote_that_swallows_a_module_boundary_extracts_as_the_whole_file_does() {
        let source = "\
mod a {
    fn f() {
        assert!(
            !s.contains(\"x\"),
            \"y z\"
        );
    }
}

mod b {
    fn g() {
        assert!(t(\"u\"), \"v\");
    }

    fn h() {}
}
";
        let at = source.find("\"x\"").unwrap();
        assert_windowed_matches_whole(source, &[(at..at, "\"")]);
    }

    /// A settle with no edit since the previous extraction runs no query and
    /// returns the previous extraction's entries.
    #[test]
    fn a_settle_with_no_edit_extracts_as_the_whole_file_does() {
        assert_windowed_matches_whole(TRAIT_IMPL, &[]);
    }

    /// Text inserted at either end of an entry lands outside it, and an edit
    /// inside an entry stops it from carrying over. The window decides the rest,
    /// so an empty window isolates these two rules.
    #[test]
    fn an_entry_carries_over_shifted_only_when_no_edit_meets_it() {
        let edit = |old: Range<usize>, new: Range<usize>| Edit { old, new };
        let patch = Patch::new(vec![
            edit(5..5, 5..8),
            edit(10..10, 13..16),
            edit(20..21, 26..26),
        ]);
        assert_eq!(
            [5..10, 18..22, 30..40].map(|range| carry(&patch, &[], &range)),
            [Some(8..13), None, Some(35..45)],
        );
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

    /// Every path a read reached, so a test can assert a path was never read
    /// rather than only that nothing came of reading it.
    fn reads(fs: &FakeFs) -> Vec<PathBuf> {
        fs.ops()
            .into_iter()
            .filter_map(|op| match op {
                FakeFsOp::Read { path } => Some(path),
                _ => None,
            })
            .collect()
    }

    /// A registered language with neither an outline nor a tags query extracts
    /// an empty shard, so the read and the parse buy nothing. Every crate in a
    /// tree carries at least one such file.
    #[test]
    fn a_language_with_no_query_is_never_read() {
        let fs = FakeFs::new();
        let path = Path::new("/repo/Cargo.toml");
        fs.write(path, b"[package]\nname = \"x\"\n").unwrap();
        let registry = LanguageRegistry::standard();
        assert!(
            registry.for_path(path).is_some(),
            "toml has to be registered, or this proves nothing",
        );

        assert_eq!(index_file(&fs, &registry, Path::new("/repo"), path), None);
        assert_eq!(reads(&fs), Vec::<PathBuf>::new());
    }

    /// JSON and markdown ship outline queries, but their grammars name no node
    /// kind a symbol comes from. The RON grammar names `enum_variant`, but RON
    /// ships no query. So the index takes nothing from any language but Rust.
    #[test]
    fn only_the_rust_language_is_indexable() {
        let registry = LanguageRegistry::standard();
        let mut indexed: Vec<(&str, bool)> = registry
            .languages()
            .iter()
            .map(|language| (language.name, indexable(language)))
            .collect();
        indexed.sort();
        assert_eq!(
            indexed,
            [
                ("csv", false),
                ("json", false),
                ("markdown", false),
                ("markdown-inline", false),
                ("ron", false),
                ("rust", true),
                ("toml", false),
                ("yaml", false),
            ],
        );
    }

    /// A manifest written before a language lost its queries still lists
    /// entries the index takes nothing from. The fingerprint check is itself a
    /// whole read, so the guard runs ahead of it.
    #[test]
    fn load_or_extract_reads_nothing_for_a_queryless_language() {
        let fs = FakeFs::new();
        let git_root = Path::new("/repo");
        let index_dir = Path::new("/idx");
        let path = Path::new("/repo/Cargo.toml");
        let text = "[package]\n";
        fs.write(path, text.as_bytes()).unwrap();

        let mut known = HashMap::new();
        known.insert(String::from("Cargo.toml"), fingerprint_bytes(text));

        let registry = LanguageRegistry::standard();
        let loaded = load_or_extract(&fs, &registry, git_root, Some(index_dir), &known, path);
        assert!(
            loaded.is_none(),
            "the manifest entry is not reason to read it"
        );
        assert_eq!(reads(&fs), Vec::<PathBuf>::new());
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
