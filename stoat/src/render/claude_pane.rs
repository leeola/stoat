use crate::{
    claude_chat::{ChatMessage, ClaudeChatState},
    render::{
        editor::render_editor,
        text::{short_path, truncate, wrap_text, write_cell, write_str},
        FrameCtx, PaneCtx,
    },
    theme::Theme,
};
use ratatui::{buffer::Buffer, layout::Rect, style::Style};
use std::collections::HashMap;

/// Restorable user-message prefix replaces the standard `"> "`
/// when [`ChatMessage::checkpoint_sha`] is set. Same width so wrap
/// math is unchanged, distinct character so the marker is
/// visually obvious.
pub(crate) const RESTORABLE_USER_PREFIX: &str = "o ";
pub(crate) const STANDARD_USER_PREFIX: &str = "> ";

/// Layout produced by [`build_chat_pane_layout`]. `lines` is the
/// flat list of styled rows that paint into the chat scrollback;
/// `message_ranges[i]` records the inclusive line-index range that
/// `chat.messages[i]` occupies, or `None` for messages that produce
/// no content.
pub(crate) struct ChatPaneLayout {
    pub(crate) lines: Vec<(Style, String)>,
    pub(crate) message_ranges: Vec<Option<(usize, usize)>>,
}

pub(crate) fn render_claude_pane(
    chat: &ClaudeChatState,
    ctx: PaneCtx<'_>,
    area: Rect,
    is_focused: bool,
    frame: FrameCtx<'_>,
    buf: &mut Buffer,
) {
    if area.height < 4 || area.width < 4 {
        return;
    }

    let PaneCtx {
        editors, buffers, ..
    } = ctx;
    let theme = frame.theme;
    let render_tick = frame.render_tick;

    let input_lines = buffers
        .get(chat.input.buffer_id)
        .map(|b| {
            let guard = b.read().expect("poisoned");
            guard.snapshot.visible_text.max_point().row + 1
        })
        .unwrap_or(1);
    let max_input = (area.height / 3).max(1);
    let input_height = (input_lines as u16).clamp(1, max_input);
    let separator_y = area.y + area.height - input_height - 1;
    let msg_area = Rect::new(area.x, area.y, area.width, separator_y - area.y);
    let input_area = Rect::new(area.x, separator_y + 1, area.width, input_height);

    use crate::theme::scope as s;
    let sep_style = theme.get(s::CHAT_SEPARATOR);
    for x in area.x..area.x + area.width {
        write_cell(buf, x, separator_y, '-', sep_style);
    }

    let meta_style = theme.get(s::CHAT_META);
    let mut header_x = msg_area.x;
    write_str(buf, header_x, msg_area.y, "Claude", meta_style);
    header_x += "Claude".chars().count() as u16;
    if chat.follow {
        const FOLLOW_BADGE: &str = " \u{25cf} follow";
        write_str(buf, header_x, msg_area.y, FOLLOW_BADGE, meta_style);
        header_x += FOLLOW_BADGE.chars().count() as u16;
    }
    if let Some(counter) = format_token_counter(&chat.usage) {
        let labeled = format!("  {counter}");
        let labeled_w = labeled.chars().count() as u16;
        if header_x + labeled_w <= msg_area.x + msg_area.width {
            write_str(buf, header_x, msg_area.y, &labeled, meta_style);
        }
    }

    let body_area = Rect::new(
        msg_area.x,
        msg_area.y + 1,
        msg_area.width,
        msg_area.height.saturating_sub(1),
    );
    if body_area.height == 0 {
        return;
    }

    let body_width = body_area.width as usize;
    let layout = build_chat_pane_layout(chat, body_width, theme, render_tick);
    let lines = &layout.lines;

    let visible_lines = body_area.height as usize;
    let skip = lines
        .len()
        .saturating_sub(visible_lines + chat.scroll_offset);
    let take = visible_lines;
    let display: Vec<_> = lines.iter().skip(skip).take(take).collect();
    let start_row = body_area.y + body_area.height.saturating_sub(display.len() as u16);
    for (i, (style, text)) in display.iter().enumerate() {
        let y = start_row + i as u16;
        let max_w = body_area.width as usize;
        for (j, ch) in text.chars().take(max_w).enumerate() {
            let x = body_area.x + j as u16;
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.set_char(ch).set_style(*style);
            }
        }
    }

    if let Some(editor) = editors.get_mut(chat.input.editor_id) {
        let input_style = if is_focused {
            theme.get(crate::theme::scope::UI_TEXT)
        } else {
            theme.get(crate::theme::scope::UI_TEXT_MUTED)
        };
        render_editor(editor, input_area, input_style, theme, buf, is_focused);
    }
}

