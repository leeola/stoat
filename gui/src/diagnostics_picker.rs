//! Diagnostics picker delegate.
//!
//! Snapshots LSP diagnostics from the workspace's
//! [`crate::diagnostics::DiagnosticSet`] at picker-open time. Local
//! scope lists every diagnostic attached to the focused editor's
//! path; workspace scope lists every `(path, diagnostic)` pair in
//! the set. Confirm dismisses the modal, opens the target file in
//! the focused pane (workspace scope only -- local scope already has
//! the buffer open), and collapses the active editor's primary
//! selection at the diagnostic's `(line, column)`.

use crate::{
    buffer::Buffer,
    editor::Editor,
    picker::{match_highlight_runs, rank_matches, Picker, PickerDelegate, PickerSecondary},
    theme::ActiveTheme,
    workspace::Workspace,
};
use gpui::{
    div, AnyElement, Context, DismissEvent, Entity, HighlightStyle, IntoElement, ParentElement,
    SharedString, Styled, StyledText, Task, WeakEntity, Window,
};
use lsp_types::{Diagnostic, DiagnosticSeverity};
use std::path::{Path, PathBuf};
use stoat_text::{Bias, Point, Selection, SelectionGoal};

const MESSAGE_MAX_CHARS: usize = 80;

/// Scope of a [`DiagnosticsPickerDelegate`]. Determines which entries
/// the renderer prefixes with a path column, and whether confirm
/// opens the target file before jumping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerScope {
    Local,
    Workspace,
}

#[derive(Debug, Clone)]
pub struct DiagnosticsEntry {
    pub line: u32,
    pub column: u32,
    pub severity: Option<DiagnosticSeverity>,
    pub message: String,
    /// Absolute path of the diagnostic's source file. `None` for
    /// `Local`-scope entries (the active editor already supplies the
    /// path); `Some` for `Workspace`-scope entries.
    pub path: Option<PathBuf>,
}

pub struct DiagnosticsPickerDelegate {
    workspace: WeakEntity<Workspace>,
    git_root: PathBuf,
    scope: PickerScope,
    entries: Vec<DiagnosticsEntry>,
    matches: Vec<(usize, Vec<u32>)>,
    selected: usize,
    query: String,
}

impl DiagnosticsPickerDelegate {
    /// Build a local-scope delegate listing every diagnostic
    /// attached to `buffer_path`. Entries sort by `(line, column)`
    /// ascending.
    pub fn from_buffer(
        workspace: WeakEntity<Workspace>,
        git_root: PathBuf,
        diagnostics: &[Diagnostic],
    ) -> Self {
        let mut entries: Vec<DiagnosticsEntry> = diagnostics
            .iter()
            .map(|diag| DiagnosticsEntry {
                line: diag.range.start.line,
                column: diag.range.start.character,
                severity: diag.severity,
                message: render_message(&diag.message),
                path: None,
            })
            .collect();
        entries.sort_by_key(|e| (e.line, e.column));
        let matches = (0..entries.len()).map(|i| (i, Vec::new())).collect();
        Self {
            workspace,
            git_root,
            scope: PickerScope::Local,
            entries,
            matches,
            selected: 0,
            query: String::new(),
        }
    }

