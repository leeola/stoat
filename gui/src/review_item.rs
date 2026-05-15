use crate::{
    buffer::Buffer,
    buffer_registry::BufferRegistry,
    diff_map::DiffMap,
    display_map::DisplayMap,
    editor::{Editor, EditorMode},
    globals::ExecutorGlobal,
    item::{DeserializeSnafu, ItemError, ItemView},
    multi_buffer::MultiBuffer,
    review_session::{ReviewSession, ReviewSessionEvent},
};
use gpui::{
    div, App, AppContext, Context, Entity, IntoElement, ParentElement, Render, SharedString,
    Styled, Subscription, Window,
};
use serde_json::Value;
use std::{ops::Range, path::PathBuf, sync::Arc};
use stoat::{
    buffer::BufferId,
    display_map::{BlockPlacement, BlockProperties, BlockStyle},
    review_session::{ChunkStatus, ReviewSource},
};
use stoat_scheduler::Executor;

/// Pane-hosted review surface. Wraps an [`Entity<ReviewSession>`] and
/// one [`ReviewFileView`] per file in the session.
///
/// `commit_summary` carries the optional commit subject line used by
/// [`ItemView::tab_label`] for the [`ReviewSource::Commit`] variant.
/// The workspace's `OpenReviewCommit` action populates it from the
/// git host after constructing the item; absent a summary the label
/// falls back to the short sha.
pub struct ReviewItem {
    session: Entity<ReviewSession>,
    files: Vec<ReviewFileView>,
    commit_summary: Option<String>,
    buffer_registry: Option<Entity<BufferRegistry>>,
    /// `(chunk, index)` cursor for the `JumpToNextMoveSource` /
    /// `JumpToPrevMoveSource` cycle. Cleared whenever the
    /// session's chunk cursor moves.
    move_cursor: Option<(stoat::review_session::ReviewChunkId, usize)>,
    _session_subscription: Option<Subscription>,
}

/// One file's view state: the workspace-relative path, the editor
/// over the file's review excerpts, the underlying multi-buffer
/// that holds those excerpts, and the source buffer the multi-buffer
/// reads from.
pub struct ReviewFileView {
    pub rel_path: String,
    pub editor: Entity<Editor>,
    pub multi_buffer: Entity<MultiBuffer>,
    pub buffer: Entity<Buffer>,
}

impl ReviewItem {
    pub fn new(session: Entity<ReviewSession>, files: Vec<ReviewFileView>) -> Self {
        Self {
            session,
            files,
            commit_summary: None,
            buffer_registry: None,
            move_cursor: None,
            _session_subscription: None,
        }
    }

    /// Build a [`ReviewItem`] for `session`, materializing one
    /// [`ReviewFileView`] per file in the session.
    ///
    /// For [`ReviewSource::WorkingTree`], each file's buffer comes
    /// from `buffer_registry` so edits and LSP attach to the
    /// workspace's live working-tree buffer. For all other sources
    /// the file's buffer is a fresh read-only [`Buffer`] materialized
    /// from the session's stored `buffer_text`.
    ///
    /// Reads [`ExecutorGlobal`] for the per-file [`DisplayMap`]; the
    /// caller must install it before constructing the entity.
    pub fn from_session(
        session: Entity<ReviewSession>,
        buffer_registry: &Entity<BufferRegistry>,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let (source_kind, file_specs) = snapshot_session(&session, cx);
        let executor = cx.global::<ExecutorGlobal>().0.clone();
        let files: Vec<ReviewFileView> = file_specs
            .into_iter()
            .enumerate()
            .map(|(file_index, spec)| {
                let view =
                    build_file_view(spec, source_kind, buffer_registry, executor.clone(), cx);
                view.editor.update(cx, |ed, cx| {
                    ed.set_review_session(Some(session.clone()), cx);
                    ed.set_review_file_index(Some(file_index), cx);
                });
                view
            })
            .collect();
        let subscription = cx.subscribe(&session, |this, _, event: &ReviewSessionEvent, cx| {
            if matches!(event, ReviewSessionEvent::Refreshed) {
                this.rebuild_files(cx);
                this.move_cursor = None;
            }
            cx.notify();
        });
        Self {
            session,
            files,
            commit_summary: None,
            buffer_registry: Some(buffer_registry.clone()),
            move_cursor: None,
            _session_subscription: Some(subscription),
        }
    }