/// Build the chat-pane scrollback layout. Pure function over the
/// chat state; used both by `render_claude_pane` (for paint) and by
/// the mouse handler in `app::Stoat::handle_claude_pane_mouse` (for
/// hit-testing screen rows back to messages).
pub(crate) fn build_chat_pane_layout(
    chat: &ClaudeChatState,
    body_width: usize,
    theme: &Theme,
    render_tick: u64,
) -> ChatPaneLayout {
    use crate::{
        badge::THROBBER_FRAMES,
        claude_chat::{ChatMessageContent, ChatRole},
        theme::scope as s,
    };

    let user_style = theme.get(s::CHAT_USER);
    let text_style = theme.get(s::CHAT_TEXT);
    let thinking_style = theme.get(s::CHAT_THINKING);
    let tool_header_style = theme.get(s::CHAT_TOOL_HEADER);
    let tool_body_style = theme.get(s::CHAT_TOOL_BODY);
    let error_style = theme.get(s::CHAT_ERROR);
    let turn_sep_style = theme.get(s::CHAT_SEPARATOR);
    let throbber_style = theme.get(s::CHAT_THROBBER);

    const TOOL_MARK: &str = "\u{23fa}";
    const TOOL_RESULT_ELBOW: &str = "\u{2514}\u{2500}";

    let result_map = build_tool_result_map(&chat.messages);
    let mut lines: Vec<(Style, String)> = Vec::new();
    let mut message_ranges: Vec<Option<(usize, usize)>> = Vec::with_capacity(chat.messages.len());

    let push_block =
        |lines: &mut Vec<(Style, String)>, block: Vec<(Style, String)>| -> Option<(usize, usize)> {
            if block.is_empty() {
                return None;
            }
            if !lines.is_empty() {
                lines.push((text_style, String::new()));
            }
            let start = lines.len();
            lines.extend(block);
            let end = lines.len() - 1;
            Some((start, end))
        };

    let render_flowing =
        |t: &str, style: Style, width: usize, prefix: &str| -> Vec<(Style, String)> {
            let inner = width.saturating_sub(prefix.chars().count());
            let mut block = Vec::new();
            let push_line = |block: &mut Vec<(Style, String)>, body: &str| {
                if body.is_empty() {
                    block.push((style, prefix.trim_end().to_string()));
                } else {
                    block.push((style, format!("{prefix}{body}")));
                }
            };
            if t.is_empty() {
                if !prefix.is_empty() {
                    push_line(&mut block, "");
                }
                return block;
            }
            for raw_line in t.lines() {
                if raw_line.trim().is_empty() {
                    push_line(&mut block, "");
                } else {
                    for wrapped in wrap_text(raw_line, inner) {
                        push_line(&mut block, &wrapped);
                    }
                }
            }
            block
        };

    for msg in &chat.messages {
        let range = match (&msg.role, &msg.content) {
            (ChatRole::User, ChatMessageContent::Text(t)) => {
                let prefix = if msg.checkpoint_sha.is_some() {
                    RESTORABLE_USER_PREFIX
                } else {
                    STANDARD_USER_PREFIX
                };
                push_block(
                    &mut lines,
                    render_flowing(t, user_style, body_width, prefix),
                )
            },
            (ChatRole::Assistant, ChatMessageContent::Text(t)) => {
                push_block(&mut lines, render_flowing(t, text_style, body_width, ""))
            },
            (ChatRole::Assistant, ChatMessageContent::Thinking { text }) => {
                let n = text.lines().count().max(1);
                push_block(
                    &mut lines,
                    vec![(thinking_style, format!("~ Thinking... ({n} lines)"))],
                )
            },
            (ChatRole::Assistant, ChatMessageContent::ToolUse { id, name, input }) => {
                let header = format_tool_header(name, input);
                let mut block = vec![(tool_header_style, format!("{TOOL_MARK} {header}"))];
                if let Some(content) = result_map.get(id.as_str()) {
                    let preview = format_tool_result_preview(content);
                    block.push((tool_body_style, format!("  {TOOL_RESULT_ELBOW} {preview}")));
                }
                push_block(&mut lines, block)
            },
            (ChatRole::Assistant, ChatMessageContent::ToolResult { .. }) => None,
            (ChatRole::Assistant, ChatMessageContent::Error(m)) => {
                push_block(&mut lines, vec![(error_style, format!("! {m}"))])
            },
            (ChatRole::Assistant, ChatMessageContent::TurnComplete { duration_ms, .. }) => {
                push_block(
                    &mut lines,
                    vec![
                        (turn_sep_style, "-".repeat(body_width)),
                        (
                            time_style_for(theme),
                            format!("  {:.1}s", *duration_ms as f64 / 1000.0),
                        ),
                    ],
                )
            },
            (ChatRole::User, _) => None,
        };
        message_ranges.push(range);
    }

    if let Some(partial) = &chat.streaming_text {
        push_block(
            &mut lines,
            render_flowing(partial, text_style, body_width, ""),
        );
    }

    if let Some(since) = chat.active_since {
        let frame = THROBBER_FRAMES[(render_tick as usize) % THROBBER_FRAMES.len()];
        let elapsed = since.elapsed().as_secs();
        let label = compute_throbber_label(&chat.messages, &result_map);
        push_block(
            &mut lines,
            vec![(throbber_style, format!("{frame} {label} ({elapsed}s)"))],
        );
    }

    ChatPaneLayout {
        lines,
        message_ranges,
    }
}

