//! Document-symbol picker delegate.
//!
//! Issues a `textDocument/documentSymbol` request for the active
//! editor's buffer, flattens nested responses into a DFS list with
//! dotted ancestor-path titles (`outer.inner`), and on confirm jumps
//! the editor's primary cursor to the selected symbol's byte offset
//! (resolved from the symbol's `selection_range.start` for nested
//! responses or `location.range.start` for flat responses).

use crate::{
    buffer::Buffer,
    editor::Editor,
    globals::{LanguageRegistry, LspHostGlobal},
    picker::{match_highlight_runs, rank_matches, Picker, PickerDelegate, PickerSecondary},
    theme::ActiveTheme,
    workspace::Workspace,
};
use gpui::{
    div, AnyElement, Context, DismissEvent, Entity, HighlightStyle, IntoElement, ParentElement,
    SharedString, Styled, StyledText, Task, WeakEntity, Window,
};
use lsp_types::{
    DocumentSymbol, DocumentSymbolParams, DocumentSymbolResponse, SymbolInformation,
    TextDocumentIdentifier, Uri,
};
use std::{
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
};
use stoat::{
    host::{LanguageServerFeature, LspServer, OffsetEncoding},
    lsp::util::lsp_pos_to_byte_offset,
};
use stoat_text::{Bias, Rope, Selection, SelectionGoal};

/// One row in the document-symbol picker. `title` is the symbol name
/// (with dotted ancestor prefix for nested responses); `anchor_offset`
/// is the byte offset in the source buffer that the cursor jumps to
/// on confirm.
#[derive(Debug, Clone)]
pub struct SymbolEntry {
    pub title: String,
    pub anchor_offset: usize,
}

pub struct SymbolPickerDelegate {
    workspace: WeakEntity<Workspace>,
    buffer_path: PathBuf,
    entries: Vec<SymbolEntry>,
    matches: Vec<(usize, Vec<u32>)>,
    selected: usize,
    query: String,
}

impl SymbolPickerDelegate {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        buffer_path: PathBuf,
        entries: Vec<SymbolEntry>,
    ) -> Self {
        let matches = (0..entries.len()).map(|i| (i, Vec::new())).collect();
        Self {
            workspace,
            buffer_path,
            entries,
            matches,
            selected: 0,
            query: String::new(),
        }
    }

    fn refilter(&mut self) {
        let trimmed = self.query.trim();
        if trimmed.is_empty() {
            self.matches = (0..self.entries.len()).map(|i| (i, Vec::new())).collect();
            if self.selected >= self.matches.len() {
                self.selected = self.matches.len().saturating_sub(1);
            }
            return;
        }

        let items = self
            .entries
            .iter()
            .enumerate()
            .map(|(i, entry)| (i, entry.title.clone()));
        let Some(mut ranked) = rank_matches(trimmed, items) else {
            self.matches.clear();
            self.selected = 0;
            return;
        };
        ranked.sort_by_key(|m| std::cmp::Reverse(m.score));
        self.matches = ranked
            .into_iter()
            .map(|m| (m.item, m.matched_indices))
            .collect();
        if self.selected >= self.matches.len() {
            self.selected = self.matches.len().saturating_sub(1);
        }
    }

    fn selected_entry(&self) -> Option<&SymbolEntry> {
        let (idx, _) = self.matches.get(self.selected)?;
        self.entries.get(*idx)
    }
}

impl PickerDelegate for SymbolPickerDelegate {
    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected
    }

    fn set_selected_index(&mut self, ix: usize, _cx: &mut Context<'_, Picker<Self>>) {
        if ix < self.matches.len() {
            self.selected = ix;
        }
    }

    fn update_matches(&mut self, query: String, _cx: &mut Context<'_, Picker<Self>>) -> Task<()> {
        self.query = query;
        self.refilter();
        Task::ready(())
    }

    fn confirm(
        &mut self,
        _secondary: Option<PickerSecondary>,
        window: &mut Window,
        cx: &mut Context<'_, Picker<Self>>,
    ) {
        let Some(entry) = self.selected_entry().cloned() else {
            return;
        };
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let buffer_path = self.buffer_path.clone();
        // Defer past the keystroke observer's outer `Workspace::update`
        // lease so the re-entrant update does not panic.
        window.defer(cx, move |_window, cx| {
            workspace.update(cx, |workspace, cx| {
                let Some(editor) = workspace
                    .buffer_for_path(&buffer_path, cx)
                    .and_then(|buffer| editor_for_buffer(workspace, &buffer, cx))
                else {
                    return;
                };
                set_cursor_to_offset(&editor, entry.anchor_offset, cx);
            });
        });
        cx.emit(DismissEvent);
    }

    fn dismissed(&mut self, _cx: &mut Context<'_, Picker<Self>>) {}

    fn render_match(&self, ix: usize, cx: &mut Context<'_, Picker<Self>>) -> AnyElement {
        let Some((entry_idx, matched)) = self.matches.get(ix) else {
            return div().into_any_element();
        };
        let Some(entry) = self.entries.get(*entry_idx) else {
            return div().into_any_element();
        };
        let color = cx.theme().statusbar_text;
        let runs = match_highlight_runs(
            &entry.title,
            matched,
            HighlightStyle {
                color: Some(gpui::white()),
                ..Default::default()
            },
        );
        let label = StyledText::new(SharedString::from(entry.title.clone())).with_highlights(runs);
        div()
            .px_2()
            .text_color(color)
            .child(label)
            .into_any_element()
    }
}