    pub fn move_cursor(&self) -> Option<(stoat::review_session::ReviewChunkId, usize)> {
        self.move_cursor
    }

    pub fn set_move_cursor(
        &mut self,
        cursor: Option<(stoat::review_session::ReviewChunkId, usize)>,
    ) {
        self.move_cursor = cursor;
    }

    /// Rebuild every [`ReviewFileView`] from the current
    /// [`ReviewSession`] state. Called when the session emits
    /// [`ReviewSessionEvent::Refreshed`] so excerpts, deletion
    /// blocks, and the file-header text reflect the freshly
    /// extracted hunks. Reuses [`BufferRegistry`]-backed buffers for
    /// working-tree sources via the registry stored at
    /// [`Self::from_session`] time; without a registry (items
    /// constructed via [`Self::new`] in tests) this method is a
    /// no-op.
    pub fn rebuild_files(&mut self, cx: &mut Context<'_, Self>) {
        let Some(registry) = self.buffer_registry.clone() else {
            return;
        };
        let (source_kind, file_specs) = snapshot_session(&self.session, cx);
        let executor = cx.global::<ExecutorGlobal>().0.clone();
        let session = self.session.clone();
        let files: Vec<ReviewFileView> = file_specs
            .into_iter()
            .enumerate()
            .map(|(file_index, spec)| {
                let view = build_file_view(spec, source_kind, &registry, executor.clone(), cx);
                view.editor.update(cx, |ed, cx| {
                    ed.set_review_session(Some(session.clone()), cx);
                    ed.set_review_file_index(Some(file_index), cx);
                });
                view
            })
            .collect();
        self.files = files;
        cx.notify();
    }

    /// Attach the commit subject line consumed by [`ItemView::tab_label`]
    /// for [`ReviewSource::Commit`]. Other variants ignore this field.
    pub fn set_commit_summary(&mut self, summary: Option<String>, cx: &mut Context<'_, Self>) {
        if self.commit_summary == summary {
            return;
        }
        self.commit_summary = summary;
        cx.notify();
    }

    pub fn commit_summary(&self) -> Option<&str> {
        self.commit_summary.as_deref()
    }

    pub fn session(&self) -> &Entity<ReviewSession> {
        &self.session
    }

    pub fn files(&self) -> &[ReviewFileView] {
        &self.files
    }