fn time_style_for(theme: &Theme) -> Style {
    theme.get(crate::theme::scope::CHAT_TIME)
}

/// Compute the body-only message area inside a claude pane's
/// rendered region, replicating the same input-height clamping that
/// [`render_claude_pane`] uses. Returns `None` when the pane is too
/// short to contain any messages. The returned rect excludes the
/// header row, the separator row, and the input area.
pub(crate) fn chat_body_area(pane_area: Rect, input_lines: u16) -> Option<Rect> {
    if pane_area.height < 4 || pane_area.width < 4 {
        return None;
    }
    let max_input = (pane_area.height / 3).max(1);
    let input_height = input_lines.clamp(1, max_input);
    if pane_area.height <= input_height + 2 {
        return None;
    }
    let separator_y = pane_area.y + pane_area.height - input_height - 1;
    let msg_area = Rect::new(
        pane_area.x,
        pane_area.y,
        pane_area.width,
        separator_y - pane_area.y,
    );
    let body_area = Rect::new(
        msg_area.x,
        msg_area.y + 1,
        msg_area.width,
        msg_area.height.saturating_sub(1),
    );
    if body_area.height == 0 {
        None
    } else {
        Some(body_area)
    }
}

/// Map a body-area screen row back to an index into `chat.messages`,
/// or `None` when the row is outside the body, on a separator
/// inserted by `push_block`, on the throbber / streaming-text
/// overlay, or simply blank. Replays the same skip / take pagination
/// `render_claude_pane` uses so the result tracks what the user
/// most recently saw.
pub(crate) fn message_at_screen_row(
    chat: &ClaudeChatState,
    body_area: Rect,
    body_width: usize,
    theme: &Theme,
    render_tick: u64,
    screen_row: u16,
) -> Option<usize> {
    if screen_row < body_area.y || screen_row >= body_area.y + body_area.height {
        return None;
    }
    let layout = build_chat_pane_layout(chat, body_width, theme, render_tick);
    let visible_lines = body_area.height as usize;
    let skip = layout
        .lines
        .len()
        .saturating_sub(visible_lines + chat.scroll_offset);
    let take = visible_lines;
    let display_count = layout.lines.len().saturating_sub(skip).min(take);
    let start_row = body_area.y + body_area.height.saturating_sub(display_count as u16);
    if screen_row < start_row {
        return None;
    }
    let display_idx = (screen_row - start_row) as usize;
    if display_idx >= display_count {
        return None;
    }
    let line_idx = skip + display_idx;
    layout
        .message_ranges
        .iter()
        .position(|range| matches!(range, Some((s, e)) if line_idx >= *s && line_idx <= *e))
}

fn format_tool_header(name: &str, input_json: &str) -> String {
    let parsed: Result<serde_json::Value, _> = serde_json::from_str(input_json);
    let Ok(value) = parsed else {
        return format!("{name}(...)");
    };
    let obj = value.as_object();

    match name {
        "Bash" => {
            if let Some(cmd) = obj.and_then(|o| o.get("command")).and_then(|v| v.as_str()) {
                return format!("Bash({})", truncate(cmd, 60));
            }
        },
        "Read" => {
            if let Some(p) = obj
                .and_then(|o| o.get("file_path"))
                .and_then(|v| v.as_str())
            {
                return format!("Read({})", short_path(p));
            }
        },
        "Edit" | "Write" | "NotebookEdit" => {
            if let Some(p) = obj
                .and_then(|o| o.get("file_path"))
                .and_then(|v| v.as_str())
            {
                return format!("{name}({})", short_path(p));
            }
        },
        "Grep" => {
            if let Some(p) = obj.and_then(|o| o.get("pattern")).and_then(|v| v.as_str()) {
                return format!("Grep({})", truncate(p, 60));
            }
        },
        "Glob" => {
            if let Some(p) = obj.and_then(|o| o.get("pattern")).and_then(|v| v.as_str()) {
                return format!("Glob({})", truncate(p, 60));
            }
        },
        _ => {},
    }

    if let Some(o) = obj {
        if let Some((k, v)) = o.iter().next() {
            let vs = match v {
                serde_json::Value::String(s) => truncate(s, 60),
                other => truncate(&other.to_string(), 60),
            };
            return format!("{name}({k}={vs})");
        }
    }
    format!("{name}(...)")
}

