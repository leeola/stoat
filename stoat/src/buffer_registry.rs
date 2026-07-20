use crate::{
    buffer::{BufferHistory, BufferId, SharedBuffer, TextBuffer},
    display_map::{HighlightStyleInterner, SemanticTokenHighlight},
    lsp::LspSymbolKind,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    ops::Range,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
    time::SystemTime,
};
use stoat_language::{
    drop_syntax_in_background, structural_diff::DiffResult, Language, SyntaxMap, SyntaxState,
};
use stoat_text::Anchor;

/// Anchored, start-sorted LSP symbol kinds for one buffer, keyed by span. Built
/// from a semantic-tokens response and queried by offset via
/// [`BufferRegistry::lsp_symbol_kind_at`].
pub(crate) type LspSymbolKindIndex = Arc<[(Range<Anchor>, LspSymbolKind)]>;

/// Memoized [`DiffResult`] for a `(buffer, base_text)` pair. Cached
/// on [`BufferRegistry`] so repeat review-view renders and consumer
/// queries do not rerun the full structural-diff pipeline. Keyed
/// on the buffer version that was diffed and a blake3 fingerprint
/// of the base text: if either changes, the cache entry is stale.
#[derive(Clone)]
pub(crate) struct CachedDiff {
    pub buffer_version: u64,
    pub base_fingerprint: [u8; 32],
    pub result: Arc<DiffResult>,
}

/// One entry surfaced by [`BufferRegistry::dirty_buffers`]. `path` is
/// `Some` for file-backed buffers and `None` for scratch buffers.
#[derive(Clone, Debug)]
pub(crate) struct DirtyBuffer {
    pub id: BufferId,
    pub path: Option<PathBuf>,
}

#[allow(dead_code)]
struct BufferEntry {
    buffer: SharedBuffer,
    path: Option<PathBuf>,
    language: Option<Arc<Language>>,
    syntax: Option<SyntaxState>,
    /// Multi-layer syntax storage. Populated alongside [`Self::syntax`]
    /// so the legacy single-tree highlight path keeps working while
    /// callers migrate to capture merging. The `parse_buffer_step`
    /// pipeline writes to both fields on every reparse.
    syntax_map: Option<SyntaxMap>,
    /// Tree-sitter highlight tokens retained across editor lifetimes. The parse
    /// pipeline stores the same `(tokens, interner)` it installs onto editors,
    /// so a fresh editor built for an already-parsed buffer can be seeded and
    /// paint styled on its first frame instead of waiting for a reparse.
    tokens: Option<(Arc<[SemanticTokenHighlight]>, Arc<HighlightStyleInterner>)>,
    /// LSP semantic tokens retained across editor lifetimes, keyed by the buffer
    /// version they were computed against. A fresh editor is seeded from them,
    /// and the trigger reinstalls them instead of re-requesting, but only while
    /// the version still matches the buffer.
    lsp_tokens: Option<(
        u64,
        Arc<[SemanticTokenHighlight]>,
        Arc<HighlightStyleInterner>,
    )>,
    /// Anchored symbol kinds from the same LSP semantic-tokens response, kept
    /// separate from [`Self::lsp_tokens`] so cursor-aware features can query the
    /// kind under an offset without the highlight styling. Start-anchor sorted.
    lsp_symbol_kinds: Option<LspSymbolKindIndex>,
    diff: Option<CachedDiff>,
    /// Marks this buffer as a transient preview surface (e.g. the
    /// file finder's preview pane). The parse pipeline pulls these
    /// into its visibility set even when the buffer is not in a
    /// split pane, so syntax highlighting reaches the preview;
    /// callers evict the buffer via [`BufferRegistry::remove`] on
    /// close so registry growth stays bounded.
    preview: bool,
    /// On-disk modification time recorded when the file was last read
    /// into or written from this buffer. The save path compares it to
    /// the file's current mtime to detect an external edit and refuse
    /// to clobber it. `None` for scratch buffers and for files whose
    /// metadata could not be read.
    disk_mtime: Option<SystemTime>,
    /// When set, [`BufferRegistry::auto_reload_paths`] reports this buffer so the
    /// auto-reload pump re-reads its file as the on-disk mtime advances. Set for
    /// the session log buffer and any buffer that opts in.
    auto_reload: bool,
    /// Monotonic tick of when this buffer was last shown in a pane, from
    /// [`BufferRegistry::mark_shown`]. It orders eviction of hidden buffers'
    /// highlight state, dropping the lowest values first.
    last_shown: u64,
}

pub(crate) struct BufferRegistry {
    buffers: HashMap<BufferId, BufferEntry>,
    path_to_id: HashMap<PathBuf, BufferId>,
    next_id: u64,
    /// Monotonic counter stamped onto [`BufferEntry::last_shown`] by
    /// [`Self::mark_shown`] to order highlight eviction by recency.
    shown_counter: u64,
}

