//! Workspace-symbol picker delegate.
//!
//! Issues `workspace/symbol` against the picker's typed query on
//! every keystroke, displays the symbol list with absolute paths,
//! and on confirm opens the symbol's file in the focused pane and
//! jumps the primary cursor to its position.

use crate::{
    buffer::Buffer,
    editor::Editor,
    globals::LanguageRegistry,
    picker::{Picker, PickerDelegate, PickerSecondary},
    theme::ActiveTheme,
    workspace::Workspace,
};
use gpui::{
    div, AnyElement, Context, DismissEvent, Entity, IntoElement, ParentElement, Styled, Task,
    WeakEntity, Window,
};
use lsp_types::{
    OneOf, Position, SymbolInformation, Uri, WorkspaceSymbol, WorkspaceSymbolParams,
    WorkspaceSymbolResponse,
};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use stoat::{
    host::{LanguageServerFeature, LspServer, OffsetEncoding},
    lsp::util::lsp_pos_to_byte_offset,
};
use stoat_language::Language;
use stoat_text::{Bias, Selection, SelectionGoal};

/// One row in the workspace-symbol picker. `title` is the symbol
/// name; `path` is the absolute filesystem path of the source file;
/// `position` is the LSP position to jump to within that file.
#[derive(Debug, Clone)]
pub struct WorkspaceSymbolEntry {
    pub title: String,
    pub path: PathBuf,
    pub position: Position,
}

pub struct WorkspaceSymbolPickerDelegate {
    workspace: WeakEntity<Workspace>,
    language: Arc<Language>,
    entries: Vec<WorkspaceSymbolEntry>,
    /// Offset encoding from the most-recent successful response.
    /// Defaults to UTF-16 (the LSP spec default) until the first
    /// response refines it.
    encoding: OffsetEncoding,
    selected: usize,
}

impl WorkspaceSymbolPickerDelegate {
    pub fn new(workspace: WeakEntity<Workspace>, language: Arc<Language>) -> Self {
        Self {
            workspace,
            language,
            entries: Vec::new(),
            encoding: OffsetEncoding::Utf16,
            selected: 0,
        }
    }

    fn selected_entry(&self) -> Option<&WorkspaceSymbolEntry> {
        self.entries.get(self.selected)
    }
}

impl PickerDelegate for WorkspaceSymbolPickerDelegate {
    fn match_count(&self) -> usize {
        self.entries.len()
    }

    fn selected_index(&self) -> usize {
        self.selected
    }

    fn set_selected_index(&mut self, ix: usize, _cx: &mut Context<'_, Picker<Self>>) {
        if ix < self.entries.len() {
            self.selected = ix;
        }
    }

    fn update_matches(&mut self, query: String, cx: &mut Context<'_, Picker<Self>>) -> Task<()> {
        let trimmed = query.trim();
        if trimmed.is_empty() {
            self.entries.clear();
            self.selected = 0;
            cx.notify();
            return Task::ready(());
        }
        let query = trimmed.to_string();
        let workspace = self.workspace.clone();
        let language = self.language.clone();
        cx.spawn(async move |this, cx| {
            let server = crate::lsp::cached_server(&Some(workspace), language, cx).await;
            let outcome = run_workspace_symbol(server, query).await;
            let Some((entries, encoding)) = outcome else {
                return;
            };
            let _ = this.update(cx, |picker, cx| {
                let delegate = picker.delegate_mut();
                delegate.entries = entries;
                delegate.encoding = encoding;
                if delegate.selected >= delegate.entries.len() {
                    delegate.selected = delegate.entries.len().saturating_sub(1);
                }
                cx.notify();
            });
        })
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
        let encoding = self.encoding;
        // Defer past the keystroke observer's outer `Workspace::update`
        // lease so the re-entrant update does not panic.
        window.defer(cx, move |_window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.open_paths(std::slice::from_ref(&entry.path), cx);
                let Some(editor) = workspace
                    .buffer_for_path(&entry.path, cx)
                    .and_then(|buffer| editor_for_buffer(workspace, &buffer, cx))
                else {
                    return;
                };
                let mb_snapshot = editor.read(cx).multi_buffer().read(cx).snapshot();
                let rope = mb_snapshot.rope().clone();
                let offset = lsp_pos_to_byte_offset(&rope, entry.position, encoding);
                set_cursor_to_offset(&editor, offset, cx);
            });
        });
        cx.emit(DismissEvent);
    }

    fn dismissed(&mut self, _cx: &mut Context<'_, Picker<Self>>) {}

    fn render_match(&self, ix: usize, cx: &mut Context<'_, Picker<Self>>) -> AnyElement {
        let Some(entry) = self.entries.get(ix) else {
            return div().into_any_element();
        };
        let display = format!(
            "{}  {}:{}",
            entry.title,
            entry.path.display(),
            entry.position.line + 1
        );
        let color = cx.theme().statusbar_text;
        div()
            .px_2()
            .text_color(color)
            .child(display)
            .into_any_element()
    }
}