    /// File index of the chunk under the session's cursor, or `None`
    /// when the session has no current chunk or the cursor's chunk is
    /// missing from the chunk map.
    pub fn active_file_index(&self, cx: &App) -> Option<usize> {
        let session = self.session.read(cx).inner();
        let id = session.cursor.current?;
        session.chunks.get(&id).map(|chunk| chunk.file_index)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SourceKind {
    WorkingTree,
    ReadOnly,
}

struct FileSpec {
    path: PathBuf,
    rel_path: String,
    buffer_text: Arc<String>,
    excerpt_ranges: Vec<Range<usize>>,
    deletion_blocks: Vec<BlockProperties>,
    header_block: BlockProperties,
}

fn snapshot_session(session: &Entity<ReviewSession>, cx: &App) -> (SourceKind, Vec<FileSpec>) {
    let session_ref = session.read(cx);
    let inner = session_ref.inner();
    let source_kind = match &inner.source {
        ReviewSource::WorkingTree { .. } => SourceKind::WorkingTree,
        _ => SourceKind::ReadOnly,
    };
    let file_specs = inner
        .files
        .iter()
        .map(|file| {
            let mut excerpt_ranges = Vec::new();
            let mut deletion_blocks = Vec::new();
            let mut staged_count = 0usize;
            for chunk_id in &file.chunks {
                let Some(chunk) = inner.chunks.get(chunk_id) else {
                    continue;
                };
                if matches!(chunk.status, ChunkStatus::Staged) {
                    staged_count += 1;
                }
                if !chunk.buffer_byte_range.is_empty() {
                    excerpt_ranges.push(chunk.buffer_byte_range.clone());
                }
                if chunk.base_byte_range.is_empty() {
                    continue;
                }
                let Some(slice) = file.base_text.get(chunk.base_byte_range.clone()) else {
                    continue;
                };
                let lines: Vec<String> = slice.lines().map(String::from).collect();
                if lines.is_empty() {
                    continue;
                }
                deletion_blocks.push(BlockProperties::from_text(
                    BlockPlacement::Above(chunk.buffer_line_range.start),
                    lines,
                    BlockStyle::Fixed,
                ));
            }
            let header_text = format!(
                "> {}   {}/{} staged",
                file.rel_path,
                staged_count,
                file.chunks.len()
            );
            let header_block = BlockProperties::from_text(
                BlockPlacement::Above(0),
                vec![header_text],
                BlockStyle::Sticky,
            );
            FileSpec {
                path: file.path.clone(),
                rel_path: file.rel_path.clone(),
                buffer_text: file.buffer_text.clone(),
                excerpt_ranges,
                deletion_blocks,
                header_block,
            }
        })
        .collect();
    (source_kind, file_specs)
}

fn build_file_view(
    spec: FileSpec,
    source_kind: SourceKind,
    buffer_registry: &Entity<BufferRegistry>,
    executor: Executor,
    cx: &mut Context<'_, ReviewItem>,
) -> ReviewFileView {
    let buffer = match source_kind {
        SourceKind::WorkingTree => {
            let (_, shared) =
                buffer_registry.update(cx, |reg, cx| reg.open(&spec.path, &spec.buffer_text, cx));
            let buffer = cx.new(|_| Buffer::from_shared(shared));
            buffer.update(cx, |b, cx| b.set_file_path(Some(spec.path.clone()), cx));
            buffer
        },
        SourceKind::ReadOnly => cx.new(|_| Buffer::with_text(BufferId::new(0), &spec.buffer_text)),
    };

    let multi_buffer = {
        let buffer = buffer.clone();
        let excerpt_ranges = spec.excerpt_ranges;
        cx.new(|cx| {
            let mut m = MultiBuffer::singleton(buffer.clone(), cx);
            if !excerpt_ranges.is_empty() {
                m.insert_excerpts(buffer, excerpt_ranges, cx);
            }
            m
        })
    };

    let display_map = {
        let buffer = buffer.clone();
        cx.new(|cx| DisplayMap::new(buffer, executor, cx))
    };
    let diff_map = {
        let buffer = buffer.clone();
        cx.new(|cx| DiffMap::new(buffer, cx))
    };

    let mut blocks = Vec::with_capacity(spec.deletion_blocks.len() + 1);
    blocks.push(spec.header_block);
    blocks.extend(spec.deletion_blocks);
    display_map.update(cx, |dm, cx| dm.insert_blocks(blocks, cx));

    let editor = cx.new(|cx| {
        Editor::new(
            multi_buffer.clone(),
            display_map,
            diff_map,
            EditorMode::full(),
            cx,
        )
    });

    ReviewFileView {
        rel_path: spec.rel_path,
        editor,
        multi_buffer,
        buffer,
    }
}

impl Render for ReviewItem {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let active = self.active_file_index(cx);
        let children: Vec<_> = self
            .files
            .iter()
            .enumerate()
            .map(|(index, file)| {
                let dimmed = active.is_some_and(|a| a != index);
                div()
                    .flex_1()
                    .opacity(if dimmed { 0.6 } else { 1.0 })
                    .child(file.editor.clone())
            })
            .collect();
        div().flex().flex_col().size_full().children(children)
    }
}

impl ItemView for ReviewItem {
    fn tab_label(&self, cx: &App) -> SharedString {
        review_source_label(
            &self.session.read(cx).inner().source,
            self.commit_summary.as_deref(),
        )
        .into()
    }

    fn deserialize(_value: Value, _cx: &mut Context<'_, Self>) -> Result<Self, ItemError> {
        DeserializeSnafu {
            reason: "ReviewItem deserialize requires workspace-persistence wiring \
                     that has not yet landed",
        }
        .fail()
    }
}

fn review_source_label(source: &ReviewSource, commit_summary: Option<&str>) -> String {
    match source {
        ReviewSource::WorkingTree { workdir } => {
            let name = workdir
                .file_name()
                .and_then(|n| n.to_str())
                .map(String::from)
                .unwrap_or_else(|| workdir.display().to_string());
            format!("Review: {name}")
        },
        ReviewSource::Commit { sha, .. } => match commit_summary {
            Some(summary) => format!("Commit: {}: {}", short_sha(sha), summary),
            None => format!("Commit: {}", short_sha(sha)),
        },
        ReviewSource::CommitRange { from, to, .. } => {
            format!("Range: {}..{}", short_sha(from), short_sha(to))
        },
        ReviewSource::AgentEdits { .. } => String::from("Agent edits"),
        ReviewSource::InMemory { .. } => String::from("Review: in-memory"),
    }
}

fn short_sha(sha: &str) -> String {
    sha.chars().take(7).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AppContext, TestAppContext};
    use std::{path::PathBuf, sync::Arc};
    use stoat::{review::ReviewFileInput, review_session::ReviewSession as InnerSession};
    use stoat_scheduler::TestScheduler;