impl BufferRegistry {
    pub(crate) fn new() -> Self {
        Self {
            buffers: HashMap::new(),
            path_to_id: HashMap::new(),
            next_id: 1,
            shown_counter: 0,
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.buffers.len()
    }

    /// True when the registry holds exactly one buffer, that buffer has no
    /// backing file path, and its text is empty or the single newline
    /// [`Self::new_scratch`] seeds. This is the state left by a new scratch
    /// without any subsequent edits. A truly empty rope is also accepted so
    /// workspaces persisted before the seed still read as fresh. Used by
    /// [`crate::workspace::Workspace::is_fresh`] to decide whether a workspace
    /// is worth persisting.
    pub(crate) fn only_empty_scratch(&self) -> bool {
        if self.buffers.len() != 1 || !self.path_to_id.is_empty() {
            return false;
        }
        let Some(entry) = self.buffers.values().next() else {
            return false;
        };
        if entry.path.is_some() {
            return false;
        }
        let guard = entry.buffer.read().expect("buffer poisoned");
        guard.snapshot.is_empty() || guard.snapshot.visible_text.to_string() == "\n"
    }

    fn allocate_id(&mut self) -> BufferId {
        let id = BufferId::new(self.next_id);
        self.next_id += 1;
        id
    }

    /// Allocate an empty document scratch buffer, seeded with a single newline
    /// so an untouched scratch presents a min-width-1 cursor. Use
    /// [`Self::new_scratch_unseeded`] for surfaces that fill the buffer
    /// themselves.
    pub(crate) fn new_scratch(&mut self) -> (BufferId, SharedBuffer) {
        self.new_scratch_inner(false, true)
    }

    /// Allocate a scratch buffer flagged as a preview surface. The
    /// parse pipeline includes preview buffers in its visibility set
    /// so syntax highlighting reaches the file finder's preview pane
    /// (and any future preview surface). Callers evict the entry via
    /// [`Self::remove`] when the surface closes.
    pub(crate) fn new_scratch_preview(&mut self) -> (BufferId, SharedBuffer) {
        self.new_scratch_inner(true, true)
    }

    /// Allocate a scratch buffer with a genuinely empty rope.
    ///
    /// [`Self::new_scratch`] seeds a newline so an untouched scratch has a
    /// min-width-1 cursor. Some surfaces overwrite the whole buffer with their
    /// own content and must start from an empty rope, since a seeded newline
    /// would prepend to what they insert. The command input and the
    /// block-decorated placeholder are such surfaces.
    pub(crate) fn new_scratch_unseeded(&mut self) -> (BufferId, SharedBuffer) {
        self.new_scratch_inner(false, false)
    }

    /// Allocate a preview-flagged scratch buffer with a genuinely empty rope.
    ///
    /// Combines [`Self::new_scratch_preview`] and [`Self::new_scratch_unseeded`]:
    /// the parse pipeline syntax-highlights it, and it starts empty so a caller
    /// inserting whole content is not prefixed by a seeded newline. The conflict
    /// resolve view seeds its swapped-in center buffer through this.
    pub(crate) fn new_scratch_preview_unseeded(&mut self) -> (BufferId, SharedBuffer) {
        self.new_scratch_inner(true, false)
    }

    fn new_scratch_inner(&mut self, preview: bool, seed: bool) -> (BufferId, SharedBuffer) {
        let id = self.allocate_id();
        let buffer = if seed {
            Arc::new(RwLock::new(TextBuffer::with_text(id, "\n")))
        } else {
            Arc::new(RwLock::new(TextBuffer::new(id)))
        };
        self.buffers.insert(
            id,
            BufferEntry {
                buffer: buffer.clone(),
                path: None,
                language: None,
                syntax: None,
                syntax_map: None,
                tokens: None,
                lsp_tokens: None,
                lsp_symbol_kinds: None,
                diff: None,
                preview,
                disk_mtime: None,
                auto_reload: false,
                last_shown: 0,
            },
        );
        (id, buffer)
    }

    /// Returns the existing buffer for `path`, or creates one with `text`.
    /// If the buffer already exists, `text` is ignored.
    pub(crate) fn open(&mut self, path: &Path, text: &str) -> (BufferId, SharedBuffer) {
        if let Some(&id) = self.path_to_id.get(path) {
            let entry = &self.buffers[&id];
            return (id, entry.buffer.clone());
        }

        let id = self.allocate_id();
        let buffer = Arc::new(RwLock::new(TextBuffer::with_text(id, text)));
        let path_buf = path.to_path_buf();
        self.path_to_id.insert(path_buf.clone(), id);
        self.buffers.insert(
            id,
            BufferEntry {
                buffer: buffer.clone(),
                path: Some(path_buf),
                language: None,
                syntax: None,
                syntax_map: None,
                tokens: None,
                lsp_tokens: None,
                lsp_symbol_kinds: None,
                diff: None,
                preview: false,
                disk_mtime: None,
                auto_reload: false,
                last_shown: 0,
            },
        );
        (id, buffer)
    }

    pub(crate) fn get(&self, id: BufferId) -> Option<SharedBuffer> {
        self.buffers.get(&id).map(|e| e.buffer.clone())
    }

    pub(crate) fn id_for_path(&self, path: &Path) -> Option<BufferId> {
        self.path_to_id.get(path).copied()
    }

    /// Drop `id` from the registry. Removes the path-to-id mapping
    /// when the entry was path-bound and returns that path so the
    /// caller can build an LSP URI for `did_close`. Returns `None`
    /// when the buffer was scratch (or unknown).
    pub(crate) fn remove(&mut self, id: BufferId) -> Option<PathBuf> {
        let entry = self.buffers.remove(&id)?;
        let path = entry.path?;
        self.path_to_id.remove(&path);
        Some(path)
    }

    /// Updates the path of an open buffer in place. No-op when `old` has no
    /// open buffer. Returns `true` if a remapping happened. Used by
    /// `WorkspaceEdit::Rename` so an open buffer for the renamed file
    /// stays addressable by its new path.
    pub(crate) fn rename_path(&mut self, old: &Path, new: &Path) -> bool {
        let Some(id) = self.path_to_id.remove(old) else {
            return false;
        };
        self.path_to_id.insert(new.to_path_buf(), id);
        if let Some(entry) = self.buffers.get_mut(&id) {
            entry.path = Some(new.to_path_buf());
        }
        true
    }

    pub(crate) fn path_for(&self, id: BufferId) -> Option<&Path> {
        self.buffers.get(&id).and_then(|e| e.path.as_deref())
    }

    /// Record the on-disk mtime baseline the save path checks against to
    /// detect an external edit. No-op for an unknown buffer id.
    pub(crate) fn set_disk_mtime(&mut self, id: BufferId, mtime: SystemTime) {
        if let Some(entry) = self.buffers.get_mut(&id) {
            entry.disk_mtime = Some(mtime);
        }
    }

    /// The on-disk mtime recorded at the last open or save, or `None` for a
    /// scratch buffer, an unknown id, or a file whose metadata never read.
    pub(crate) fn disk_mtime(&self, id: BufferId) -> Option<SystemTime> {
        self.buffers.get(&id).and_then(|e| e.disk_mtime)
    }

    /// Flag `id` to be re-read from disk as its file grows, or clear the flag.
    /// No-op for an unknown id. The auto-reload pump only acts on flagged,
    /// path-bound buffers.
    ///
    /// Called when a buffer opts into file-following, such as the session log
    /// buffer and the `:auto-reload` command.
    #[allow(dead_code)]
    pub(crate) fn set_auto_reload(&mut self, id: BufferId, on: bool) {
        if let Some(entry) = self.buffers.get_mut(&id) {
            entry.auto_reload = on;
        }
    }

    /// The `(id, path)` of every auto-reload-flagged buffer that is path-bound,
    /// for the auto-reload pump to poll. Scratch buffers are skipped since they
    /// have no file to re-read.
    pub(crate) fn auto_reload_paths(&self) -> Vec<(BufferId, PathBuf)> {
        self.buffers
            .iter()
            .filter(|(_, e)| e.auto_reload)
            .filter_map(|(&id, e)| e.path.as_ref().map(|p| (id, p.clone())))
            .collect()
    }

    /// Returns paths of currently-open path-bound buffers in lexicographic
    /// order. Scratch buffers (with no path) are skipped. The deterministic
    /// ordering matches what the file finder shows for the All scope.
    pub(crate) fn open_paths(&self) -> Vec<PathBuf> {
        let mut paths: Vec<PathBuf> = self.path_to_id.keys().cloned().collect();
        paths.sort();
        paths
    }

    /// Every buffer whose `dirty` flag is set: path-bound first sorted by
    /// path, scratch buffers after sorted by id. Used by `QuitAll` to drive
    /// the unsaved-buffers confirmation modal.
    pub(crate) fn dirty_buffers(&self) -> Vec<DirtyBuffer> {
        let mut out: Vec<DirtyBuffer> = self
            .buffers
            .iter()
            .filter(|(_, entry)| entry.buffer.read().expect("buffer poisoned").dirty)
            .map(|(id, entry)| DirtyBuffer {
                id: *id,
                path: entry.path.clone(),
            })
            .collect();
        out.sort_by(|a, b| match (&a.path, &b.path) {
            (Some(ap), Some(bp)) => ap.cmp(bp),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => a.id.cmp(&b.id),
        });
        out
    }

    pub(crate) fn language_for(&self, id: BufferId) -> Option<Arc<Language>> {
        self.buffers.get(&id)?.language.clone()
    }

    pub(crate) fn set_language(&mut self, id: BufferId, lang: Arc<Language>) {
        if let Some(entry) = self.buffers.get_mut(&id) {
            entry.language = Some(lang);
            entry.syntax = None;
            entry.syntax_map = None;
            entry.tokens = None;
            entry.lsp_tokens = None;
            entry.lsp_symbol_kinds = None;
        }
    }

    /// All buffer ids flagged as preview surfaces. The parse pipeline
    /// pulls these into its visibility set so syntax highlighting
    /// reaches transient preview panes (currently only the file
    /// finder).
    pub(crate) fn preview_buffer_ids(&self) -> Vec<BufferId> {
        self.buffers
            .iter()
            .filter_map(|(id, entry)| entry.preview.then_some(*id))
            .collect()
    }

    /// Path-bearing buffers with no language assigned yet.
    ///
    /// Session restore rebuilds every buffer with `language: None`
    /// because the language is regenerable and deliberately not
    /// persisted. Restored file buffers therefore need their language
    /// re-detected from the path before the parse pipeline will
    /// highlight them, since it skips any buffer whose language is
    /// `None`. Scratch buffers, lacking a path, are excluded.
    pub(crate) fn buffers_needing_language(&self) -> Vec<(BufferId, PathBuf)> {
        self.buffers
            .iter()
            .filter(|(_, entry)| entry.language.is_none())
            .filter_map(|(id, entry)| entry.path.clone().map(|path| (*id, path)))
            .collect()
    }

    /// Drop any cached syntax / syntax_map for `id`. Used by callers
    /// that swap a preview buffer's content -- the new content's
    /// syntax must be parsed from scratch, not merged into stale
    /// state.
    pub(crate) fn clear_syntax(&mut self, id: BufferId) {
        if let Some(entry) = self.buffers.get_mut(&id) {
            if let Some(state) = entry.syntax.take() {
                drop_syntax_in_background(state);
            }
            entry.syntax_map = None;
            entry.tokens = None;
            entry.lsp_tokens = None;
            entry.lsp_symbol_kinds = None;
        }
    }

    pub(crate) fn syntax_version(&self, id: BufferId) -> Option<u64> {
        self.buffers.get(&id)?.syntax.as_ref().map(|s| s.version)
    }

    /// Borrow the stored [`SyntaxState`] (tree plus the rope it parsed) for `id`,
    /// if the parse pipeline has produced one. Used by auto-indent to read the
    /// syntax tree.
    pub(crate) fn syntax(&self, id: BufferId) -> Option<&SyntaxState> {
        self.buffers.get(&id)?.syntax.as_ref()
    }

    pub(crate) fn store_syntax(&mut self, id: BufferId, state: SyntaxState) {
        if let Some(entry) = self.buffers.get_mut(&id) {
            // Send the displaced state to a background drainer so its
            // potentially-large tree-sitter tree drops off the main thread.
            if let Some(prev) = entry.syntax.replace(state) {
                drop_syntax_in_background(prev);
            }
        }
    }

    /// Retain the tree-sitter highlight tokens the parse pipeline installed onto
    /// this buffer's editors, so a later fresh editor can be seeded from them.
    pub(crate) fn store_tokens(
        &mut self,
        id: BufferId,
        tokens: Arc<[SemanticTokenHighlight]>,
        interner: Arc<HighlightStyleInterner>,
    ) {
        if let Some(entry) = self.buffers.get_mut(&id) {
            entry.tokens = Some((tokens, interner));
        }
    }

    /// The retained `(tokens, interner)` pair for `id`, if the parse pipeline
    /// has produced tree-sitter tokens for it.
    pub(crate) fn tokens_for(
        &self,
        id: BufferId,
    ) -> Option<(Arc<[SemanticTokenHighlight]>, Arc<HighlightStyleInterner>)> {
        self.buffers.get(&id)?.tokens.clone()
    }

    /// Retain the LSP semantic tokens computed for `id` at buffer `version`, so
    /// a fresh editor can be seeded and the trigger can skip a re-request while
    /// the version still matches.
    pub(crate) fn store_lsp_tokens(
        &mut self,
        id: BufferId,
        version: u64,
        tokens: Arc<[SemanticTokenHighlight]>,
        interner: Arc<HighlightStyleInterner>,
    ) {
        if let Some(entry) = self.buffers.get_mut(&id) {
            entry.lsp_tokens = Some((version, tokens, interner));
        }
    }

    /// The retained `(version, tokens, interner)` triple for `id`, if an LSP
    /// semantic-tokens response has been applied to it.
    pub(crate) fn lsp_tokens_for(
        &self,
        id: BufferId,
    ) -> Option<(
        u64,
        Arc<[SemanticTokenHighlight]>,
        Arc<HighlightStyleInterner>,
    )> {
        self.buffers.get(&id)?.lsp_tokens.clone()
    }

    /// Retain the anchored symbol-kind index from the LSP semantic-tokens
    /// response for `id`, replacing any prior index.
    ///
    /// Stored even when empty, so a buffer whose response carried no symbol
    /// tokens reads as an existing index with no match rather than as having no
    /// index at all.
    pub(crate) fn store_lsp_symbol_kinds(&mut self, id: BufferId, kinds: LspSymbolKindIndex) {
        if let Some(entry) = self.buffers.get_mut(&id) {
            entry.lsp_symbol_kinds = Some(kinds);
        }
    }

    /// The [`LspSymbolKind`] naming the symbol at buffer `offset`, resolved
    /// against the buffer's current text so it tracks edits.
    ///
    /// Returns `None` when no index exists (no server, or the response has not
    /// arrived), `Some(None)` when the index exists but no token covers `offset`,
    /// and `Some(Some(kind))` when one does. A caller shows every option for
    /// `None` and hides the symbol-targeted ones for `Some(None)`.
    pub(crate) fn lsp_symbol_kind_at(
        &self,
        id: BufferId,
        offset: usize,
    ) -> Option<Option<LspSymbolKind>> {
        let entry = self.buffers.get(&id)?;
        let index = entry.lsp_symbol_kinds.as_ref()?;
        let snapshot = entry.buffer.read().ok()?.snapshot.clone();

        // Tokens are start-anchor sorted, so the last one starting at or before
        // offset is the only candidate whose span can contain it.
        let after =
            index.partition_point(|(range, _)| snapshot.resolve_anchor(&range.start) <= offset);
        let Some((range, kind)) = after.checked_sub(1).map(|i| &index[i]) else {
            return Some(None);
        };
        if snapshot.resolve_anchor(&range.end) > offset {
            Some(Some(*kind))
        } else {
            Some(None)
        }
    }

    /// Stamp `id` as the most-recently-shown buffer for highlight-eviction
    /// recency, called whenever a buffer is shown in a pane.
    pub(crate) fn mark_shown(&mut self, id: BufferId) {
        self.shown_counter += 1;
        if let Some(entry) = self.buffers.get_mut(&id) {
            entry.last_shown = self.shown_counter;
        }
    }

    /// Evict retained highlight state from the least-recently-shown hidden
    /// buffers, keeping at most `cap` of them beyond the `visible` set. Returns
    /// the evicted ids.
    ///
    /// A buffer is a candidate when it holds any highlight state (syntax tree,
    /// syntax map, tree-sitter or LSP tokens) and is not in `visible`. The
    /// oldest candidates past `cap` have that state dropped, with the syntax
    /// tree draining on a background thread as in [`Self::clear_syntax`].
    pub(crate) fn evict_hidden_highlights(
        &mut self,
        visible: &[BufferId],
        cap: usize,
    ) -> Vec<BufferId> {
        let mut candidates: Vec<(BufferId, u64)> = self
            .buffers
            .iter()
            .filter(|(id, entry)| {
                !visible.contains(id)
                    && (entry.syntax.is_some()
                        || entry.syntax_map.is_some()
                        || entry.tokens.is_some()
                        || entry.lsp_tokens.is_some()
                        || entry.lsp_symbol_kinds.is_some())
            })
            .map(|(id, entry)| (*id, entry.last_shown))
            .collect();
        if candidates.len() <= cap {
            return Vec::new();
        }

        candidates.sort_by_key(|(_, last_shown)| *last_shown);
        let evict_count = candidates.len() - cap;
        let evicted: Vec<BufferId> = candidates
            .iter()
            .take(evict_count)
            .map(|(id, _)| *id)
            .collect();

        for id in &evicted {
            if let Some(entry) = self.buffers.get_mut(id) {
                if let Some(state) = entry.syntax.take() {
                    drop_syntax_in_background(state);
                }
                entry.syntax_map = None;
                entry.tokens = None;
                entry.lsp_tokens = None;
                entry.lsp_symbol_kinds = None;
            }
        }
        evicted
    }

    /// Move the prior [`SyntaxState`] out of the registry. The caller is
    /// expected to update it (`tree.edit` + reparse) and put it back via
    /// [`Self::store_syntax`]. Returns `None` if no state has been stored.
    pub(crate) fn take_syntax(&mut self, id: BufferId) -> Option<SyntaxState> {
        self.buffers.get_mut(&id)?.syntax.take()
    }

    /// Borrow the multi-layer [`SyntaxMap`] for `id`, if one has been
    /// installed by the parse pipeline. Used by the capture-merging
    /// highlight path.
    #[allow(dead_code)]
    pub(crate) fn syntax_map(&self, id: BufferId) -> Option<&SyntaxMap> {
        self.buffers.get(&id)?.syntax_map.as_ref()
    }

    /// Replace the multi-layer [`SyntaxMap`] for `id`. Called by
    /// `parse_buffer_step` after each successful reparse so the
    /// capture-merging consumers always see the latest layer set.
    pub(crate) fn store_syntax_map(&mut self, id: BufferId, map: SyntaxMap) {
        if let Some(entry) = self.buffers.get_mut(&id) {
            entry.syntax_map = Some(map);
        }
    }

    /// Move the multi-layer [`SyntaxMap`] for `id` out of the
    /// registry, so the next reparse can interpolate it incrementally
    /// before reinstalling.
    pub(crate) fn take_syntax_map(&mut self, id: BufferId) -> Option<SyntaxMap> {
        self.buffers.get_mut(&id)?.syntax_map.take()
    }

    /// Return a cached [`DiffResult`] for `(buffer, base_text)` if one
    /// was stored against the current buffer version and base
    /// fingerprint; otherwise `None`. Callers recompute and cache via
    /// [`Self::store_diff`] on miss.
    #[allow(dead_code)]
    pub(crate) fn cached_diff(
        &self,
        id: BufferId,
        buffer_version: u64,
        base_fingerprint: [u8; 32],
    ) -> Option<Arc<DiffResult>> {
        let entry = self.buffers.get(&id)?.diff.as_ref()?;
        if entry.buffer_version == buffer_version && entry.base_fingerprint == base_fingerprint {
            Some(entry.result.clone())
        } else {
            None
        }
    }

    /// Store a newly-computed [`DiffResult`] for `id`. Supersedes any
    /// prior cache entry regardless of version/fingerprint; callers
    /// that want stale detection should check [`Self::cached_diff`]
    /// before recomputing.
    #[allow(dead_code)]
    pub(crate) fn store_diff(
        &mut self,
        id: BufferId,
        buffer_version: u64,
        base_fingerprint: [u8; 32],
        result: Arc<DiffResult>,
    ) {
        if let Some(entry) = self.buffers.get_mut(&id) {
            entry.diff = Some(CachedDiff {
                buffer_version,
                base_fingerprint,
                result,
            });
        }
    }

    /// Drop any cached diff for `id`. Call when the buffer's base
    /// text changes or when the buffer is closed.
    #[allow(dead_code)]
    pub(crate) fn invalidate_diff(&mut self, id: BufferId) {
        if let Some(entry) = self.buffers.get_mut(&id) {
            entry.diff = None;
        }
    }

    /// Capture the registry state for persistence. Each entry carries its
    /// full [`BufferHistory`] so replay on restore reconstructs identical
    /// fragment trees and anchors. Scratch buffers (no path) are included so
    /// their edit history also round-trips.
    pub(crate) fn snapshot(&self) -> BufferRegistrySnapshot {
        let mut entries: Vec<BufferEntrySnap> = self
            .buffers
            .iter()
            .map(|(id, entry)| BufferEntrySnap {
                id: *id,
                path: entry.path.clone(),
                history: {
                    let guard = entry.buffer.read().expect("buffer poisoned");
                    guard.history()
                },
            })
            .collect();
        entries.sort_by_key(|e| e.id);
        BufferRegistrySnapshot {
            entries,
            next_id: self.next_id,
        }
    }

    /// Rehydrate a registry from a [`BufferRegistrySnapshot`]. For each entry
    /// the saved [`BufferHistory`] is replayed on a fresh buffer, which
    /// reconstructs the fragment tree, undo stack, and dirty state exactly as
    /// they were at save time. The on-disk file is not read: if it has drifted
    /// we'd have to choose between it and the saved edits, and the saved edits
    /// win unconditionally since persistence represents the user's explicit
    /// last-known state.
    pub(crate) fn restore_from(&mut self, snap: BufferRegistrySnapshot) {
        self.buffers.clear();
        self.path_to_id.clear();
        self.next_id = snap.next_id.max(1);

        for entry in snap.entries {
            let buffer = Arc::new(RwLock::new(TextBuffer::from_history(
                entry.id,
                &entry.history,
            )));
            if let Some(path) = entry.path.as_ref() {
                self.path_to_id.insert(path.clone(), entry.id);
            }
            self.buffers.insert(
                entry.id,
                BufferEntry {
                    buffer,
                    path: entry.path,
                    language: None,
                    syntax: None,
                    syntax_map: None,
                    tokens: None,
                    lsp_tokens: None,
                    lsp_symbol_kinds: None,
                    diff: None,
                    preview: false,
                    disk_mtime: None,
                    auto_reload: false,
                    last_shown: 0,
                },
            );
        }
    }
}

/// Serializable view of [`BufferRegistry`]. Each entry carries its
/// [`BufferHistory`] (the replayable op log) so restoration reconstructs the
/// fragment tree, anchors, undo stack, and dirty state exactly. Syntax and
/// diff caches are regenerable and deliberately not persisted.
#[derive(Debug, Default, Serialize, Deserialize)]
pub(crate) struct BufferRegistrySnapshot {
    pub entries: Vec<BufferEntrySnap>,
    pub next_id: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct BufferEntrySnap {
    pub id: BufferId,
    pub path: Option<PathBuf>,
    pub history: BufferHistory,
}

/// 32-byte blake3 hash of `text`. Used both to key [`CachedDiff`] in
/// the buffer registry and to populate
/// [`stoat_language::structural_diff::BufferRef::fingerprint`] for
/// cross-file move detection in the structural diff pipeline.
#[allow(dead_code)]
pub(crate) fn fingerprint_bytes(text: &str) -> [u8; 32] {
    blake3::hash(text.as_bytes()).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scratch_generates_unique_ids() {
        let mut reg = BufferRegistry::new();
        let (id1, _) = reg.new_scratch();
        let (id2, _) = reg.new_scratch();
        assert_ne!(id1, id2);
    }

    #[test]
    fn open_deduplicates_by_path() {
        let mut reg = BufferRegistry::new();
        let (id1, buf1) = reg.open(Path::new("/a.txt"), "hello");
        let (id2, buf2) = reg.open(Path::new("/a.txt"), "ignored");
        assert_eq!(id1, id2);
        assert!(Arc::ptr_eq(&buf1, &buf2));
        let guard = buf1.read().unwrap();
        assert_eq!(guard.rope().to_string(), "hello");
    }

    #[test]
    fn new_scratch_preview_marks_entry_and_lists_via_preview_buffer_ids() {
        let mut reg = BufferRegistry::new();
        let (plain_id, _) = reg.new_scratch();
        let (preview_id, _) = reg.new_scratch_preview();
        let preview_ids = reg.preview_buffer_ids();
        assert_eq!(preview_ids, vec![preview_id]);
        assert!(!preview_ids.contains(&plain_id));
    }

    #[test]
    fn clear_syntax_is_noop_when_no_state_stored() {
        let mut reg = BufferRegistry::new();
        let (id, _) = reg.new_scratch_preview();
        assert_eq!(reg.syntax_version(id), None);
        reg.clear_syntax(id);
        assert_eq!(reg.syntax_version(id), None);
    }

    #[test]
    fn clear_syntax_and_set_language_drop_retained_tokens() {
        use stoat_language::LanguageRegistry;

        let mut reg = BufferRegistry::new();
        let (id, _) = reg.open(Path::new("/a.rs"), "fn a() {}\n");
        let tokens: Arc<[SemanticTokenHighlight]> = Arc::from(Vec::new());
        let interner = Arc::new(HighlightStyleInterner::default());

        reg.store_tokens(id, tokens.clone(), interner.clone());
        assert!(reg.tokens_for(id).is_some(), "stored tokens are retained");

        reg.clear_syntax(id);
        assert!(
            reg.tokens_for(id).is_none(),
            "clear_syntax drops retained tokens"
        );

        reg.store_tokens(id, tokens, interner);
        let lang = LanguageRegistry::standard()
            .for_path(Path::new("/a.rs"))
            .expect("rust language");
        reg.set_language(id, lang);
        assert!(
            reg.tokens_for(id).is_none(),
            "set_language drops retained tokens"
        );
    }

    #[test]
    fn evict_hidden_highlights_spares_visible_and_the_newest_cap() {
        let mut reg = BufferRegistry::new();
        let tokens: Arc<[SemanticTokenHighlight]> = Arc::from(Vec::new());
        let interner = Arc::new(HighlightStyleInterner::default());
        let ids: Vec<BufferId> = (0..5)
            .map(|_| {
                let (id, _) = reg.new_scratch();
                reg.store_tokens(id, tokens.clone(), interner.clone());
                reg.mark_shown(id);
                id
            })
            .collect();

        // ids[0] is the oldest but marked visible, so it survives. Over the four
        // remaining hidden buffers with cap 2, the two oldest evict.
        let evicted = reg.evict_hidden_highlights(&[ids[0]], 2);
        assert_eq!(evicted, vec![ids[1], ids[2]]);
        assert!(
            reg.tokens_for(ids[0]).is_some(),
            "visible spared despite being oldest"
        );
        assert!(reg.tokens_for(ids[1]).is_none());
        assert!(reg.tokens_for(ids[2]).is_none());
        assert!(reg.tokens_for(ids[3]).is_some(), "within the newest cap");
        assert!(reg.tokens_for(ids[4]).is_some());
    }

    #[test]
    fn lsp_symbol_kind_at_reports_hit_gap_and_no_index() {
        let mut reg = BufferRegistry::new();
        let (id, buffer) = reg.open(Path::new("/a.rs"), "hello world");
        let snapshot = buffer.read().unwrap().snapshot.clone();
        let start = snapshot.anchors_at_batch(&[0usize], stoat_text::Bias::Right)[0];
        let end = snapshot.anchors_at_batch(&[5usize], stoat_text::Bias::Left)[0];
        let kinds: LspSymbolKindIndex = Arc::from(vec![(start..end, LspSymbolKind::Function)]);
        reg.store_lsp_symbol_kinds(id, kinds);

        assert_eq!(
            reg.lsp_symbol_kind_at(id, 2),
            Some(Some(LspSymbolKind::Function)),
            "an offset inside the token span resolves its kind"
        );
        assert_eq!(
            reg.lsp_symbol_kind_at(id, 8),
            Some(None),
            "an offset outside every span reports the index has no match"
        );

        let (empty, _) = reg.new_scratch();
        assert_eq!(
            reg.lsp_symbol_kind_at(empty, 0),
            None,
            "a buffer with no index reports none"
        );
    }

    #[test]
    fn open_different_paths() {
        let mut reg = BufferRegistry::new();
        let (id1, _) = reg.open(Path::new("/a.txt"), "a");
        let (id2, _) = reg.open(Path::new("/b.txt"), "b");
        assert_ne!(id1, id2);
    }

    #[test]
    fn get_existing() {
        let mut reg = BufferRegistry::new();
        let (id, original) = reg.new_scratch();
        let fetched = reg.get(id).unwrap();
        assert!(Arc::ptr_eq(&original, &fetched));
    }

    #[test]
    fn get_nonexistent() {
        let reg = BufferRegistry::new();
        assert!(reg.get(BufferId::new(999)).is_none());
    }

    #[test]
    fn path_for_scratch_is_none() {
        let mut reg = BufferRegistry::new();
        let (id, _) = reg.new_scratch();
        assert!(reg.path_for(id).is_none());
    }

    #[test]
    fn path_for_file_buffer() {
        let mut reg = BufferRegistry::new();
        let (id, _) = reg.open(Path::new("/foo/bar.rs"), "");
        assert_eq!(reg.path_for(id), Some(Path::new("/foo/bar.rs")));
    }

    #[test]
    fn diff_cache_hits_on_matching_version_and_fingerprint() {
        let mut reg = BufferRegistry::new();
        let (id, _) = reg.new_scratch();
        let result = Arc::new(DiffResult::default());
        let fp = fingerprint_bytes("base text");
        reg.store_diff(id, 7, fp, result.clone());
        let hit = reg.cached_diff(id, 7, fp).expect("cache hit");
        assert!(Arc::ptr_eq(&hit, &result));
    }

    #[test]
    fn diff_cache_miss_on_version_change() {
        let mut reg = BufferRegistry::new();
        let (id, _) = reg.new_scratch();
        let fp = fingerprint_bytes("base");
        reg.store_diff(id, 1, fp, Arc::new(DiffResult::default()));
        assert!(reg.cached_diff(id, 2, fp).is_none());
    }

    #[test]
    fn diff_cache_miss_on_fingerprint_change() {
        let mut reg = BufferRegistry::new();
        let (id, _) = reg.new_scratch();
        let fp1 = fingerprint_bytes("one");
        let fp2 = fingerprint_bytes("two");
        reg.store_diff(id, 1, fp1, Arc::new(DiffResult::default()));
        assert!(reg.cached_diff(id, 1, fp2).is_none());
    }

    #[test]
    fn diff_cache_invalidate_clears_entry() {
        let mut reg = BufferRegistry::new();
        let (id, _) = reg.new_scratch();
        let fp = fingerprint_bytes("x");
        reg.store_diff(id, 1, fp, Arc::new(DiffResult::default()));
        reg.invalidate_diff(id);
        assert!(reg.cached_diff(id, 1, fp).is_none());
    }

    #[test]
    fn fingerprint_differs_per_text() {
        assert_ne!(fingerprint_bytes("a"), fingerprint_bytes("b"));
        assert_eq!(fingerprint_bytes("abc"), fingerprint_bytes("abc"));
    }
}
