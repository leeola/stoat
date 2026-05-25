//! Diff hunk panel: a dockable side pane listing every diff hunk
//! in a host editor's buffer. Selecting a row jumps the editor's
//! primary cursor to the hunk's first changed line.

use crate::{
    diff_map::DiffMapEvent,
    editor::Editor,
    item::{DeserializeSnafu, ItemError, ItemKind, ItemView},
    theme::ActiveTheme,
};
use gpui::{
    div, px, App, Context, Entity, InteractiveElement, IntoElement, MouseButton, ParentElement,
    Render, SharedString, Styled, Subscription, WeakEntity, Window,
};
use serde_json::Value;
use stoat::DiffHunkStatus;

pub struct DiffHunkPanel {
    editor: WeakEntity<Editor>,
    hunks: Vec<HunkRow>,
    selected: usize,
    _diff_sub: Option<Subscription>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct HunkRow {
    start_line: u32,
    end_line: u32,
    status: DiffHunkStatus,
}

impl DiffHunkPanel {
    pub fn new(editor: &Entity<Editor>, cx: &mut Context<'_, Self>) -> Self {
        let weak_editor = editor.downgrade();
        let diff_map = editor.read(cx).diff_map().clone();
        let hunks = snapshot_hunks(&diff_map, cx);
        let sub = cx.subscribe(&diff_map, |this, _, _event: &DiffMapEvent, cx| {
            if let Some(editor) = this.editor.upgrade() {
                let dm = editor.read(cx).diff_map().clone();
                this.hunks = snapshot_hunks(&dm, cx);
                if this.selected >= this.hunks.len() {
                    this.selected = this.hunks.len().saturating_sub(1);
                }
                cx.notify();
            }
        });
        Self {
            editor: weak_editor,
            hunks,
            selected: 0,
            _diff_sub: Some(sub),
        }
    }

    /// Select the hunk at `idx` and jump the host editor's primary
    /// cursor to that hunk's first buffer line. No-op when the
    /// index is out of range or the host editor has been dropped.
    pub fn select_hunk(&mut self, idx: usize, cx: &mut Context<'_, Self>) {
        let Some(hunk) = self.hunks.get(idx).copied() else {
            return;
        };
        self.selected = idx;
        if let Some(editor) = self.editor.upgrade() {
            editor.update(cx, |ed, cx| {
                ed.set_cursor_at_buffer_row(hunk.start_line, cx)
            });
        }
        cx.notify();
    }
}

fn snapshot_hunks(diff_map: &Entity<crate::diff_map::DiffMap>, cx: &App) -> Vec<HunkRow> {
    diff_map
        .read(cx)
        .diff()
        .hunks_in_range(0..u32::MAX)
        .iter()
        .map(|h| HunkRow {
            start_line: h.buffer_start_line,
            end_line: h.buffer_line_range.end,
            status: h.status,
        })
        .collect()
}

fn status_glyph(status: DiffHunkStatus) -> &'static str {
    match status {
        DiffHunkStatus::Added => "+",
        DiffHunkStatus::Modified => "~",
        DiffHunkStatus::Deleted => "-",
        DiffHunkStatus::Moved => ">",
    }
}

impl Render for DiffHunkPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let theme = cx.theme();
        let mut list = div().size_full().px_2().py_1();
        for (idx, hunk) in self.hunks.iter().enumerate() {
            let label = format!(
                "{}  L{}-{}",
                status_glyph(hunk.status),
                hunk.start_line + 1,
                hunk.end_line.max(hunk.start_line + 1),
            );
            let selected = idx == self.selected && !self.hunks.is_empty();
            let row_color = if selected {
                theme.diff_current_hunk
            } else {
                theme.diff_context
            };
            let mut row = div()
                .px_1()
                .py_0p5()
                .text_color(row_color)
                .child(label)
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, _window, cx| {
                        this.select_hunk(idx, cx);
                    }),
                );
            if selected {
                row = row.bg(theme.selection);
            }
            list = list.child(row);
        }
        list.line_height(px(20.0))
    }
}

impl ItemView for DiffHunkPanel {
    fn tab_label(&self, _cx: &App) -> SharedString {
        SharedString::from("Diff Hunks")
    }

