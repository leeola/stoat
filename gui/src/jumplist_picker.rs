//! Jumplist picker delegate.
//!
//! Snapshots the focused editor's [`stoat::jumplist::JumpList`] at
//! picker-open time. Each entry resolves a saved byte offset into a
//! `(line, column)` pair plus a trimmed snippet of the line content
//! so the modal reads at a glance. Confirm dismisses the modal and
//! calls back into the editor to collapse the primary selection at
//! the chosen offset while advancing the jumplist's navigation
//! cursor so future `JumpBackward` / `JumpForward` walks resume from
//! that point.

use crate::{
    editor::Editor,
    picker::{match_highlight_runs, rank_matches, Picker, PickerDelegate, PickerSecondary},
    theme::ActiveTheme,
    workspace::Workspace,
};
use gpui::{
    div, AnyElement, Context, DismissEvent, Entity, HighlightStyle, IntoElement, ParentElement,
    SharedString, Styled, StyledText, Task, WeakEntity, Window,
};
use stoat::jumplist::JumpList;

const SNIPPET_MAX_CHARS: usize = 80;

#[derive(Debug, Clone)]
pub struct JumplistEntry {
    pub line: u32,
    pub column: u32,
    pub snippet: String,
}

pub struct JumplistPickerDelegate {
    editor: WeakEntity<Editor>,
    entries: Vec<JumplistEntry>,
    matches: Vec<(usize, Vec<u32>)>,
    selected: usize,
    query: String,
}

