use crate::{
    buffer::BufferId,
    editor_state::{EditorId, EditorState},
    render::{editor::render_editor, pane::mode_segment, text::write_str},
    theme::{scope, Theme},
    workspace::Workspace,
};
use ratatui::{buffer::Buffer, layout::Rect};
use slotmap::SlotMap;
use stoat_scheduler::Executor;
use stoat_text::{Bias, SelectionGoal};

/// Embeddable multi-line text editor backed by the standard [`EditorState`].
/// Used for every compact text-input surface in the TUI: command palette,
/// help search, run pane, Claude chat, reword. Height grows with the rope
/// line count up to `max_height`, so a single-line field looks compact while
/// still being a real editor. When the global mode is `"prompt"`, the view
/// hides its modeline and `Enter` is routed to submission; in other modes
/// the modeline is shown and submission requires an explicit action.
#[derive(Debug, Clone)]
pub(crate) struct InputView {
    pub(crate) editor_id: EditorId,
    pub(crate) buffer_id: BufferId,
    pub(crate) target: SubmitTarget,
    /// Cap used by `desired_height`. Currently unread because callers allocate
    /// fixed single-row regions for palette / run and an external 1/3-pane
    /// clamp for Claude; retained so the eventual dynamic-sizing migration of
    /// Claude chat (and reword) can use it without re-adding the field.
    #[allow(dead_code)]
    pub(crate) max_height: u16,
    /// Mode to transition to when the view is focused. Callers typically use
    /// `"prompt"` for simple submit-on-Enter behavior and `"normal"` (or
    /// `"reword"`, etc.) for Helix-style modal editing with explicit submit.
    /// Currently stored at construction time but not yet read because
    /// focus-change-driven mode transitions are not wired; see
    /// `jiggly-toasting-cherny.md` Phase D.
    #[allow(dead_code)]
    pub(crate) start_mode: &'static str,
}

/// Identifies which consumer owns an [`InputView`], used by
/// `SubmitPromptInput` to route submission to the correct handler. The
/// concrete [`crate::run::RunId`] / [`crate::host::ClaudeSessionId`] for
/// targets that need them is resolved from pane focus at dispatch time, not
/// stored here, so the [`InputView`] stays constructible before the owning
/// consumer has its slotmap key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum SubmitTarget {
    PaletteFilter,
    PaletteArg,
    HelpSearch,
    Run,
    Reword,
    ClaudeChat,
    FileFinder,
    RenameSymbol,
    WorkspaceSymbolPicker,
}

impl InputView {
    pub(crate) fn create(
        ws: &mut Workspace,
        executor: Executor,
        target: SubmitTarget,
        seed: &str,
        start_mode: &'static str,
        max_height: u16,
    ) -> Self {
        let (buffer_id, shared_buffer) = ws.buffers.new_scratch();
        if !seed.is_empty() {
            let mut guard = shared_buffer.write().expect("buffer poisoned");
            guard.edit(0..0, seed);
        }
        let editor_state = EditorState::new(buffer_id, shared_buffer, executor);
        let editor_id = ws.editors.insert(editor_state);

        if !seed.is_empty() {
            if let Some(editor) = ws.editors.get_mut(editor_id) {
                let snapshot = editor.display_map.snapshot();
                let buf_snap = snapshot.buffer_snapshot();
                let anchor = buf_snap.anchor_at(seed.len(), Bias::Right);
                editor.selections.transform(buf_snap, |s| {
                    let mut new = s.clone();
                    new.collapse_to(anchor, SelectionGoal::None);
                    new
                });
            }
        }

        Self {
            editor_id,
            buffer_id,
            target,
            max_height,
            start_mode,
        }
    }

    /// Remove the underlying editor slot. Scratch buffers stay in the registry
    /// (no public removal API today); this matches the existing reword/Claude
    /// teardown and is safe for transient inputs.
    pub(crate) fn dispose(&self, ws: &mut Workspace) {
        ws.editors.remove(self.editor_id);
    }

    pub(crate) fn text(&self, ws: &Workspace) -> String {
        let Some(buffer) = ws.buffers.get(self.buffer_id) else {
            return String::new();
        };
        let guard = buffer.read().expect("buffer poisoned");
        guard.snapshot.visible_text.to_string()
    }

