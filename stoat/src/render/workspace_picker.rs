use crate::{
    render::text::write_str,
    workspace_picker::{PathDisplay, WorkspacePicker, WorkspaceStatus},
};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    widgets::{Clear, Widget},
};
use std::path::Path;

pub(crate) fn render_workspace_picker(
    picker: &WorkspacePicker,
    theme: &crate::theme::Theme,
    area: Rect,
    buf: &mut Buffer,
    scene: Option<&mut stoatty_widgets::ApcScene>,
) {
    if area.width < 60 || area.height < 8 {
        return;
    }

    let entries = picker.entries();
    if entries.is_empty() {
        return;
    }
    let max_entries = 10u16;
    let entry_rows = (entries.len() as u16).min(max_entries);

    let box_width = 90u16.min(area.width.saturating_sub(4));
    if box_width < 60 {
        return;
    }
    let box_height = 3 + entry_rows;
    if box_height > area.height {
        return;
    }

    let x = area.x + (area.width.saturating_sub(box_width)) / 2;
    let y = area.y + (area.height.saturating_sub(box_height)) / 2;
    let picker_area = Rect::new(x, y, box_width, box_height);

    let modal_style = theme.get(crate::theme::scope::UI_MODAL_PICKER);
    Clear.render(picker_area, buf);
    let inner = crate::render::chrome::modal_frame(
        buf,
        picker_area,
        Some(" workspaces "),
        modal_style,
        theme,
        scene,
    );

    const NAME_W: u16 = 12;
    const BUF_W: u16 = 5;
    const RUN_W: u16 = 5;
    const EDIT_W: u16 = 6;

    let path_display = picker.path_display();
    let show_path = !matches!(path_display, PathDisplay::Omit);

    let edit_col_x = inner.x + inner.width.saturating_sub(1 + EDIT_W);
    let run_col_x = edit_col_x.saturating_sub(RUN_W);
    let buf_col_x = run_col_x.saturating_sub(BUF_W);
    let marker_x = inner.x + 1;
    let name_x = marker_x + 2;
    let path_x = name_x + NAME_W + 2;
    let path_w = buf_col_x.saturating_sub(2).saturating_sub(path_x);

    let right_pad = |label: &str, width: u16| format!("{:>w$}", label, w = width as usize);

    let row_style = theme.get(crate::theme::scope::UI_TEXT);
    let current_style = theme.get(crate::theme::scope::UI_PROMPT);
    let selected_style = theme.get(crate::theme::scope::UI_SELECTION);
    let header_style = theme.get(crate::theme::scope::UI_TEXT_MUTED);

    let header_row = inner.y;
    write_str(buf, name_x, header_row, "name", header_style);
    if show_path {
        write_str(buf, path_x, header_row, "path", header_style);
    }
    write_str(
        buf,
        buf_col_x,
        header_row,
        &right_pad("buf", BUF_W),
        header_style,
    );
    write_str(
        buf,
        run_col_x,
        header_row,
        &right_pad("run", RUN_W),
        header_style,
    );
    write_str(
        buf,
        edit_col_x,
        header_row,
        &right_pad("edit", EDIT_W),
        header_style,
    );

    let entries_top = inner.y + 1;
    let selected = picker.selected();

    for (i, entry) in entries.iter().take(max_entries as usize).enumerate() {
        let row = entries_top + i as u16;
        let is_selected = i == selected;
        let base_style = if is_selected {
            selected_style
        } else {
            match entry.status {
                WorkspaceStatus::Active => current_style,
                WorkspaceStatus::Background => row_style,
                WorkspaceStatus::Inactive => header_style,
            }
        };

        for col in inner.x..inner.x + inner.width {
            buf[(col, row)].set_char(' ').set_style(base_style);
        }

        let marker = match entry.status {
            WorkspaceStatus::Active => "*",
            WorkspaceStatus::Background => " ",
            WorkspaceStatus::Inactive => "\u{00b7}",
        };
        write_str(buf, marker_x, row, marker, base_style);
        let name: String = entry.basename.chars().take(NAME_W as usize).collect();
        write_str(buf, name_x, row, &name, base_style);
        if show_path {
            let context: &Path = match &path_display {
                PathDisplay::Omit => unreachable!("show_path guards against Omit"),
                PathDisplay::Relative(ancestor) => ancestor.as_path(),
                PathDisplay::TildeAbsolute => Path::new(""),
            };
            let path = crate::paths::display_relative(&entry.git_root, context);
            let path_trimmed: String = path.chars().take(path_w as usize).collect();
            write_str(buf, path_x, row, &path_trimmed, base_style);
        }
        // An inactive row has no live runs or editors, so those counts blank
        // rather than reading a misleading zero.
        let inactive = entry.status == WorkspaceStatus::Inactive;
        let count = |n: usize, blank: bool| if blank { String::new() } else { n.to_string() };
        write_str(
            buf,
            buf_col_x,
            row,
            &right_pad(&count(entry.buffer_count, false), BUF_W),
            base_style,
        );
        write_str(
            buf,
            run_col_x,
            row,
            &right_pad(&count(entry.run_count, inactive), RUN_W),
            base_style,
        );
        write_str(
            buf,
            edit_col_x,
            row,
            &right_pad(&count(entry.editor_count, inactive), EDIT_W),
            base_style,
        );
    }
}
