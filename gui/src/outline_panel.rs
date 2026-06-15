//! Symbol outline panel dock item.
//!
//! Renders the active editor's `textDocument/documentSymbol` response
//! as an indented tree in a right [`crate::dock::Dock`]. The
//! `ToggleOutlinePanel` action opens and closes it. The panel observes
//! the workspace, which re-notifies on every active-editor change, so
//! the highlight follows the cursor live; the symbol tree is re-fetched
//! only when the active editor or its buffer version changes, so cursor
//! moves do not spam the language server. Clicking a row jumps the
//! editor's primary cursor to that symbol.

use crate::{
    editor::{scroll::autoscroll::AutoscrollStrategy, Editor, EditorEvent},
    globals::LanguageRegistry,
    item::{DeserializeSnafu, ItemError, ItemKind, ItemView},
    theme::ActiveTheme,
    workspace::Workspace,
};
use gpui::{
    div, rems, App, Context, Entity, EntityId, InteractiveElement, IntoElement, ParentElement,
    Render, SharedString, StatefulInteractiveElement, Styled, Subscription, WeakEntity, Window,
};
use lsp_types::{
    DocumentSymbol, DocumentSymbolParams, DocumentSymbolResponse, SymbolInformation,
    TextDocumentIdentifier, Uri,
};
use serde_json::Value;
use std::{ops::Range, path::Path, str::FromStr};
use stoat::{
    host::{LanguageServerFeature, OffsetEncoding},
    lsp::util::lsp_pos_to_byte_offset,
};
use stoat_text::{Bias, Rope, Selection, SelectionGoal};

/// One symbol row in the outline tree. `depth` is the nesting level
/// (0 for top-level symbols), `range` is the symbol's full byte range
/// used to decide which row encloses the cursor, and `anchor_offset`
/// is the byte offset the cursor jumps to on click.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutlineEntry {
    pub title: String,
    pub depth: usize,
    pub range: Range<usize>,
    pub anchor_offset: usize,
}

pub struct OutlinePanel {
    workspace: WeakEntity<Workspace>,
    entries: Vec<OutlineEntry>,
    /// `(editor id, buffer version)` of the last symbol fetch. A
    /// mismatch on the next sync drives a re-fetch; an equal value
    /// means only the cursor moved, so the panel re-renders without
    /// re-querying the server.
    fetched: Option<(EntityId, u64)>,
    _observe: Subscription,
}

impl OutlinePanel {
    pub fn new(workspace: Entity<Workspace>, cx: &mut Context<'_, Self>) -> Self {
        let observe = cx.observe(&workspace, |this, ws, cx| this.sync(&ws, cx));
        // The first sync reads the workspace, which may be mid-update
        // when the panel is created from the toggle dispatch. Defer it
        // past the current update so the read does not re-enter.
        let weak_self = cx.weak_entity();
        let weak_workspace = workspace.downgrade();
        cx.defer(move |cx| {
            let (Some(panel), Some(workspace)) = (weak_self.upgrade(), weak_workspace.upgrade())
            else {
                return;
            };
            panel.update(cx, |panel, cx| panel.sync(&workspace, cx));
        });
        Self {
            workspace: workspace.downgrade(),
            entries: Vec::new(),
            fetched: None,
            _observe: observe,
        }
    }

    /// Re-render and, when the active editor or its buffer version has
    /// changed since the last fetch, kick off a fresh symbol request.
    fn sync(&mut self, workspace: &Entity<Workspace>, cx: &mut Context<'_, Self>) {
        match active_editor(workspace, cx).and_then(|ed| editor_key(&ed, cx).map(|k| (ed, k))) {
            None => {
                self.entries.clear();
                self.fetched = None;
            },
            Some((editor, key)) => {
                if self.fetched != Some(key) {
                    self.fetched = Some(key);
                    self.refresh_symbols(editor, cx);
                }
            },
        }
        cx.notify();
    }