/// Convert a [`DocumentSymbolResponse`] into a flat list of picker
/// entries with byte offsets resolved against `rope`. Nested responses
/// flatten DFS with dotted ancestor-path prefixes; flat responses map
/// each [`SymbolInformation`] to one entry. Mirrors the TUI's
/// `stoat::action_handlers::lsp::symbol_picker_entries`.
pub fn symbol_picker_entries(
    rope: &Rope,
    encoding: OffsetEncoding,
    response: DocumentSymbolResponse,
) -> Vec<SymbolEntry> {
    let mut entries: Vec<SymbolEntry> = Vec::new();
    match response {
        DocumentSymbolResponse::Flat(items) => {
            for SymbolInformation { name, location, .. } in items {
                let offset = lsp_pos_to_byte_offset(rope, location.range.start, encoding);
                entries.push(SymbolEntry {
                    title: name,
                    anchor_offset: offset,
                });
            }
        },
        DocumentSymbolResponse::Nested(items) => {
            let mut ancestors: Vec<String> = Vec::new();
            walk_nested(rope, encoding, items, &mut ancestors, &mut entries);
        },
    }
    entries
}

fn walk_nested(
    rope: &Rope,
    encoding: OffsetEncoding,
    items: Vec<DocumentSymbol>,
    ancestors: &mut Vec<String>,
    out: &mut Vec<SymbolEntry>,
) {
    for symbol in items {
        let offset = lsp_pos_to_byte_offset(rope, symbol.selection_range.start, encoding);
        let title = if ancestors.is_empty() {
            symbol.name.clone()
        } else {
            format!("{}.{}", ancestors.join("."), symbol.name)
        };
        out.push(SymbolEntry {
            title,
            anchor_offset: offset,
        });
        if let Some(children) = symbol.children {
            ancestors.push(symbol.name);
            walk_nested(rope, encoding, children, ancestors, out);
            ancestors.pop();
        }
    }
}

/// Launch the LSP server for the active editor's buffer, issue
/// `textDocument/documentSymbol`, flatten the response into picker
/// entries, and open the symbol picker as a modal. No-op when no
/// editor is active, the buffer has no path, no language is
/// registered for that path, the server fails to launch, the server
/// does not advertise [`LanguageServerFeature::DocumentSymbols`], or
/// the response is empty.
pub fn open_symbol_picker(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<'_, Workspace>,
) {
    let weak_editor = workspace
        .input_state_machine()
        .read(cx)
        .active_editor()
        .cloned();
    let Some(editor) = weak_editor.and_then(|w| w.upgrade()) else {
        return;
    };
    let Some(path) = editor.read(cx).file_path().map(Path::to_path_buf) else {
        return;
    };
    let registry = &cx.global::<LanguageRegistry>().0;
    let Some(language) = registry.for_path(&path) else {
        return;
    };
    let host = cx.global::<LspHostGlobal>().0.clone();
    let mb_snapshot = editor.read(cx).multi_buffer().read(cx).snapshot();
    let rope = mb_snapshot.rope().clone();
    let Some(uri) = path_to_uri(&path) else {
        return;
    };
    let workspace_root = path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| path.clone());

    let weak_workspace = cx.weak_entity();
    let path_for_task = path.clone();
    cx.spawn_in(window, async move |_, cx| {
        let server = match host.launch(&language, &workspace_root).await {
            Ok(s) => Arc::<dyn LspServer>::from(s),
            Err(err) => {
                tracing::warn!(
                    target: "stoat_gui::symbol_picker",
                    ?err,
                    "failed to launch LSP server for document symbols"
                );
                return;
            },
        };
        let _ = server.initialize(Some(uri.clone())).await;
        if !server.supports_feature(LanguageServerFeature::DocumentSymbols) {
            return;
        }
        let encoding = server.offset_encoding();
        let params = DocumentSymbolParams {
            text_document: TextDocumentIdentifier { uri },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        };
        let response = match server.document_symbol(params).await {
            Ok(Some(r)) => r,
            Ok(None) => return,
            Err(err) => {
                tracing::warn!(
                    target: "stoat_gui::symbol_picker",
                    ?err,
                    "document_symbol request failed"
                );
                return;
            },
        };
        let entries = symbol_picker_entries(&rope, encoding, response);
        if entries.is_empty() {
            return;
        }
        let _ = weak_workspace
            .clone()
            .update_in(cx, |workspace, window, cx| {
                let weak_workspace_inner = cx.weak_entity();
                let buffer_path = path_for_task.clone();
                workspace.toggle_modal::<Picker<SymbolPickerDelegate>, _>(
                    window,
                    cx,
                    move |window, cx| {
                        let delegate =
                            SymbolPickerDelegate::new(weak_workspace_inner, buffer_path, entries);
                        Picker::new(delegate, window, cx)
                    },
                );
            });
    })
    .detach();
}