fn format_tool_result_preview(content: &str) -> String {
    let first = content.lines().next().unwrap_or("");
    let total_lines = content.lines().count();
    let preview = truncate(first, 80);
    if total_lines > 1 {
        format!("{preview} (+{} more lines)", total_lines - 1)
    } else {
        preview
    }
}

/// Hardcoded context window for current Claude models. Replace with a
/// per-model lookup once `ModelInfo` carries window size.
const CONTEXT_WINDOW_TOKENS: u64 = 200_000;

fn format_token_count(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{:.1}k", tokens as f64 / 1_000.0)
    } else {
        tokens.to_string()
    }
}

fn format_token_counter(usage: &crate::host::TokenUsage) -> Option<String> {
    let input_total =
        usage.input_tokens + usage.cache_creation_input_tokens + usage.cache_read_input_tokens;
    if input_total == 0 && usage.output_tokens == 0 {
        return None;
    }
    Some(format!(
        "{} / {} ctx, {} out",
        format_token_count(input_total),
        format_token_count(CONTEXT_WINDOW_TOKENS),
        format_token_count(usage.output_tokens),
    ))
}

fn build_tool_result_map(messages: &[ChatMessage]) -> HashMap<&str, &str> {
    use crate::claude_chat::ChatMessageContent;
    let mut m = HashMap::new();
    for msg in messages {
        if let ChatMessageContent::ToolResult { id, content } = &msg.content {
            m.insert(id.as_str(), content.as_str());
        }
    }
    m
}

fn compute_throbber_label(messages: &[ChatMessage], result_map: &HashMap<&str, &str>) -> String {
    use crate::claude_chat::{ChatMessageContent, ChatRole};
    for msg in messages.iter().rev() {
        if !matches!(msg.role, ChatRole::Assistant) {
            break;
        }
        match &msg.content {
            ChatMessageContent::ToolUse { id, name, .. } => {
                if !result_map.contains_key(id.as_str()) {
                    return format!("Running {name}...");
                }
            },
            ChatMessageContent::Thinking { .. } | ChatMessageContent::Text(_) => {
                return "Thinking...".to_string();
            },
            ChatMessageContent::TurnComplete { .. }
            | ChatMessageContent::ToolResult { .. }
            | ChatMessageContent::Error(_) => continue,
        }
    }
    "Thinking...".to_string()
}

#[cfg(test)]
mod tests {
    use super::{format_token_count, format_token_counter};
    use crate::host::TokenUsage;

    #[test]
    fn format_token_count_under_thousand() {
        assert_eq!(format_token_count(0), "0");
        assert_eq!(format_token_count(999), "999");
    }

    #[test]
    fn format_token_count_kilo_range() {
        assert_eq!(format_token_count(1_000), "1.0k");
        assert_eq!(format_token_count(1_500), "1.5k");
        assert_eq!(format_token_count(12_345), "12.3k");
    }

    #[test]
    fn format_token_count_mega_range() {
        assert_eq!(format_token_count(1_000_000), "1.0M");
        assert_eq!(format_token_count(2_500_000), "2.5M");
    }

    #[test]
    fn format_token_counter_skips_zero_usage() {
        assert_eq!(format_token_counter(&TokenUsage::default()), None);
    }

    #[test]
    fn format_token_counter_sums_input_variants() {
        let usage = TokenUsage {
            input_tokens: 1_000,
            output_tokens: 200,
            cache_creation_input_tokens: 500,
            cache_read_input_tokens: 300,
        };
        assert_eq!(
            format_token_counter(&usage),
            Some("1.8k / 200.0k ctx, 200 out".to_string())
        );
    }

    #[test]
    fn format_token_counter_renders_when_only_output_set() {
        let usage = TokenUsage {
            output_tokens: 50,
            ..TokenUsage::default()
        };
        assert_eq!(
            format_token_counter(&usage),
            Some("0 / 200.0k ctx, 50 out".to_string())
        );
    }
}
