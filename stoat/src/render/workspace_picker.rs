use crate::{
    render::{
        paint::style_rgb,
        picker::{FilterHeader, Footer, FILTER_PROMPT},
        text::{write_str, write_str_clipped},
    },
    workspace::Workspace,
    workspace_picker::{WorkspacePicker, WorkspaceStatus},
};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Style},
    widgets::{Block, Borders, StatefulWidget},
};
use std::path::Path;
use stoat_widgets::bar::Bar;
use unicode_width::UnicodeWidthStr;

/// Rows of workspaces the modal shows at once. A longer list scrolls under the
/// selection rather than growing the box past a readable height.
const MAX_ENTRY_ROWS: u16 = 10;

/// Rows the chrome takes inside the border above the list, which are the
/// filter input, its separator, and the column header.
const HEADER_ROWS: u16 = 3;

/// Rows the chrome takes inside the border below the list, which is the footer
/// of the status legend and the bound keys.
const FOOTER_ROWS: u16 = 1;

/// Columns the box asks for, which hold the footer's legend and all of its key
/// hints in whole cells.
const RECOMMENDED_COLUMNS: u16 = 112;

/// The footer's hints, each a bound key and the word for what it does.
const SESSION_HINTS: [(&str, &str); 6] = [
    ("Enter", "switch"),
    ("A-Enter", "sub-session"),
    ("C-r", "rename"),
    ("C-x", "close"),
    ("Del", "delete"),
    ("S-Tab", "scope"),
];

/// A status dot's geometry in sixteenths of the marker cell. The cell is twice
/// as tall as it is wide, so the dot is twice as wide as tall to read square.
const DOT_X: i16 = 5;
const DOT_Y: i16 = 6;
const DOT_WIDTH: u16 = 6;
const DOT_HEIGHT: u16 = 3;

/// Lay the workspace picker's modal out within `area`, returning its outer box
/// and the inner rect holding the header, the rows, and the footer, or
/// [`None`] when `area` is too small to host it or there is nothing to list.
pub(crate) fn workspace_picker_layout(area: Rect, entries_len: usize) -> Option<(Rect, Rect)> {
    if entries_len == 0 {
        return None;
    }
    let entry_rows = (entries_len as u16).min(MAX_ENTRY_ROWS);
    let content_height = 2 + HEADER_ROWS + entry_rows + FOOTER_ROWS;
    let modal = crate::render::chrome::modal_box(
        area,
        (0, content_height),
        (RECOMMENDED_COLUMNS, 6),
        (60, 6),
        0,
    )?;
    Some((modal, Block::default().borders(Borders::ALL).inner(modal)))
}

/// The rows the list paints in, inside the modal's `inner` rect, below the
/// filter input, its rule, and the column header, and above the footer.
///
/// The paint and the pointer's hit test both read it, so a press lands on the
/// row drawn under it.
pub(crate) fn workspace_picker_list(inner: Rect) -> Rect {
    Rect::new(
        inner.x,
        inner.y + HEADER_ROWS,
        inner.width,
        inner.height.saturating_sub(HEADER_ROWS + FOOTER_ROWS),
    )
}