    /// Insert `text` at the current cursor position. Mirrors the logic in
    /// [`crate::app::Stoat::editor_insert`] but parameterized by workspace +
    /// the view's own editor / buffer ids so consumers that ever want to
    /// drive input mutations outside the focused-editor short-circuit
    /// (`handle_insert_key`) can do so directly. Currently unused because
    /// every live consumer routes typing through the global keymap, but
    /// kept as a building block for future short-circuited flows (eg. a
    /// paste action that bypasses insert mode).
    #[allow(dead_code)]
    pub(crate) fn insert_at_cursor(&self, ws: &mut Workspace, text: &str) {
        let Some(editor) = ws.editors.get_mut(self.editor_id) else {
            return;
        };
        let Some(buffer) = ws.buffers.get(self.buffer_id) else {
            return;
        };
        let display_snapshot = editor.display_map.snapshot();
        let buf_snapshot = display_snapshot.buffer_snapshot();
        let sel = editor.selections.newest_anchor().clone();
        let offset = buf_snapshot.resolve_anchor(&sel.head());
        {
            let mut guard = buffer.write().expect("buffer poisoned");
            guard.edit(offset..offset, text);
        }
        let new_display = editor.display_map.snapshot();
        let new_buf = new_display.buffer_snapshot();
        let anchor = new_buf.anchor_at(offset + text.len(), Bias::Right);
        editor.selections.transform(new_buf, |s| {
            let mut new = s.clone();
            new.collapse_to(anchor, SelectionGoal::None);
            new
        });
    }

    /// Delete the grapheme immediately before the cursor.
    #[allow(dead_code)]
    pub(crate) fn delete_before_cursor(&self, ws: &mut Workspace) {
        let Some(editor) = ws.editors.get_mut(self.editor_id) else {
            return;
        };
        let Some(buffer) = ws.buffers.get(self.buffer_id) else {
            return;
        };
        let display_snapshot = editor.display_map.snapshot();
        let buf_snapshot = display_snapshot.buffer_snapshot();
        let sel = editor.selections.newest_anchor().clone();
        let offset = buf_snapshot.resolve_anchor(&sel.head());
        if offset == 0 {
            return;
        }
        let rope = buf_snapshot.rope();
        let prev_len = rope
            .reversed_chars_at(offset)
            .next()
            .map(|ch| ch.len_utf8())
            .unwrap_or(0);
        if prev_len == 0 {
            return;
        }
        let start = offset - prev_len;
        {
            let mut guard = buffer.write().expect("buffer poisoned");
            guard.edit(start..offset, "");
        }
        let new_display = editor.display_map.snapshot();
        let new_buf = new_display.buffer_snapshot();
        let anchor = new_buf.anchor_at(start, Bias::Right);
        editor.selections.transform(new_buf, |s| {
            let mut new = s.clone();
            new.collapse_to(anchor, SelectionGoal::None);
            new
        });
    }

    /// Move the cursor by `delta` graphemes. Negative values move left,
    /// positive right. Saturates at buffer boundaries.
    #[allow(dead_code)]
    pub(crate) fn move_cursor(&self, ws: &mut Workspace, delta: i32) {
        let Some(editor) = ws.editors.get_mut(self.editor_id) else {
            return;
        };
        let display_snapshot = editor.display_map.snapshot();
        let buf_snapshot = display_snapshot.buffer_snapshot();
        let sel = editor.selections.newest_anchor().clone();
        let current = buf_snapshot.resolve_anchor(&sel.head());
        let rope = buf_snapshot.rope();
        let total = rope.len();
        let new_offset = if delta < 0 {
            let mut offset = current;
            for _ in 0..(-delta) {
                if offset == 0 {
                    break;
                }
                let prev_len = rope
                    .reversed_chars_at(offset)
                    .next()
                    .map(|ch| ch.len_utf8())
                    .unwrap_or(0);
                if prev_len == 0 {
                    break;
                }
                offset -= prev_len;
            }
            offset
        } else {
            let mut offset = current;
            for _ in 0..delta {
                if offset >= total {
                    break;
                }
                let next_len = rope
                    .chars_at(offset)
                    .next()
                    .map(|ch| ch.len_utf8())
                    .unwrap_or(0);
                if next_len == 0 {
                    break;
                }
                offset += next_len;
            }
            offset
        };
        let anchor = buf_snapshot.anchor_at(new_offset, Bias::Right);
        editor.selections.transform(buf_snapshot, |s| {
            let mut new = s.clone();
            new.collapse_to(anchor, SelectionGoal::None);
            new
        });
    }

    /// Move the cursor to the start or end of the rope.
    #[allow(dead_code)]
    pub(crate) fn move_to_boundary(&self, ws: &mut Workspace, end: bool) {
        let Some(editor) = ws.editors.get_mut(self.editor_id) else {
            return;
        };
        let display_snapshot = editor.display_map.snapshot();
        let buf_snapshot = display_snapshot.buffer_snapshot();
        let offset = if end { buf_snapshot.rope().len() } else { 0 };
        let anchor = buf_snapshot.anchor_at(offset, Bias::Right);
        editor.selections.transform(buf_snapshot, |s| {
            let mut new = s.clone();
            new.collapse_to(anchor, SelectionGoal::None);
            new
        });
    }

    /// Cursor column in the rope (0-indexed, counted in bytes). For a
    /// single-line buffer this is the visual cursor column - used by
    /// render paths that draw a custom cursor glyph (help search) rather
    /// than relying on [`Self::render`]'s editor-style cursor.
    pub(crate) fn cursor_column(&self, ws: &mut Workspace) -> usize {
        let Some(editor) = ws.editors.get_mut(self.editor_id) else {
            return 0;
        };
        let display_snapshot = editor.display_map.snapshot();
        let buf_snapshot = display_snapshot.buffer_snapshot();
        let sel = editor.selections.newest_anchor();
        let point = buf_snapshot.point_for_anchor(&sel.head());
        point.column as usize
    }

