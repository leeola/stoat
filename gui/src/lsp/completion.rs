use crate::{
    editor::{Editor, EditorEvent},
    globals::{LanguageRegistry, LspHostGlobal, UserSnippetsGlobal},
    theme::ActiveTheme,
};
use gpui::{
    deferred, div, point, Bounds, Context, Entity, IntoElement, ParentElement, Pixels, Point,
    Render, SharedString, Size, Styled, Subscription, Task, WeakEntity, Window,
};
use lsp_types::{
    CompletionContext as LspCompletionContext, CompletionItem as LspCompletionItem,
    CompletionParams, CompletionResponse, CompletionTextEdit, CompletionTriggerKind,
    TextDocumentIdentifier, TextDocumentPositionParams, Uri,
};
use std::{ops::Range, path::Path, str::FromStr, sync::Arc};
use stoat::{
    host::{LanguageServerFeature, LspHost},
    lsp::util::{byte_offset_to_lsp_pos, lsp_range_to_byte_range},
    snippet::UserSnippet,
};

/// Floating popup that lists completions from the language server
/// at the active editor's cursor. Mirrors [`HoverPopup`] in shape:
/// observes the host editor's [`EditorEvent::Changed`] stream,
/// fires `textDocument/completion` while the user is in
/// "insert" mode, and renders the results as a `gpui::deferred`
/// list anchored below the cursor cell.
///
/// Acceptance applies the selected item's edit to the host buffer
/// and clears the popup. Navigation (`select_next` /
/// `select_prev`) wraps around within the items vec; cursor motion
/// that leaves the originating prefix range clears the popup.
///
/// FIXME: launches a fresh language server per request (same
/// caveat as hover); the per-language LSP server cache will fix
/// both popups at once.
pub struct CompletionPopup {
    editor: WeakEntity<Editor>,
    items: Vec<CompletionEntry>,
    selected_idx: usize,
    anchor_offset: usize,
    prefix_range: Range<usize>,
    last_signature: Option<(usize, String)>,
    pending_task: Option<Task<()>>,
    /// Monotonic id for the most recent completion RPC spawn.
    /// Bumped at dispatch and re-checked in the response branch so
    /// a late reply for an earlier cursor cannot overwrite the
    /// popup's current items.
    request_seq: u64,
    _subscription: Subscription,
}

/// Single accepted-into-popup completion item. Strips the LSP shape
/// down to the fields needed by `accept` / render: label for the
/// row text, `insert_text` for the buffer edit, and an optional
/// per-item `replace_range` that widens the default prefix span
/// when the server returns its own `text_edit`.
#[derive(Clone, Debug)]
pub struct CompletionEntry {
    pub label: SharedString,
    pub insert_text: String,
    pub replace_range: Option<Range<usize>>,
}

impl CompletionPopup {
    pub fn new(editor: Entity<Editor>, cx: &mut Context<'_, Self>) -> Self {
        let weak = editor.downgrade();
        let subscription = cx.subscribe(&editor, |this, _editor, _event: &EditorEvent, cx| {
            this.reconcile(cx);
        });
        Self {
            editor: weak,
            items: Vec::new(),
            selected_idx: 0,
            anchor_offset: 0,
            prefix_range: 0..0,
            last_signature: None,
            pending_task: None,
            request_seq: 0,
            _subscription: subscription,
        }
    }

    pub(crate) fn bump_request_id(&mut self) -> u64 {
        self.request_seq += 1;
        self.request_seq
    }

    pub(crate) fn request_id(&self) -> u64 {
        self.request_seq
    }

    pub fn items(&self) -> &[CompletionEntry] {
        &self.items
    }

    /// Whether the popup currently has items the user can
    /// select. Used by SmartTab to decide between accepting
    /// the popup and inserting an indent character.
    pub fn is_visible(&self) -> bool {
        !self.items.is_empty()
    }

    /// Force the next [`Self::reconcile`] to ignore the
    /// signature cache and re-issue the LSP completion request.
    /// Mirrors the TUI's `last_completion_signature = None` then
    /// `request::trigger` pattern used by
    /// [`stoat_action::TriggerCompletion`].
    pub fn trigger_request(&mut self, cx: &mut Context<'_, Self>) {
        self.last_signature = None;
        self.reconcile(cx);
    }