impl JumplistPickerDelegate {
    /// Materialize the picker's entries from `jumplist`, resolving
    /// each saved offset against `multi_buffer`'s rope. Offsets past
    /// the rope end clamp to the end. The initial selection mirrors
    /// the jumplist's navigation cursor, clamped to the entry count.
    pub fn from_editor(
        editor: WeakEntity<Editor>,
        jumplist: &JumpList,
        multi_buffer: &Entity<crate::multi_buffer::MultiBuffer>,
        cx: &gpui::App,
    ) -> Self {
        let snapshot = multi_buffer.read(cx).snapshot();
        let rope = snapshot.rope();
        let rope_len = rope.len();
        let entries: Vec<JumplistEntry> = jumplist
            .entries()
            .iter()
            .map(|&offset| {
                let clipped = offset.min(rope_len);
                let point = rope.offset_to_point(clipped);
                let raw = rope.line_at_row(point.row);
                let trimmed = raw.trim_start();
                let snippet: String = trimmed.chars().take(SNIPPET_MAX_CHARS).collect();
                JumplistEntry {
                    line: point.row + 1,
                    column: point.column + 1,
                    snippet,
                }
            })
            .collect();
        let matches = (0..entries.len()).map(|i| (i, Vec::new())).collect();
        let cursor = jumplist.cursor();
        let selected = cursor.min(entries.len().saturating_sub(1));
        Self {
            editor,
            entries,
            matches,
            selected,
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
            .map(|(i, entry)| (i, render_row(entry)));
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

    fn selected_entry_idx(&self) -> Option<usize> {
        self.matches.get(self.selected).map(|(idx, _)| *idx)
    }
}

impl PickerDelegate for JumplistPickerDelegate {
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
        _window: &mut Window,
        cx: &mut Context<'_, Picker<Self>>,
    ) {
        let Some(entry_idx) = self.selected_entry_idx() else {
            return;
        };
        let Some(editor) = self.editor.upgrade() else {
            return;
        };
        editor.update(cx, |ed, cx| {
            ed.handle_jump_to_jumplist_entry(entry_idx, cx);
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
        let display = render_row(entry);
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

/// Open the jumplist picker for the focused editor. No-op when no
/// editor is active or its jumplist is empty.
pub fn open_jumplist_picker(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<'_, Workspace>,
) {
    let weak_editor = workspace
        .input_state_machine()
        .read(cx)
        .active_editor()
        .cloned();
    let Some(editor) = weak_editor.clone().and_then(|w| w.upgrade()) else {
        return;
    };
    let (jumplist, multi_buffer) = editor.read_with(cx, |ed, _| {
        (ed.jumplist().clone(), ed.multi_buffer().clone())
    });
    if jumplist.entries().is_empty() {
        return;
    }
    let weak_editor = weak_editor.expect("weak_editor present after upgrade");
    workspace.toggle_modal::<Picker<JumplistPickerDelegate>, _>(window, cx, move |window, cx| {
        let delegate =
            JumplistPickerDelegate::from_editor(weak_editor, &jumplist, &multi_buffer, cx);
        Picker::new(delegate, window, cx)
    });
}

fn render_row(entry: &JumplistEntry) -> String {
    format!("{:>4}:{:<3} {}", entry.line, entry.column, entry.snippet)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::globals::{ExecutorGlobal, FsHostGlobal, FsWatchHostGlobal};
    use gpui::{TestAppContext, VisualTestContext};
    use std::{
        path::{Path, PathBuf},
        sync::Arc,
    };
    use stoat::host::{fake::FakeFs, FsWatchHost};
    use stoat_host::NoopFsWatcher;
    use stoat_scheduler::{Executor, TestScheduler};
    use stoat_text::{Bias, Selection, SelectionGoal};

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

    fn editor_for_path(h: &mut Harness<'_>, path: &Path) -> Entity<Editor> {
        h.workspace
            .read_with(h.vcx, |w, cx| {
                let buffer = w.buffer_for_path(path, cx).expect("buffer for path");
                let target_id = buffer.entity_id();
                let pane_tree = w.pane_tree().read(cx);
                for pane_id in pane_tree.split_pane_ids() {
                    let pane = pane_tree.pane(pane_id).expect("pane id valid");
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
            })
            .expect("editor for opened path")
    }

    fn activate_editor(h: &mut Harness<'_>, editor: &Entity<Editor>) {
        let sm = h
            .workspace
            .read_with(h.vcx, |w, _| w.input_state_machine().clone());
        sm.update(h.vcx, |sm, _| {
            sm.set_active_editor(Some(editor.downgrade()))
        });
    }

    fn seed_cursor(editor: &Entity<Editor>, h: &mut Harness<'_>, offset: usize) {
        editor.update(h.vcx, |ed, cx| {
            let snapshot = ed.multi_buffer().read(cx).snapshot();
            let anchor = snapshot.anchor_at(offset, Bias::Left);
            ed.selections_mut().replace_with(
                vec![Selection {
                    id: 1,
                    start: anchor,
                    end: anchor,
                    reversed: false,
                    goal: SelectionGoal::None,
                }],
                &snapshot,
            );
        });
    }

    fn save_selection(editor: &Entity<Editor>, h: &mut Harness<'_>) {
        editor.update(h.vcx, |ed, cx| ed.handle_save_selection(cx));
    }

    fn primary_offset(editor: &Entity<Editor>, h: &mut Harness<'_>) -> usize {
        editor.update(h.vcx, |ed, cx| {
            let snapshot = ed.multi_buffer().read(cx).snapshot();
            let sel = ed
                .selections()
                .all_anchors()
                .iter()
                .max_by_key(|s| s.id)
                .expect("at least one selection");
            snapshot.resolve_anchor(&sel.head())
        })
    }

    fn open_jumplist_picker_test(
        editor: &Entity<Editor>,
        h: &mut Harness<'_>,
    ) -> Option<Entity<Picker<JumplistPickerDelegate>>> {
        let (jumplist, multi_buffer) = editor.read_with(h.vcx, |ed, _| {
            (ed.jumplist().clone(), ed.multi_buffer().clone())
        });
        if jumplist.entries().is_empty() {
            return None;
        }
        let weak_editor = editor.downgrade();
        h.workspace.update_in(h.vcx, |w, window, cx| {
            w.toggle_modal::<Picker<JumplistPickerDelegate>, _>(window, cx, move |window, cx| {
                let delegate =
                    JumplistPickerDelegate::from_editor(weak_editor, &jumplist, &multi_buffer, cx);
                Picker::new(delegate, window, cx)
            });
        });
        h.vcx.run_until_parked();
        h.workspace
            .read_with(h.vcx, |w, cx| w.modal_layer().read(cx).active_modal())
    }

    #[test]
    fn from_editor_lists_every_entry_with_line_col_and_snippet() {
        let mut cx = TestAppContext::single();
        let fake_fs = fake_fs_with_files(&[("a.rs", "alpha\nbeta\ngamma\n")]);
        let mut h = new_harness(&mut cx, fake_fs);

        let path = PathBuf::from("/repo/a.rs");
        h.workspace.update(h.vcx, |w, cx| {
            w.open_paths(std::slice::from_ref(&path), cx);
        });
        h.vcx.run_until_parked();
        let editor = editor_for_path(&mut h, &path);

        for offset in [0, 6, 11] {
            seed_cursor(&editor, &mut h, offset);
            save_selection(&editor, &mut h);
        }

        let (jumplist, multi_buffer) = editor.read_with(h.vcx, |ed, _| {
            (ed.jumplist().clone(), ed.multi_buffer().clone())
        });
        let delegate = h.workspace.update(h.vcx, |_, cx| {
            JumplistPickerDelegate::from_editor(editor.downgrade(), &jumplist, &multi_buffer, cx)
        });

        assert_eq!(delegate.entries.len(), 3);
        let positions: Vec<(u32, u32)> = delegate
            .entries
            .iter()
            .map(|e| (e.line, e.column))
            .collect();
        assert_eq!(positions, vec![(1, 1), (2, 1), (3, 1)]);
        let snippets: Vec<&str> = delegate
            .entries
            .iter()
            .map(|e| e.snippet.as_str())
            .collect();
        assert_eq!(snippets, vec!["alpha", "beta", "gamma"]);
    }

    #[test]
    fn refilter_narrows_against_snippet_query() {
        let mut cx = TestAppContext::single();
        let fake_fs = fake_fs_with_files(&[("a.rs", "alpha first\nbeta second\ngamma third\n")]);
        let mut h = new_harness(&mut cx, fake_fs);

        let path = PathBuf::from("/repo/a.rs");
        h.workspace.update(h.vcx, |w, cx| {
            w.open_paths(std::slice::from_ref(&path), cx);
        });
        h.vcx.run_until_parked();
        let editor = editor_for_path(&mut h, &path);
        for offset in [0, 12, 24] {
            seed_cursor(&editor, &mut h, offset);
            save_selection(&editor, &mut h);
        }

        let (jumplist, multi_buffer) = editor.read_with(h.vcx, |ed, _| {
            (ed.jumplist().clone(), ed.multi_buffer().clone())
        });
        let mut delegate = h.workspace.update(h.vcx, |_, cx| {
            JumplistPickerDelegate::from_editor(editor.downgrade(), &jumplist, &multi_buffer, cx)
        });

        delegate.query = "gamma".to_string();
        delegate.refilter();
        let visible: Vec<&str> = delegate
            .matches
            .iter()
            .map(|(i, _)| delegate.entries[*i].snippet.as_str())
            .collect();
        assert_eq!(visible, vec!["gamma third"]);
    }

    #[test]
    fn open_jumplist_picker_opens_modal_with_entries() {
        let mut cx = TestAppContext::single();
        let fake_fs = fake_fs_with_files(&[("a.rs", "alpha\nbeta\n")]);
        let mut h = new_harness(&mut cx, fake_fs);

        let path = PathBuf::from("/repo/a.rs");
        h.workspace.update(h.vcx, |w, cx| {
            w.open_paths(std::slice::from_ref(&path), cx);
        });
        h.vcx.run_until_parked();
        let editor = editor_for_path(&mut h, &path);
        activate_editor(&mut h, &editor);
        for offset in [0, 6] {
            seed_cursor(&editor, &mut h, offset);
            save_selection(&editor, &mut h);
        }

        h.workspace.update_in(h.vcx, |w, window, cx| {
            open_jumplist_picker(w, window, cx);
        });
        h.vcx.run_until_parked();

        let picker: Entity<Picker<JumplistPickerDelegate>> = h
            .workspace
            .read_with(h.vcx, |w, cx| w.modal_layer().read(cx).active_modal())
            .expect("jumplist picker modal should be open");
        let snippets = picker.read_with(h.vcx, |p, _| {
            p.delegate()
                .entries
                .iter()
                .map(|e| e.snippet.clone())
                .collect::<Vec<_>>()
        });
        assert_eq!(snippets, vec!["alpha", "beta"]);
    }

    #[test]
    fn open_jumplist_picker_noop_when_jumplist_empty() {
        let mut cx = TestAppContext::single();
        let fake_fs = fake_fs_with_files(&[("a.rs", "alpha\n")]);
        let mut h = new_harness(&mut cx, fake_fs);

        let path = PathBuf::from("/repo/a.rs");
        h.workspace.update(h.vcx, |w, cx| {
            w.open_paths(std::slice::from_ref(&path), cx);
        });
        h.vcx.run_until_parked();
        let editor = editor_for_path(&mut h, &path);
        activate_editor(&mut h, &editor);

        h.workspace.update_in(h.vcx, |w, window, cx| {
            open_jumplist_picker(w, window, cx);
        });
        h.vcx.run_until_parked();

        let has_modal = h.workspace.read_with(h.vcx, |w, cx| {
            w.modal_layer()
                .read(cx)
                .active_modal::<Picker<JumplistPickerDelegate>>()
                .is_some()
        });
        assert!(
            !has_modal,
            "jumplist picker should not open when the jumplist is empty"
        );
    }

    #[test]
    fn confirm_jumps_cursor_and_advances_jumplist_cursor() {
        let mut cx = TestAppContext::single();
        let fake_fs = fake_fs_with_files(&[("a.rs", "alpha\nbeta\ngamma\n")]);
        let mut h = new_harness(&mut cx, fake_fs);

        let path = PathBuf::from("/repo/a.rs");
        h.workspace.update(h.vcx, |w, cx| {
            w.open_paths(std::slice::from_ref(&path), cx);
        });
        h.vcx.run_until_parked();
        let editor = editor_for_path(&mut h, &path);
        activate_editor(&mut h, &editor);
        for offset in [0, 6, 11] {
            seed_cursor(&editor, &mut h, offset);
            save_selection(&editor, &mut h);
        }
        seed_cursor(&editor, &mut h, 0);

        let picker = open_jumplist_picker_test(&editor, &mut h).expect("picker open");
        picker.update(h.vcx, |p, cx| p.delegate_mut().set_selected_index(1, cx));
        picker.update_in(h.vcx, |p, window, cx| {
            p.delegate_mut().confirm(None, window, cx)
        });
        h.vcx.run_until_parked();

        assert_eq!(primary_offset(&editor, &mut h), 6);
        let cursor = editor.read_with(h.vcx, |ed, _| ed.jumplist().cursor());
        assert_eq!(cursor, 2);
    }
}