    /// Replace the entire buffer text and move the cursor to the end. Used by
    /// consumers like the run pane where history navigation swaps the whole
    /// line, or when the consumer needs to clear the input after submission.
    #[allow(dead_code)]
    pub(crate) fn replace_text(&self, ws: &mut Workspace, text: &str) {
        let Some(buffer) = ws.buffers.get(self.buffer_id) else {
            return;
        };
        let old_len = {
            let guard = buffer.read().expect("buffer poisoned");
            guard.snapshot.visible_text.len()
        };
        {
            let mut guard = buffer.write().expect("buffer poisoned");
            guard.edit(0..old_len, text);
        }
        if let Some(editor) = ws.editors.get_mut(self.editor_id) {
            let snapshot = editor.display_map.snapshot();
            let buf_snap = snapshot.buffer_snapshot();
            let anchor = buf_snap.anchor_at(text.len(), Bias::Right);
            editor.selections.transform(buf_snap, |s| {
                let mut new = s.clone();
                new.collapse_to(anchor, SelectionGoal::None);
                new
            });
        }
    }

    /// Desired row count for layout, clamped to `self.max_height`. Adds one
    /// row for the per-view modeline when the mode is non-prompt. Currently
    /// unused - palette and run callers allocate their own fixed-height
    /// single-row regions - but kept on the type because the Claude chat
    /// dynamic-height path (`max_input = area.height / 3` clamp) will want
    /// this once its renderer is migrated off the manual line-count helper.
    #[allow(dead_code)]
    pub(crate) fn desired_height(&self, ws: &Workspace, current_mode: &str) -> u16 {
        let Some(buffer) = ws.buffers.get(self.buffer_id) else {
            return 1;
        };
        let content_rows = {
            let guard = buffer.read().expect("buffer poisoned");
            guard.line_count().max(1) as u16
        };
        let status_row: u16 = if current_mode == "prompt" { 0 } else { 1 };
        (content_rows + status_row).clamp(1, self.max_height)
    }

    /// Draw the editor into `area`. Splits off a single modeline row when the
    /// current mode is not `"prompt"`, so prompt-mode inputs look like a bare
    /// field while modal modes expose the mode indicator. Takes the editor
    /// slotmap directly (not `&mut Workspace`) so callers that already hold
    /// split borrows of workspace fields - like
    /// [`crate::render::pane::render_pane`] via [`crate::render::PaneCtx`] -
    /// can reuse their existing borrow without conflict.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn render(
        &self,
        editors: &mut SlotMap<EditorId, EditorState>,
        area: Rect,
        is_focused: bool,
        current_mode: &str,
        theme: &Theme,
        mode_badges: &std::collections::BTreeMap<String, String>,
        buf: &mut Buffer,
    ) {
        if area.width == 0 || area.height == 0 {
            return;
        }

        let show_status = current_mode != "prompt" && area.height >= 2;
        let (body_area, status_area) = if show_status {
            (
                Rect::new(area.x, area.y, area.width, area.height - 1),
                Some(Rect::new(area.x, area.y + area.height - 1, area.width, 1)),
            )
        } else {
            (area, None)
        };

        let fallback = theme.get(scope::UI_TEXT);
        if let Some(editor) = editors.get_mut(self.editor_id) {
            render_editor(editor, body_area, fallback, theme, buf, is_focused);
        }

        if let Some(status_area) = status_area {
            render_input_status(
                editors,
                self.editor_id,
                status_area,
                current_mode,
                theme,
                mode_badges,
                buf,
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn render_input_status(
    editors: &mut SlotMap<EditorId, EditorState>,
    editor_id: EditorId,
    area: Rect,
    mode: &str,
    theme: &Theme,
    mode_badges: &std::collections::BTreeMap<String, String>,
    buf: &mut Buffer,
) {
    let bar_style = theme.get(scope::UI_STATUSBAR_FOCUSED);
    for col in area.x..area.x + area.width {
        buf[(col, area.y)].set_char(' ').set_style(bar_style);
    }

    let (label, color) = mode_segment(mode, theme, mode_badges);
    let mode_label_style = theme.get(scope::UI_MODE_LABEL).bg(color);
    let label_text = format!(" {label} ");
    write_str(buf, area.x, area.y, &label_text, mode_label_style);

    if let Some(editor) = editors.get_mut(editor_id) {
        if let Some((row, col)) = crate::render::editor::editor_cursor_position(editor) {
            let pos_text = format!(" {row}:{col} ");
            let pos_len = pos_text.chars().count() as u16;
            let right_edge = area.x + area.width;
            if pos_len <= area.width {
                let start = right_edge - pos_len;
                write_str(buf, start, area.y, &pos_text, bar_style);
            }
        }
    }
}