    pub fn selected_idx(&self) -> usize {
        self.selected_idx
    }

    pub fn select_next(&mut self, cx: &mut Context<'_, Self>) {
        if self.items.is_empty() {
            return;
        }
        self.selected_idx = (self.selected_idx + 1) % self.items.len();
        cx.notify();
    }

    pub fn select_prev(&mut self, cx: &mut Context<'_, Self>) {
        if self.items.is_empty() {
            return;
        }
        self.selected_idx = if self.selected_idx == 0 {
            self.items.len() - 1
        } else {
            self.selected_idx - 1
        };
        cx.notify();
    }

    /// Apply the currently-selected entry to the host buffer and
    /// dismiss the popup. Returns `true` when an edit landed and
    /// `false` when the popup was empty or the editor was no
    /// longer live.
    pub fn accept(&mut self, cx: &mut Context<'_, Self>) -> bool {
        let Some(entry) = self.items.get(self.selected_idx).cloned() else {
            return false;
        };
        let Some(editor) = self.editor.upgrade() else {
            self.clear(cx);
            return false;
        };
        let range = entry
            .replace_range
            .clone()
            .unwrap_or_else(|| self.prefix_range.clone());
        let multi = editor.read(cx).multi_buffer().clone();
        let Some(buffer) = multi.read(cx).as_singleton().cloned() else {
            return false;
        };
        buffer.update(cx, |b, cx| b.edit(range, &entry.insert_text, cx));
        self.clear(cx);
        true
    }

    fn clear(&mut self, cx: &mut Context<'_, Self>) {
        self.pending_task = None;
        self.items.clear();
        self.selected_idx = 0;
        self.anchor_offset = 0;
        self.prefix_range = 0..0;
        self.last_signature = None;
        cx.notify();
    }

    fn reconcile(&mut self, cx: &mut Context<'_, Self>) {
        let Some(editor) = self.editor.upgrade() else {
            self.clear(cx);
            return;
        };
        if !is_in_insert_mode(&editor, cx) {
            if !self.items.is_empty() || self.pending_task.is_some() {
                self.clear(cx);
            }
            return;
        }
        let Some(request) = CompletionRequest::build(&editor, cx) else {
            if !self.items.is_empty() {
                self.clear(cx);
            }
            return;
        };

        let cursor_in_range = self.prefix_range.start <= request.cursor_offset
            && request.cursor_offset <= self.prefix_range.end;
        if !cursor_in_range && !self.items.is_empty() {
            self.clear(cx);
        }

        let signature = request.signature.clone();
        if self.last_signature.as_ref() == Some(&signature) {
            return;
        }
        self.last_signature = Some(signature);

        let anchor = request.cursor_offset;
        let prefix_range = request.prefix_range.clone();
        let request_id = self.bump_request_id();
        let task = cx.spawn(async move |this, cx| {
            let entries = request.run().await;
            let _ = this.update(cx, |popup, cx| {
                if popup.request_id() != request_id {
                    // A newer completion request superseded this one;
                    // the stale entries would overwrite items the user
                    // has since moved past.
                    return;
                }
                popup.items = entries;
                popup.selected_idx = 0;
                popup.anchor_offset = anchor;
                popup.prefix_range = prefix_range;
                cx.notify();
            });
        });
        self.pending_task = Some(task);
    }
}

impl Render for CompletionPopup {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        if self.items.is_empty() {
            return empty().into_any_element();
        }
        let Some(editor) = self.editor.upgrade() else {
            return empty().into_any_element();
        };
        let (bounds, cell, display_map, multi_buffer) = {
            let editor_ref = editor.read(cx);
            (
                editor_ref.text_region_bounds(),
                editor_ref.cell_size(),
                editor_ref.display_map().clone(),
                editor_ref.multi_buffer().clone(),
            )
        };
        let Some(bounds) = bounds else {
            return empty().into_any_element();
        };
        let Some(cell) = cell else {
            return empty().into_any_element();
        };
        let display_snapshot = display_map.update(cx, |dm, _| dm.snapshot());
        let mb_snapshot = multi_buffer.read(cx).snapshot();
        let cursor_point = mb_snapshot.rope().offset_to_point(self.anchor_offset);
        let display = display_snapshot.buffer_to_display(cursor_point);
        let origin = popup_origin(bounds, cell, display.row, display.column);

