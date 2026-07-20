use crate::{
    help::{format_arg, Help, SnapshotState},
    keymap::{collect_predicate_fields, evaluate, KeymapState, ResolvedAction, StateValue},
    render::{
        pane::mode_segment,
        text::{wrap_text, write_str, write_str_clipped},
    },
};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::Style,
    widgets::{Block, Borders, Clear, Widget},
};
use stoat_config::Predicate;

/// The on-screen rectangles of the help modal, derived from a terminal `area`
/// by [`help_layout`].
///
/// Shared by the renderer and the smooth-scroll emit so the pooled list and
/// detail regions match the painted ones exactly.
pub(crate) struct HelpLayout {
    /// The bordered modal box.
    pub(crate) modal: Rect,
    /// Inside the border: search row, status row, then the body split.
    pub(crate) inner: Rect,
    /// The entry list, also a smooth-scroll pool region.
    pub(crate) list: Rect,
    /// The selected entry's detail, also a smooth-scroll pool region.
    pub(crate) detail: Rect,
}

/// Lay out the help modal within `area`, or `None` when `area` is too small to
/// host it.
pub(crate) fn help_layout(area: Rect) -> Option<HelpLayout> {
    if area.width < 40 || area.height < 12 {
        return None;
    }

    let box_width = 120u16.min(area.width.saturating_sub(4));
    let box_height = 36u16.min(area.height.saturating_sub(4));
    if box_width < 40 || box_height < 12 {
        return None;
    }

    let x = area.x + (area.width.saturating_sub(box_width)) / 2;
    let y = area.y + (area.height.saturating_sub(box_height)) / 2;
    let modal = Rect::new(x, y, box_width, box_height);
    // The title rides the top border, so it does not shrink the inner rect.
    let inner = Block::default().borders(Borders::ALL).inner(modal);

    let body_top = inner.y + 2;
    let body_height = (inner.y + inner.height).saturating_sub(body_top);
    if body_height == 0 {
        return None;
    }

    let body_width = inner.width;
    let list_width = (body_width * 42 / 100).max(20);
    let detail_width = body_width.saturating_sub(list_width + 1);
    let list = Rect::new(inner.x, body_top, list_width, body_height);
    let detail = Rect::new(
        inner.x + list_width + 1,
        body_top,
        detail_width,
        body_height,
    );

    Some(HelpLayout {
        modal,
        inner,
        list,
        detail,
    })
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn render_help(
    help: &Help,
    stoat_mode: &str,
    ws: &mut crate::workspace::Workspace,
    theme: &crate::theme::Theme,
    mode_badges: &std::collections::BTreeMap<String, String>,
    area: Rect,
    buf: &mut Buffer,
    mut scene: Option<&mut stoatty_widgets::ApcScene>,
) {
    use crate::help::{help_input_mode, HelpInput, HelpScope};
    let input_mode = help_input_mode(stoat_mode);

    let Some(layout) = help_layout(area) else {
        return;
    };

    let title = match help.scope() {
        HelpScope::Active => format!(" help: active ({}) ", help.snapshot_mode()),
        HelpScope::All => " help: all actions ".to_string(),
    };
    let modal_style = theme.get(crate::theme::scope::UI_MODAL_HELP);
    Clear.render(layout.modal, buf);
    crate::render::chrome::modal_frame(
        buf,
        layout.modal,
        Some(title.as_str()),
        modal_style,
        theme,
        scene.as_deref_mut(),
    );

    let inner = layout.inner;
    let prompt_style = theme.get(crate::theme::scope::UI_PROMPT);
    let separator = theme.get(crate::theme::scope::UI_BORDER_INACTIVE);

    let search_row = inner.y;
    let prompt = match input_mode {
        HelpInput::Insert => "> ",
        HelpInput::Normal => ": ",
    };
    write_str(buf, inner.x, search_row, prompt, prompt_style);
    let input_area = Rect::new(inner.x + 2, search_row, inner.width.saturating_sub(2), 1);
    help.input.render(
        &mut ws.editors,
        input_area,
        true,
        "prompt",
        theme,
        mode_badges,
        buf,
    );

    let status_row = search_row + 1;
    let status_base = theme.get(crate::theme::scope::UI_STATUSBAR_FOCUSED);
    for col in inner.x..inner.x + inner.width {
        buf[(col, status_row)].set_char(' ').set_style(status_base);
    }
    let mode_str = match input_mode {
        HelpInput::Insert => "insert",
        HelpInput::Normal => "normal",
    };
    let (mode_label, mode_bg) = mode_segment(mode_str, theme, mode_badges);
    let mode_style = theme.get(crate::theme::scope::UI_MODE_LABEL).bg(mode_bg);
    let chip = format!(" {mode_label} ");
    write_str(buf, inner.x, status_row, &chip, mode_style);

    let summary = context_summary(help.context());
    if !summary.is_empty() {
        let summary_x = inner.x + chip.chars().count() as u16 + 1;
        let summary_style = status_base.patch(theme.get(crate::theme::scope::UI_TEXT_MUTED));
        write_str_clipped(
            buf,
            summary_x,
            status_row,
            &summary,
            summary_style,
            inner.x + inner.width,
        );
    }

    let list_rect = layout.list;
    let detail_rect = layout.detail;

    crate::render::chrome::vline(
        buf,
        list_rect.x + list_rect.width,
        list_rect.y,
        list_rect.height,
        separator,
        scene,
    );

    let list_scroll = help
        .selected()
        .saturating_sub(list_rect.height.saturating_sub(1) as usize);
    paint_help_list_rows(help, list_rect, list_scroll, theme, buf);
    paint_help_detail_rows(help, detail_rect, help.detail_scroll() as usize, theme, buf);
}

/// Paint help entry rows into `area` starting at `start_row`, one row per entry,
/// with the right-aligned key column, action name, and short description.
///
/// Shared by the live list, which derives `start_row` from the selection, and
/// the smooth-scroll pool, which paints absolute pages, so both render identical
/// rows.
pub(crate) fn paint_help_list_rows(
    help: &Help,
    area: Rect,
    start_row: usize,
    theme: &crate::theme::Theme,
    buf: &mut Buffer,
) {
    let row_style = theme.get(crate::theme::scope::UI_TEXT);
    let selected_style = theme.get(crate::theme::scope::UI_SELECTION);
    let key_style = theme.get(crate::theme::scope::UI_KEY_LABEL);
    let muted = theme.get(crate::theme::scope::UI_TEXT_MUTED);

    let filtered = help.filtered();
    let entries = help.entries();
    let selected = help.selected();
    let rows = area.height as usize;
    if rows == 0 {
        return;
    }

    let key_col_width: usize = filtered
        .iter()
        .skip(start_row)
        .take(rows)
        .map(|&i| {
            entries[i]
                .key_label
                .as_deref()
                .map(|s| s.chars().count())
                .unwrap_or(0)
        })
        .max()
        .unwrap_or(0)
        .min(10);

    let row_end = area.x + area.width;
    for (row_idx, &entry_idx) in filtered.iter().skip(start_row).take(rows).enumerate() {
        let row = area.y + row_idx as u16;
        let entry = &entries[entry_idx];
        let is_selected = entry_idx == *filtered.get(selected).unwrap_or(&usize::MAX);
        let base = if is_selected {
            selected_style
        } else {
            row_style
        };

        for col in area.x..row_end {
            buf[(col, row)].set_char(' ').set_style(base);
        }

        let key_text = entry.key_label.as_deref().unwrap_or("");
        let padded = format!("{key_text:>width$}", width = key_col_width);
        let key_display_style = if is_selected { base } else { key_style };
        write_str_clipped(buf, area.x + 1, row, &padded, key_display_style, row_end);
        let name_col = area.x + 1 + key_col_width as u16 + 2;
        if name_col < row_end {
            write_str_clipped(buf, name_col, row, entry.def.name(), base, row_end);
        }
        let name_w = entry.def.name().chars().count() as u16;
        let desc_col = name_col + name_w + 2;
        if desc_col < row_end {
            let desc_style = if is_selected { base } else { muted };
            write_str_clipped(
                buf,
                desc_col,
                row,
                entry.def.short_desc(),
                desc_style,
                row_end,
            );
        }
    }
}

/// Paint the selected help entry's detail into `area` starting at line
/// `start_row`: name, binding, description, parameters, and example.
///
/// Shared by the live detail pane, which derives `start_row` from
/// `detail_scroll`, and the smooth-scroll pool, which paints absolute pages.
pub(crate) fn paint_help_detail_rows(
    help: &Help,
    area: Rect,
    start_row: usize,
    theme: &crate::theme::Theme,
    buf: &mut Buffer,
) {
    let heading = theme.get(crate::theme::scope::UI_HEADING);
    let body_style = theme.get(crate::theme::scope::UI_TEXT);
    let muted = theme.get(crate::theme::scope::UI_TEXT_MUTED);
    let key_style = theme.get(crate::theme::scope::UI_KEY_LABEL);

    let Some(entry) = help.selected_entry() else {
        return;
    };
    let width = area.width.saturating_sub(1) as usize;
    if width == 0 || area.height == 0 {
        return;
    }

    let mut lines: Vec<(String, Style)> = Vec::new();
    lines.push((entry.def.name().to_string(), heading));

    let bindings = help.bindings_for(entry.def.name());
    if bindings.is_empty() {
        lines.push(("(unbound)".to_string(), muted));
    } else {
        lines.push(("Bindings:".to_string(), heading));
        for binding in bindings {
            let sequence = binding
                .actions
                .iter()
                .map(format_action)
                .collect::<Vec<_>>()
                .join(", ");
            let head = format!("  {}  {sequence}", binding.label);
            let head_style = if binding.active { key_style } else { muted };
            for wrapped in wrap_text(&head, width) {
                lines.push((wrapped, head_style));
            }
            for predicate in &binding.predicates {
                for wrapped in wrap_text(&predicate_line(predicate, help.context()), width) {
                    lines.push((wrapped, muted));
                }
            }
        }
    }

    lines.push((entry.def.short_desc().to_string(), body_style));
    lines.push((String::new(), body_style));

    for wrapped in wrap_text(entry.def.long_desc(), width) {
        lines.push((wrapped, body_style));
    }

    let params = entry.def.params();
    if !params.is_empty() {
        lines.push((String::new(), body_style));
        lines.push(("Parameters:".to_string(), heading));
        for p in params {
            let required = if p.required { "*" } else { "" };
            let head = format!("  {}{}: {}: {}", p.name, required, p.kind, p.description);
            for wrapped in wrap_text(&head, width) {
                lines.push((wrapped, body_style));
            }
        }
    }

    lines.push((String::new(), body_style));
    lines.push(("Example:".to_string(), heading));
    let example = format_example(entry);
    lines.push((format!("  {example}"), muted));

    let rows = area.height as usize;
    let end_x = area.x + area.width;
    for (row_idx, (text, style)) in lines.iter().skip(start_row).take(rows).enumerate() {
        let row = area.y + row_idx as u16;
        write_str_clipped(buf, area.x + 1, row, text, *style, end_x);
    }
}

/// Render one action of a binding's sequence as `name(arg, ...)`, or bare `name`
/// when it takes no arguments, so `AutoReload(follow)` reads distinctly from
/// `AutoReload(on)`.
fn format_action(action: &ResolvedAction) -> String {
    let args: Vec<String> = action.args.iter().filter_map(format_arg).collect();
    if args.is_empty() {
        action.name.clone()
    } else {
        format!("{}({})", action.name, args.join(", "))
    }
}

/// Render one predicate of a binding's condition. A `[x]`/`[ ]` box marks whether
/// it holds in `context`, followed by the predicate source and each field it
/// tests with its current value (`unset` when the field is absent).
fn predicate_line(predicate: &Predicate, context: &SnapshotState) -> String {
    let mark = if evaluate(predicate, context) { "[x]" } else { "[ ]" };

    let mut fields = Vec::new();
    collect_predicate_fields(predicate, &mut fields);
    fields.dedup();
    let values: Vec<String> = fields
        .iter()
        .map(|field| {
            let value = context
                .get(field)
                .map_or_else(|| "unset".to_string(), display_state_value);
            format!("{field}: {value}")
        })
        .collect();

    if values.is_empty() {
        format!("    {mark} {predicate}")
    } else {
        format!("    {mark} {predicate}   {}", values.join(", "))
    }
}

/// Render a snapshot [`StateValue`] as the text shown in the help context.
fn display_state_value(value: &StateValue) -> String {
    match value {
        StateValue::String(text) => text.to_string(),
        StateValue::Number(number) => number.to_string(),
        StateValue::Bool(flag) => flag.to_string(),
    }
}

/// Summarize the captured help context for the status row. Each set string field
/// renders as `field value` and each true boolean as the bare field, joined by
/// ` · `.
fn context_summary(context: &SnapshotState) -> String {
    let mut parts = Vec::new();
    for field in ["view", "pane", "modal", "token", "lang"] {
        if let Some(StateValue::String(value)) = context.get(field) {
            parts.push(format!("{field} {value}"));
        }
    }
    for field in ["modified", "has_selection", "diags"] {
        if matches!(context.get(field), Some(StateValue::Bool(true))) {
            parts.push(field.to_string());
        }
    }
    parts.join(" · ")
}

fn format_example(entry: &crate::help::HelpEntry) -> String {
    let name = entry.def.name();
    let params = entry.def.params();
    if params.is_empty() {
        return format!("{name}()");
    }
    if !entry.bound_args.is_empty() {
        let args: Vec<String> = entry
            .bound_args
            .iter()
            .filter_map(format_arg)
            .collect();
        return format!("{name}({})", args.join(", "));
    }
    let placeholders: Vec<String> = params.iter().map(|p| p.name.to_string()).collect();
    format!("{name}({})", placeholders.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap::ResolvedArg;
    use stoat_config::{Spanned, Value};

    fn context(fields: &[(&str, &str)]) -> SnapshotState {
        SnapshotState(
            fields
                .iter()
                .map(|(field, value)| (field.to_string(), StateValue::String((*value).into())))
                .collect(),
        )
    }

    #[test]
    fn predicate_line_marks_holds_and_annotates_the_field() {
        let predicate = Predicate::Eq(
            Spanned::new("view".to_string(), 0..0),
            Spanned::new(Value::Ident("diff".to_string()), 0..0),
        );

        assert_eq!(
            predicate_line(&predicate, &context(&[("view", "file")])),
            "    [ ] view == diff   view: file"
        );
        assert_eq!(
            predicate_line(&predicate, &context(&[("view", "diff")])),
            "    [x] view == diff   view: diff"
        );
        assert_eq!(
            predicate_line(&predicate, &context(&[])),
            "    [ ] view == diff   view: unset"
        );
    }

    #[test]
    fn format_action_renders_args_then_bare() {
        let auto = ResolvedAction {
            name: "AutoReload".to_string(),
            args: vec![ResolvedArg {
                name: None,
                value: Value::Ident("follow".to_string()),
            }],
        };
        assert_eq!(format_action(&auto), "AutoReload(follow)");

        let bare = ResolvedAction {
            name: "ToggleKeyHints".to_string(),
            args: Vec::new(),
        };
        assert_eq!(format_action(&bare), "ToggleKeyHints");
    }
}