    fn install_executor(cx: &mut TestAppContext) {
        cx.update(|cx| {
            cx.set_global(ExecutorGlobal(Executor::new(
                Arc::new(TestScheduler::new()),
            )));
        });
    }

    fn make_session(cx: &mut TestAppContext, source: ReviewSource) -> Entity<ReviewSession> {
        cx.update(|cx| cx.new(|_| ReviewSession::new(InnerSession::new(source))))
    }

    fn session_with_file(
        cx: &mut TestAppContext,
        source: ReviewSource,
        path: &str,
        base_text: &str,
        buffer_text: &str,
    ) -> Entity<ReviewSession> {
        cx.update(|cx| {
            cx.new(|_| {
                let mut inner = InnerSession::new(source);
                inner.add_files(vec![ReviewFileInput {
                    path: PathBuf::from(path),
                    rel_path: path.to_string(),
                    language: None,
                    base_text: Arc::new(base_text.to_string()),
                    buffer_text: Arc::new(buffer_text.to_string()),
                }]);
                ReviewSession::new(inner)
            })
        })
    }

    fn new_item(cx: &mut TestAppContext, source: ReviewSource) -> Entity<ReviewItem> {
        cx.update(|cx| {
            let session = cx.new(|_| ReviewSession::new(InnerSession::new(source)));
            cx.new(|_| ReviewItem::new(session, Vec::new()))
        })
    }

    #[test]
    fn tab_label_reflects_working_tree_source() {
        let mut cx = TestAppContext::single();
        let item = new_item(
            &mut cx,
            ReviewSource::WorkingTree {
                workdir: PathBuf::from("/repos/stoat"),
            },
        );
        item.read_with(&cx, |item, app| {
            assert_eq!(item.tab_label(app), SharedString::from("Review: stoat"));
        });
    }

    #[test]
    fn tab_label_reflects_commit_source() {
        let mut cx = TestAppContext::single();
        let item = new_item(
            &mut cx,
            ReviewSource::Commit {
                workdir: PathBuf::from("/repos/stoat"),
                sha: "abcdef1234567890".to_string(),
            },
        );
        item.read_with(&cx, |item, app| {
            assert_eq!(item.tab_label(app), SharedString::from("Commit: abcdef1"));
        });
    }

    #[test]
    fn tab_label_reflects_commit_source_with_summary() {
        let mut cx = TestAppContext::single();
        let item = new_item(
            &mut cx,
            ReviewSource::Commit {
                workdir: PathBuf::from("/repos/stoat"),
                sha: "abcdef1234567890".to_string(),
            },
        );
        item.update(&mut cx, |item, cx| {
            item.set_commit_summary(Some("fix the thing".to_string()), cx);
        });
        item.read_with(&cx, |item, app| {
            assert_eq!(
                item.tab_label(app),
                SharedString::from("Commit: abcdef1: fix the thing"),
            );
        });
    }

    #[test]
    fn set_commit_summary_clears_when_passed_none() {
        let mut cx = TestAppContext::single();
        let item = new_item(
            &mut cx,
            ReviewSource::Commit {
                workdir: PathBuf::from("/repos/stoat"),
                sha: "abc".to_string(),
            },
        );
        item.update(&mut cx, |item, cx| {
            item.set_commit_summary(Some("first".to_string()), cx);
            item.set_commit_summary(None, cx);
        });
        item.read_with(&cx, |item, _| {
            assert_eq!(item.commit_summary(), None);
        });
    }

    #[test]
    fn tab_label_reflects_commit_range_source() {
        let mut cx = TestAppContext::single();
        let item = new_item(
            &mut cx,
            ReviewSource::CommitRange {
                workdir: PathBuf::from("/repos/stoat"),
                from: "1111111aaaa".to_string(),
                to: "2222222bbbb".to_string(),
            },
        );
        item.read_with(&cx, |item, app| {
            assert_eq!(
                item.tab_label(app),
                SharedString::from("Range: 1111111..2222222")
            );
        });
    }

