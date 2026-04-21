use crate::{
    host::RebaseTodoOp,
    pane::Pane,
    rebase::RebaseState,
    render::{
        layout::split_pane_status,
        pane::render_overlay_status,
        text::{truncate_to_cols, write_str},
        FrameCtx,
    },
};
use ratatui::buffer::Buffer;

pub(crate) fn render_rebase(
    pane: &Pane,
    is_focused: bool,
    state: &RebaseState,
    frame: FrameCtx<'_>,
    buf: &mut Buffer,
) {
    let theme = frame.theme;
    let (inner, status_area) = split_pane_status(pane.area);
    render_overlay_status(status_area, is_focused, frame, "rebase", buf);

    if inner.width < 10 || inner.height == 0 {
        return;
    }

    use crate::theme::scope as s;
    let sel_style = theme.get(crate::theme::scope::UI_SELECTION_REVERSED);
    let pick_style = theme.get(s::VCS_REBASE_PICK);
    let squash_style = theme.get(s::VCS_REBASE_SQUASH);
    let fixup_style = theme.get(s::VCS_REBASE_FIXUP);
    let reword_style = theme.get(s::VCS_REBASE_REWORD);
    let edit_style = theme.get(s::VCS_REBASE_EDIT);
    let drop_style = theme.get(s::VCS_REBASE_DROP);
    let summary_style = theme.get(s::UI_TEXT);
    let sha_style = theme.get(s::UI_KEY_LABEL);

    let help_rows: u16 = 2;
    let list_height = inner.height.saturating_sub(help_rows);

    for (i, entry) in state.todo.iter().take(list_height as usize).enumerate() {
        let y = inner.y + i as u16;
        let is_selected = i == state.selected;
        if is_selected {
            for x in inner.x..inner.x + inner.width {
                buf[(x, y)].set_style(sel_style);
            }
        }
        let (label, op_style) = match entry.op {
            RebaseTodoOp::Pick => ("pick  ", pick_style),
            RebaseTodoOp::Squash => ("squash", squash_style),
            RebaseTodoOp::Fixup => ("fixup ", fixup_style),
            RebaseTodoOp::Drop => ("drop  ", drop_style),
            RebaseTodoOp::Reword => ("reword", reword_style),
            RebaseTodoOp::Edit => ("edit  ", edit_style),
        };
        let row_style = if is_selected {
            sel_style
        } else {
            summary_style
        };
        write_str(
            buf,
            inner.x,
            y,
            label,
            if is_selected { sel_style } else { op_style },
        );
        let sha_x = inner.x + label.len() as u16 + 1;
        write_str(
            buf,
            sha_x,
            y,
            &entry.commit.short_sha,
            if is_selected { sel_style } else { sha_style },
        );
        let summary_x = sha_x + entry.commit.short_sha.len() as u16 + 1;
        let remaining = (inner.x + inner.width).saturating_sub(summary_x);
        if remaining > 0 {
            let summary = truncate_to_cols(&entry.commit.summary, remaining as usize);
            write_str(buf, summary_x, y, &summary, row_style);
        }
    }

    if list_height < inner.height {
        let help_y = inner.y + inner.height - help_rows;
        let help1 = "j/k move  K/J reorder  p/s/f/d set op  Enter run  q abort";
        let help2 = format!(
            "{} entries, onto {}",
            state.todo.len(),
            if state.onto.is_empty() {
                "<root>".to_string()
            } else {
                state.onto.chars().take(7).collect::<String>()
            }
        );
        let help_style = theme.get(crate::theme::scope::UI_TEXT_MUTED);
        write_str(
            buf,
            inner.x,
            help_y,
            &truncate_to_cols(help1, inner.width as usize),
            help_style,
        );
        write_str(
            buf,
            inner.x,
            help_y + 1,
            &truncate_to_cols(&help2, inner.width as usize),
            help_style,
        );
    }
}