    /// Read the active editor's path, language, and buffer text, then
    /// issue an async `documentSymbol` request and store the resulting
    /// tree. Clears the panel when the buffer has no path or no
    /// registered language.
    fn refresh_symbols(&mut self, editor: Entity<Editor>, cx: &mut Context<'_, Self>) {
        let Some(path) = editor.read(cx).file_path().map(Path::to_path_buf) else {
            self.entries.clear();
            return;
        };
        let language = {
            let registry = &cx.global::<LanguageRegistry>().0;
            registry.for_path(&path)
        };
        let Some(language) = language else {
            self.entries.clear();
            return;
        };
        let Some(uri) = path_to_uri(&path) else {
            return;
        };
        let rope = editor
            .read(cx)
            .multi_buffer()
            .read(cx)
            .snapshot()
            .rope()
            .clone();
        let workspace = Some(self.workspace.clone());

        cx.spawn(async move |this, cx| {
            let Some(server) = crate::lsp::cached_server(&workspace, language, cx).await else {
                return;
            };
            if !server.supports_feature(LanguageServerFeature::DocumentSymbols) {
                return;
            }
            let encoding = server.offset_encoding();
            let params = DocumentSymbolParams {
                text_document: TextDocumentIdentifier { uri },
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            };
            let entries = match server.document_symbol(params).await {
                Ok(Some(response)) => outline_entries(&rope, encoding, response),
                Ok(None) => Vec::new(),
                Err(err) => {
                    tracing::warn!(
                        target: "stoat_gui::outline_panel",
                        ?err,
                        "document_symbol request failed"
                    );
                    return;
                },
            };
            let _ = this.update(cx, |panel, cx| {
                panel.entries = entries;
                cx.notify();
            });
        })
        .detach();
    }

    /// Byte offset of the active editor's primary cursor, used to pick
    /// the enclosing symbol to highlight.
    fn cursor_offset(&self, cx: &App) -> Option<usize> {
        let workspace = self.workspace.upgrade()?;
        let editor = active_editor(&workspace, cx)?;
        let editor = editor.read(cx);
        let snapshot = editor.multi_buffer().read(cx).snapshot();
        Some(snapshot.resolve_anchor(&editor.selections().newest_anchor().head()))
    }

    fn jump_to(&mut self, offset: usize, cx: &mut Context<'_, Self>) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let Some(editor) = active_editor(&workspace, cx) else {
            return;
        };
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
            ed.request_autoscroll(AutoscrollStrategy::Center, cx);
            cx.emit(EditorEvent::Changed);
            cx.notify();
        });
    }
}

/// Flatten a [`DocumentSymbolResponse`] into a depth-tagged tree with
/// byte offsets resolved against `rope`. Nested responses keep each
/// symbol's nesting depth and full range; flat responses are all
/// depth 0 with the location range.
pub fn outline_entries(
    rope: &Rope,
    encoding: OffsetEncoding,
    response: DocumentSymbolResponse,
) -> Vec<OutlineEntry> {
    let mut out = Vec::new();
    match response {
        DocumentSymbolResponse::Flat(items) => {
            for SymbolInformation { name, location, .. } in items {
                let start = lsp_pos_to_byte_offset(rope, location.range.start, encoding);
                let end = lsp_pos_to_byte_offset(rope, location.range.end, encoding);
                out.push(OutlineEntry {
                    title: name,
                    depth: 0,
                    range: start..end,
                    anchor_offset: start,
                });
            }
        },
        DocumentSymbolResponse::Nested(items) => walk_nested(rope, encoding, items, 0, &mut out),
    }
    out
}

fn walk_nested(
    rope: &Rope,
    encoding: OffsetEncoding,
    items: Vec<DocumentSymbol>,
    depth: usize,
    out: &mut Vec<OutlineEntry>,
) {
    for symbol in items {
        let start = lsp_pos_to_byte_offset(rope, symbol.range.start, encoding);
        let end = lsp_pos_to_byte_offset(rope, symbol.range.end, encoding);
        let anchor = lsp_pos_to_byte_offset(rope, symbol.selection_range.start, encoding);
        out.push(OutlineEntry {
            title: symbol.name.clone(),
            depth,
            range: start..end,
            anchor_offset: anchor,
        });
        if let Some(children) = symbol.children {
            walk_nested(rope, encoding, children, depth + 1, out);
        }
    }
}

/// Index of the innermost symbol whose range contains `cursor` -- the
/// one with the smallest range -- or `None` when the cursor sits in no
/// symbol.
fn enclosing_index(entries: &[OutlineEntry], cursor: usize) -> Option<usize> {
    entries
        .iter()
        .enumerate()
        .filter(|(_, e)| e.range.start <= cursor && cursor < e.range.end)
        .min_by_key(|(_, e)| e.range.end - e.range.start)
        .map(|(i, _)| i)
}

fn active_editor(workspace: &Entity<Workspace>, cx: &App) -> Option<Entity<Editor>> {
    workspace
        .read(cx)
        .input_state_machine()
        .read(cx)
        .active_editor()
        .cloned()
        .and_then(|w| w.upgrade())
}

fn editor_key(editor: &Entity<Editor>, cx: &App) -> Option<(EntityId, u64)> {
    let version = editor
        .read(cx)
        .multi_buffer()
        .read(cx)
        .as_singleton()?
        .read(cx)
        .version();
    Some((editor.entity_id(), version))
}

