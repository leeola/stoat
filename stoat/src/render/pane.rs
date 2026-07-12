use crate::{
    buffer_registry::BufferRegistry,
    editor_state::{EditorId, EditorState},
    pane::{Divider, DividerOrientation, Pane, View},
    render::{
        editor::{editor_cursor_position, render_editor_with_overlay},
        layout::split_pane_status,
        review::style_rgb,
        run_pane::render_run_pane,
        term_pane::render_term_pane,
        undercurl::UndercurlSpan,
        FrameCtx, PaneCtx,
    },
};
use lsp_types::DiagnosticSeverity;
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::Text,
    widgets::{Paragraph, Widget},
};
use slotmap::SlotMap;
use std::path::Path;
use stoatty_widgets::{
    status_bar::{StatusBar, StatusSegment},
    ApcScene,
};

pub(crate) fn render_pane(
    pane: &Pane,
    is_focused: bool,
    ctx: PaneCtx<'_>,
    frame: FrameCtx<'_>,
    buf: &mut Buffer,
    scene: &mut ApcScene,
    undercurls: &mut Vec<UndercurlSpan>,
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
        terms,
    } = ctx;

    match &pane.view {
        View::Label(label) => {
            Paragraph::new(Text::styled(label.clone(), text_style))
                .centered()
                .render(content_area, buf);
        },
        View::Editor(editor_id) => {
            if let Some(editor) = editors.get_mut(*editor_id) {
                let labels = if is_focused {
                    frame.goto_word_labels
                } else {
                    None
                };
                let diagnostic_info = buffers
                    .path_for(editor.buffer_id)
                    .map(|path| (path, frame.diagnostics));
                render_editor_with_overlay(
                    editor,
                    content_area,
                    text_style,
                    theme,
                    buf,
                    is_focused,
                    frame.stoatty,
                    frame.line_numbers,
                    frame.mode == "insert",
                    if is_focused { frame.hover_cell } else { None },
                    labels,
                    frame.search_query,
                    diagnostic_info,
                    Some(&mut *scene),
                    Some(undercurls),
                );
            }
        },
        View::Run(run_id) => {
            if let Some(run_state) = runs.get(*run_id) {
                render_run_pane(run_state, editors, theme, content_area, is_focused, buf);
            }
        },
        View::Agent(term_id) | View::Terminal(term_id) => {
            if let Some(term) = terms.get(*term_id) {
                render_term_pane(term, content_area, is_focused, buf);
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
        Some(scene),
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
    buf: &mut Buffer,
    scene: Option<&mut ApcScene>,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let base_style = if is_focused {
        frame.theme.get(crate::theme::scope::UI_STATUSBAR_FOCUSED)
    } else {
        frame.theme.get(crate::theme::scope::UI_STATUSBAR_UNFOCUSED)
    };

    let y = area.y;
    let end_x = area.x + area.width;
    for x in area.x..end_x {
        buf[(x, y)].set_char(' ').set_style(base_style);
    }

    let left = overlay_status_segments(is_focused, area, frame);
    render_status_segments(area, base_style, frame, &left, &[], buf, scene);
}

/// Build the overlay status bar's left segments in paint order.
///
/// Mode and workspace show only when focused, then a screen label that shows
/// unconditionally, left-padded when it leads. The screen segment differs from
/// the pane status's focus-gated one.
fn overlay_status_segments(is_focused: bool, area: Rect, frame: FrameCtx<'_>) -> Vec<StatusSeg> {
    let theme = frame.theme;
    let base_style = if is_focused {
        theme.get(crate::theme::scope::UI_STATUSBAR_FOCUSED)
    } else {
        theme.get(crate::theme::scope::UI_STATUSBAR_UNFOCUSED)
    };
    let end_x = area.x + area.width;

    let mut left: Vec<StatusSeg> = Vec::new();
    let mut cursor = area.x;
    if is_focused {
        let (mode_label, mode_bg) = mode_segment(frame.mode, theme, frame.mode_badges);
        let mode_style = theme.get(crate::theme::scope::UI_MODE_LABEL).bg(mode_bg);
        push_left(
            &mut left,
            &mut cursor,
            end_x,
            format!(" {mode_label} "),
            mode_style,
        );
        push_left(
            &mut left,
            &mut cursor,
            end_x,
            format!(" {} ", frame.workspace_name),
            base_style.add_modifier(Modifier::BOLD),
        );
    }
    if let Some((screen_label, screen_color)) = screen_segment(frame.screen, theme) {
        let left_pad = if cursor == area.x { " " } else { "" };
        push_left(
            &mut left,
            &mut cursor,
            end_x,
            format!("{left_pad}{screen_label} "),
            base_style.fg(screen_color),
        );
    }
    let _ = cursor;
    left
}

pub(crate) fn render_pane_dividers(
    dividers: &[Divider],
    theme: &crate::theme::Theme,
    buf: &mut Buffer,
    mut scene: Option<&mut ApcScene>,
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
                let height = d.y.saturating_add(d.len).min(buf_end_y).saturating_sub(d.y);
                crate::render::chrome::vline(buf, d.x, d.y, height, style, scene.as_deref_mut());
            },
            DividerOrientation::Horizontal => {
                if d.y >= buf_end_y {
                    continue;
                }
                let width = d.x.saturating_add(d.len).min(buf_end_x).saturating_sub(d.x);
                crate::render::chrome::hline(buf, d.x, d.y, width, style, scene.as_deref_mut());
            },
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn render_pane_status(
    view: &View,
    is_focused: bool,
    area: Rect,
    frame: FrameCtx<'_>,
    editors: &mut SlotMap<EditorId, EditorState>,
    buffers: &BufferRegistry,
    buf: &mut Buffer,
    scene: Option<&mut ApcScene>,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let base_style = if is_focused {
        frame.theme.get(crate::theme::scope::UI_STATUSBAR_FOCUSED)
    } else {
        frame.theme.get(crate::theme::scope::UI_STATUSBAR_UNFOCUSED)
    };

    let y = area.y;
    let end_x = area.x + area.width;
    for x in area.x..end_x {
        buf[(x, y)].set_char(' ').set_style(base_style);
    }

    let (left, right) = status_segments(view, is_focused, area, frame, editors, buffers);
    render_status_segments(area, base_style, frame, &left, &right, buf, scene);
}

/// Render the built status segments as rich APC components inside stoatty, or
/// into cells otherwise.
///
/// Rich mode needs stoatty, a scene, and every segment color as RGB. When any
/// color falls outside RGB the whole bar drops to the cell fallback, so a theme
/// without RGB status colors keeps today's cell rendering.
fn render_status_segments(
    area: Rect,
    base_style: Style,
    frame: FrameCtx<'_>,
    left: &[StatusSeg],
    right: &[StatusSeg],
    buf: &mut Buffer,
    scene: Option<&mut ApcScene>,
) {
    let rich = scene.filter(|_| frame.stoatty).and_then(|scene| {
        let separator = style_rgb(frame.theme.get(crate::theme::scope::UI_TEXT_MUTED).fg)?;
        let left_rich = resolve_rich_segments(left, base_style)?;
        let right_rich = resolve_rich_segments(right, base_style)?;
        Some((scene, separator, left_rich, right_rich))
    });

    match rich {
        Some((scene, separator, left_rich, right_rich)) => {
            StatusBar {
                left: &left_rich,
                right: &right_rich,
                scale: 160,
                separator,
            }
            .draw_components(area, buf, scene);
        },
        None => paint_status_fallback(buf, area, left, right),
    }
}

/// One built status-bar segment pairing painted text with its cell style.
type StatusSeg = (String, Style);

/// Build the left- and right-anchored status segments as `(text, style)` pairs
/// in paint order.
///
/// Both the cell fallback and the rich status path consume these, so the two
/// renderings stay in lockstep. The left cursor and right anchor track the same
/// cell arithmetic [`paint_segment`] applies, so a segment enters the list only
/// when it would be painted and the `lsp_message` truncation matches today's.
fn status_segments(
    view: &View,
    is_focused: bool,
    area: Rect,
    frame: FrameCtx<'_>,
    editors: &mut SlotMap<EditorId, EditorState>,
    buffers: &BufferRegistry,
) -> (Vec<StatusSeg>, Vec<StatusSeg>) {
    let theme = frame.theme;
    let base_style = if is_focused {
        theme.get(crate::theme::scope::UI_STATUSBAR_FOCUSED)
    } else {
        theme.get(crate::theme::scope::UI_STATUSBAR_UNFOCUSED)
    };
    let end_x = area.x + area.width;

    let mut left: Vec<(String, Style)> = Vec::new();
    let mut cursor = area.x;
    if is_focused {
        let (label, mode_bg) = mode_segment(frame.mode, theme, frame.mode_badges);
        let mode_style = theme.get(crate::theme::scope::UI_MODE_LABEL).bg(mode_bg);
        push_left(
            &mut left,
            &mut cursor,
            end_x,
            format!(" {label} "),
            mode_style,
        );
        push_left(
            &mut left,
            &mut cursor,
            end_x,
            format!(" {} ", frame.workspace_name),
            base_style.add_modifier(Modifier::BOLD),
        );
        if let Some((screen_label, screen_color)) = screen_segment(frame.screen, theme) {
            push_left(
                &mut left,
                &mut cursor,
                end_x,
                format!(" {screen_label} "),
                base_style.fg(screen_color),
            );
        }
    }

    let (filename, dirty, cursor_pos) =
        pane_status_info(view, frame.workspace_root, editors, buffers);
    if let Some(name) = filename {
        let left_pad = if cursor == area.x { " " } else { "" };
        let text = if dirty {
            format!("{left_pad}{name} [+] ")
        } else {
            format!("{left_pad}{name} ")
        };
        push_left(&mut left, &mut cursor, end_x, text, base_style);
    }

    let mut right: Vec<(String, Style)> = Vec::new();
    let mut right_anchor = end_x;
    if let Some((line, col)) = cursor_pos {
        let text = format!(" {line}:{col} ");
        let width = text.chars().count() as u16;
        let start = right_anchor.saturating_sub(width);
        if start >= cursor {
            right.push((text, base_style));
            right_anchor = start;
        }
    }
    if is_focused {
        if let Some(count) = frame.pending_count {
            let text = format!(" {count} ");
            let width = text.chars().count() as u16;
            let start = right_anchor.saturating_sub(width);
            if start >= cursor {
                right.push((text, base_style.add_modifier(Modifier::BOLD)));
                right_anchor = start;
            }
        }
        if let Some((text, worst)) =
            focused_diagnostic_label(view, editors, buffers, frame.diagnostics)
        {
            let width = text.chars().count() as u16;
            let start = right_anchor.saturating_sub(width);
            if start >= cursor {
                let badge_style = base_style
                    .add_modifier(Modifier::BOLD)
                    .patch(theme.get(diagnostic_severity_scope(worst)));
                right.push((text, badge_style));
                right_anchor = start;
            }
        }
        if let Some((typ, message)) = frame.lsp_message {
            let available = right_anchor.saturating_sub(cursor) as usize;
            if available > 0 {
                let text: String = message.chars().take(available).collect();
                let width = text.chars().count() as u16;
                let start = right_anchor.saturating_sub(width);
                let style = if typ == lsp_types::MessageType::ERROR {
                    base_style.patch(theme.get(crate::theme::scope::UI_ERROR))
                } else {
                    base_style
                };
                right.push((text, style));
                right_anchor = start;
            }
        }
        if let Some(entry) = frame.lsp_progress {
            let text = lsp_progress_label(entry);
            let width = text.chars().count() as u16;
            let start = right_anchor.saturating_sub(width);
            if start >= cursor {
                right.push((text, base_style.add_modifier(Modifier::ITALIC)));
                #[cfg(feature = "perf")]
                {
                    right_anchor = start;
                }
            }
        }
        #[cfg(feature = "perf")]
        if let Some(seg) = frame.perf {
            let text = crate::render::perf_label(seg);
            let width = text.chars().count() as u16;
            let start = right_anchor.saturating_sub(width);
            if start >= cursor {
                right.push((text, base_style.add_modifier(Modifier::DIM)));
            }
        }
    }
    let _ = cursor;
    let _ = right_anchor;
    (left, right)
}

/// Append a left-anchored segment and advance `cursor` as [`paint_segment`]
/// would, clamping at `end_x`.
fn push_left(left: &mut Vec<StatusSeg>, cursor: &mut u16, end_x: u16, text: String, style: Style) {
    *cursor = cursor
        .saturating_add(text.chars().count() as u16)
        .min(end_x);
    left.push((text, style));
}

/// Paint the built status segments into cells through [`paint_segment`], the
/// graceful-degradation path for a terminal without the rich components.
fn paint_status_fallback(buf: &mut Buffer, area: Rect, left: &[StatusSeg], right: &[StatusSeg]) {
    let y = area.y;
    let end_x = area.x + area.width;

    let mut cursor = area.x;
    for (text, style) in left {
        cursor = paint_segment(buf, y, cursor, end_x, text, *style);
    }

    let mut right_anchor = end_x;
    for (text, style) in right {
        let width = text.chars().count() as u16;
        let start = right_anchor.saturating_sub(width);
        paint_segment(buf, y, start, right_anchor, text, *style);
        right_anchor = start;
    }
    let _ = cursor;
    let _ = right_anchor;
}

/// Resolve each segment's fg/bg to RGB for the rich path, defaulting a missing
/// channel to `base`'s.
///
/// Returns `None` if any resolved color is not RGB, since a theme that cannot
/// supply RGB status colors drives the cell fallback rather than the rich bar.
fn resolve_rich_segments(segments: &[StatusSeg], base: Style) -> Option<Vec<StatusSegment<'_>>> {
    segments
        .iter()
        .map(|(text, style)| {
            let fg = style_rgb(style.fg.or(base.fg))?;
            let bg = style_rgb(style.bg.or(base.bg))?;
            Some(StatusSegment {
                text: text.as_str(),
                fg,
                bg,
            })
        })
        .collect()
}

/// Builds the status-bar diagnostic label for the focused pane's
/// editor along with its worst severity, or `None` when the pane is
/// not an editor, has no path, or has no diagnostics. Format:
/// ` Ee Ww Ii Hh ` showing each present severity's count; the worst
/// severity drives the badge's foreground color at the call site.
fn focused_diagnostic_label(
    view: &View,
    editors: &SlotMap<EditorId, EditorState>,
    buffers: &BufferRegistry,
    diagnostics: &crate::diagnostics::DiagnosticSet,
) -> Option<(String, DiagnosticSeverity)> {
    let View::Editor(editor_id) = view else {
        return None;
    };
    let editor = editors.get(*editor_id)?;
    let path = buffers.path_for(editor.buffer_id)?;
    let summary = diagnostics.summarize(path);
    let worst = summary.worst?;
    let mut parts = Vec::new();
    if summary.error > 0 {
        parts.push(format!("E{}", summary.error));
    }
    if summary.warning > 0 {
        parts.push(format!("W{}", summary.warning));
    }
    if summary.information > 0 {
        parts.push(format!("I{}", summary.information));
    }
    if summary.hint > 0 {
        parts.push(format!("H{}", summary.hint));
    }
    Some((format!(" {} ", parts.join(" ")), worst))
}

fn diagnostic_severity_scope(severity: DiagnosticSeverity) -> &'static str {
    use crate::theme::scope;
    match severity {
        DiagnosticSeverity::ERROR => scope::UI_DIAGNOSTIC_ERROR,
        DiagnosticSeverity::WARNING => scope::UI_DIAGNOSTIC_WARNING,
        DiagnosticSeverity::INFORMATION => scope::UI_DIAGNOSTIC_INFO,
        DiagnosticSeverity::HINT => scope::UI_DIAGNOSTIC_HINT,
        _ => scope::UI_DIAGNOSTIC_ERROR,
    }
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

pub(crate) fn mode_segment(
    mode: &str,
    theme: &crate::theme::Theme,
    mode_badges: &std::collections::BTreeMap<String, String>,
) -> (std::borrow::Cow<'static, str>, Color) {
    use crate::theme::scope;
    let (default_label, default, legacy_scope) = match mode {
        "normal" => ("NOR", Color::Blue, scope::UI_STATUSLINE_NORMAL),
        "insert" => ("INS", Color::Green, scope::UI_STATUSLINE_INSERT),
        "select" => ("SEL", Color::Yellow, scope::UI_STATUSLINE_SELECT),
        "prompt" => ("PMT", Color::Green, scope::UI_STATUSLINE_PROMPT),
        "run" => ("RUN", Color::Magenta, scope::UI_STATUSLINE_RUN),
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
        _ => ("---", Color::Gray, scope::UI_STATUSLINE_DEFAULT),
    };
    let per_mode_scope = format!("ui.statusline.{mode}");
    let color = theme
        .try_get(&per_mode_scope)
        .and_then(|s| s.fg)
        .or_else(|| theme.try_get(legacy_scope).and_then(|s| s.fg))
        .unwrap_or(default);
    let label = match mode_badges.get(mode) {
        Some(badge) => std::borrow::Cow::Owned(badge.clone()),
        None => std::borrow::Cow::Borrowed(default_label),
    };
    (label, color)
}

/// The status-bar label and color for the foreground app `screen`, or `None`
/// for a plain editor with no screen over it.
///
/// App screens are no longer editor modes, so they are labelled separately from
/// the mode cell. Color resolves through the same `ui.statusline.<screen>`
/// scopes [`mode_segment`] used, so a theme restyles both consistently.
pub(crate) fn screen_segment(
    screen: Option<&str>,
    theme: &crate::theme::Theme,
) -> Option<(&'static str, Color)> {
    use crate::theme::scope;
    let (label, default, screen_scope) = match screen? {
        "diff" => ("review", Color::Cyan, scope::UI_STATUSLINE_REVIEW),
        "commits" => ("commits", Color::Yellow, scope::UI_STATUSLINE_COMMITS),
        "rebase" => ("rebase", Color::Red, scope::UI_STATUSLINE_REBASE),
        "reword" => ("reword", Color::Red, scope::UI_STATUSLINE_REWORD),
        "conflict" => ("conflict", Color::LightRed, scope::UI_STATUSLINE_CONFLICT),
        _ => return None,
    };
    let color = theme
        .try_get(screen_scope)
        .and_then(|s| s.fg)
        .unwrap_or(default);
    Some((label, color))
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
        View::Agent(_) => (Some("[agent]".to_string()), false, None),
        View::Terminal(_) => (Some("[term]".to_string()), false, None),
        View::Label(label) => (Some(label.clone()), false, None),
    }
}

#[cfg(test)]
mod tests {
    use crate::{action_handlers::dispatch, Stoat};
    use lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range};
    use std::path::PathBuf;
    use stoat_action::OpenFile;

    fn diag(severity: DiagnosticSeverity) -> Diagnostic {
        Diagnostic {
            range: Range {
                start: Position {
                    line: 0,
                    character: 0,
                },
                end: Position {
                    line: 0,
                    character: 1,
                },
            },
            severity: Some(severity),
            message: String::new(),
            ..Default::default()
        }
    }

    #[test]
    fn snapshot_status_bar_diagnostic_badge_warning_color() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/diag-status");
        let path = root.join("a.txt");
        h.fake_fs().insert_file(&path, b"alpha\n");
        h.stoat.active_workspace_mut().git_root = root;
        dispatch(&mut h.stoat, &OpenFile { path: path.clone() });
        h.settle();
        h.stoat
            .diagnostics
            .replace_for_path(path, vec![diag(DiagnosticSeverity::WARNING)]);
        h.assert_snapshot("status_bar_diagnostic_badge_warning_color");
    }
}