    #[test]
    fn tab_label_reflects_agent_edits_source() {
        let mut cx = TestAppContext::single();
        let item = new_item(
            &mut cx,
            ReviewSource::AgentEdits {
                edits: Arc::new(Vec::new()),
            },
        );
        item.read_with(&cx, |item, app| {
            assert_eq!(item.tab_label(app), SharedString::from("Agent edits"));
        });
    }

    #[test]
    fn tab_label_reflects_in_memory_source() {
        let mut cx = TestAppContext::single();
        let item = new_item(
            &mut cx,
            ReviewSource::InMemory {
                files: Arc::new(Vec::new()),
            },
        );
        item.read_with(&cx, |item, app| {
            assert_eq!(item.tab_label(app), SharedString::from("Review: in-memory"));
        });
    }

    #[test]
    fn is_dirty_is_false() {
        let mut cx = TestAppContext::single();
        let item = new_item(
            &mut cx,
            ReviewSource::InMemory {
                files: Arc::new(Vec::new()),
            },
        );
        item.read_with(&cx, |item, app| {
            assert!(!item.is_dirty(app));
        });
    }

    #[test]
    fn deserialize_returns_error_until_persistence_wires_through() {
        let mut cx = TestAppContext::single();
        let item = new_item(
            &mut cx,
            ReviewSource::InMemory {
                files: Arc::new(Vec::new()),
            },
        );
        let err = item.update(&mut cx, |_, cx| {
            ReviewItem::deserialize(Value::Null, cx).err()
        });
        assert!(matches!(err, Some(ItemError::Deserialize { .. })));
    }

    #[test]
    fn from_session_with_empty_session_creates_empty_files_list() {
        let mut cx = TestAppContext::single();
        install_executor(&mut cx);
        let session = make_session(
            &mut cx,
            ReviewSource::InMemory {
                files: Arc::new(Vec::new()),
            },
        );
        let registry = cx.update(|cx| cx.new(|_| BufferRegistry::new()));

        let item = cx.update(|cx| {
            let session = session.clone();
            let registry = registry.clone();
            cx.new(|cx| ReviewItem::from_session(session, &registry, cx))
        });

        item.read_with(&cx, |item, _| {
            assert!(item.files().is_empty());
        });
    }

    #[test]
    fn from_session_with_in_memory_source_creates_one_view_per_file() {
        let mut cx = TestAppContext::single();
        install_executor(&mut cx);
        let session = session_with_file(
            &mut cx,
            ReviewSource::InMemory {
                files: Arc::new(Vec::new()),
            },
            "a.txt",
            "alpha\nbeta\n",
            "alpha modified\nbeta\n",
        );
        let registry = cx.update(|cx| cx.new(|_| BufferRegistry::new()));

        let item = cx.update(|cx| {
            let session = session.clone();
            let registry = registry.clone();
            cx.new(|cx| ReviewItem::from_session(session, &registry, cx))
        });

        item.read_with(&cx, |item, _| {
            assert_eq!(item.files().len(), 1);
            assert_eq!(item.files()[0].rel_path, "a.txt");
        });
        registry.read_with(&cx, |r, _| {
            assert_eq!(r.len(), 0, "in-memory source must not register buffers");
        });
    }

    #[test]
    fn from_session_with_working_tree_source_registers_buffer() {
        let mut cx = TestAppContext::single();
        install_executor(&mut cx);
        let session = session_with_file(
            &mut cx,
            ReviewSource::WorkingTree {
                workdir: PathBuf::from("/repos/stoat"),
            },
            "/repos/stoat/a.txt",
            "alpha\n",
            "alpha modified\n",
        );
        let registry = cx.update(|cx| cx.new(|_| BufferRegistry::new()));

        let _item = cx.update(|cx| {
            let session = session.clone();
            let registry = registry.clone();
            cx.new(|cx| ReviewItem::from_session(session, &registry, cx))
        });

        registry.read_with(&cx, |r, _| {
            assert_eq!(r.len(), 1);
            assert!(r
                .id_for_path(&PathBuf::from("/repos/stoat/a.txt"))
                .is_some());
        });
    }