pub(crate) fn render_workspace_picker(
    picker: &mut WorkspacePicker,
    ws: &mut Workspace,
    theme: &crate::theme::Theme,
    chrome: &crate::render::editor::ResolvedChrome,
    area: Rect,
    buf: &mut Buffer,
    scene: &mut stoat_widgets::ApcScene,
) {
    let Some((picker_area, inner)) = workspace_picker_layout(area, picker.entries().len()) else {
        return;
    };

    let modal_style = theme.get(crate::theme::scope::UI_MODAL_PICKER);
    crate::render::clear_modal(picker_area, buf, theme, modal_style);
    crate::render::chrome::modal_frame(buf, picker_area, None, modal_style, theme, &mut *scene);

    let label = match (picker.renaming(), picker.family_title()) {
        (true, _) => "rename".to_owned(),
        (false, None) => "sessions".to_owned(),
        (false, Some(family)) => format!("sessions \u{00B7} {family}"),
    };
    let family = picker.family_title().is_some();
    crate::render::picker::filter_header(
        buf,
        inner,
        &FilterHeader {
            label: &label,
            prompt: FILTER_PROMPT,
            count: Some((picker.filtered().len(), picker.entries().len())),
            chips: &[("all", !family), ("family", family)],
        },
        &picker.input,
        ws,
        theme,
        chrome,
        scene,
    );

    const NAME_W: u16 = 24;
    const BUF_W: u16 = 5;
    const RUN_W: u16 = 5;
    const EDIT_W: u16 = 6;

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
    let selected_style = theme.list_selected(crate::theme::scope::UI_SELECTION);
    let header_style = theme.get(crate::theme::scope::UI_TEXT_MUTED);
    let match_style = theme.list_match();
    let key_style = theme.get(crate::theme::scope::UI_KEY_LABEL);
    let parent_style = theme.get(crate::theme::scope::UI_TEXT_MUTED);
    let disabled_style = theme.get(crate::theme::scope::UI_TEXT_DISABLED);
    let run_style = theme.get(crate::theme::scope::UI_STATUSLINE_RUN);
    let edit_style = theme.get(crate::theme::scope::UI_TEXT_DIM);

    let query = picker.input.text(ws);
    let query = query.trim();
    let creating = picker.filtered().is_empty() && !query.is_empty() && !picker.renaming();

    let header_row = inner.y + 2;
    if creating {
        // Alt-Enter makes a root session too when the create leaves the fresh
        // launch workspace, as `workspace_picker_create_sub` decides.
        let sub_session = picker.omitted_workspace != Some(ws.id) || !ws.is_fresh();
        let family = format!(" new sub-session of {}", picker.family_name());
        let mut hint = vec![("Enter", key_style), (" new session", header_style)];
        if sub_session {
            hint.extend([
                ("   ", header_style),
                ("Alt-Enter", key_style),
                (family.as_str(), header_style),
            ]);
        }
        let end_x = inner.x + inner.width;
        let mut x = name_x;
        for (text, style) in hint {
            write_str_clipped(buf, x, header_row, text, style, end_x);
            x = x.saturating_add(text.width() as u16);
        }
    } else {
        write_str(buf, name_x, header_row, "name", header_style);
        write_str(buf, path_x, header_row, "path", header_style);
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
    }

    let list = workspace_picker_list(inner);
    let entries_top = list.y;
    let rows = list.height as usize;
    picker.viewport_rows = Some(rows);
    let selected = picker.selected();
    let entries = picker.entries();

    // Reused across the painted rows rather than allocated per row, since a row
    // past the indexed block derives its offsets here.
    let mut derived = Vec::new();
    let mut matching = crate::fuzzy::Scratch::default();

    let start = crate::render::picker::window_start(selected, rows);
    for (i, &idx) in picker.filtered().iter().enumerate().skip(start).take(rows) {
        let entry = &entries[idx];
        let indices = picker.row_indices(i, &mut derived, &mut matching);
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
        if is_selected {
            crate::render::chrome::selection_bar(inner.x, row, theme, buf, scene);
        }

        paint_marker(entry.status, marker_x, row, base_style, theme, buf, scene);

        let label = picker.display_label(idx);
        // The label ends with the row's own name, where the haystack's match
        // offsets start, so the offsets shift past any parent prefix.
        let prefix = label.chars().count() - entry.basename.chars().count();
        let name: String = label.chars().take(NAME_W as usize).collect();
        write_str(buf, name_x, row, &name, base_style);
        for j in 0..prefix.min(name.chars().count()) {
            let style = if j + 1 == prefix {
                disabled_style
            } else {
                parent_style
            };
            buf[(name_x + j as u16, row)].set_style(style);
        }
        for j in prefix..name.chars().count() {
            if indices.binary_search(&((j - prefix) as u32)).is_ok() {
                buf[(name_x + j as u16, row)].set_style(match_style);
            }
        }

        let path = crate::paths::display_relative(&entry.git_root, Path::new(""));
        let host = entry
            .remote_host
            .as_deref()
            .map(|h| format!("{h}:"))
            .unwrap_or_default();
        paint_root(
            buf,
            path_x,
            row,
            path_w,
            (&host, base_style.patch(run_style)),
            &path,
            base_style,
        );

        // An inactive row has no live runs or editors, so those counts blank
        // rather than reading a misleading zero.
        let inactive = entry.status == WorkspaceStatus::Inactive;
        let count = |n: usize, blank: bool| if blank { String::new() } else { n.to_string() };
        let runs_style = match entry.run_count {
            0 => disabled_style,
            _ => run_style,
        };
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
            base_style.patch(runs_style),
        );
        write_str(
            buf,
            edit_col_x,
            row,
            &right_pad(&count(entry.editor_count, inactive), EDIT_W),
            base_style.patch(edit_style),
        );
    }

    if creating {
        for col in inner.x..inner.x + inner.width {
            buf[(col, entries_top)]
                .set_char(' ')
                .set_style(selected_style);
        }
        crate::render::chrome::selection_bar(inner.x, entries_top, theme, buf, scene);
        write_str(buf, marker_x, entries_top, "+", selected_style);
        write_str_clipped(
            buf,
            name_x,
            entries_top,
            query,
            selected_style,
            name_x + NAME_W,
        );

        let root = crate::paths::display_relative(&ws.git_root, Path::new(""));
        paint_root(
            buf,
            path_x,
            entries_top,
            path_w,
            ("", selected_style),
            &root,
            selected_style,
        );
    }

    let footer = Rect::new(
        inner.x,
        inner.y + inner.height.saturating_sub(FOOTER_ROWS),
        inner.width,
        FOOTER_ROWS,
    );
    let legend = [
        ("\u{2022}", status_style(WorkspaceStatus::Active, theme)),
        (" active  ", header_style),
        ("\u{2022}", status_style(WorkspaceStatus::Background, theme)),
        (" open  ", header_style),
        ("\u{2022}", status_style(WorkspaceStatus::Inactive, theme)),
        (" saved", header_style),
    ];
    let parts = Footer {
        lead: &legend,
        hints: &SESSION_HINTS,
        ..Footer::default()
    };
    crate::render::picker::modal_footer(buf, footer, &parts, theme, scene);
}