fn editor_for_buffer(
    workspace: &Workspace,
    buffer: &Entity<Buffer>,
    cx: &gpui::App,
) -> Option<Entity<Editor>> {
    let target_id = buffer.entity_id();
    let pane_tree = workspace.pane_tree().read(cx);
    for pane_id in pane_tree.split_pane_ids() {
        let pane = pane_tree.pane(pane_id)?;
        for item in pane.read(cx).items() {
            let Ok(editor) = item.to_any_view().downcast::<Editor>() else {
                continue;
            };
            let mb_singleton = editor
                .read(cx)
                .multi_buffer()
                .read(cx)
                .as_singleton()
                .cloned();
            if mb_singleton.as_ref().map(Entity::entity_id) == Some(target_id) {
                return Some(editor);
            }
        }
    }
    None
}

fn set_cursor_to_offset(editor: &Entity<Editor>, offset: usize, cx: &mut Context<'_, Workspace>) {
    editor.update(cx, |ed, cx| {
        let snapshot = ed.multi_buffer().read(cx).snapshot();
        let anchor = snapshot.anchor_at(offset, Bias::Left);
        let new_id = ed
            .selections()
            .all_anchors()
            .iter()
            .map(|s| s.id)
            .max()
            .map(|m| m + 1)
            .unwrap_or(1);
        let selection = Selection {
            id: new_id,
            start: anchor,
            end: anchor,
            reversed: false,
            goal: SelectionGoal::None,
        };
        ed.selections_mut().replace_with(vec![selection], &snapshot);
    });
}