        let theme = cx.theme();
        let selected = self.selected_idx;
        let rows: Vec<_> = self
            .items
            .iter()
            .take(10)
            .enumerate()
            .map(|(idx, entry)| {
                let mut row = div().child(entry.label.clone());
                if idx == selected {
                    row = row
                        .bg(theme.popup_selection_background)
                        .text_color(theme.popup_selection_text);
                } else {
                    row = row.text_color(theme.popup_text);
                }
                row.into_any_element()
            })
            .collect();

        deferred(
            div()
                .absolute()
                .left(origin.x)
                .top(origin.y)
                .bg(theme.popup_background)
                .border_1()
                .border_color(theme.popup_border)
                .child(div().flex().flex_col().children(rows)),
        )
        .with_priority(2)
        .into_any_element()
    }
}

fn empty() -> impl IntoElement {
    div()
}

fn popup_origin(bounds: Bounds<Pixels>, cell: Size<Pixels>, row: u32, col: u32) -> Point<Pixels> {
    let x = bounds.origin.x + cell.width * col as f32;
    let y = bounds.origin.y + cell.height * (row + 1) as f32;
    point(x, y)
}

fn is_in_insert_mode(editor: &Entity<Editor>, cx: &Context<'_, CompletionPopup>) -> bool {
    let Some(workspace) = editor.read(cx).workspace().cloned() else {
        return false;
    };
    let Some(workspace) = workspace.upgrade() else {
        return false;
    };
    let sm = workspace.read(cx).input_state_machine().clone();
    sm.read(cx).mode() == "insert"
}

struct CompletionRequest {
    host: Arc<dyn LspHost>,
    language: Arc<stoat_language::Language>,
    workspace_root: std::path::PathBuf,
    uri: Uri,
    cursor_offset: usize,
    prefix_range: Range<usize>,
    rope: stoat_text::Rope,
    signature: (usize, String),
    /// Completion rows for the active language's user snippets whose
    /// prefix matches the typed text, appended after the server's
    /// results in [`Self::run`].
    snippet_entries: Vec<CompletionEntry>,
}

impl CompletionRequest {
    fn build(editor: &Entity<Editor>, cx: &mut Context<'_, CompletionPopup>) -> Option<Self> {
        let host = cx.global::<LspHostGlobal>().0.clone();
        let path = editor.read(cx).file_path()?.to_path_buf();
        let language = cx.global::<LanguageRegistry>().0.for_path(&path)?;
        let uri = path_to_uri(&path)?;
        let mb_snapshot = editor.read(cx).multi_buffer().read(cx).snapshot();
        let rope = mb_snapshot.rope().clone();
        let head = editor.read(cx).selections().all_anchors().first()?.head();
        let cursor_offset = mb_snapshot.resolve_anchor(&head);
        let prefix_range = word_prefix_range(&rope, cursor_offset);
        if prefix_range.is_empty() {
            return None;
        }
        let prefix_text = rope.slice(prefix_range.clone()).to_string();
        let snippet_entries = cx
            .try_global::<UserSnippetsGlobal>()
            .and_then(|g| g.0.get(language.name))
            .map(|snippets| snippet_completion_entries(snippets, &prefix_text))
            .unwrap_or_default();
        let workspace_root = path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| path.clone());
        Some(Self {
            host,
            language,
            workspace_root,
            uri,
            cursor_offset,
            prefix_range,
            rope,
            signature: (cursor_offset, prefix_text),
            snippet_entries,
        })
    }

    async fn run(mut self) -> Vec<CompletionEntry> {
        let snippet_entries = std::mem::take(&mut self.snippet_entries);
        let mut entries = self.lsp_entries().await;
        entries.extend(snippet_entries);
        entries
    }

    async fn lsp_entries(self) -> Vec<CompletionEntry> {
        let server = match self.host.launch(&self.language, &self.workspace_root).await {
            Ok(s) => s,
            Err(err) => {
                tracing::warn!(target: "stoat_gui::lsp::completion", ?err, "failed to launch LSP server");
                return Vec::new();
            },
        };
        let _ = server.initialize(Some(self.uri.clone())).await;
        if !server.supports_feature(LanguageServerFeature::Completion) {
            return Vec::new();
        }
        let encoding = server.offset_encoding();
        let position = byte_offset_to_lsp_pos(&self.rope, self.cursor_offset, encoding);
        let params = CompletionParams {
            text_document_position: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier {
                    uri: self.uri.clone(),
                },
                position,
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
            context: Some(LspCompletionContext {
                trigger_kind: CompletionTriggerKind::INVOKED,
                trigger_character: None,
            }),
        };
        let response = match server.completion(params).await {
            Ok(Some(r)) => r,
            Ok(None) => return Vec::new(),
            Err(err) => {
                tracing::warn!(target: "stoat_gui::lsp::completion", ?err, "completion request failed");
                return Vec::new();
            },
        };
        let items = match response {
            CompletionResponse::Array(items) => items,
            CompletionResponse::List(list) => list.items,
        };
        items
            .into_iter()
            .map(|item| translate_item(item, &self.rope, encoding))
            .collect()
    }
}