/// The color a row's status dot takes, as a style whose foreground is the dot.
///
/// The active session reads in `ui.toggle.active`, a session open in the
/// background in `ui.text.dim`, and a saved one in the overlay's strong line.
fn status_style(status: WorkspaceStatus, theme: &crate::theme::Theme) -> Style {
    match status {
        WorkspaceStatus::Active => theme.get(crate::theme::scope::UI_TOGGLE_ACTIVE),
        WorkspaceStatus::Background => theme.get(crate::theme::scope::UI_TEXT_DIM),
        WorkspaceStatus::Inactive => crate::render::picker::line_strong(theme)
            .map_or_else(Style::default, |[r, g, b]| {
                Style::default().fg(Color::Rgb(r, g, b))
            }),
    }
}

/// Paint the mark of `status` into the marker cell at `(x, row)`.
///
/// Under a live scene the mark is a square dot in [`status_style`]'s color.
/// Otherwise, or when the theme gives the status no RGB color, it is a glyph
/// in `glyph_style`, `*` for the active session and U+00B7 for a saved one.
fn paint_marker(
    status: WorkspaceStatus,
    x: u16,
    row: u16,
    glyph_style: Style,
    theme: &crate::theme::Theme,
    buf: &mut Buffer,
    scene: &mut stoat_widgets::ApcScene,
) {
    let dot = style_rgb(status_style(status, theme).fg).filter(|_| scene.live());
    if let Some(color) = dot {
        Bar {
            x: DOT_X,
            y: DOT_Y,
            width: DOT_WIDTH,
            height: DOT_HEIGHT,
            color,
        }
        .render(Rect::new(x, row, 1, 1), buf, scene);
        return;
    }
    let glyph = match status {
        WorkspaceStatus::Active => "*",
        WorkspaceStatus::Background => " ",
        WorkspaceStatus::Inactive => "\u{00b7}",
    };
    write_str(buf, x, row, glyph, glyph_style);
}

/// Paints `host` in its style and then `path` into the `width` cells of the
/// path column from `x`.
///
/// A path too wide for what `host` leaves gives up its head to an ellipsis,
/// per [`tail_within`].
fn paint_root(
    buf: &mut Buffer,
    x: u16,
    row: u16,
    width: u16,
    (host, host_style): (&str, Style),
    path: &str,
    style: Style,
) {
    let end = x + width;
    write_str_clipped(buf, x, row, host, host_style, end);

    let host_w = host.chars().count();
    let path_x = x + host_w as u16;
    match tail_within(path, (width as usize).saturating_sub(host_w)) {
        Some(tail) => {
            write_str_clipped(buf, path_x, row, "\u{2026}", style, end);
            write_str_clipped(buf, path_x + 1, row, tail, style, end);
        },
        None => write_str_clipped(buf, path_x, row, path, style, end),
    }
}

/// Returns the tail of `path` to paint after an ellipsis when `path` overflows
/// `room` characters.
///
/// Returns [`None`] when `path` fits, or when `room` has no character left
/// after the ellipsis. The tail stays, rather than the head, because the last
/// segments of a root name the project.
fn tail_within(path: &str, room: usize) -> Option<&str> {
    let len = path.chars().count();
    if len <= room || room <= 1 {
        return None;
    }

    let dropped = len - (room - 1);
    let start = path
        .char_indices()
        .nth(dropped)
        .map_or(path.len(), |(byte, _)| byte);
    Some(&path[start..])
}

#[cfg(test)]
mod tests {
    use super::{
        render_workspace_picker, workspace_picker_layout, DOT_HEIGHT, DOT_WIDTH, DOT_X, DOT_Y,
        FOOTER_ROWS, HEADER_ROWS, MAX_ENTRY_ROWS,
    };
    use crate::{
        buffer::BufferId,
        editor_state::EditorId,
        input_view::{InputView, SubmitTarget},
        render::picker::test_support::{
            match_theme, row_text, selected_rows, selection_theme, MATCH_FG,
        },
        run::RunState,
        ssh::{RemoteTarget, Transport},
        theme::Theme,
        workspace::{
            registry::{RegistryEntry, WorkspaceMeta},
            Workspace, WorkspaceId, WorkspaceUid,
        },
        workspace_picker::WorkspacePicker,
    };
    use ratatui::{buffer::Buffer, layout::Rect, style::Color};
    use slotmap::SlotMap;
    use std::{path::PathBuf, sync::Arc, time::UNIX_EPOCH};
    use stoat_scheduler::{Executor, TestScheduler};
    use stoat_widgets::ApcScene;
    use stoatty_protocol::command::{self, BarCommand, Command};

    /// A picker over `count` workspaces named `ws00`..., the first of them
    /// active, alongside the slotmap the renderer paints its input through.
    fn picker_over(
        count: usize,
    ) -> (
        WorkspacePicker,
        SlotMap<WorkspaceId, Workspace>,
        WorkspaceId,
    ) {
        let (workspaces, active) =
            workspaces_over(count, |i| PathBuf::from(format!("/tmp/ws{i:02}")));
        let picker = picker_for(&workspaces, active);
        (picker, workspaces, active)
    }

