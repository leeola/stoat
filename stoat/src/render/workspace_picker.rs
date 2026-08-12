use crate::{
    render::text::write_str,
    workspace::Workspace,
    workspace_picker::{PathDisplay, WorkspacePicker, WorkspaceStatus},
};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    widgets::{Block, Borders},
};
use std::path::Path;

/// Rows of workspaces the modal shows at once. A longer list scrolls under the
/// selection rather than growing the box past a readable height.
const MAX_ENTRY_ROWS: u16 = 10;

/// Rows the chrome takes inside the border above the list, which are the
/// filter input, its separator, and the column header.
const HEADER_ROWS: u16 = 3;

/// Lay the workspace picker's modal out within `area`, returning its outer box
/// and the inner rect holding the header and rows, or [`None`] when `area` is
/// too small to host it or there is nothing to list.
fn workspace_picker_layout(area: Rect, entries_len: usize) -> Option<(Rect, Rect)> {
    if entries_len == 0 {
        return None;
    }
    let entry_rows = (entries_len as u16).min(MAX_ENTRY_ROWS);
    let content_height = 2 + HEADER_ROWS + entry_rows;
    let modal = crate::render::chrome::modal_box(area, (0, content_height), (90, 6), (60, 6), 0)?;
    Some((modal, Block::default().borders(Borders::ALL).inner(modal)))
}