async fn run_workspace_symbol(
    server: Option<Arc<dyn LspServer>>,
    query: String,
) -> Option<(Vec<WorkspaceSymbolEntry>, OffsetEncoding)> {
    let server = server?;
    if !server.supports_feature(LanguageServerFeature::WorkspaceSymbols) {
        return None;
    }
    let encoding = server.offset_encoding();
    let params = WorkspaceSymbolParams {
        query,
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
    };
    let response = match server.workspace_symbol(params).await {
        Ok(Some(r)) => r,
        Ok(None) => return Some((Vec::new(), encoding)),
        Err(err) => {
            tracing::warn!(
                target: "stoat_gui::workspace_symbol_picker",
                ?err,
                "workspace_symbol request failed"
            );
            return None;
        },
    };
    Some((workspace_symbol_entries(response), encoding))
}

/// Flatten a [`WorkspaceSymbolResponse`] into picker entries.
/// Drops entries whose URI cannot be turned into a filesystem path;
/// the `Nested` variant's `OneOf::Right(WorkspaceLocation)` carries
/// no range, so falls back to `Position::new(0, 0)`.
pub fn workspace_symbol_entries(response: WorkspaceSymbolResponse) -> Vec<WorkspaceSymbolEntry> {
    let mut entries: Vec<WorkspaceSymbolEntry> = Vec::new();
    match response {
        WorkspaceSymbolResponse::Flat(items) => {
            for SymbolInformation { name, location, .. } in items {
                let Some(path) = uri_to_path(&location.uri) else {
                    continue;
                };
                entries.push(WorkspaceSymbolEntry {
                    title: name,
                    path,
                    position: location.range.start,
                });
            }
        },
        WorkspaceSymbolResponse::Nested(items) => {
            for WorkspaceSymbol { name, location, .. } in items {
                let (uri, position) = match location {
                    OneOf::Left(loc) => (loc.uri, loc.range.start),
                    OneOf::Right(workspace_loc) => (workspace_loc.uri, Position::new(0, 0)),
                };
                let Some(path) = uri_to_path(&uri) else {
                    continue;
                };
                entries.push(WorkspaceSymbolEntry {
                    title: name,
                    path,
                    position,
                });
            }
        },
    }
    entries
}