    /// `count` workspaces named `ws00`..., the first of them active, each rooted
    /// at `root(i)` so a caller decides whether the roots differ.
    fn workspaces_over(
        count: usize,
        root: impl Fn(usize) -> PathBuf,
    ) -> (SlotMap<WorkspaceId, Workspace>, WorkspaceId) {
        let executor: Executor = Arc::new(TestScheduler::new()).executor();
        let mut workspaces: SlotMap<WorkspaceId, Workspace> = SlotMap::with_key();
        let mut active = WorkspaceId::default();
        for i in 0..count {
            let id = workspaces.insert(Workspace::new(root(i), &executor, crate::test_notify()));
            workspaces[id].id = id;
            workspaces[id].name = format!("ws{i:02}");
            if i == 0 {
                active = id;
            }
        }
        (workspaces, active)
    }

    fn picker_for(
        workspaces: &SlotMap<WorkspaceId, Workspace>,
        active: WorkspaceId,
    ) -> WorkspacePicker {
        WorkspacePicker::new(workspaces, active, Vec::new(), null_input())
    }

    /// An input over the null editor key.
    ///
    /// The picker renders through the active workspace's editors, and a null
    /// editor key resolves to nothing, so the input row paints blank.
    fn null_input() -> InputView {
        InputView {
            editor_id: EditorId::default(),
            buffer_id: BufferId::new(0),
            target: SubmitTarget::WorkspacePicker,
        }
    }

    fn render(
        picker: &mut WorkspacePicker,
        workspaces: &mut SlotMap<WorkspaceId, Workspace>,
        active: WorkspaceId,
        buf: &mut Buffer,
        area: Rect,
    ) {
        let theme = selection_theme();
        render_with(
            &theme,
            picker,
            workspaces,
            active,
            buf,
            area,
            &mut ApcScene::new(),
        );
    }

    fn render_with(
        theme: &Theme,
        picker: &mut WorkspacePicker,
        workspaces: &mut SlotMap<WorkspaceId, Workspace>,
        active: WorkspaceId,
        buf: &mut Buffer,
        area: Rect,
        scene: &mut ApcScene,
    ) {
        render_workspace_picker(
            picker,
            &mut workspaces[active],
            theme,
            &crate::render::editor::ResolvedChrome::resolve(theme),
            area,
            buf,
            scene,
        );
    }

    #[test]
    fn layout_holds_the_header_above_one_row_per_workspace() {
        let (modal, inner) =
            workspace_picker_layout(Rect::new(0, 0, 120, 40), 3).expect("the area hosts the modal");
        assert_eq!(modal.width, 112, "the box holds at its recommended width");
        assert_eq!(
            inner.height,
            HEADER_ROWS + 3 + FOOTER_ROWS,
            "the filter, separator, and column labels sit above the rows, and the footer below"
        );
    }