pub(crate) fn render_workspace_picker(
    picker: &mut WorkspacePicker,
    ws: &mut Workspace,
    theme: &crate::theme::Theme,
    chrome: &crate::render::editor::ResolvedChrome,
    area: Rect,
    buf: &mut Buffer,
    scene: &mut stoatty_widgets::ApcScene,
) {
    let Some((picker_area, inner)) = workspace_picker_layout(area, picker.entries().len()) else {
        return;
    };

    let modal_style = theme.get(crate::theme::scope::UI_MODAL_PICKER);
    crate::render::clear_themed(picker_area, buf, theme);
    crate::render::chrome::modal_frame(
        buf,
        picker_area,
        Some(" workspaces "),
        modal_style,
        theme,
        &mut *scene,
    );

    crate::render::picker::filter_header(buf, inner, ">", &picker.input, ws, theme, chrome, scene);

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
    let match_style = theme.get(crate::theme::scope::UI_SEARCH_MATCH);

    let header_row = inner.y + 2;
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

    let entries_top = inner.y + HEADER_ROWS;
    let rows = inner.height.saturating_sub(HEADER_ROWS) as usize;
    picker.viewport_rows = Some(rows);
    let selected = picker.selected();
    let match_indices = picker.match_indices();
    let entries = picker.entries();

    let start = crate::render::picker::window_start(selected, rows);
    for (i, &idx) in picker.filtered().iter().enumerate().skip(start).take(rows) {
        let entry = &entries[idx];
        let indices = &match_indices[i];
        let row = entries_top + (i - start) as u16;
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
        for (j, _) in name.chars().enumerate() {
            if indices.binary_search(&(j as u32)).is_ok() {
                buf[(name_x + j as u16, row)].set_style(match_style);
            }
        }

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

#[cfg(test)]
mod tests {
    use super::{render_workspace_picker, workspace_picker_layout, HEADER_ROWS, MAX_ENTRY_ROWS};
    use crate::{
        buffer::BufferId,
        editor_state::EditorId,
        input_view::{InputView, SubmitTarget},
        render::picker::test_support::{row_text, selected_rows, selection_theme},
        workspace::{Workspace, WorkspaceId},
        workspace_picker::WorkspacePicker,
    };
    use ratatui::{buffer::Buffer, layout::Rect};
    use slotmap::SlotMap;
    use std::{path::PathBuf, sync::Arc};
    use stoat_scheduler::{Executor, TestScheduler};

    /// A picker over `count` workspaces named `ws00`..., the first of them
    /// active, alongside the slotmap the renderer paints its input through.
    fn picker_over(
        count: usize,
    ) -> (
        WorkspacePicker,
        SlotMap<WorkspaceId, Workspace>,
        WorkspaceId,
    ) {
        let executor: Executor = Arc::new(TestScheduler::new()).executor();
        let mut workspaces: SlotMap<WorkspaceId, Workspace> = SlotMap::with_key();
        let mut active = WorkspaceId::default();
        for i in 0..count {
            let id = workspaces.insert(Workspace::new(
                PathBuf::from(format!("/tmp/ws{i:02}")),
                &executor,
                crate::test_notify(),
            ));
            workspaces[id].id = id;
            workspaces[id].name = format!("ws{i:02}");
            if i == 0 {
                active = id;
            }
        }
        // The picker renders through the active workspace's editors, and a null
        // editor key resolves to nothing, so the input row paints blank.
        let input = InputView {
            editor_id: EditorId::default(),
            buffer_id: BufferId::new(0),
            target: SubmitTarget::WorkspacePicker,
            max_height: 1,
        };
        let picker = WorkspacePicker::new(&workspaces, active, Vec::new(), input);
        (picker, workspaces, active)
    }

    fn render(
        picker: &mut WorkspacePicker,
        workspaces: &mut SlotMap<WorkspaceId, Workspace>,
        active: WorkspaceId,
        buf: &mut Buffer,
        area: Rect,
    ) {
        let theme = selection_theme();
        render_workspace_picker(
            picker,
            &mut workspaces[active],
            &theme,
            &crate::render::editor::ResolvedChrome::resolve(&theme),
            area,
            buf,
            &mut stoatty_widgets::ApcScene::new(),
        );
    }

    #[test]
    fn layout_holds_the_header_above_one_row_per_workspace() {
        let (modal, inner) =
            workspace_picker_layout(Rect::new(0, 0, 120, 40), 3).expect("the area hosts the modal");
        assert_eq!(modal.width, 90, "the box holds at its recommended width");
        assert_eq!(
            inner.height,
            HEADER_ROWS + 3,
            "the filter, separator, and column labels sit above the rows"
        );
    }

    #[test]
    fn layout_caps_the_rows_it_shows() {
        let (_, inner) = workspace_picker_layout(Rect::new(0, 0, 120, 40), 30)
            .expect("the area hosts the modal");
        assert_eq!(inner.height, HEADER_ROWS + MAX_ENTRY_ROWS);
    }

    #[test]
    fn layout_none_when_too_small_or_empty() {
        assert_eq!(workspace_picker_layout(Rect::new(0, 0, 60, 40), 3), None);
        assert_eq!(workspace_picker_layout(Rect::new(0, 0, 120, 7), 3), None);
        assert_eq!(workspace_picker_layout(Rect::new(0, 0, 120, 40), 0), None);
    }

    #[test]
    fn paging_moves_by_half_the_rendered_rows_and_stops_at_the_ends() {
        let (mut picker, mut workspaces, active) = picker_over(15);
        let area = Rect::new(0, 0, 100, 30);
        render(
            &mut picker,
            &mut workspaces,
            active,
            &mut Buffer::empty(area),
            area,
        );

        let half = picker.viewport_rows.expect("the render stamped a viewport") / 2;
        assert!(
            half > 1,
            "a meaningful page needs more than one row: {half}"
        );

        picker.page(1);
        assert_eq!(picker.selected(), half, "a page down covers half a screen");
        picker.page(-1);
        assert_eq!(picker.selected(), 0, "and a page up returns");

        for _ in 0..15 {
            picker.page(1);
        }
        assert_eq!(picker.selected(), 14, "paging past the end stops on it");
        for _ in 0..15 {
            picker.page(-1);
        }
        assert_eq!(picker.selected(), 0, "and past the start stops there");
    }

    #[test]
    fn the_last_of_more_workspaces_than_fit_paints_as_selected() {
        let (mut picker, mut workspaces, active) = picker_over(15);
        while picker.selected() + 1 < picker.entries().len() {
            picker.select_next();
        }
        assert_eq!(picker.selected(), 14, "the last entry is selected");
        let last = picker.entries()[picker.filtered()[14]].basename.clone();

        let area = Rect::new(0, 0, 100, 30);
        let mut buf = Buffer::empty(area);
        render(&mut picker, &mut workspaces, active, &mut buf, area);

        let rows = selected_rows(&buf);
        assert_eq!(rows.len(), 1, "the selection is on screen exactly once");
        let text = row_text(&buf, rows[0]);
        assert!(
            text.contains(&last),
            "and it is the selected entry ({last}) that paints there: {text:?}"
        );
    }
}