fn path_to_uri(path: &Path) -> Option<Uri> {
    let s = path.to_str()?;
    Uri::from_str(&format!("file://{s}")).ok()
}

impl Render for OutlinePanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let text_color = cx.theme().statusbar_text;
        let selection_bg = cx.theme().modal_selection;
        let highlight = self
            .cursor_offset(cx)
            .and_then(|cursor| enclosing_index(&self.entries, cursor));
        let rows: Vec<(usize, f32, SharedString, usize, bool)> = self
            .entries
            .iter()
            .enumerate()
            .map(|(ix, e)| {
                (
                    ix,
                    e.depth as f32,
                    SharedString::from(e.title.clone()),
                    e.anchor_offset,
                    Some(ix) == highlight,
                )
            })
            .collect();
        let children = rows.into_iter().map(|(ix, depth, title, anchor, hot)| {
            let mut row = div()
                .id(("outline-row", ix))
                .pl(rems(depth + 0.5))
                .pr_2()
                .text_color(text_color)
                .child(title)
                .on_click(cx.listener(move |this, _event, _window, cx| this.jump_to(anchor, cx)));
            if hot {
                row = row.bg(selection_bg);
            }
            row
        });
        div().flex().flex_col().size_full().children(children)
    }
}

impl ItemView for OutlinePanel {
    fn tab_label(&self, _cx: &App) -> SharedString {
        "Outline".into()
    }

    fn item_kind(&self) -> ItemKind {
        ItemKind::OutlinePanel
    }