fn translate_item(
    lsp_item: LspCompletionItem,
    rope: &stoat_text::Rope,
    encoding: stoat::host::OffsetEncoding,
) -> CompletionEntry {
    let (replace_range, edit_text) = match &lsp_item.text_edit {
        Some(CompletionTextEdit::Edit(edit)) => (
            Some(lsp_range_to_byte_range(rope, edit.range, encoding)),
            Some(edit.new_text.clone()),
        ),
        Some(CompletionTextEdit::InsertAndReplace(edit)) => (
            Some(lsp_range_to_byte_range(rope, edit.replace, encoding)),
            Some(edit.new_text.clone()),
        ),
        None => (None, None),
    };
    let insert_text = edit_text
        .or_else(|| lsp_item.insert_text.clone())
        .unwrap_or_else(|| lsp_item.label.clone());
    CompletionEntry {
        label: lsp_item.label.into(),
        insert_text,
        replace_range,
    }
}

/// Completion rows for the `snippets` whose prefix starts with the typed
/// `prefix`, in input order. The inserted text is the snippet body with
/// tabstops inlined, since the popup applies a flat edit rather than an
/// interactive expansion.
fn snippet_completion_entries(snippets: &[UserSnippet], prefix: &str) -> Vec<CompletionEntry> {
    snippets
        .iter()
        .filter(|snippet| snippet.prefix.starts_with(prefix))
        .map(|snippet| CompletionEntry {
            label: snippet_label(snippet),
            insert_text: snippet.expanded_body(),
            replace_range: None,
        })
        .collect()
}

/// Row label for a user snippet: its prefix, plus the description when
/// one is set.
fn snippet_label(snippet: &UserSnippet) -> SharedString {
    match &snippet.description {
        Some(description) => format!("{}  {}", snippet.prefix, description).into(),
        None => snippet.prefix.clone().into(),
    }
}

fn word_prefix_range(rope: &stoat_text::Rope, cursor_offset: usize) -> Range<usize> {
    if cursor_offset == 0 || cursor_offset > rope.len() {
        return cursor_offset..cursor_offset;
    }
    let row = rope.offset_to_point(cursor_offset).row;
    let line_start = rope.point_to_offset(stoat_text::Point::new(row, 0));
    let slice = rope.slice(line_start..cursor_offset).to_string();
    let mut start = slice.len();
    for (idx, ch) in slice.char_indices().rev() {
        if ch.is_alphanumeric() || ch == '_' {
            start = idx;
        } else {
            break;
        }
    }
    (line_start + start)..cursor_offset
}