    #[test]
    fn layout_caps_the_rows_it_shows() {
        let (_, inner) = workspace_picker_layout(Rect::new(0, 0, 120, 40), 30)
            .expect("the area hosts the modal");
        assert_eq!(inner.height, HEADER_ROWS + MAX_ENTRY_ROWS + FOOTER_ROWS);
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

        let start = picker.selected();
        picker.page(1);
        assert_eq!(
            picker.selected(),
            start + half,
            "a page down covers half a screen"
        );
        picker.page(-1);
        assert_eq!(picker.selected(), start, "and a page up returns");

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

    /// The input starts one column past the prompt, so the seeded name paints
    /// nothing over the longer rename prompt. The input is a real one, since
    /// the null editor of `picker_for` paints no text to overlap.
    /// A query on a sub-session's own name lights that name and not the parent
    /// prefix in front of it.
    #[test]
    fn a_sub_session_lights_its_own_name_past_the_parent_prefix() {
        let (mut workspaces, active) =
            workspaces_over(2, |i| PathBuf::from(format!("/tmp/ws{i:02}")));
        for (id, ws) in workspaces.iter_mut() {
            let sub = id != active;
            ws.uid = WorkspaceUid(if sub { 2 } else { 1 });
            ws.parent = sub.then_some(WorkspaceUid(1));
        }
        let mut picker = picker_for(&workspaces, active);
        picker.refilter("ws01");

        let area = Rect::new(0, 0, 100, 30);
        let mut buf = Buffer::empty(area);
        render_with(
            &match_theme(),
            &mut picker,
            &mut workspaces,
            active,
            &mut buf,
            area,
            &mut ApcScene::new(),
        );

        let label: Vec<char> = "ws00/ws01".chars().collect();
        let (row, col) = (0..area.height)
            .find_map(|y| {
                let text: Vec<char> = row_text(&buf, y).chars().collect();
                let x = text.windows(label.len()).position(|w| w == label)?;
                Some((y, x as u16))
            })
            .expect("the sub-session row reads parent/name");
        let lit: Vec<u16> = (0..area.width)
            .filter(|&x| buf[(x, row)].fg == MATCH_FG)
            .collect();
        assert_eq!(
            lit,
            (col + 5..col + 9).collect::<Vec<_>>(),
            "the match lights `ws01` past the `ws00/` prefix"
        );
    }

    #[test]
    fn the_family_scope_names_its_root_in_the_header() {
        let (mut picker, mut workspaces, active) = picker_over(2);
        picker.toggle_scope();

        let area = Rect::new(0, 0, 100, 30);
        let mut buf = Buffer::empty(area);
        render(&mut picker, &mut workspaces, active, &mut buf, area);

        let painted: Vec<String> = (0..area.height).map(|y| row_text(&buf, y)).collect();
        assert!(
            painted
                .iter()
                .any(|row| row.contains("sessions \u{00B7} ws00 \u{203A}")),
            "the header names the family by its root:\n{}",
            painted.join("\n")
        );
    }

    /// The header names both scopes, `all` and `family`, and the one the list
    /// shows takes the chip's background: `all` until S-Tab narrows the list
    /// to the active session's family.
    ///
    /// The scene is dead, so the chip's cells carry its background rather than
    /// a panel.
    #[test]
    fn the_header_chip_names_the_scope() {
        let theme = {
            let (config, _) = stoat_config::parse(r##"theme t { ui.chip.bg = "#040506"; }"##);
            Theme::from_config(&config.expect("theme parses"), "t").expect("theme loads")
        };
        let header = |family: bool| {
            let (mut picker, mut workspaces, active) = picker_over(2);
            if family {
                picker.toggle_scope();
            }
            let area = Rect::new(0, 0, 120, 30);
            let mut buf = Buffer::empty(area);
            let mut scene = ApcScene::new();
            scene.set_live(false);
            render_with(
                &theme,
                &mut picker,
                &mut workspaces,
                active,
                &mut buf,
                area,
                &mut scene,
            );

            let (_, inner) = workspace_picker_layout(area, 2).expect("the area hosts the modal");
            let columns = inner.x..inner.x + inner.width;
            let header: String = row_text(&buf, inner.y)
                .chars()
                .skip(inner.x as usize)
                .take(inner.width as usize)
                .collect();
            let words: Vec<&str> = header.split_whitespace().collect();
            let chip: String = columns
                .filter(|&x| buf[(x, inner.y)].bg == Color::Rgb(4, 5, 6))
                .map(|x| buf[(x, inner.y)].symbol())
                .collect();
            (words[words.len() - 2..].join(" "), chip.trim().to_owned())
        };
        assert_eq!(
            [header(false), header(true)],
            [
                ("all family".to_owned(), "all".to_owned()),
                ("all family".to_owned(), "family".to_owned()),
            ],
        );
    }

    #[test]
    fn a_rename_names_itself_in_the_header() {
        let (mut workspaces, active) =
            workspaces_over(3, |i| PathBuf::from(format!("/tmp/ws{i:02}")));
        let executor: Executor = Arc::new(TestScheduler::new()).executor();
        let input = InputView::create(
            &mut workspaces[active],
            executor,
            SubmitTarget::WorkspacePicker,
            "ws01",
            "insert",
        );
        let mut picker = WorkspacePicker::new(&workspaces, active, Vec::new(), input);
        picker.rename_start();

        let area = Rect::new(0, 0, 100, 30);
        let mut buf = Buffer::empty(area);
        render(&mut picker, &mut workspaces, active, &mut buf, area);

        let painted: Vec<String> = (0..area.height).map(|y| row_text(&buf, y)).collect();
        assert!(
            painted
                .iter()
                .any(|row| row.contains("rename \u{203A} ws01")),
            "the header reads rename, and the name starts past its prompt:\n{}",
            painted.join("\n")
        );
    }

    /// The create row takes the first list row and names the session that
    /// Enter makes. Its counts stay blank, because that session does not exist
    /// yet.
    #[test]
    fn a_query_with_no_row_paints_a_create_row_in_its_place() {
        let (buf, inner) = painted_with_query("notes");
        assert_eq!(
            selected_words(&buf, inner),
            [["+", "notes", "/tmp/ws00"]],
            "one selected row names the query and the active root"
        );
    }

    #[test]
    fn a_query_with_a_row_paints_no_create_row() {
        let (buf, inner) = painted_with_query("ws01");

        let selected: Vec<Vec<String>> = selected_words(&buf, inner)
            .into_iter()
            .map(|words| words.into_iter().take(2).collect())
            .collect();
        assert_eq!(
            selected,
            [["ws01", "/tmp/ws01"]],
            "the selection rides the matching row"
        );
        assert!(
            (inner.y..inner.y + inner.height).all(|y| buf[(inner.x + 1, y)].symbol() != "+"),
            "no row paints the create marker"
        );
        assert_eq!(
            header_text(&buf, inner)
                .split_whitespace()
                .collect::<Vec<_>>(),
            ["name", "path", "buf", "run", "edit"],
            "the header labels every column"
        );
    }

    /// Enter on an empty query with no row closes the picker and creates
    /// nothing, so an empty list offers no create row.
    #[test]
    fn an_empty_query_over_no_row_paints_no_create_row() {
        let (mut workspaces, active) =
            workspaces_over(2, |i| PathBuf::from(format!("/tmp/ws{i:02}")));
        for (uid, ws) in (1..).zip(workspaces.values_mut()) {
            ws.uid = WorkspaceUid(uid);
        }
        let mut picker = picker_for(&workspaces, active);
        picker.omit_active();
        picker.toggle_scope();
        assert!(
            picker.filtered().is_empty(),
            "the active family has no row left"
        );

        let area = Rect::new(0, 0, 100, 30);
        let mut buf = Buffer::empty(area);
        render(&mut picker, &mut workspaces, active, &mut buf, area);
        assert_eq!(
            selected_rows(&buf),
            Vec::<u16>::new(),
            "no row paints as selected"
        );
    }

    /// The picker over three workspaces with `query` typed into a real input
    /// and ranked, painted, alongside the modal's inner rect.
    fn painted_with_query(query: &str) -> (Buffer, Rect) {
        painted_with(query, |_| {})
    }

    /// [`painted_with_query`] with `adjust` run on the picker as it opens,
    /// before the query ranks.
    fn painted_with(query: &str, adjust: impl FnOnce(&mut WorkspacePicker)) -> (Buffer, Rect) {
        let (mut picker, mut workspaces, active) = picker_with(query, adjust);
        let area = Rect::new(0, 0, 100, 30);
        let mut buf = Buffer::empty(area);
        render(&mut picker, &mut workspaces, active, &mut buf, area);
        let (_, inner) = workspace_picker_layout(area, picker.entries().len())
            .expect("the area hosts the modal");
        (buf, inner)
    }

    /// The picker over three workspaces with `query` typed into a real input
    /// and ranked, after `adjust` runs on the picker as it opens.
    fn picker_with(
        query: &str,
        adjust: impl FnOnce(&mut WorkspacePicker),
    ) -> (
        WorkspacePicker,
        SlotMap<WorkspaceId, Workspace>,
        WorkspaceId,
    ) {
        let (mut workspaces, active) =
            workspaces_over(3, |i| PathBuf::from(format!("/tmp/ws{i:02}")));
        let executor: Executor = Arc::new(TestScheduler::new()).executor();
        let input = InputView::create(
            &mut workspaces[active],
            executor,
            SubmitTarget::WorkspacePicker,
            query,
            "insert",
        );
        let mut picker = WorkspacePicker::new(&workspaces, active, Vec::new(), input);
        adjust(&mut picker);
        picker.refilter(query);
        (picker, workspaces, active)
    }

    /// The column header row's text inside the modal's border.
    fn header_text(buf: &Buffer, inner: Rect) -> String {
        row_text(buf, inner.y + HEADER_ROWS - 1)
            .chars()
            .skip(inner.x as usize)
            .take(inner.width as usize)
            .collect()
    }

    /// The create row's header names both keys and the family a sub-session
    /// joins.
    #[test]
    fn a_create_row_hints_both_keys_in_the_header() {
        let (buf, inner) = painted_with_query("notes");
        assert_eq!(
            header_text(&buf, inner).trim(),
            "Enter new session   Alt-Enter new sub-session of ws00"
        );
    }

    /// A create that leaves the fresh launch workspace makes a root session on
    /// either key, so the header names Enter alone.
    #[test]
    fn a_create_row_from_a_fresh_launch_hints_enter_alone() {
        let (buf, inner) = painted_with("notes", WorkspacePicker::omit_active);
        assert_eq!(header_text(&buf, inner).trim(), "Enter new session");
    }

    /// The words of each selected row inside the modal's border, top first.
    fn selected_words(buf: &Buffer, inner: Rect) -> Vec<Vec<String>> {
        selected_rows(buf)
            .into_iter()
            .map(|row| {
                row_text(buf, row)
                    .chars()
                    .skip(inner.x as usize)
                    .take(inner.width as usize)
                    .collect::<String>()
                    .split_whitespace()
                    .map(str::to_owned)
                    .collect()
            })
            .collect()
    }

    /// Picking a remote row leaves this machine, so the host it goes to leads
    /// the row's root in the path column.
    #[test]
    fn a_remote_row_carries_its_host_into_the_path_column() {
        let (mut workspaces, active) =
            workspaces_over(2, |i| PathBuf::from(format!("/tmp/ws{i:02}")));
        let remote = workspaces.keys().find(|id| *id != active).unwrap();
        workspaces[remote].remote = Some(RemoteTarget {
            transport: Transport::Ssh,
            host: "box".to_owned(),
            args: Vec::new(),
        });
        let mut picker = picker_for(&workspaces, active);

        let area = Rect::new(0, 0, 100, 30);
        let mut buf = Buffer::empty(area);
        render(&mut picker, &mut workspaces, active, &mut buf, area);

        let painted: Vec<String> = (0..area.height).map(|r| row_text(&buf, r)).collect();
        assert!(
            painted.iter().any(|line| line.contains("box:/tmp/ws01")),
            "the remote row reads host:root: {painted:?}",
        );
    }

    /// Sessions of one repository share a root, and their generated names do
    /// not say where they live, so each row still paints the root.
    #[test]
    fn rows_that_share_a_root_each_paint_it() {
        let (mut workspaces, active) = workspaces_over(2, |_| PathBuf::from("/tmp/same"));
        let mut picker = picker_for(&workspaces, active);

        let area = Rect::new(0, 0, 100, 30);
        let mut buf = Buffer::empty(area);
        render(&mut picker, &mut workspaces, active, &mut buf, area);

        let (_, inner) = workspace_picker_layout(area, 2).expect("the area hosts the modal");
        assert_eq!(
            header_text(&buf, inner)
                .split_whitespace()
                .collect::<Vec<_>>(),
            ["name", "path", "buf", "run", "edit"],
            "the header labels every column",
        );

        let painted: Vec<String> = (0..area.height).map(|r| row_text(&buf, r)).collect();
        assert_eq!(
            painted
                .iter()
                .filter(|line| line.contains("/tmp/same"))
                .count(),
            2,
            "each row paints the shared root: {painted:?}",
        );
    }

    /// The tail of a root names the project, so a root wider than its column
    /// gives up its head to an ellipsis.
    #[test]
    fn a_root_wider_than_its_column_keeps_its_tail() {
        let root = (0..8)
            .fold(PathBuf::from("/tmp"), |root, n| {
                root.join(format!("segment-{n}"))
            })
            .join("leaf");
        let (mut workspaces, active) = workspaces_over(1, |_| root.clone());
        let mut picker = picker_for(&workspaces, active);

        let area = Rect::new(0, 0, 100, 30);
        let mut buf = Buffer::empty(area);
        render(&mut picker, &mut workspaces, active, &mut buf, area);

        let painted: Vec<String> = (0..area.height).map(|r| row_text(&buf, r)).collect();
        let row = painted
            .iter()
            .find(|line| line.contains("ws00"))
            .expect("the name column paints ws00");
        let tail = row
            .split_once('\u{2026}')
            .and_then(|(_, rest)| rest.split(' ').next())
            .unwrap_or_default();
        assert!(
            tail.ends_with("/leaf") && root.to_string_lossy().ends_with(tail),
            "an ellipsis leads the root's tail: {row:?}",
        );
    }

    /// The foregrounds [`parts_theme`] gives the parts of a row.
    const ACTIVE: Color = Color::Rgb(0, 0, 1);
    const DIM: Color = Color::Rgb(0, 0, 2);
    const SAVED: Color = Color::Rgb(0, 0, 3);
    const MUTED: Color = Color::Rgb(0, 0, 4);
    const DISABLED: Color = Color::Rgb(0, 0, 5);
    const RUN: Color = Color::Rgb(0, 0, 6);
    const PROMPT: Color = Color::Rgb(0, 0, 7);
    const SELECTED: Color = Color::Rgb(0, 0, 8);
    const KEY: Color = Color::Rgb(0, 0, 9);

    fn parts_theme() -> Theme {
        let src = r##"theme t {
            ui.toggle.active.fg = "#000001";
            ui.text.dim.fg = "#000002";
            ui.surface.overlay.fg = "#000003";
            ui.text.muted.fg = "#000004";
            ui.text.disabled.fg = "#000005";
            ui.statusline.run.fg = "#000006";
            ui.prompt.fg = "#000007";
            ui.selection.fg = "#000008";
            ui.key_label.fg = "#000009";
        }"##;
        let (config, _) = stoat_config::parse(src);
        Theme::from_config(&config.expect("theme parses"), "t").expect("theme loads")
    }

    /// `picker` painted at 120 by 30 with [`parts_theme`] under a scene that is
    /// `live` or not, alongside that scene and the modal's inner rect.
    fn painted(
        picker: &mut WorkspacePicker,
        workspaces: &mut SlotMap<WorkspaceId, Workspace>,
        active: WorkspaceId,
        live: bool,
    ) -> (Buffer, ApcScene, Rect) {
        let area = Rect::new(0, 0, 120, 30);
        let mut buf = Buffer::empty(area);
        let mut scene = ApcScene::new();
        scene.set_live(live);
        let theme = parts_theme();
        render_with(
            &theme, picker, workspaces, active, &mut buf, area, &mut scene,
        );

        let (_, inner) = workspace_picker_layout(area, picker.entries().len())
            .expect("the area hosts the modal");
        (buf, scene, inner)
    }

    /// The picker over the active session, an open one, and a saved one, top
    /// to bottom, [`painted`] under a scene that is `live` or not.
    fn painted_statuses(live: bool) -> (Buffer, ApcScene, Rect) {
        let (mut workspaces, active) =
            workspaces_over(2, |i| PathBuf::from(format!("/tmp/ws{i:02}")));
        let saved = RegistryEntry {
            meta: WorkspaceMeta {
                uid: WorkspaceUid(9),
                name: "ws02".to_owned(),
                git_root: PathBuf::from("/tmp/ws02"),
                buffer_count: 1,
                remote_host: None,
                parent: None,
            },
            state_path: PathBuf::from("/tmp/ws02.ron"),
            mtime: UNIX_EPOCH,
        };
        let mut picker = WorkspacePicker::new(&workspaces, active, vec![saved], null_input());
        painted(&mut picker, &mut workspaces, active, live)
    }

    #[test]
    fn each_status_marks_its_row_with_a_dot_in_its_color() {
        let (_, mut scene, inner) = painted_statuses(true);
        let dots: Vec<(i16, i16, Color)> = command::decode_stream(scene.buffer())
            .into_iter()
            .filter_map(|cmd| match cmd {
                Command::Bar(BarCommand {
                    x,
                    y,
                    width: DOT_WIDTH,
                    height: DOT_HEIGHT,
                    color: [r, g, b],
                    ..
                }) => Some((x, y, Color::Rgb(r, g, b))),
                _ => None,
            })
            .collect();

        let sixteenths = |cell: u16| cell as i16 * 16;
        let dot = |row: u16, color: Color| {
            let y = sixteenths(inner.y + HEADER_ROWS + row) + DOT_Y;
            (sixteenths(inner.x + 1) + DOT_X, y, color)
        };
        assert_eq!(
            dots,
            [dot(0, ACTIVE), dot(1, DIM), dot(2, SAVED)],
            "the active, open, and saved rows each carry one dot"
        );
    }

    /// A scene that is not live draws no dots, so a row marks its status with
    /// a glyph, which an open session leaves blank.
    #[test]
    fn without_a_live_scene_each_status_marks_its_row_with_a_glyph() {
        let (buf, _, inner) = painted_statuses(false);
        let marks: Vec<&str> = (0..3)
            .map(|row| buf[(inner.x + 1, inner.y + HEADER_ROWS + row)].symbol())
            .collect();
        assert_eq!(marks, ["*", " ", "\u{00b7}"]);
    }

    /// The footer leads with a legend of the three status dots, then lists the
    /// bound keys.
    #[test]
    fn the_footer_leads_with_the_status_legend() {
        let (buf, _, inner) = painted_statuses(true);
        let row = inner.y + inner.height - 1;
        let text: String = row_text(&buf, row)
            .chars()
            .skip(inner.x as usize)
            .take(inner.width as usize)
            .collect();
        assert_eq!(
            text.trim_end(),
            " \u{2022} active  \u{2022} open  \u{2022} saved  Enter switch  A-Enter sub-session  \
             C-r rename  C-x close  Del delete  S-Tab scope"
        );

        let dots: Vec<Color> = (inner.x..inner.x + inner.width)
            .filter(|&x| buf[(x, row)].symbol() == "\u{2022}")
            .map(|x| buf[(x, row)].fg)
            .collect();
        assert_eq!(
            dots,
            [ACTIVE, DIM, SAVED],
            "each legend dot takes its status color"
        );
    }

    /// A sub-session's parent prefix paints muted with a disabled slash. A
    /// host and a live run count paint in the run color, a zero run count
    /// disabled, and the edit count dim.
    #[test]
    fn a_row_paints_each_part_in_its_own_color() {
        let (mut workspaces, root) =
            workspaces_over(2, |i| PathBuf::from(format!("/tmp/ws{i:02}")));
        let sub = workspaces.keys().find(|&id| id != root).unwrap();
        workspaces[root].uid = WorkspaceUid(1);
        workspaces[sub].uid = WorkspaceUid(2);
        workspaces[sub].parent = Some(WorkspaceUid(1));
        workspaces[sub].remote = Some(RemoteTarget {
            transport: Transport::Ssh,
            host: "box".to_owned(),
            args: Vec::new(),
        });
        let executor: Executor = Arc::new(TestScheduler::new()).executor();
        let run = RunState::new(PathBuf::from("/tmp/ws01"), &mut workspaces[sub], executor);
        workspaces[sub].runs.insert(run);

        let mut picker = picker_for(&workspaces, sub);
        let (buf, _, inner) = painted(&mut picker, &mut workspaces, sub, true);

        let top = inner.y + HEADER_ROWS;
        assert_eq!(
            [pieces(&buf, inner, top), pieces(&buf, inner, top + 1)],
            [
                owned(&[
                    ("ws00", MUTED),
                    ("/", DISABLED),
                    ("ws01", PROMPT),
                    ("box:", RUN),
                    ("/tmp/ws01", PROMPT),
                    ("2", PROMPT),
                    ("1", RUN),
                    ("2", DIM),
                ]),
                owned(&[
                    ("ws00", SELECTED),
                    ("/tmp/ws00", SELECTED),
                    ("1", SELECTED),
                    ("0", DISABLED),
                    ("1", DIM),
                ]),
            ],
            "the active sub-session row, then the selected root row"
        );
    }

    #[test]
    fn a_create_row_hints_each_key_in_the_key_color() {
        let (mut picker, mut workspaces, active) = picker_with("notes", |_| {});
        let (buf, _, inner) = painted(&mut picker, &mut workspaces, active, true);
        assert_eq!(
            pieces(&buf, inner, inner.y + HEADER_ROWS - 1),
            owned(&[
                ("Enter", KEY),
                ("new", MUTED),
                ("session", MUTED),
                ("Alt-Enter", KEY),
                ("new", MUTED),
                ("sub-session", MUTED),
                ("of", MUTED),
                ("ws00", MUTED),
            ])
        );
    }

    /// `expected` as owned text, to compare with what [`pieces`] reads.
    fn owned(expected: &[(&str, Color)]) -> Vec<(String, Color)> {
        expected
            .iter()
            .map(|&(text, fg)| (text.to_owned(), fg))
            .collect()
    }

    /// The text of `row` inside the modal's border past the selection bar, cut
    /// at every space and every change of foreground, each piece with its
    /// foreground.
    fn pieces(buf: &Buffer, inner: Rect, row: u16) -> Vec<(String, Color)> {
        let mut pieces: Vec<(String, Color)> = Vec::new();
        let mut joined = false;
        for x in inner.x + 1..inner.x + inner.width {
            let cell = &buf[(x, row)];
            if cell.symbol() == " " {
                joined = false;
                continue;
            }
            match pieces.last_mut() {
                Some((text, fg)) if joined && *fg == cell.fg => text.push_str(cell.symbol()),
                _ => pieces.push((cell.symbol().to_owned(), cell.fg)),
            }
            joined = true;
        }
        pieces
    }
}