    #[test]
    fn from_session_builds_multi_buffer_per_file() {
        let mut cx = TestAppContext::single();
        install_executor(&mut cx);
        let session = session_with_file(
            &mut cx,
            ReviewSource::InMemory {
                files: Arc::new(Vec::new()),
            },
            "a.txt",
            "alpha\nbeta\ngamma\n",
            "alpha modified\nbeta\ngamma\n",
        );
        let registry = cx.update(|cx| cx.new(|_| BufferRegistry::new()));

        let item = cx.update(|cx| {
            let session = session.clone();
            let registry = registry.clone();
            cx.new(|cx| ReviewItem::from_session(session, &registry, cx))
        });

        item.read_with(&cx, |item, cx| {
            let view = &item.files()[0];
            assert!(!view.multi_buffer.read(cx).is_singleton());
        });
    }

    #[test]
    fn from_session_inserts_deletion_blocks_for_modified_chunks() {
        let mut cx = TestAppContext::single();
        install_executor(&mut cx);
        let with_block = session_with_file(
            &mut cx,
            ReviewSource::InMemory {
                files: Arc::new(Vec::new()),
            },
            "a.txt",
            "old line\n",
            "new line\n",
        );
        let no_block = session_with_file(
            &mut cx,
            ReviewSource::InMemory {
                files: Arc::new(Vec::new()),
            },
            "b.txt",
            "",
            "added line\n",
        );
        let registry = cx.update(|cx| cx.new(|_| BufferRegistry::new()));

        let item_with = cx.update(|cx| {
            let session = with_block.clone();
            let registry = registry.clone();
            cx.new(|cx| ReviewItem::from_session(session, &registry, cx))
        });
        let item_without = cx.update(|cx| {
            let session = no_block.clone();
            let registry = registry.clone();
            cx.new(|cx| ReviewItem::from_session(session, &registry, cx))
        });

        let display_with = item_with.read_with(&cx, |item, cx| {
            item.files()[0].editor.read(cx).display_map().clone()
        });
        let display_without = item_without.read_with(&cx, |item, cx| {
            item.files()[0].editor.read(cx).display_map().clone()
        });

        let max_with = display_with.update(&mut cx, |dm, _| dm.snapshot().max_point().row);
        let max_without = display_without.update(&mut cx, |dm, _| dm.snapshot().max_point().row);

        assert!(
            max_with > max_without,
            "deletion block must add at least one display row \
             (with_block max_point.row={max_with}, no_block max_point.row={max_without})",
        );
    }

    #[test]
    fn from_session_inserts_file_header_block() {
        use stoat::display_map::BlockRowKind;
        let mut cx = TestAppContext::single();
        install_executor(&mut cx);
        let session = session_with_file(
            &mut cx,
            ReviewSource::InMemory {
                files: Arc::new(Vec::new()),
            },
            "a.txt",
            "",
            "alpha\n",
        );
        let registry = cx.update(|cx| cx.new(|_| BufferRegistry::new()));

        let item = cx.update(|cx| {
            let session = session.clone();
            let registry = registry.clone();
            cx.new(|cx| ReviewItem::from_session(session, &registry, cx))
        });

        let display = item.read_with(&cx, |item, cx| {
            item.files()[0].editor.read(cx).display_map().clone()
        });
        let row_0_is_block = display.update(&mut cx, |dm, _| {
            matches!(dm.snapshot().classify_row(0), BlockRowKind::Block { .. })
        });
        assert!(
            row_0_is_block,
            "row 0 must be the file-header block, not a buffer row",
        );
    }

    #[test]
    fn active_file_index_returns_none_when_no_cursor() {
        let mut cx = TestAppContext::single();
        let item = new_item(
            &mut cx,
            ReviewSource::InMemory {
                files: Arc::new(Vec::new()),
            },
        );
        item.read_with(&cx, |item, app| {
            assert_eq!(item.active_file_index(app), None);
        });
    }

    #[test]
    fn active_file_index_tracks_session_cursor() {
        let mut cx = TestAppContext::single();
        install_executor(&mut cx);
        let session = cx.update(|cx| {
            cx.new(|_| {
                let mut inner = InnerSession::new(ReviewSource::InMemory {
                    files: Arc::new(Vec::new()),
                });
                inner.add_files(vec![
                    ReviewFileInput {
                        path: PathBuf::from("a.txt"),
                        rel_path: "a.txt".to_string(),
                        language: None,
                        base_text: Arc::new("a\n".to_string()),
                        buffer_text: Arc::new("aa\n".to_string()),
                    },
                    ReviewFileInput {
                        path: PathBuf::from("b.txt"),
                        rel_path: "b.txt".to_string(),
                        language: None,
                        base_text: Arc::new("b\n".to_string()),
                        buffer_text: Arc::new("bb\n".to_string()),
                    },
                ]);
                ReviewSession::new(inner)
            })
        });
        let registry = cx.update(|cx| cx.new(|_| BufferRegistry::new()));

        let item = cx.update(|cx| {
            let session = session.clone();
            let registry = registry.clone();
            cx.new(|cx| ReviewItem::from_session(session, &registry, cx))
        });

        item.read_with(&cx, |item, app| {
            assert_eq!(
                item.active_file_index(app),
                Some(0),
                "first chunk is in file 0; cursor defaults to first chunk on add_files",
            );
        });

        // Advance the cursor and assert it moves to the next chunk.
        session.update(&mut cx, |s, cx| {
            s.next(cx);
        });
        cx.run_until_parked();

        item.read_with(&cx, |item, app| {
            assert_eq!(item.active_file_index(app), Some(1));
        });
    }