/// Open the workspace-symbol picker as a modal. The active editor
/// supplies the language used to launch the per-language LSP
/// server. No-op when no editor is active, the buffer has no path,
/// or no language is registered for that path.
pub fn open_workspace_symbol_picker(
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
    let weak_workspace = cx.weak_entity();
    workspace.toggle_modal::<Picker<WorkspaceSymbolPickerDelegate>, _>(
        window,
        cx,
        move |window, cx| {
            let delegate = WorkspaceSymbolPickerDelegate::new(weak_workspace, language);
            Picker::new(delegate, window, cx)
        },
    );
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

fn uri_to_path(uri: &Uri) -> Option<PathBuf> {
    let s = uri.as_str();
    let stripped = s.strip_prefix("file://")?;
    Some(PathBuf::from(stripped))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::globals::{ExecutorGlobal, FsHostGlobal, FsWatchHostGlobal, LspHostGlobal};
    use gpui::{TestAppContext, VisualTestContext};
    use lsp_types::{Location, Range, SymbolKind};
    use std::str::FromStr;
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

    fn flat_info(name: &str, path: &str, line: u32) -> SymbolInformation {
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

    fn nested_left(name: &str, path: &str, line: u32) -> WorkspaceSymbol {
        WorkspaceSymbol {
            name: name.to_string(),
            kind: SymbolKind::FUNCTION,
            tags: None,
            container_name: None,
            location: OneOf::Left(Location {
                uri: uri(path),
                range: rng(pos(line, 0), pos(line, 5)),
            }),
            data: None,
        }
    }

    fn nested_right(name: &str, path: &str) -> WorkspaceSymbol {
        WorkspaceSymbol {
            name: name.to_string(),
            kind: SymbolKind::FUNCTION,
            tags: None,
            container_name: None,
            location: OneOf::Right(lsp_types::WorkspaceLocation { uri: uri(path) }),
            data: None,
        }
    }

    #[test]
    fn workspace_symbol_entries_flat_maps_each_item() {
        let response = WorkspaceSymbolResponse::Flat(vec![
            flat_info("alpha", "/repo/a.rs", 0),
            flat_info("beta", "/repo/b.rs", 4),
        ]);
        let entries = workspace_symbol_entries(response);
        let names: Vec<&str> = entries.iter().map(|e| e.title.as_str()).collect();
        let paths: Vec<&Path> = entries.iter().map(|e| e.path.as_path()).collect();
        let lines: Vec<u32> = entries.iter().map(|e| e.position.line).collect();
        assert_eq!(names, vec!["alpha", "beta"]);
        assert_eq!(
            paths,
            vec![Path::new("/repo/a.rs"), Path::new("/repo/b.rs")]
        );
        assert_eq!(lines, vec![0, 4]);
    }

    #[test]
    fn workspace_symbol_entries_nested_left_keeps_range_start() {
        let response = WorkspaceSymbolResponse::Nested(vec![nested_left("gamma", "/repo/c.rs", 7)]);
        let entries = workspace_symbol_entries(response);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].title, "gamma");
        assert_eq!(entries[0].path, PathBuf::from("/repo/c.rs"));
        assert_eq!(entries[0].position, pos(7, 0));
    }

    #[test]
    fn workspace_symbol_entries_nested_right_falls_back_to_zero() {
        let response = WorkspaceSymbolResponse::Nested(vec![nested_right("delta", "/repo/d.rs")]);
        let entries = workspace_symbol_entries(response);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].title, "delta");
        assert_eq!(entries[0].path, PathBuf::from("/repo/d.rs"));
        assert_eq!(entries[0].position, pos(0, 0));
    }

    #[test]
    fn workspace_symbol_entries_drops_non_file_uris() {
        #[allow(deprecated)]
        let bad = SymbolInformation {
            name: "epsilon".to_string(),
            kind: SymbolKind::FUNCTION,
            tags: None,
            deprecated: None,
            location: Location {
                uri: Uri::from_str("http://example.com/file.rs").expect("http uri parses"),
                range: rng(pos(0, 0), pos(0, 1)),
            },
            container_name: None,
        };
        let response = WorkspaceSymbolResponse::Flat(vec![bad]);
        let entries = workspace_symbol_entries(response);
        assert!(entries.is_empty());
    }

    fn new_delegate() -> WorkspaceSymbolPickerDelegate {
        WorkspaceSymbolPickerDelegate::new(
            WeakEntity::new_invalid(),
            stoat_language::LanguageRegistry::standard()
                .find_by_name("rust")
                .expect("rust language registered"),
        )
    }

    #[test]
    fn fresh_delegate_lists_zero_matches() {
        let delegate = new_delegate();
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
    /// machine routes the picker dispatch to it (production wires
    /// this through pane-tree focus transitions).
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

    fn type_query(h: &mut Harness<'_>, query: &str) {
        let picker: Entity<Picker<WorkspaceSymbolPickerDelegate>> = h
            .workspace
            .read_with(h.vcx, |w, cx| w.modal_layer().read(cx).active_modal())
            .expect("picker modal is open");
        let buffer = picker.read_with(h.vcx, |p, cx| {
            p.query_editor()
                .read(cx)
                .multi_buffer()
                .read(cx)
                .as_singleton()
                .expect("single-line editor has singleton buffer")
                .clone()
        });
        buffer.update(h.vcx, |b, cx| {
            let len = b.text().len();
            b.edit(0..len, query, cx);
        });
    }

    #[test]
    fn open_workspace_symbol_picker_makes_modal_active() {
        let mut cx = TestAppContext::single();
        let fake_fs = fake_fs_with_files(&[("a.rs", "")]);
        let mut h = new_harness(&mut cx, fake_fs);

        h.lsp.set_capabilities(lsp_types::ServerCapabilities {
            workspace_symbol_provider: Some(OneOf::Left(true)),
            ..Default::default()
        });
        h.workspace.update(h.vcx, |w, cx| {
            w.open_paths(&[PathBuf::from("/repo/a.rs")], cx);
        });
        h.vcx.run_until_parked();
        activate_editor_for_path(&mut h, Path::new("/repo/a.rs"));

        h.workspace.update_in(h.vcx, |w, window, cx| {
            open_workspace_symbol_picker(w, window, cx);
        });
        h.vcx.run_until_parked();

        let active = h.workspace.read_with(h.vcx, |w, cx| {
            w.modal_layer()
                .read(cx)
                .active_modal::<Picker<WorkspaceSymbolPickerDelegate>>()
                .is_some()
        });
        assert!(active, "workspace symbol picker should be the active modal");
    }

    #[test]
    fn typing_query_populates_entries_from_lsp() {
        let mut cx = TestAppContext::single();
        let fake_fs = fake_fs_with_files(&[("a.rs", "")]);
        let mut h = new_harness(&mut cx, fake_fs);

        h.lsp.set_capabilities(lsp_types::ServerCapabilities {
            workspace_symbol_provider: Some(OneOf::Left(true)),
            ..Default::default()
        });
        h.lsp
            .add_workspace_symbol("Foo", "FooStruct", SymbolKind::STRUCT, "/repo/a.rs", 3, 0);
        h.lsp
            .add_workspace_symbol("Foo", "FooHelper", SymbolKind::FUNCTION, "/repo/a.rs", 9, 0);

        h.workspace.update(h.vcx, |w, cx| {
            w.open_paths(&[PathBuf::from("/repo/a.rs")], cx);
        });
        h.vcx.run_until_parked();
        activate_editor_for_path(&mut h, Path::new("/repo/a.rs"));

        h.workspace.update_in(h.vcx, |w, window, cx| {
            open_workspace_symbol_picker(w, window, cx);
        });
        h.vcx.run_until_parked();

        type_query(&mut h, "Foo");
        h.vcx.run_until_parked();

        let picker: Entity<Picker<WorkspaceSymbolPickerDelegate>> = h
            .workspace
            .read_with(h.vcx, |w, cx| w.modal_layer().read(cx).active_modal())
            .expect("picker is open");
        let names = picker.read_with(h.vcx, |p, _| {
            p.delegate()
                .entries
                .iter()
                .map(|e| e.title.clone())
                .collect::<Vec<_>>()
        });
        assert_eq!(names, vec!["FooStruct", "FooHelper"]);
    }

    #[test]
    fn empty_query_leaves_entries_empty() {
        let mut cx = TestAppContext::single();
        let fake_fs = fake_fs_with_files(&[("a.rs", "")]);
        let mut h = new_harness(&mut cx, fake_fs);

        h.lsp.set_capabilities(lsp_types::ServerCapabilities {
            workspace_symbol_provider: Some(OneOf::Left(true)),
            ..Default::default()
        });
        h.lsp
            .add_workspace_symbol("Foo", "FooStruct", SymbolKind::STRUCT, "/repo/a.rs", 0, 0);

        h.workspace.update(h.vcx, |w, cx| {
            w.open_paths(&[PathBuf::from("/repo/a.rs")], cx);
        });
        h.vcx.run_until_parked();
        activate_editor_for_path(&mut h, Path::new("/repo/a.rs"));

        h.workspace.update_in(h.vcx, |w, window, cx| {
            open_workspace_symbol_picker(w, window, cx);
        });
        h.vcx.run_until_parked();

        type_query(&mut h, "");
        h.vcx.run_until_parked();

        let picker: Entity<Picker<WorkspaceSymbolPickerDelegate>> = h
            .workspace
            .read_with(h.vcx, |w, cx| w.modal_layer().read(cx).active_modal())
            .expect("picker is open");
        let count = picker.read_with(h.vcx, |p, _| p.delegate().entries.len());
        assert_eq!(count, 0);
    }
}