fn path_to_uri(path: &Path) -> Option<Uri> {
    let path_str = path.to_str()?;
    Uri::from_str(&format!("file://{path_str}")).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        buffer::Buffer, diff_map::DiffMap, display_map::DisplayMap, editor::EditorMode,
        globals::ExecutorGlobal, input_state_machine::InputStateMachine, keymap_loader,
        multi_buffer::MultiBuffer, workspace::Workspace,
    };
    use gpui::{AppContext, TestAppContext};
    use std::{collections::HashMap, path::PathBuf};
    use stoat::{
        buffer::BufferId,
        host::fake::{FakeLsp, FakeLspHost},
    };
    use stoat_scheduler::{Executor, TestScheduler};

    fn install_globals(cx: &mut TestAppContext) -> Arc<FakeLsp> {
        let lsp = Arc::new(FakeLsp::new());
        let lsp_host = Arc::new(FakeLspHost::new(lsp.clone())) as Arc<dyn LspHost>;
        let executor = Executor::new(Arc::new(TestScheduler::new()));
        cx.update(|cx| {
            cx.set_global(LspHostGlobal(lsp_host));
            cx.set_global(LanguageRegistry::standard());
            cx.set_global(ExecutorGlobal(executor));
        });
        lsp
    }

    fn build_workspace_editor(
        cx: &mut TestAppContext,
        path: &Path,
        text: &str,
    ) -> (Entity<Workspace>, Entity<Editor>) {
        let path = path.to_path_buf();
        let text = text.to_string();
        cx.update(|cx| {
            let workspace = cx.new(|cx| Workspace::new("main", PathBuf::from("/tmp/repo"), cx));
            let workspace_handle = workspace.downgrade();
            let buffer = cx.new(|_| Buffer::with_text(BufferId::new(0), &text));
            let executor = cx.global::<ExecutorGlobal>().0.clone();
            let multi = cx.new({
                let buffer = buffer.clone();
                |cx| MultiBuffer::singleton(buffer, cx)
            });
            let display = cx.new({
                let buffer = buffer.clone();
                |cx| DisplayMap::new(buffer, executor.clone(), cx)
            });
            let diff = cx.new({
                let buffer = buffer.clone();
                |cx| DiffMap::new(buffer, cx)
            });
            let editor = cx.new(|cx| {
                let mut ed = Editor::new(multi, display, diff, EditorMode::full(), cx);
                ed.set_workspace(Some(workspace_handle));
                ed.set_file_path(Some(path), cx);
                ed
            });
            (workspace, editor)
        })
    }

    fn set_mode(cx: &mut TestAppContext, ws: &Entity<Workspace>, mode: &str) {
        let sm: Entity<InputStateMachine> =
            ws.read_with(cx, |w, _| w.input_state_machine().clone());
        // Compile an empty keymap and reuse the sm's transition_mode helper.
        let _ = keymap_loader::compile_from_source("");
        sm.update(cx, |sm, _| {
            sm.set_mode_for_test(stoat::keymap::StateValue::String(mode.into()))
        });
    }

    #[test]
    fn bump_request_id_increments_and_records_latest() {
        let mut cx = TestAppContext::single();
        let _lsp = install_globals(&mut cx);
        let path = PathBuf::from("/tmp/main.rs");
        let (_workspace, editor) = build_workspace_editor(&mut cx, &path, "x\n");
        let popup = cx.update(|cx| cx.new(|cx| CompletionPopup::new(editor.clone(), cx)));

        let first = popup.update(&mut cx, |p, _| p.bump_request_id());
        let second = popup.update(&mut cx, |p, _| p.bump_request_id());

        assert_eq!(first, 1);
        assert_eq!(second, 2);
        assert_eq!(
            popup.read_with(&cx, |p, _| p.request_id()),
            2,
            "request_id must track the most recent bump",
        );
    }

    #[test]
    fn popup_populates_after_edit_in_insert_mode() {
        let mut cx = TestAppContext::single();
        let lsp = install_globals(&mut cx);
        let path = PathBuf::from("/tmp/main.rs");
        lsp.set_capabilities(lsp_types::ServerCapabilities {
            completion_provider: Some(lsp_types::CompletionOptions::default()),
            ..Default::default()
        });
        // Program completions at the cursor position the test will land on.
        // The text "fo" leaves the cursor at byte offset 2 (line 0, col 2).
        lsp.set_completions(path.to_str().unwrap(), 0, 2, &["foo", "format"]);

        let (workspace, editor) = build_workspace_editor(&mut cx, &path, "fo");
        set_mode(&mut cx, &workspace, "insert");
        // Move cursor to end of text via direct selection mutation.
        editor.update(&mut cx, |ed, cx| {
            let snap = ed.multi_buffer().read(cx).snapshot();
            let anchor = snap.anchor_at(2, stoat_text::Bias::Left);
            let selection = stoat_text::Selection {
                id: 1,
                start: anchor,
                end: anchor,
                reversed: false,
                goal: stoat_text::SelectionGoal::None,
            };
            ed.selections_mut().replace_with(vec![selection], &snap);
        });

        let popup = cx.update(|cx| {
            let e = editor.clone();
            cx.new(|cx| CompletionPopup::new(e, cx))
        });

        // Trigger a Changed event by pushing a buffer edit.
        editor.update(&mut cx, |ed, cx| {
            let buffer = ed.multi_buffer().read(cx).as_singleton().cloned().unwrap();
            buffer.update(cx, |b, cx| b.edit(2..2, "", cx));
        });
        cx.run_until_parked();

        let labels: Vec<String> = popup.read_with(&cx, |p, _| {
            p.items().iter().map(|i| i.label.to_string()).collect()
        });
        assert_eq!(labels, vec!["foo".to_string(), "format".to_string()]);
        assert_eq!(popup.read_with(&cx, |p, _| p.selected_idx()), 0);
    }

    #[test]
    fn popup_stays_empty_outside_insert_mode() {
        let mut cx = TestAppContext::single();
        let lsp = install_globals(&mut cx);
        let path = PathBuf::from("/tmp/main.rs");
        lsp.set_capabilities(lsp_types::ServerCapabilities {
            completion_provider: Some(lsp_types::CompletionOptions::default()),
            ..Default::default()
        });
        lsp.set_completions(path.to_str().unwrap(), 0, 2, &["foo"]);

        let (_workspace, editor) = build_workspace_editor(&mut cx, &path, "fo");
        let popup = cx.update(|cx| {
            let e = editor.clone();
            cx.new(|cx| CompletionPopup::new(e, cx))
        });
        editor.update(&mut cx, |ed, cx| {
            let buffer = ed.multi_buffer().read(cx).as_singleton().cloned().unwrap();
            buffer.update(cx, |b, cx| b.edit(2..2, "", cx));
        });
        cx.run_until_parked();

        let empty = popup.read_with(&cx, |p, _| p.items().is_empty());
        assert!(empty);
    }

    #[test]
    fn accept_applies_insert_text_over_prefix_range() {
        let mut cx = TestAppContext::single();
        let lsp = install_globals(&mut cx);
        let path = PathBuf::from("/tmp/main.rs");
        lsp.set_capabilities(lsp_types::ServerCapabilities {
            completion_provider: Some(lsp_types::CompletionOptions::default()),
            ..Default::default()
        });
        lsp.set_completions(path.to_str().unwrap(), 0, 2, &["foo_bar"]);

        let (workspace, editor) = build_workspace_editor(&mut cx, &path, "fo");
        set_mode(&mut cx, &workspace, "insert");
        editor.update(&mut cx, |ed, cx| {
            let snap = ed.multi_buffer().read(cx).snapshot();
            let anchor = snap.anchor_at(2, stoat_text::Bias::Left);
            let selection = stoat_text::Selection {
                id: 1,
                start: anchor,
                end: anchor,
                reversed: false,
                goal: stoat_text::SelectionGoal::None,
            };
            ed.selections_mut().replace_with(vec![selection], &snap);
        });
        let popup = cx.update(|cx| {
            let e = editor.clone();
            cx.new(|cx| CompletionPopup::new(e, cx))
        });
        editor.update(&mut cx, |ed, cx| {
            let buffer = ed.multi_buffer().read(cx).as_singleton().cloned().unwrap();
            buffer.update(cx, |b, cx| b.edit(2..2, "", cx));
        });
        cx.run_until_parked();
        assert!(!popup.read_with(&cx, |p, _| p.items().is_empty()));

        let accepted = popup.update(&mut cx, |p, cx| p.accept(cx));
        assert!(accepted);
        let text = editor.read_with(&cx, |ed, cx| {
            ed.multi_buffer().read(cx).snapshot().rope().to_string()
        });
        assert_eq!(text, "foo_bar");
        assert!(popup.read_with(&cx, |p, _| p.items().is_empty()));
    }

    #[test]
    fn select_next_wraps_around() {
        let mut cx = TestAppContext::single();
        let lsp = install_globals(&mut cx);
        let path = PathBuf::from("/tmp/main.rs");
        lsp.set_capabilities(lsp_types::ServerCapabilities {
            completion_provider: Some(lsp_types::CompletionOptions::default()),
            ..Default::default()
        });
        lsp.set_completions(path.to_str().unwrap(), 0, 2, &["a", "b"]);

        let (workspace, editor) = build_workspace_editor(&mut cx, &path, "fo");
        set_mode(&mut cx, &workspace, "insert");
        editor.update(&mut cx, |ed, cx| {
            let snap = ed.multi_buffer().read(cx).snapshot();
            let anchor = snap.anchor_at(2, stoat_text::Bias::Left);
            let selection = stoat_text::Selection {
                id: 1,
                start: anchor,
                end: anchor,
                reversed: false,
                goal: stoat_text::SelectionGoal::None,
            };
            ed.selections_mut().replace_with(vec![selection], &snap);
        });
        let popup = cx.update(|cx| {
            let e = editor.clone();
            cx.new(|cx| CompletionPopup::new(e, cx))
        });
        editor.update(&mut cx, |ed, cx| {
            let buffer = ed.multi_buffer().read(cx).as_singleton().cloned().unwrap();
            buffer.update(cx, |b, cx| b.edit(2..2, "", cx));
        });
        cx.run_until_parked();

        popup.update(&mut cx, |p, cx| p.select_next(cx));
        assert_eq!(popup.read_with(&cx, |p, _| p.selected_idx()), 1);
        popup.update(&mut cx, |p, cx| p.select_next(cx));
        assert_eq!(popup.read_with(&cx, |p, _| p.selected_idx()), 0);
        popup.update(&mut cx, |p, cx| p.select_prev(cx));
        assert_eq!(popup.read_with(&cx, |p, _| p.selected_idx()), 1);
    }

    #[test]
    fn snippet_entries_filter_by_prefix_and_inline_body() {
        let snippets = vec![
            UserSnippet {
                prefix: "fn".into(),
                body: "fn ${1:name}()".into(),
                description: Some("function".into()),
            },
            UserSnippet {
                prefix: "for".into(),
                body: "for ${1:x} in ${2:xs}".into(),
                description: None,
            },
            UserSnippet {
                prefix: "let".into(),
                body: "let ${1:x};".into(),
                description: None,
            },
        ];

        let entries = snippet_completion_entries(&snippets, "f");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].label.as_ref(), "fn  function");
        assert_eq!(entries[0].insert_text, "fn name()");
        assert_eq!(entries[1].label.as_ref(), "for");
        assert_eq!(entries[1].insert_text, "for x in xs");
    }

    #[test]
    fn user_snippets_appear_after_lsp_results() {
        let mut cx = TestAppContext::single();
        let lsp = install_globals(&mut cx);
        let path = PathBuf::from("/tmp/main.rs");
        lsp.set_capabilities(lsp_types::ServerCapabilities {
            completion_provider: Some(lsp_types::CompletionOptions::default()),
            ..Default::default()
        });
        lsp.set_completions(path.to_str().unwrap(), 0, 2, &["foo_bar"]);

        let language = cx.update(|cx| {
            cx.global::<LanguageRegistry>()
                .0
                .for_path(&path)
                .unwrap()
                .name
        });
        cx.update(|cx| {
            cx.set_global(UserSnippetsGlobal(HashMap::from([(
                language.to_string(),
                vec![UserSnippet {
                    prefix: "form".into(),
                    body: "FORM".into(),
                    description: None,
                }],
            )])));
        });

        let (workspace, editor) = build_workspace_editor(&mut cx, &path, "fo");
        set_mode(&mut cx, &workspace, "insert");
        editor.update(&mut cx, |ed, cx| {
            let snap = ed.multi_buffer().read(cx).snapshot();
            let anchor = snap.anchor_at(2, stoat_text::Bias::Left);
            let selection = stoat_text::Selection {
                id: 1,
                start: anchor,
                end: anchor,
                reversed: false,
                goal: stoat_text::SelectionGoal::None,
            };
            ed.selections_mut().replace_with(vec![selection], &snap);
        });
        let popup = cx.update(|cx| {
            let e = editor.clone();
            cx.new(|cx| CompletionPopup::new(e, cx))
        });
        editor.update(&mut cx, |ed, cx| {
            let buffer = ed.multi_buffer().read(cx).as_singleton().cloned().unwrap();
            buffer.update(cx, |b, cx| b.edit(2..2, "", cx));
        });
        cx.run_until_parked();

        let labels = popup.read_with(&cx, |p, _| {
            p.items()
                .iter()
                .map(|e| e.label.to_string())
                .collect::<Vec<_>>()
        });
        assert_eq!(labels, vec!["foo_bar".to_string(), "form".to_string()]);
    }
}
