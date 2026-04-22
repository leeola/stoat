use crate::{
    claude_chat::{ChatMessage, ClaudeChatState},
    render::{
        editor::render_editor,
        text::{short_path, truncate, wrap_text, write_cell, write_str},
        FrameCtx, PaneCtx,
    },
};
use ratatui::{buffer::Buffer, layout::Rect, style::Style};
use std::collections::HashMap;

pub(crate) fn render_claude_pane(
    chat: &ClaudeChatState,
    ctx: PaneCtx<'_>,
    area: Rect,
    is_focused: bool,
    frame: FrameCtx<'_>,
    buf: &mut Buffer,
) {
    use crate::{
        badge::THROBBER_FRAMES,
        claude_chat::{ChatMessageContent, ChatRole},
    };

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
    let time_style = theme.get(s::CHAT_TIME);
    write_str(buf, msg_area.x, msg_area.y, "Claude", meta_style);

    let body_area = Rect::new(
        msg_area.x,
        msg_area.y + 1,
        msg_area.width,
        msg_area.height.saturating_sub(1),
    );
    if body_area.height == 0 {
        return;
    }

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
    let body_width = body_area.width as usize;
    let mut lines: Vec<(Style, String)> = Vec::new();

    let push_block = |lines: &mut Vec<(Style, String)>, block: Vec<(Style, String)>| {
        if block.is_empty() {
            return;
        }
        if !lines.is_empty() {
            lines.push((text_style, String::new()));
        }
        lines.extend(block);
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
        match (&msg.role, &msg.content) {
            (ChatRole::User, ChatMessageContent::Text(t)) => {
                push_block(&mut lines, render_flowing(t, user_style, body_width, "> "));
            },
            (ChatRole::Assistant, ChatMessageContent::Text(t)) => {
                push_block(&mut lines, render_flowing(t, text_style, body_width, ""));
            },
            (ChatRole::Assistant, ChatMessageContent::Thinking { text }) => {
                let n = text.lines().count().max(1);
                push_block(
                    &mut lines,
                    vec![(thinking_style, format!("~ Thinking... ({n} lines)"))],
                );
            },
            (ChatRole::Assistant, ChatMessageContent::ToolUse { id, name, input }) => {
                let header = format_tool_header(name, input);
                let mut block = vec![(tool_header_style, format!("{TOOL_MARK} {header}"))];
                if let Some(content) = result_map.get(id.as_str()) {
                    let preview = format_tool_result_preview(content);
                    block.push((tool_body_style, format!("  {TOOL_RESULT_ELBOW} {preview}")));
                }
                push_block(&mut lines, block);
            },
            (ChatRole::Assistant, ChatMessageContent::ToolResult { .. }) => {},
            (ChatRole::Assistant, ChatMessageContent::Error(m)) => {
                push_block(&mut lines, vec![(error_style, format!("! {m}"))]);
            },
            (ChatRole::Assistant, ChatMessageContent::TurnComplete { duration_ms, .. }) => {
                push_block(
                    &mut lines,
                    vec![
                        (turn_sep_style, "-".repeat(body_width)),
                        (
                            time_style,
                            format!("  {:.1}s", *duration_ms as f64 / 1000.0),
                        ),
                    ],
                );
            },
            (ChatRole::User, _) => {},
        }
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