    #[test]
    fn from_session_attaches_session_and_file_index_to_each_editor() {
        let mut cx = TestAppContext::single();
        install_executor(&mut cx);
        let session = cx.update(|cx| {
            cx.new(|_| {
                let mut inner = InnerSession::new(ReviewSource::InMemory {
                    files: Arc::new(Vec::new()),
                });
                inner.add_files(vec![
                    ReviewFileInput {
                        path: PathBuf::from("a.txt"),
                        rel_path: "a.txt".to_string(),
                        language: None,
                        base_text: Arc::new("a\n".to_string()),
                        buffer_text: Arc::new("aa\n".to_string()),
                    },
                    ReviewFileInput {
                        path: PathBuf::from("b.txt"),
                        rel_path: "b.txt".to_string(),
                        language: None,
                        base_text: Arc::new("b\n".to_string()),
                        buffer_text: Arc::new("bb\n".to_string()),
                    },
                ]);
                ReviewSession::new(inner)
            })
        });
        let registry = cx.update(|cx| cx.new(|_| BufferRegistry::new()));

        let item = cx.update(|cx| {
            let session = session.clone();
            let registry = registry.clone();
            cx.new(|cx| ReviewItem::from_session(session, &registry, cx))
        });

        item.read_with(&cx, |item, cx| {
            let session_id = item.session().entity_id();
            for (expected_index, file) in item.files().iter().enumerate() {
                let editor = file.editor.read(cx);
                assert_eq!(
                    editor.review_file_index(),
                    Some(expected_index),
                    "editor for file {expected_index} should know its index",
                );
                assert_eq!(
                    editor.review_session().map(|s| s.entity_id()),
                    Some(session_id),
                    "editor for file {expected_index} should share the same session",
                );
            }
        });
    }

    #[test]
    fn rebuild_files_updates_file_views_after_refresh() {
        use stoat::review::ReviewFileInput as Input;
        let mut cx = TestAppContext::single();
        install_executor(&mut cx);
        let session = session_with_file(
            &mut cx,
            ReviewSource::InMemory {
                files: Arc::new(Vec::new()),
            },
            "a.txt",
            "alpha\nbeta\n",
            "alpha modified\nbeta\n",
        );
        let registry = cx.update(|cx| cx.new(|_| BufferRegistry::new()));

        let item = cx.update(|cx| {
            let session = session.clone();
            let registry = registry.clone();
            cx.new(|cx| ReviewItem::from_session(session, &registry, cx))
        });
        item.read_with(&cx, |item, _| assert_eq!(item.files().len(), 1));

        session.update(&mut cx, |s, cx| {
            s.refresh_files(
                vec![
                    Input {
                        path: PathBuf::from("a.txt"),
                        rel_path: "a.txt".to_string(),
                        language: None,
                        base_text: Arc::new("alpha\nbeta\n".to_string()),
                        buffer_text: Arc::new("alpha modified\nbeta\n".to_string()),
                    },
                    Input {
                        path: PathBuf::from("b.txt"),
                        rel_path: "b.txt".to_string(),
                        language: None,
                        base_text: Arc::new("".to_string()),
                        buffer_text: Arc::new("brand new\n".to_string()),
                    },
                ],
                cx,
            );
        });
        cx.run_until_parked();

        item.read_with(&cx, |item, _| {
            assert_eq!(
                item.files().len(),
                2,
                "Refreshed event must rebuild file views to reflect the new file count",
            );
            assert_eq!(item.files()[0].rel_path, "a.txt");
            assert_eq!(item.files()[1].rel_path, "b.txt");
        });
    }
}
