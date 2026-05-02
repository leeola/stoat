use crate::{
    buffer_registry::BufferRegistry,
    editor_state::{EditorId, EditorState},
    pane::{Divider, DividerOrientation, Pane, View},
    render::{
        claude_pane::render_claude_pane,
        editor::{editor_cursor_position, render_editor},
        layout::split_pane_status,
        run_pane::render_run_pane,
        FrameCtx, PaneCtx,
    },
};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::Text,
    widgets::{Paragraph, Widget},
};
use slotmap::SlotMap;
use std::path::Path;

pub(crate) fn render_pane(
    pane: &Pane,
    is_focused: bool,
    ctx: PaneCtx<'_>,
    frame: FrameCtx<'_>,
    buf: &mut Buffer,
) {
    let theme = frame.theme;
    let text_style = if is_focused {
        theme.get(crate::theme::scope::UI_TEXT)
    } else {
        theme.get(crate::theme::scope::UI_TEXT_MUTED)
    };
    let (content_area, status_area) = split_pane_status(pane.area);

    let PaneCtx {
        editors,
        buffers,
        runs,
        chats,
    } = ctx;

    match &pane.view {
        View::Label(label) => {
            Paragraph::new(Text::styled(label.clone(), text_style))
                .centered()
                .render(content_area, buf);
        },
        View::Editor(editor_id) => {
            if let Some(editor) = editors.get_mut(*editor_id) {
                render_editor(editor, content_area, text_style, theme, buf, is_focused);
            }
        },
        View::Run(run_id) => {
            if let Some(run_state) = runs.get(*run_id) {
                render_run_pane(run_state, editors, theme, content_area, is_focused, buf);
            }
        },
        View::Claude(session_id) => {
            if let Some(chat) = chats.get(session_id) {
                let chat_ctx = PaneCtx {
                    editors,
                    buffers,
                    runs,
                    chats,
                };
                render_claude_pane(chat, chat_ctx, content_area, is_focused, frame, buf);
            }
        },
    }

    render_pane_status(
        &pane.view,
        is_focused,
        status_area,
        frame,
        editors,
        buffers,
        buf,
    );
}

/// Minimal status bar for overlay panes (commits/rebase/reword/conflict).
/// Does not know about editors or buffers; shows only mode + workspace +
/// a short label identifying the overlay. Matches the visual style of
/// [`render_pane_status`] for a focused pane.
pub(crate) fn render_overlay_status(
    area: Rect,
    is_focused: bool,
    frame: FrameCtx<'_>,
    label: &str,
    buf: &mut Buffer,
) {
    let workspace_name = frame.workspace_name;
    let mode = frame.mode;
    let theme = frame.theme;
    if area.width == 0 || area.height == 0 {
        return;
    }
    let base_style = if is_focused {
        theme.get(crate::theme::scope::UI_STATUSBAR_FOCUSED)
    } else {
        theme.get(crate::theme::scope::UI_STATUSBAR_UNFOCUSED)
    };
    let y = area.y;
    let end_x = area.x + area.width;
    for x in area.x..end_x {
        buf[(x, y)].set_char(' ').set_style(base_style);
    }

    let mut cursor = area.x;
    if is_focused {
        let (mode_label, mode_bg) = mode_segment(mode, theme);
        let mode_style = theme.get(crate::theme::scope::UI_MODE_LABEL).bg(mode_bg);
        cursor = paint_segment(
            buf,
            y,
            cursor,
            end_x,
            &format!(" {mode_label} "),
            mode_style,
        );
        cursor = paint_segment(
            buf,
            y,
            cursor,
            end_x,
            &format!(" {workspace_name} "),
            base_style.add_modifier(Modifier::BOLD),
        );
    }
    let left_pad = if cursor == area.x { " " } else { "" };
    paint_segment(
        buf,
        y,
        cursor,
        end_x,
        &format!("{left_pad}{label} "),
        base_style,
    );
}