fn path_to_uri(path: &Path) -> Option<Uri> {
    let s = path.to_str()?;
    Uri::from_str(&format!("file://{s}")).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::globals::{ExecutorGlobal, FsHostGlobal, FsWatchHostGlobal};
    use gpui::{TestAppContext, VisualTestContext};
    use lsp_types::{Location, Position, Range, SymbolKind};
    use stoat::host::{
        fake::{FakeFs, FakeLsp, FakeLspHost},
        FsWatchHost, LspHost,
    };
    use stoat_host::NoopFsWatcher;
    use stoat_scheduler::{Executor, TestScheduler};

    fn pos(line: u32, character: u32) -> Position {
        Position { line, character }
    }

    fn rng(start: Position, end: Position) -> Range {
        Range { start, end }
    }

    fn uri(path: &str) -> Uri {
        Uri::from_str(&format!("file://{path}")).expect("file:// uri parses")
    }

    fn nested_symbol(name: &str, line: u32, children: Vec<DocumentSymbol>) -> DocumentSymbol {
        #[allow(deprecated)]
        DocumentSymbol {
            name: name.to_string(),
            detail: None,
            kind: SymbolKind::FUNCTION,
            tags: None,
            deprecated: None,
            range: rng(pos(line, 0), pos(line, 10)),
            selection_range: rng(pos(line, 0), pos(line, 5)),
            children: if children.is_empty() {
                None
            } else {
                Some(children)
            },
        }
    }

    fn flat_symbol(name: &str, path: &str, line: u32) -> SymbolInformation {
        #[allow(deprecated)]
        SymbolInformation {
            name: name.to_string(),
            kind: SymbolKind::FUNCTION,
            tags: None,
            deprecated: None,
            location: Location {
                uri: uri(path),
                range: rng(pos(line, 0), pos(line, 5)),
            },
            container_name: None,
        }
    }

    fn rope_with_lines(line_count: usize) -> Rope {
        let text = (0..line_count)
            .map(|i| format!("line {i:02} text\n"))
            .collect::<String>();
        Rope::from(text.as_str())
    }

    #[test]
    fn symbol_picker_entries_flat_keeps_each_item() {
        let rope = rope_with_lines(5);
        let response = DocumentSymbolResponse::Flat(vec![
            flat_symbol("alpha", "/src/a.rs", 0),
            flat_symbol("beta", "/src/a.rs", 2),
        ]);
        let entries = symbol_picker_entries(&rope, OffsetEncoding::Utf16, response);
        let titles: Vec<&str> = entries.iter().map(|e| e.title.as_str()).collect();
        assert_eq!(titles, vec!["alpha", "beta"]);
        let offsets: Vec<usize> = entries.iter().map(|e| e.anchor_offset).collect();
        let line_len = "line 00 text\n".len();
        assert_eq!(offsets, vec![0, line_len * 2]);
    }

    #[test]
    fn symbol_picker_entries_nested_prefixes_ancestors() {
        let rope = rope_with_lines(10);
        let response = DocumentSymbolResponse::Nested(vec![nested_symbol(
            "outer",
            0,
            vec![nested_symbol(
                "inner",
                2,
                vec![nested_symbol("leaf", 4, vec![])],
            )],
        )]);
        let entries = symbol_picker_entries(&rope, OffsetEncoding::Utf16, response);
        let titles: Vec<&str> = entries.iter().map(|e| e.title.as_str()).collect();
        assert_eq!(titles, vec!["outer", "outer.inner", "outer.inner.leaf"]);
    }

    #[test]
    fn symbol_picker_entries_empty_response_yields_empty_vec() {
        let rope = rope_with_lines(1);
        let entries = symbol_picker_entries(
            &rope,
            OffsetEncoding::Utf16,
            DocumentSymbolResponse::Flat(Vec::new()),
        );
        assert!(entries.is_empty());
    }

    fn new_delegate(entries: Vec<SymbolEntry>) -> SymbolPickerDelegate {
        SymbolPickerDelegate::new(
            WeakEntity::new_invalid(),
            PathBuf::from("/src/a.rs"),
            entries,
        )
    }

    fn entry(title: &str, anchor_offset: usize) -> SymbolEntry {
        SymbolEntry {
            title: title.to_string(),
            anchor_offset,
        }
    }

    #[test]
    fn delegate_lists_every_entry_when_query_empty() {
        let delegate = new_delegate(vec![entry("alpha", 0), entry("beta", 10)]);
        assert_eq!(delegate.match_count(), 2);
    }

    #[test]
    fn delegate_refilter_narrows_by_title() {
        let mut delegate = new_delegate(vec![
            entry("alpha", 0),
            entry("alphabet", 5),
            entry("beta", 10),
        ]);
        delegate.query = "alph".to_string();
        delegate.refilter();
        let titles: Vec<&str> = delegate
            .matches
            .iter()
            .map(|(i, _)| delegate.entries[*i].title.as_str())
            .collect();
        assert!(titles.contains(&"alpha"));
        assert!(titles.contains(&"alphabet"));
        assert!(!titles.contains(&"beta"));
    }

    #[test]
    fn delegate_no_entries_yields_empty_matches() {
        let delegate = new_delegate(Vec::new());
        assert_eq!(delegate.match_count(), 0);
    }

    fn install_globals(cx: &mut TestAppContext, fake_fs: Arc<FakeFs>) -> Arc<FakeLsp> {
        let lsp = Arc::new(FakeLsp::new());
        let lsp_host = Arc::new(FakeLspHost::new(lsp.clone())) as Arc<dyn LspHost>;
        let executor = Executor::new(Arc::new(TestScheduler::new()));
        cx.update(|cx| {
            cx.set_global(ExecutorGlobal(executor));
            cx.set_global(FsHostGlobal(fake_fs));
            cx.set_global(FsWatchHostGlobal(
                Arc::new(NoopFsWatcher::new()) as Arc<dyn FsWatchHost>
            ));
            cx.set_global(LspHostGlobal(lsp_host));
            cx.set_global(LanguageRegistry::standard());
        });
        lsp
    }

    fn fake_fs_with_files(files: &[(&str, &str)]) -> Arc<FakeFs> {
        let fs = FakeFs::new();
        let root = Path::new("/repo");
        fs.insert_files(
            files
                .iter()
                .map(|(rel, content)| (root.join(rel), content.as_bytes())),
        );
        Arc::new(fs)
    }

    struct Harness<'a> {
        workspace: Entity<Workspace>,
        vcx: &'a mut VisualTestContext,
        #[allow(dead_code)]
        lsp: Arc<FakeLsp>,
    }

    fn new_harness<'a>(cx: &'a mut TestAppContext, fake_fs: Arc<FakeFs>) -> Harness<'a> {
        let lsp = install_globals(cx, fake_fs);
        let (workspace, vcx) =
            cx.add_window_view(|_window, cx| Workspace::new("main", PathBuf::from("/repo"), cx));
        Harness {
            workspace,
            vcx,
            lsp,
        }
    }

    /// Activate the editor opened against `path` so the input state
    /// machine routes the picker dispatch to it. Production wires
    /// this via the pane tree's `Changed` event after a focus
    /// transition; tests skip the focus dance by activating
    /// directly.
    fn activate_editor_for_path(h: &mut Harness<'_>, path: &Path) {
        let editor = h
            .workspace
            .read_with(h.vcx, |w, cx| {
                w.buffer_for_path(path, cx)
                    .and_then(|buffer| editor_for_buffer(w, &buffer, cx))
            })
            .expect("editor for opened path");
        let sm = h
            .workspace
            .read_with(h.vcx, |w, _| w.input_state_machine().clone());
        sm.update(h.vcx, |sm, _| {
            sm.set_active_editor(Some(editor.downgrade()))
        });
    }

    #[test]
    fn open_symbol_picker_opens_modal_with_entries() {
        let mut cx = TestAppContext::single();
        let fake_fs = fake_fs_with_files(&[("a.rs", "fn alpha() {}\nfn beta() {}\n")]);
        let mut h = new_harness(&mut cx, fake_fs);

        h.lsp.set_capabilities(lsp_types::ServerCapabilities {
            document_symbol_provider: Some(lsp_types::OneOf::Left(true)),
            ..Default::default()
        });
        h.lsp.set_document_symbols(
            "/repo/a.rs",
            DocumentSymbolResponse::Flat(vec![
                flat_symbol("alpha", "/repo/a.rs", 0),
                flat_symbol("beta", "/repo/a.rs", 1),
            ]),
        );

        h.workspace.update(h.vcx, |w, cx| {
            w.open_paths(&[PathBuf::from("/repo/a.rs")], cx);
        });
        h.vcx.run_until_parked();
        activate_editor_for_path(&mut h, Path::new("/repo/a.rs"));

        h.workspace.update_in(h.vcx, |w, window, cx| {
            open_symbol_picker(w, window, cx);
        });
        h.vcx.run_until_parked();

        let picker: Entity<Picker<SymbolPickerDelegate>> = h
            .workspace
            .read_with(h.vcx, |w, cx| w.modal_layer().read(cx).active_modal())
            .expect("symbol picker modal should be open");
        let titles = picker.read_with(h.vcx, |p, _| {
            p.delegate()
                .entries
                .iter()
                .map(|e| e.title.clone())
                .collect::<Vec<_>>()
        });
        assert_eq!(titles, vec!["alpha", "beta"]);
    }

    #[test]
    fn open_symbol_picker_noop_when_response_empty() {
        let mut cx = TestAppContext::single();
        let fake_fs = fake_fs_with_files(&[("a.rs", "")]);
        let h = new_harness(&mut cx, fake_fs);

        h.lsp.set_capabilities(lsp_types::ServerCapabilities {
            document_symbol_provider: Some(lsp_types::OneOf::Left(true)),
            ..Default::default()
        });
        h.lsp
            .set_document_symbols("/repo/a.rs", DocumentSymbolResponse::Flat(Vec::new()));

        h.workspace.update(h.vcx, |w, cx| {
            w.open_paths(&[PathBuf::from("/repo/a.rs")], cx);
        });
        h.vcx.run_until_parked();

        h.workspace.update_in(h.vcx, |w, window, cx| {
            open_symbol_picker(w, window, cx);
        });
        h.vcx.run_until_parked();

        let has_modal = h.workspace.read_with(h.vcx, |w, cx| {
            w.modal_layer()
                .read(cx)
                .active_modal::<Picker<SymbolPickerDelegate>>()
                .is_some()
        });
        assert!(!has_modal, "empty symbol response should not open a modal");
    }
}