    /// Build a workspace-scope delegate listing every `(path,
    /// diagnostic)` pair in `pairs`. Entries sort by `(path, line,
    /// column)` ascending so the rendered list reads predictably.
    pub fn from_workspace<'a, I>(
        workspace: WeakEntity<Workspace>,
        git_root: PathBuf,
        pairs: I,
    ) -> Self
    where
        I: IntoIterator<Item = (&'a Path, &'a [Diagnostic])>,
    {
        let mut entries: Vec<DiagnosticsEntry> = pairs
            .into_iter()
            .flat_map(|(path, diags)| {
                let path = path.to_path_buf();
                diags.iter().map(move |diag| DiagnosticsEntry {
                    line: diag.range.start.line,
                    column: diag.range.start.character,
                    severity: diag.severity,
                    message: render_message(&diag.message),
                    path: Some(path.clone()),
                })
            })
            .collect();
        entries.sort_by(|a, b| {
            a.path
                .as_deref()
                .cmp(&b.path.as_deref())
                .then_with(|| a.line.cmp(&b.line))
                .then_with(|| a.column.cmp(&b.column))
        });
        let matches = (0..entries.len()).map(|i| (i, Vec::new())).collect();
        Self {
            workspace,
            git_root,
            scope: PickerScope::Workspace,
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
        let scope = self.scope;
        let git_root = self.git_root.clone();
        let items = self.entries.iter().enumerate().map(|(i, entry)| {
            let display = render_row(entry, scope, &git_root);
            (i, display)
        });
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

    fn selected_entry(&self) -> Option<&DiagnosticsEntry> {
        let (idx, _) = self.matches.get(self.selected)?;
        self.entries.get(*idx)
    }
}

impl PickerDelegate for DiagnosticsPickerDelegate {
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
        let line = entry.line;
        let column = entry.column;
        // Defer past the keystroke observer's outer `Workspace::update`
        // lease so the re-entrant update does not panic.
        window.defer(cx, move |_window, cx| {
            workspace.update(cx, |workspace, cx| {
                if let Some(path) = entry.path.as_deref() {
                    workspace.open_paths(&[path.to_path_buf()], cx);
                }
                let editor = match entry.path.as_deref() {
                    Some(path) => workspace
                        .buffer_for_path(path, cx)
                        .and_then(|buffer| editor_for_buffer(workspace, &buffer, cx)),
                    None => workspace
                        .input_state_machine()
                        .read(cx)
                        .active_editor()
                        .cloned()
                        .and_then(|w| w.upgrade()),
                };
                let Some(editor) = editor else {
                    return;
                };
                set_cursor_to_point(&editor, line, column, cx);
            });
        });
        cx.emit(DismissEvent);
    }

    fn dismissed(&mut self, _cx: &mut Context<'_, Picker<Self>>) {}

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        cx: &mut Context<'_, Picker<Self>>,
    ) -> AnyElement {
        let Some((entry_idx, matched)) = self.matches.get(ix) else {
            return div().into_any_element();
        };
        let Some(entry) = self.entries.get(*entry_idx) else {
            return div().into_any_element();
        };
        let display = render_row(entry, self.scope, &self.git_root);
        let color = cx.theme().statusbar_text;
        let runs = match_highlight_runs(
            &display,
            matched,
            HighlightStyle {
                color: Some(gpui::white()),
                ..Default::default()
            },
        );
        let label = StyledText::new(SharedString::from(display)).with_highlights(runs);
        let mut row = div().px_2().text_color(color).child(label);
        if selected {
            row = row.bg(gpui::white().opacity(0.1));
        }
        row.into_any_element()
    }
}

/// Open the diagnostics picker for the focused editor's buffer. No-op
/// when no editor is active, the buffer has no path, the workspace
/// has no diagnostic set attached, or the path has no diagnostics.
pub fn open_diagnostics_picker(
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
    let set = workspace.diagnostics().read(cx);
    let diagnostics = set.get(&path).to_vec();
    if diagnostics.is_empty() {
        return;
    }
    let git_root = workspace.git_root().clone();
    let weak_workspace = cx.weak_entity();
    workspace.toggle_modal::<Picker<DiagnosticsPickerDelegate>, _>(
        window,
        cx,
        move |window, cx| {
            let delegate =
                DiagnosticsPickerDelegate::from_buffer(weak_workspace, git_root, &diagnostics);
            Picker::new(delegate, window, cx)
        },
    );
}

/// Open the diagnostics picker over every `(path, diagnostic)` pair
/// the workspace's [`DiagnosticSet`] currently holds. No-op when the
/// set is empty.
pub fn open_workspace_diagnostics_picker(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<'_, Workspace>,
) {
    let set_entity = workspace.diagnostics().clone();
    let git_root = workspace.git_root().clone();
    let weak_workspace = cx.weak_entity();
    let has_any = set_entity.read(cx).iter().next().is_some();
    if !has_any {
        return;
    }
    workspace.toggle_modal::<Picker<DiagnosticsPickerDelegate>, _>(
        window,
        cx,
        move |window, cx| {
            let set = set_entity.read(cx);
            let delegate =
                DiagnosticsPickerDelegate::from_workspace(weak_workspace, git_root, set.iter());
            Picker::new(delegate, window, cx)
        },
    );
}

