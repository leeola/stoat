use crate::{
    buffer::BufferId,
    editor_state::{EditorId, EditorState},
    render::{
        editor::{render_editor, ResolvedChrome},
        pane::mode_segment,
        text::write_str,
    },
    theme::{scope, Theme},
    workspace::Workspace,
};
use ratatui::{buffer::Buffer, layout::Rect};
use slotmap::SlotMap;
use stoat_scheduler::Executor;
use stoat_text::{Bias, SelectionGoal};

/// Embeddable multi-line text editor backed by the standard [`EditorState`].
/// Used for every compact text-input surface in the TUI: command palette,
/// help search, run pane, reword. A one-row field is still a real editor.
/// When the global mode is `"prompt"`, the view hides its modeline and
/// `Enter` is routed to submission; in other modes the modeline is shown and
/// submission requires an explicit action.
#[derive(Debug, Clone)]
pub(crate) struct InputView {
    pub(crate) editor_id: EditorId,
    pub(crate) buffer_id: BufferId,
    pub(crate) target: SubmitTarget,
}

impl InputView {
    /// An input holding no real workspace slots, for a unit test that builds a
    /// picker but never renders or types into it.
    #[cfg(test)]
    pub(crate) fn test_dummy() -> Self {
        Self {
            editor_id: EditorId::default(),
            buffer_id: BufferId::new(0),
            target: SubmitTarget::PaletteFilter,
        }
    }
}

/// Identifies which consumer owns an [`InputView`], used by
/// `SubmitPromptInput` to route submission to the correct handler. The
/// concrete [`crate::run::RunId`] for targets that need it is resolved from
/// pane focus at dispatch time, not stored here, so the [`InputView`] stays
/// constructible before the owning consumer has its slotmap key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SubmitTarget {
    PaletteFilter,
    HelpSearch,
    Run,
    Reword,
    CommitMessage,
    FileFinder,
    SymbolFinder,
    CommitPicker,
    WorkspacePicker,
    LocationPicker,
    JumplistPicker,
    WalkthroughPicker,
    PaneFinder,
    DiagnosticsPicker,
    RenameSymbol,
    Search,
    CodeSearch,
    SplitSelection,
    KeepRemoveSelections,
    Shell,
}

impl InputView {
    pub(crate) fn create(
        ws: &mut Workspace,
        executor: Executor,
        target: SubmitTarget,
        seed: &str,
        start_mode: &'static str,
    ) -> Self {
        let (buffer_id, shared_buffer) = ws.buffers.new_scratch();
        if !seed.is_empty() {
            let mut guard = shared_buffer.write().expect("buffer poisoned");
            guard.edit(0..0, seed);
        }
        let mut editor_state =
            EditorState::new(buffer_id, shared_buffer, executor, ws.redraw_notify.clone());
        editor_state.mode = start_mode.to_string();
        let editor_id = ws.editors.insert(editor_state);

        if !seed.is_empty()
            && let Some(editor) = ws.editors.get_mut(editor_id)
        {
            let snapshot = editor.display_map.snapshot();
            let buf_snap = snapshot.buffer_snapshot();
            let anchor = buf_snap.anchor_at(seed.len(), Bias::Right);
            editor.selections.transform(buf_snap, |s| {
                let mut new = s.clone();
                new.collapse_to(anchor, SelectionGoal::None);
                new
            });
        }

        Self {
            editor_id,
            buffer_id,
            target,
        }
    }

    /// Remove the editor slot and the owned scratch buffer.
    ///
    /// The scratch buffer is path-less and gets dirtied by a non-empty seed or
    /// by typing, so leaving it in the registry would make it count as an
    /// unsaved buffer and surface in the quit-all confirmation. Both slots are
    /// dropped so a closed transient input leaves no trace.
    pub(crate) fn dispose(&self, ws: &mut Workspace) {
        ws.editors.remove(self.editor_id);
        ws.buffers.remove(self.buffer_id);
    }

    pub(crate) fn text(&self, ws: &Workspace) -> String {
        let Some(buffer) = ws.buffers.get(self.buffer_id) else {
            return String::new();
        };
        let guard = buffer.read().expect("buffer poisoned");
        guard.snapshot.visible_text.to_string()
    }

    /// Replace the entire buffer text and move the cursor to the end. Used by
    /// consumers like the run pane where history navigation swaps the whole
    /// line, or when the consumer needs to clear the input after submission.
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
        chrome: &ResolvedChrome,
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
            render_editor(
                editor,
                body_area,
                fallback,
                theme,
                Some(chrome),
                buf,
                is_focused,
            );
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

    if let Some(editor) = editors.get_mut(editor_id)
        && let Some((row, col)) = crate::render::editor::editor_cursor_position(editor)
    {
        let pos_text = format!(" {row}:{col} ");
        let pos_len = pos_text.chars().count() as u16;
        let right_edge = area.x + area.width;
        if pos_len <= area.width {
            let start = right_edge - pos_len;
            write_str(buf, start, area.y, &pos_text, bar_style);
        }
    }
}