pub(crate) fn render_pane_dividers(
    dividers: &[Divider],
    theme: &crate::theme::Theme,
    buf: &mut Buffer,
) {
    let dim = theme.get(crate::theme::scope::UI_BORDER_INACTIVE);
    let lit = theme.get(crate::theme::scope::UI_BORDER_FOCUSED);
    for d in dividers {
        let style = if d.touches_focus { lit } else { dim };
        let buf_end_x = buf.area.x + buf.area.width;
        let buf_end_y = buf.area.y + buf.area.height;
        match d.orientation {
            DividerOrientation::Vertical => {
                if d.x >= buf_end_x {
                    continue;
                }
                for yy in d.y..d.y.saturating_add(d.len).min(buf_end_y) {
                    buf[(d.x, yy)].set_char('│').set_style(style);
                }
            },
            DividerOrientation::Horizontal => {
                if d.y >= buf_end_y {
                    continue;
                }
                for xx in d.x..d.x.saturating_add(d.len).min(buf_end_x) {
                    buf[(xx, d.y)].set_char('─').set_style(style);
                }
            },
        }
    }
}

fn render_pane_status(
    view: &View,
    is_focused: bool,
    area: Rect,
    frame: FrameCtx<'_>,
    editors: &mut SlotMap<EditorId, EditorState>,
    buffers: &BufferRegistry,
    buf: &mut Buffer,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let workspace_name = frame.workspace_name;
    let workspace_root = frame.workspace_root;
    let mode = frame.mode;
    let theme = frame.theme;

    let base_style = if is_focused {
        theme.get(crate::theme::scope::UI_STATUSBAR_FOCUSED)
    } else {
        theme.get(crate::theme::scope::UI_STATUSBAR_UNFOCUSED)
    };

    let y = area.y;
    let end_x = area.x + area.width;
    for x in area.x..end_x {
        buf[(x, y)].set_char(' ').set_style(base_style);
    }

    let mut cursor = area.x;
    if is_focused {
        let (label, mode_bg) = mode_segment(mode, theme);
        let mode_style = theme.get(crate::theme::scope::UI_MODE_LABEL).bg(mode_bg);
        cursor = paint_segment(buf, y, cursor, end_x, &format!(" {label} "), mode_style);
        let ws_style = base_style.add_modifier(Modifier::BOLD);
        cursor = paint_segment(
            buf,
            y,
            cursor,
            end_x,
            &format!(" {workspace_name} "),
            ws_style,
        );
    }

    let (filename, dirty, cursor_pos) = pane_status_info(view, workspace_root, editors, buffers);
    if let Some(name) = filename {
        let left_pad = if cursor == area.x { " " } else { "" };
        let text = if dirty {
            format!("{left_pad}{name} [+] ")
        } else {
            format!("{left_pad}{name} ")
        };
        cursor = paint_segment(buf, y, cursor, end_x, &text, base_style);
    }

    let mut right_anchor = end_x;
    if let Some((line, col)) = cursor_pos {
        let text = format!(" {line}:{col} ");
        let width = text.chars().count() as u16;
        let start = right_anchor.saturating_sub(width);
        if start >= cursor {
            paint_segment(buf, y, start, right_anchor, &text, base_style);
            right_anchor = start;
        }
    }
    if is_focused {
        if let Some(count) = frame.pending_count {
            let text = format!(" {count} ");
            let width = text.chars().count() as u16;
            let start = right_anchor.saturating_sub(width);
            if start >= cursor {
                paint_segment(
                    buf,
                    y,
                    start,
                    right_anchor,
                    &text,
                    base_style.add_modifier(Modifier::BOLD),
                );
                right_anchor = start;
            }
        }
        if let Some(entry) = frame.lsp_progress {
            let text = lsp_progress_label(entry);
            let width = text.chars().count() as u16;
            let start = right_anchor.saturating_sub(width);
            if start >= cursor {
                paint_segment(
                    buf,
                    y,
                    start,
                    right_anchor,
                    &text,
                    base_style.add_modifier(Modifier::ITALIC),
                );
            }
        }
    }
    let _ = cursor;
}

/// Formats an LSP progress entry for the status bar. Always padded with
/// leading and trailing spaces so adjacent segments stay separated.
fn lsp_progress_label(entry: &crate::lsp::progress::LspProgressEntry) -> String {
    let mut body = entry.title.clone();
    if let Some(message) = &entry.message {
        if !body.is_empty() {
            body.push_str(": ");
        }
        body.push_str(message);
    }
    if let Some(pct) = entry.percentage {
        if !body.is_empty() {
            body.push(' ');
        }
        body.push_str(&format!("{pct}%"));
    }
    if body.is_empty() {
        body.push_str("...");
    }
    format!(" {body} ")
}