fn render_row(entry: &DiagnosticsEntry, scope: PickerScope, git_root: &Path) -> String {
    let sev = severity_glyph(entry.severity);
    let pos = format!("{:>4}:{:<3}", entry.line + 1, entry.column + 1);
    match scope {
        PickerScope::Local => format!("{pos} {sev} {}", entry.message),
        PickerScope::Workspace => {
            let path = entry
                .path
                .as_deref()
                .map(|p| display_path(p, git_root))
                .unwrap_or_default();
            format!("{path}  {pos} {sev} {}", entry.message)
        },
    }
}

fn render_message(raw: &str) -> String {
    raw.replace('\n', " ")
        .chars()
        .take(MESSAGE_MAX_CHARS)
        .collect()
}

fn severity_glyph(severity: Option<DiagnosticSeverity>) -> &'static str {
    match severity {
        Some(DiagnosticSeverity::ERROR) => "E",
        Some(DiagnosticSeverity::WARNING) => "W",
        Some(DiagnosticSeverity::INFORMATION) => "I",
        Some(DiagnosticSeverity::HINT) => "H",
        _ => " ",
    }
}

fn display_path(path: &Path, git_root: &Path) -> String {
    match path.strip_prefix(git_root) {
        Ok(rel) => rel.to_string_lossy().into_owned(),
        Err(_) => path.to_string_lossy().into_owned(),
    }
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

fn set_cursor_to_point(
    editor: &Entity<Editor>,
    line: u32,
    column: u32,
    cx: &mut Context<'_, Workspace>,
) {
    editor.update(cx, |ed, cx| {
        let snapshot = ed.multi_buffer().read(cx).snapshot();
        let rope = snapshot.rope();
        let offset = rope
            .point_to_offset(Point::new(line, column))
            .min(rope.len());
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::globals::{ExecutorGlobal, FsHostGlobal, FsWatchHostGlobal};
    use gpui::{TestAppContext, VisualTestContext};
    use lsp_types::{Position, Range};
    use std::sync::Arc;
    use stoat::host::{fake::FakeFs, FsWatchHost};
    use stoat_host::NoopFsWatcher;
    use stoat_scheduler::{Executor, TestScheduler};

    fn install_globals(cx: &mut TestAppContext, fake_fs: Arc<FakeFs>) {
        let executor = Executor::new(Arc::new(TestScheduler::new()));
        cx.update(|cx| {
            cx.set_global(ExecutorGlobal(executor));
            cx.set_global(FsHostGlobal(fake_fs));
            cx.set_global(FsWatchHostGlobal(
                Arc::new(NoopFsWatcher::new()) as Arc<dyn FsWatchHost>
            ));
        });
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

    fn diag(line: u32, column: u32, message: &str, severity: DiagnosticSeverity) -> Diagnostic {
        Diagnostic {
            range: Range {
                start: Position {
                    line,
                    character: column,
                },
                end: Position {
                    line,
                    character: column + 1,
                },
            },
            severity: Some(severity),
            code: None,
            code_description: None,
            source: None,
            message: message.to_string(),
            related_information: None,
            tags: None,
            data: None,
        }
    }

    struct Harness<'a> {
        workspace: Entity<Workspace>,
        vcx: &'a mut VisualTestContext,
    }

    fn new_harness<'a>(cx: &'a mut TestAppContext, fake_fs: Arc<FakeFs>) -> Harness<'a> {
        install_globals(cx, fake_fs);
        let (workspace, vcx) =
            cx.add_window_view(|_window, cx| Workspace::new("main", PathBuf::from("/repo"), cx));
        Harness { workspace, vcx }
    }

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

    fn weak_workspace() -> WeakEntity<Workspace> {
        WeakEntity::new_invalid()
    }

    #[test]
    fn from_buffer_sorts_entries_by_position() {
        let diagnostics = vec![
            diag(2, 2, "third", DiagnosticSeverity::WARNING),
            diag(0, 0, "first", DiagnosticSeverity::ERROR),
            diag(1, 1, "second", DiagnosticSeverity::INFORMATION),
        ];
        let delegate = DiagnosticsPickerDelegate::from_buffer(
            weak_workspace(),
            PathBuf::from("/repo"),
            &diagnostics,
        );
        assert_eq!(delegate.entries.len(), 3);
        let messages: Vec<&str> = delegate
            .entries
            .iter()
            .map(|e| e.message.as_str())
            .collect();
        assert_eq!(messages, vec!["first", "second", "third"]);
        assert_eq!(delegate.scope, PickerScope::Local);
        assert!(delegate.entries.iter().all(|e| e.path.is_none()));
    }

    #[test]
    fn from_workspace_lists_pairs_across_paths_sorted_by_path() {
        let a = PathBuf::from("/repo/a.rs");
        let b = PathBuf::from("/repo/b.rs");
        let b_diags = vec![
            diag(1, 0, "b-second", DiagnosticSeverity::WARNING),
            diag(0, 0, "b-first", DiagnosticSeverity::ERROR),
        ];
        let a_diags = vec![diag(0, 0, "a-first", DiagnosticSeverity::ERROR)];
        let pairs: Vec<(&Path, &[Diagnostic])> = vec![
            (b.as_path(), b_diags.as_slice()),
            (a.as_path(), a_diags.as_slice()),
        ];
        let delegate = DiagnosticsPickerDelegate::from_workspace(
            weak_workspace(),
            PathBuf::from("/repo"),
            pairs,
        );
        let titles: Vec<&str> = delegate
            .entries
            .iter()
            .map(|e| e.message.as_str())
            .collect();
        assert_eq!(titles, vec!["a-first", "b-first", "b-second"]);
        assert_eq!(delegate.scope, PickerScope::Workspace);
        assert!(delegate.entries.iter().all(|e| e.path.is_some()));
    }

    #[test]
    fn refilter_narrows_against_message_query() {
        let diagnostics = vec![
            diag(0, 0, "first error", DiagnosticSeverity::ERROR),
            diag(1, 0, "second warning", DiagnosticSeverity::WARNING),
            diag(2, 0, "third error", DiagnosticSeverity::ERROR),
        ];
        let mut delegate = DiagnosticsPickerDelegate::from_buffer(
            weak_workspace(),
            PathBuf::from("/repo"),
            &diagnostics,
        );
        delegate.query = "error".to_string();
        delegate.refilter();
        let visible: Vec<&str> = delegate
            .matches
            .iter()
            .map(|(i, _)| delegate.entries[*i].message.as_str())
            .collect();
        assert!(visible.contains(&"first error"));
        assert!(visible.contains(&"third error"));
        assert!(!visible.contains(&"second warning"));
    }

    #[test]
    fn render_message_truncates_and_strips_newlines() {
        let long = "a".repeat(200);
        let multi = format!("first\nsecond\n{long}");
        let rendered = render_message(&multi);
        assert_eq!(rendered.chars().count(), MESSAGE_MAX_CHARS);
        assert!(!rendered.contains('\n'));
    }

    #[test]
    fn empty_delegate_lists_zero_matches() {
        let delegate =
            DiagnosticsPickerDelegate::from_buffer(weak_workspace(), PathBuf::from("/repo"), &[]);
        assert_eq!(delegate.match_count(), 0);
    }

    #[test]
    fn open_diagnostics_picker_opens_modal_with_local_entries() {
        let mut cx = TestAppContext::single();
        let fake_fs = fake_fs_with_files(&[("a.rs", "alpha\nbeta\n")]);
        let mut h = new_harness(&mut cx, fake_fs);

        let path = PathBuf::from("/repo/a.rs");
        h.workspace.update(h.vcx, |w, cx| {
            w.open_paths(std::slice::from_ref(&path), cx);
        });
        h.vcx.run_until_parked();
        activate_editor_for_path(&mut h, &path);

        h.workspace.update(h.vcx, |w, cx| {
            w.diagnostics().update(cx, |set, cx| {
                set.replace_for_path(
                    path.clone(),
                    vec![
                        diag(0, 0, "first", DiagnosticSeverity::ERROR),
                        diag(1, 1, "second", DiagnosticSeverity::WARNING),
                    ],
                    cx,
                );
            });
        });
        h.vcx.run_until_parked();

        h.workspace.update_in(h.vcx, |w, window, cx| {
            open_diagnostics_picker(w, window, cx);
        });
        h.vcx.run_until_parked();

        let picker: Entity<Picker<DiagnosticsPickerDelegate>> = h
            .workspace
            .read_with(h.vcx, |w, cx| w.modal_layer().read(cx).active_modal())
            .expect("diagnostics picker modal should be open");
        let messages = picker.read_with(h.vcx, |p, _| {
            p.delegate()
                .entries
                .iter()
                .map(|e| e.message.clone())
                .collect::<Vec<_>>()
        });
        assert_eq!(messages, vec!["first", "second"]);
        let scope = picker.read_with(h.vcx, |p, _| p.delegate().scope);
        assert_eq!(scope, PickerScope::Local);
    }

    #[test]
    fn open_diagnostics_picker_noop_when_no_diagnostics_for_path() {
        let mut cx = TestAppContext::single();
        let fake_fs = fake_fs_with_files(&[("a.rs", "alpha\n")]);
        let mut h = new_harness(&mut cx, fake_fs);

        let path = PathBuf::from("/repo/a.rs");
        h.workspace.update(h.vcx, |w, cx| {
            w.open_paths(std::slice::from_ref(&path), cx);
        });
        h.vcx.run_until_parked();
        activate_editor_for_path(&mut h, &path);

        h.workspace.update_in(h.vcx, |w, window, cx| {
            open_diagnostics_picker(w, window, cx);
        });
        h.vcx.run_until_parked();

        let has_modal = h.workspace.read_with(h.vcx, |w, cx| {
            w.modal_layer()
                .read(cx)
                .active_modal::<Picker<DiagnosticsPickerDelegate>>()
                .is_some()
        });
        assert!(
            !has_modal,
            "diagnostics picker should not open when the path has no diagnostics"
        );
    }

    #[test]
    fn open_workspace_diagnostics_picker_lists_pairs_across_files() {
        let mut cx = TestAppContext::single();
        let fake_fs = fake_fs_with_files(&[("a.rs", "alpha\n"), ("b.rs", "beta\n")]);
        let h = new_harness(&mut cx, fake_fs);

        let a = PathBuf::from("/repo/a.rs");
        let b = PathBuf::from("/repo/b.rs");
        h.workspace.update(h.vcx, |w, cx| {
            w.diagnostics().update(cx, |set, cx| {
                set.replace_for_path(
                    a.clone(),
                    vec![diag(0, 0, "a-first", DiagnosticSeverity::ERROR)],
                    cx,
                );
                set.replace_for_path(
                    b.clone(),
                    vec![diag(2, 0, "b-first", DiagnosticSeverity::WARNING)],
                    cx,
                );
            });
        });
        h.vcx.run_until_parked();

        h.workspace.update_in(h.vcx, |w, window, cx| {
            open_workspace_diagnostics_picker(w, window, cx);
        });
        h.vcx.run_until_parked();

        let picker: Entity<Picker<DiagnosticsPickerDelegate>> = h
            .workspace
            .read_with(h.vcx, |w, cx| w.modal_layer().read(cx).active_modal())
            .expect("workspace diagnostics picker modal should be open");
        let entries = picker.read_with(h.vcx, |p, _| {
            p.delegate()
                .entries
                .iter()
                .map(|e| (e.path.clone(), e.message.clone()))
                .collect::<Vec<_>>()
        });
        assert_eq!(
            entries,
            vec![
                (Some(a.clone()), "a-first".to_string()),
                (Some(b.clone()), "b-first".to_string()),
            ]
        );
        let scope = picker.read_with(h.vcx, |p, _| p.delegate().scope);
        assert_eq!(scope, PickerScope::Workspace);
    }

    #[test]
    fn open_workspace_diagnostics_picker_noop_when_set_empty() {
        let mut cx = TestAppContext::single();
        let fake_fs = fake_fs_with_files(&[("a.rs", "alpha\n")]);
        let h = new_harness(&mut cx, fake_fs);

        h.workspace.update_in(h.vcx, |w, window, cx| {
            open_workspace_diagnostics_picker(w, window, cx);
        });
        h.vcx.run_until_parked();

        let has_modal = h.workspace.read_with(h.vcx, |w, cx| {
            w.modal_layer()
                .read(cx)
                .active_modal::<Picker<DiagnosticsPickerDelegate>>()
                .is_some()
        });
        assert!(
            !has_modal,
            "workspace diagnostics picker should not open when the set is empty"
        );
    }
}