    fn deserialize(_value: Value, _cx: &mut Context<'_, Self>) -> Result<Self, ItemError>
    where
        Self: Sized,
    {
        DeserializeSnafu {
            reason: "DiffHunkPanel is transient and not persisted",
        }
        .fail()
    }

    fn item_kind(&self) -> ItemKind {
        ItemKind::Unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        buffer::Buffer, diff_map::DiffMap, display_map::DisplayMap, editor::EditorMode,
        multi_buffer::MultiBuffer,
    };
    use gpui::{AppContext, TestAppContext};
    use std::sync::Arc;
    use stoat::{buffer::BufferId, diff_map::DiffHunk};
    use stoat_scheduler::{Executor, TestScheduler};

    fn new_editor(cx: &mut TestAppContext, text: &str) -> Entity<Editor> {
        let buffer = cx.update(|cx| cx.new(|_| Buffer::with_text(BufferId::new(0), text)));
        let executor = Executor::new(Arc::new(TestScheduler::new()));
        let multi_buffer = {
            let buffer = buffer.clone();
            cx.update(|cx| cx.new(|cx| MultiBuffer::singleton(buffer, cx)))
        };
        let display_map = {
            let buffer = buffer.clone();
            cx.update(|cx| cx.new(|cx| DisplayMap::new(buffer, executor, cx)))
        };
        let diff_map = {
            let buffer = buffer.clone();
            cx.update(|cx| cx.new(|cx| DiffMap::new(buffer, cx)))
        };
        cx.update(|cx| {
            cx.new(|cx| Editor::new(multi_buffer, display_map, diff_map, EditorMode::full(), cx))
        })
    }

    fn added_hunk(lines: std::ops::Range<u32>) -> DiffHunk {
        DiffHunk {
            status: DiffHunkStatus::Added,
            buffer_start_line: lines.start,
            buffer_line_range: lines,
            base_byte_range: 0..0,
            anchor_range: None,
            token_detail: None,
        }
    }

    fn seed_hunks(editor: &Entity<Editor>, cx: &mut TestAppContext, hunks: Vec<DiffHunk>) {
        editor.update(cx, |ed, cx| {
            let new = stoat::DiffMap::from_hunks(hunks, None);
            ed.diff_map().update(cx, |dm, cx| dm.set_diff(new, cx));
        });
        cx.run_until_parked();
    }

    fn cursor_row(editor: &Entity<Editor>, cx: &mut TestAppContext) -> u32 {
        editor.update(cx, |ed, cx| {
            let snapshot = ed.multi_buffer().read(cx).snapshot();
            let head = ed.selections().all_anchors()[0].head();
            snapshot.point_for_anchor(&head).row
        })
    }

    #[test]
    fn snapshots_hunks_from_attached_editor() {
        let mut cx = TestAppContext::single();
        let editor = new_editor(&mut cx, "alpha\nbeta\ngamma\ndelta\nepsilon");
        seed_hunks(&editor, &mut cx, vec![added_hunk(1..2), added_hunk(3..4)]);

        let panel = cx.update(|cx| cx.new(|cx| DiffHunkPanel::new(&editor, cx)));
        panel.read_with(&cx, |p, _| {
            assert_eq!(p.hunks.len(), 2);
        });
    }

    #[test]
    fn select_hunk_moves_editor_cursor_to_hunk_start() {
        let mut cx = TestAppContext::single();
        let editor = new_editor(&mut cx, "alpha\nbeta\ngamma\ndelta\nepsilon");
        seed_hunks(&editor, &mut cx, vec![added_hunk(1..2), added_hunk(3..4)]);

        let panel = cx.update(|cx| cx.new(|cx| DiffHunkPanel::new(&editor, cx)));
        panel.update(&mut cx, |p, cx| p.select_hunk(1, cx));

        assert_eq!(cursor_row(&editor, &mut cx), 3);
        panel.read_with(&cx, |p, _| assert_eq!(p.selected, 1));
    }

    #[test]
    fn select_hunk_out_of_range_is_noop() {
        let mut cx = TestAppContext::single();
        let editor = new_editor(&mut cx, "alpha\nbeta");
        seed_hunks(&editor, &mut cx, vec![added_hunk(0..1)]);

        let panel = cx.update(|cx| cx.new(|cx| DiffHunkPanel::new(&editor, cx)));
        panel.update(&mut cx, |p, cx| p.select_hunk(5, cx));

        assert_eq!(cursor_row(&editor, &mut cx), 0);
        panel.read_with(&cx, |p, _| assert_eq!(p.selected, 0));
    }

    #[test]
    fn diff_map_change_refreshes_hunk_snapshot() {
        let mut cx = TestAppContext::single();
        let editor = new_editor(&mut cx, "alpha\nbeta\ngamma");
        seed_hunks(&editor, &mut cx, vec![added_hunk(1..2)]);

        let panel = cx.update(|cx| cx.new(|cx| DiffHunkPanel::new(&editor, cx)));
        panel.read_with(&cx, |p, _| assert_eq!(p.hunks.len(), 1));

        seed_hunks(&editor, &mut cx, vec![added_hunk(0..1), added_hunk(2..3)]);

        panel.read_with(&cx, |p, _| assert_eq!(p.hunks.len(), 2));
    }
}