fn paint_segment(
    buf: &mut Buffer,
    y: u16,
    start_x: u16,
    end_x: u16,
    text: &str,
    style: Style,
) -> u16 {
    let mut x = start_x;
    for ch in text.chars() {
        if x >= end_x {
            break;
        }
        buf[(x, y)].set_char(ch).set_style(style);
        x += 1;
    }
    x
}

pub(crate) fn mode_segment(mode: &str, theme: &crate::theme::Theme) -> (&'static str, Color) {
    use crate::theme::scope;
    let (label, default, scope_name) = match mode {
        "normal" => ("NOR", Color::Blue, scope::UI_STATUSLINE_NORMAL),
        "insert" => ("INS", Color::Green, scope::UI_STATUSLINE_INSERT),
        "select" => ("SEL", Color::Yellow, scope::UI_STATUSLINE_SELECT),
        "prompt" => ("PMT", Color::Green, scope::UI_STATUSLINE_PROMPT),
        "run" => ("RUN", Color::Magenta, scope::UI_STATUSLINE_RUN),
        "commits" => ("COM", Color::Yellow, scope::UI_STATUSLINE_COMMITS),
        "rebase" => ("REB", Color::Red, scope::UI_STATUSLINE_REBASE),
        "reword" | "reword_insert" => ("RWD", Color::Red, scope::UI_STATUSLINE_REWORD),
        "conflict" => ("CNF", Color::LightRed, scope::UI_STATUSLINE_CONFLICT),
        "review" => ("REV", Color::Cyan, scope::UI_STATUSLINE_REVIEW),
        "goto" => ("GTO", Color::DarkGray, scope::UI_STATUSLINE_SUBMODE),
        "z" => ("VWA", Color::DarkGray, scope::UI_STATUSLINE_SUBMODE),
        "bracket_next" => ("BNX", Color::DarkGray, scope::UI_STATUSLINE_SUBMODE),
        "bracket_prev" => ("BPV", Color::DarkGray, scope::UI_STATUSLINE_SUBMODE),
        "match" => ("MAT", Color::DarkGray, scope::UI_STATUSLINE_SUBMODE),
        "select_goto" => ("SLG", Color::DarkGray, scope::UI_STATUSLINE_SUBMODE),
        "space" => ("SPC", Color::DarkGray, scope::UI_STATUSLINE_SUBMODE),
        "space_workspace" => ("SWS", Color::DarkGray, scope::UI_STATUSLINE_SUBMODE),
        "space_pane_nav" => ("SPN", Color::DarkGray, scope::UI_STATUSLINE_SUBMODE),
        "space_pane_nav_new" => ("SNN", Color::DarkGray, scope::UI_STATUSLINE_SUBMODE),
        "claude" => ("CLA", Color::DarkGray, scope::UI_STATUSLINE_SUBMODE),
        _ => ("---", Color::Gray, scope::UI_STATUSLINE_DEFAULT),
    };
    let color = theme.get(scope_name).fg.unwrap_or(default);
    (label, color)
}

fn pane_status_info(
    view: &View,
    workspace_root: &Path,
    editors: &mut SlotMap<EditorId, EditorState>,
    buffers: &BufferRegistry,
) -> (Option<String>, bool, Option<(u32, u32)>) {
    match view {
        View::Editor(editor_id) => {
            let Some(editor) = editors.get_mut(*editor_id) else {
                return (None, false, None);
            };
            let buffer_id = editor.buffer_id;
            let path = buffers.path_for(buffer_id);
            let filename = path
                .map(|p| crate::paths::display_relative(p, workspace_root))
                .or_else(|| Some("[scratch]".to_string()));
            let dirty = buffers
                .get(buffer_id)
                .and_then(|b| b.read().ok().map(|g| g.dirty))
                .unwrap_or(false);
            let cursor_pos = editor_cursor_position(editor);
            (filename, dirty, cursor_pos)
        },
        View::Run(_) => (Some("[run]".to_string()), false, None),
        View::Claude(_) => (Some("[claude]".to_string()), false, None),
        View::Label(label) => (Some(label.clone()), false, None),
    }
}