    fn deserialize(_value: Value, _cx: &mut Context<'_, Self>) -> Result<Self, ItemError>
    where
        Self: Sized,
    {
        DeserializeSnafu {
            reason: "OutlinePanel is transient and not persisted",
        }
        .fail()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::globals::{ExecutorGlobal, FsHostGlobal, FsWatchHostGlobal, LspHostGlobal};
    use gpui::{AppContext, TestAppContext, VisualTestContext};
    use lsp_types::{Location, Position, Range as LspRange, SymbolKind};
    use std::{
        path::{Path, PathBuf},
        sync::Arc,
    };
    use stoat::host::{
        fake::{FakeFs, FakeLsp, FakeLspHost},
        FsWatchHost, LspHost,
    };
    use stoat_host::NoopFsWatcher;
    use stoat_scheduler::{Executor, TestScheduler};

    fn pos(line: u32, character: u32) -> Position {
        Position { line, character }
    }

    fn rng(start: Position, end: Position) -> LspRange {
        LspRange { start, end }
    }

    fn uri(path: &str) -> Uri {
        Uri::from_str(&format!("file://{path}")).expect("file:// uri parses")
    }

    fn nested(
        name: &str,
        line: u32,
        end_line: u32,
        children: Vec<DocumentSymbol>,
    ) -> DocumentSymbol {
        #[allow(deprecated)]
        DocumentSymbol {
            name: name.to_string(),
            detail: None,
            kind: SymbolKind::FUNCTION,
            tags: None,
            deprecated: None,
            range: rng(pos(line, 0), pos(end_line, 0)),
            selection_range: rng(pos(line, 3), pos(line, 8)),
            children: if children.is_empty() {
                None
            } else {
                Some(children)
            },
        }
    }

    fn flat(name: &str, path: &str, line: u32) -> SymbolInformation {
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
        Rope::from(
            (0..line_count)
                .map(|i| format!("line {i:02} text\n"))
                .collect::<String>()
                .as_str(),
        )
    }

    #[test]
    fn outline_entries_nested_keeps_depth_and_anchor() {
        let rope = rope_with_lines(10);
        let response = DocumentSymbolResponse::Nested(vec![nested(
            "outer",
            0,
            6,
            vec![nested("inner", 2, 4, vec![nested("leaf", 3, 4, vec![])])],
        )]);
        let entries = outline_entries(&rope, OffsetEncoding::Utf16, response);
        let shape: Vec<(&str, usize)> = entries
            .iter()
            .map(|e| (e.title.as_str(), e.depth))
            .collect();
        assert_eq!(
            shape,
            vec![("outer", 0), ("inner", 1), ("leaf", 2)],
            "nested symbols keep their nesting depth"
        );
        let line_len = "line 00 text\n".len();
        assert_eq!(
            entries[1].anchor_offset,
            line_len * 2 + 3,
            "anchor resolves selection_range.start"
        );
    }

    #[test]
    fn outline_entries_flat_are_all_depth_zero() {
        let rope = rope_with_lines(5);
        let response =
            DocumentSymbolResponse::Flat(vec![flat("alpha", "/a.rs", 0), flat("beta", "/a.rs", 2)]);
        let entries = outline_entries(&rope, OffsetEncoding::Utf16, response);
        assert_eq!(
            entries.iter().map(|e| e.depth).collect::<Vec<_>>(),
            vec![0, 0]
        );
        assert_eq!(
            entries.iter().map(|e| e.title.clone()).collect::<Vec<_>>(),
            vec!["alpha", "beta"]
        );
    }

    #[test]
    fn enclosing_index_picks_innermost_symbol() {
        let entries = vec![
            OutlineEntry {
                title: "outer".into(),
                depth: 0,
                range: 0..100,
                anchor_offset: 0,
            },
            OutlineEntry {
                title: "inner".into(),
                depth: 1,
                range: 20..40,
                anchor_offset: 20,
            },
        ];
        assert_eq!(enclosing_index(&entries, 30), Some(1), "innermost wins");
        assert_eq!(enclosing_index(&entries, 10), Some(0), "outer only");
        assert_eq!(
            enclosing_index(&entries, 200),
            None,
            "cursor past all symbols"
        );
    }

    fn install_globals(cx: &mut TestAppContext, fake_fs: Arc<FakeFs>) -> Arc<FakeLsp> {
        let lsp = Arc::new(FakeLsp::new());
        let lsp_host = Arc::new(FakeLspHost::new(lsp.clone())) as Arc<dyn LspHost>;
        cx.update(|cx| {
            cx.set_global(ExecutorGlobal(Executor::new(
                Arc::new(TestScheduler::new()),
            )));
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

    fn editor_for_buffer(
        workspace: &Workspace,
        buffer: &Entity<crate::buffer::Buffer>,
        cx: &App,
    ) -> Option<Entity<Editor>> {
        let target_id = buffer.entity_id();
        let pane_tree = workspace.pane_tree().read(cx);
        for pane_id in pane_tree.split_pane_ids() {
            let pane = pane_tree.pane(pane_id)?;
            for item in pane.read(cx).items() {
                let Ok(editor) = item.to_any_view().downcast::<Editor>() else {
                    continue;
                };
                let singleton = editor
                    .read(cx)
                    .multi_buffer()
                    .read(cx)
                    .as_singleton()
                    .cloned();
                if singleton.as_ref().map(Entity::entity_id) == Some(target_id) {
                    return Some(editor);
                }
            }
        }
        None
    }

    fn activate_editor_for_path(
        workspace: &Entity<Workspace>,
        vcx: &mut VisualTestContext,
        path: &Path,
    ) {
        let editor = workspace
            .read_with(vcx, |w, cx| {
                w.buffer_for_path(path, cx)
                    .and_then(|buffer| editor_for_buffer(w, &buffer, cx))
            })
            .expect("editor for opened path");
        let sm = workspace.read_with(vcx, |w, _| w.input_state_machine().clone());
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(editor.downgrade())));
    }

    #[test]
    fn panel_populates_symbol_tree_from_active_editor() {
        let mut cx = TestAppContext::single();
        let fake_fs = fake_fs_with_files(&[("a.rs", "fn alpha() {}\nfn beta() {}\n")]);
        let lsp = install_globals(&mut cx, fake_fs);
        lsp.set_capabilities(lsp_types::ServerCapabilities {
            document_symbol_provider: Some(lsp_types::OneOf::Left(true)),
            ..Default::default()
        });
        lsp.set_document_symbols(
            "/repo/a.rs",
            DocumentSymbolResponse::Flat(vec![
                flat("alpha", "/repo/a.rs", 0),
                flat("beta", "/repo/a.rs", 1),
            ]),
        );
        let (workspace, vcx) =
            cx.add_window_view(|_window, cx| Workspace::new("main", PathBuf::from("/repo"), cx));
        workspace.update(vcx, |w, cx| {
            w.open_paths(&[PathBuf::from("/repo/a.rs")], cx)
        });
        vcx.run_until_parked();
        activate_editor_for_path(&workspace, vcx, Path::new("/repo/a.rs"));

        let panel = vcx.update(|_window, cx| cx.new(|cx| OutlinePanel::new(workspace.clone(), cx)));
        vcx.run_until_parked();

        let titles = panel.read_with(vcx, |p, _| {
            p.entries
                .iter()
                .map(|e| e.title.clone())
                .collect::<Vec<_>>()
        });
        assert_eq!(titles, vec!["alpha", "beta"]);
    }
}
