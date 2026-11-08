use crate::{
    help::{format_arg, Help, HelpEntry, SnapshotState},
    keymap::{KeymapState, ResolvedAction, StateValue},
    render::{
        picker::{FilterHeader, Footer, FILTER_PROMPT},
        text::{wrap_text, write_str_clipped},
    },
};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
    widgets::{Block, Borders},
};
use std::ops::Range;
use stoat_action::registry;
use unicode_width::UnicodeWidthStr;

/// Box rows the chrome takes around the body, which are the two borders, the
/// header row and the rule under it, and the footer row.
const CHROME_ROWS: u16 = 5;

/// The help footer's hints, each a bound key and the word for what it does.
const HELP_HINTS: [(&str, &str); 6] = [
    ("Enter", "run"),
    ("j/k", "move"),
    ("C-d/C-u", "scroll detail"),
    ("S-Tab", "applicable / all"),
    ("i", "search"),
    ("Esc", "list"),
];

/// Widest key a list row or a binding row keeps before it clips, so one long
/// chord does not push every name across the pane.
const KEY_COLUMNS: usize = 10;

/// Rows the help list would need for the scope's whole entry set.
///
/// Reads the unfiltered entries rather than the search result, so narrowing the
/// search never resizes the box. A scope flip rebuilds the entries and is
/// meant to resize it.
pub(crate) fn help_content_rows(help: &Help) -> u16 {
    u16::try_from(help.entries().len()).unwrap_or(u16::MAX)
}

/// The on-screen rectangles of the help modal, derived from a terminal `area`
/// by [`help_layout`].
///
/// Shared by the renderer and the smooth-scroll emit so the pooled list and
/// detail regions match the painted ones exactly.
pub(crate) struct HelpLayout {
    /// The bordered modal box.
    pub(crate) modal: Rect,
    /// Inside the border, holding the header rows, the body split, and the
    /// footer.
    pub(crate) inner: Rect,
    /// The entry list, also a smooth-scroll pool region.
    pub(crate) list: Rect,
    /// The selected entry's detail, also a smooth-scroll pool region.
    pub(crate) detail: Rect,
    /// The footer row of key hints, under the list and the detail.
    pub(crate) footer: Rect,
}

/// Lay out the help modal within `area`, or `None` when `area` is too small to
/// host it.
///
/// `content_rows` is the entries the list holds, which the box grows to fit
/// past its recommended 36 rows. It counts the list rather than the detail
/// pane's wrapped lines, since detail length tracks the selection and would
/// move the box as the user walks the list. `zoom` is the user's step count
/// from [`modal_zoom`](crate::app::Stoat::modal_zoom).
pub(crate) fn help_layout(area: Rect, content_rows: u16, zoom: i8) -> Option<HelpLayout> {
    let modal = crate::render::chrome::modal_box(
        area,
        (120, content_rows.saturating_add(CHROME_ROWS)),
        (crate::render::chrome::list_modal_width(area), 36),
        (40, 12),
        zoom,
    )?;
    let inner = Block::default().borders(Borders::ALL).inner(modal);

    let body_top = inner.y + 2;
    let footer_y = (inner.y + inner.height).saturating_sub(1);
    let body_height = footer_y.saturating_sub(body_top);
    if body_height == 0 {
        return None;
    }

    let body_width = inner.width;
    let list_width = (body_width * 52 / 100).max(20);
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
        footer: Rect::new(inner.x, footer_y, inner.width, 1),
    })
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn render_help(
    help: &Help,
    stoat_mode: &str,
    ws: &mut crate::workspace::Workspace,
    theme: &crate::theme::Theme,
    chrome: &crate::render::editor::ResolvedChrome,
    area: Rect,
    zoom: i8,
    buf: &mut Buffer,
    scene: &mut stoat_widgets::ApcScene,
) {
    use crate::help::{help_input_mode, HelpInput, HelpScope};

    let Some(layout) = help_layout(area, help_content_rows(help), zoom) else {
        return;
    };

    let modal_style = theme.get(crate::theme::scope::UI_MODAL_HELP);
    crate::render::clear_modal(layout.modal, buf, theme, modal_style);
    crate::render::chrome::modal_frame(buf, layout.modal, None, modal_style, theme, &mut *scene);

    // Normal mode walks the list with plain letter keys, so its prompt differs
    // from the one a typed search shows.
    let prompt = match help_input_mode(stoat_mode) {
        HelpInput::Insert => FILTER_PROMPT,
        HelpInput::Normal => ":",
    };
    let applicable = matches!(help.scope(), HelpScope::Active);
    crate::render::picker::filter_header(
        buf,
        layout.inner,
        &FilterHeader {
            label: "help",
            prompt,
            count: Some((help.filtered().len(), help.entries().len())),
            chips: &[("applicable", applicable), ("all", !applicable)],
        },
        &help.input,
        ws,
        theme,
        chrome,
        &mut *scene,
    );

    let list_rect = layout.list;
    crate::render::chrome::vline(
        buf,
        list_rect.x + list_rect.width,
        list_rect.y,
        list_rect.height,
        theme.get(crate::theme::scope::UI_BORDER_INACTIVE),
        scene,
    );

    let list_scroll =
        crate::render::picker::window_start(help.selected(), list_rect.height as usize);
    paint_help_list_rows(help, list_rect, list_scroll, theme, buf, scene);
    paint_help_detail_rows(
        help,
        layout.detail,
        help.detail_scroll() as usize,
        theme,
        buf,
    );

    let note = footer_note(help);
    let parts = Footer {
        hints: &HELP_HINTS,
        note: Some(&note),
        ..Footer::default()
    };
    crate::render::picker::modal_footer(buf, layout.footer, &parts, theme, scene);
}

/// The footer's note, which is the mode help opened from and the context it
/// captured there.
fn footer_note(help: &Help) -> String {
    let summary = context_summary(help.context());
    match summary.is_empty() {
        true => help.snapshot_mode().to_owned(),
        false => format!("{} \u{00B7} {summary}", help.snapshot_mode()),
    }
}

/// Paint help entry rows into `area` starting at `start_row`, one row per entry,
/// with the right-aligned key column, action name, and short description.
///
/// Each key reads bold in `ui.key_label`, and an entry with no key reads U+2014
/// in `ui.text.disabled`. The search text is marked where it occurs in each
/// name, and the short description reads in `ui.text.dim`.
///
/// Shared by the live list, which derives `start_row` from the selection, and
/// the smooth-scroll pool, which paints absolute pages, so both render identical
/// rows. The selected row's accent bar goes into `scene`.
pub(crate) fn paint_help_list_rows(
    help: &Help,
    area: Rect,
    start_row: usize,
    theme: &crate::theme::Theme,
    buf: &mut Buffer,
    scene: &mut stoat_widgets::ApcScene,
) {
    let row_style = theme.get(crate::theme::scope::UI_TEXT);
    let selected_style = theme.list_selected(crate::theme::scope::UI_SELECTION);
    let key_style = theme
        .get(crate::theme::scope::UI_KEY_LABEL)
        .add_modifier(Modifier::BOLD);
    let unbound_style = theme.get(crate::theme::scope::UI_TEXT_DISABLED);
    let desc_style = theme.get(crate::theme::scope::UI_TEXT_DIM);
    let match_style = theme.list_match();

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
                .map_or(1, |label| label.chars().count())
        })
        .max()
        .unwrap_or(0)
        .min(KEY_COLUMNS);

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
        if is_selected {
            crate::render::chrome::selection_bar(area.x, row, theme, buf, scene);
        }

        let (key_text, key_cell_style) = match entry.key_label.as_deref() {
            Some(label) => (label, key_style),
            None => ("\u{2014}", unbound_style),
        };
        let padded = format!("{key_text:>width$}", width = key_col_width);
        write_str_clipped(
            buf,
            area.x + 1,
            row,
            &padded,
            base.patch(key_cell_style),
            row_end,
        );

        let name = entry.def.name();
        let name_col = area.x + 1 + key_col_width as u16 + 2;
        if name_col < row_end {
            write_str_clipped(buf, name_col, row, name, base, row_end);
            for col in needle_span(name, help.needle()).unwrap_or_default() {
                let col = name_col + col as u16;
                if col < row_end {
                    buf[(col, row)].set_style(match_style);
                }
            }
        }
        let name_w = name.chars().count() as u16;
        let desc_col = name_col + name_w + 2;
        if desc_col < row_end {
            write_str_clipped(
                buf,
                desc_col,
                row,
                entry.def.short_desc(),
                base.patch(desc_style),
                row_end,
            );
        }
    }
}

/// The chars of `name` where `needle`, already lowercase, first occurs in the
/// name read in lowercase, or `None` for an empty needle or a name without it.
fn needle_span(name: &str, needle: &str) -> Option<Range<usize>> {
    if needle.is_empty() {
        return None;
    }
    let lower = name.to_lowercase();
    let byte = lower.find(needle)?;
    let start = lower[..byte].chars().count();
    Some(start..start + needle.chars().count())
}

/// One row of the help detail, as spans painted left to right from the pane's
/// first column, and a status that ends one cell from its right edge.
#[derive(Default)]
struct DetailLine {
    spans: Vec<(String, Style)>,
    status: Option<(&'static str, Style)>,
}

impl DetailLine {
    fn text(text: impl Into<String>, style: Style) -> Self {
        Self {
            spans: vec![(text.into(), style)],
            status: None,
        }
    }
}

/// Paint the selected help entry's detail into `area` starting at line
/// `start_row`, on the `ui.surface.raised` fill.
///
/// The detail reads as the entry's name and palette command, its short and long
/// descriptions, a `bindings` table, and its parameters. A table row is a
/// binding's key, its predicates, and whether it is live where help opened.
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
    let raised = theme.get(crate::theme::scope::UI_SURFACE_RAISED);
    for y in area.y..area.y + area.height {
        for x in area.x..area.x + area.width {
            buf[(x, y)].set_char(' ').set_style(raised);
        }
    }

    let Some(entry) = help.selected_entry() else {
        return;
    };
    let width = area.width.saturating_sub(1) as usize;
    if width == 0 || area.height == 0 {
        return;
    }

    let rows = area.height as usize;
    let end_x = area.x + area.width;
    let lines = detail_lines(help, entry, width, theme);
    for (row_idx, line) in lines.iter().skip(start_row).take(rows).enumerate() {
        let row = area.y + row_idx as u16;
        let mut spans_end = end_x;
        if let Some((status, style)) = line.status {
            let x = end_x.saturating_sub(1 + status.width() as u16);
            write_str_clipped(buf, x, row, status, style, end_x);
            spans_end = x.saturating_sub(1);
        }
        let mut x = area.x + 1;
        for (text, style) in &line.spans {
            write_str_clipped(buf, x, row, text, *style, spans_end);
            x = x.saturating_add(text.width() as u16);
        }
    }
}

/// The detail's rows for `entry`, wrapping its long description and parameters
/// at `width`.
fn detail_lines(
    help: &Help,
    entry: &HelpEntry,
    width: usize,
    theme: &crate::theme::Theme,
) -> Vec<DetailLine> {
    let text = theme.get(crate::theme::scope::UI_TEXT);
    let bold = text.add_modifier(Modifier::BOLD);

    let mut title = vec![(entry.def.name().to_owned(), bold)];
    if let Some(command) = registry::lookup(entry.def.name()) {
        title.push((
            format!(" :{}", command.command_name),
            theme.get(crate::theme::scope::UI_TEXT_MUTED),
        ));
    }
    let mut lines = vec![
        DetailLine {
            spans: title,
            status: None,
        },
        DetailLine::text(entry.def.short_desc(), text),
        DetailLine::default(),
    ];
    let dim = theme.get(crate::theme::scope::UI_TEXT_DIM);
    for wrapped in wrap_text(entry.def.long_desc(), width) {
        lines.push(DetailLine::text(wrapped, dim));
    }

    lines.push(DetailLine::default());
    lines.push(DetailLine {
        spans: vec![
            ("bindings".to_owned(), bold),
            (
                "  evaluated where help opened".to_owned(),
                theme.get(crate::theme::scope::UI_TEXT_MUTED),
            ),
        ],
        status: None,
    });
    lines.extend(binding_lines(help, entry, theme));

    let params = entry.def.params();
    if !params.is_empty() {
        lines.push(DetailLine::default());
        lines.push(DetailLine::text(
            "Parameters:",
            theme.get(crate::theme::scope::UI_HEADING),
        ));
        for p in params {
            let required = if p.required { "*" } else { "" };
            let head = format!("  {}{}: {}: {}", p.name, required, p.kind, p.description);
            for wrapped in wrap_text(&head, width) {
                lines.push(DetailLine::text(wrapped, text));
            }
        }
    }
    lines
}

/// The `bindings` table's rows for `entry`, one per binding with its key
/// right-aligned, its first predicate, and its status.
///
/// A further predicate takes a row of its own under the key column. So does a
/// binding's action sequence when it says more than the action's name, which
/// is what keeps `AutoReload(follow)` apart from `AutoReload(on)`.
fn binding_lines(help: &Help, entry: &HelpEntry, theme: &crate::theme::Theme) -> Vec<DetailLine> {
    let muted = theme.get(crate::theme::scope::UI_TEXT_MUTED);
    let bindings = help.bindings_for(entry.def.name());
    if bindings.is_empty() {
        return vec![DetailLine::text("(unbound)", muted)];
    }

    let dim = theme.get(crate::theme::scope::UI_TEXT_DIM);
    let live_key = theme
        .get(crate::theme::scope::UI_KEY_LABEL)
        .add_modifier(Modifier::BOLD);
    let off_key = theme
        .get(crate::theme::scope::UI_TEXT_DISABLED)
        .add_modifier(Modifier::BOLD);
    let live = (
        "\u{25CF} live",
        theme.get(crate::theme::scope::UI_TOGGLE_ACTIVE),
    );
    let not_here = ("\u{25CB} not here", muted);
    let key_width = bindings
        .iter()
        .map(|binding| binding.label.chars().count())
        .max()
        .unwrap_or(0)
        .min(KEY_COLUMNS);
    let indent = " ".repeat(key_width + 2);

    let mut lines = Vec::new();
    for binding in bindings {
        let (key_style, predicate_style, status) = match binding.active {
            true => (live_key, dim, live),
            false => (off_key, muted, not_here),
        };
        let mut predicates = binding
            .predicates
            .iter()
            .map(|predicate| predicate.to_string());
        lines.push(DetailLine {
            spans: vec![
                (format!("{:>key_width$}  ", binding.label), key_style),
                (predicates.next().unwrap_or_default(), predicate_style),
            ],
            status: Some(status),
        });
        for predicate in predicates {
            lines.push(DetailLine {
                spans: vec![(indent.clone(), muted), (predicate, predicate_style)],
                status: None,
            });
        }
        let says_more = binding.actions.len() > 1
            || binding.actions.iter().any(|action| !action.args.is_empty());
        if says_more {
            let sequence = binding
                .actions
                .iter()
                .map(format_action)
                .collect::<Vec<_>>()
                .join(", ");
            lines.push(DetailLine {
                spans: vec![(indent.clone(), muted), (sequence, muted)],
                status: None,
            });
        }
    }
    lines
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

/// Summarize the captured help context for the footer's note. Each set string
/// field renders as `field value` and each true boolean as the bare field,
/// joined by ` · `.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap::ResolvedArg;
    use stoat_config::Value;

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

    #[test]
    fn a_short_entry_list_keeps_the_recommended_box() {
        let laid = help_layout(Rect::new(0, 0, 200, 60), 5, 0).expect("the area hosts help");
        assert_eq!(
            laid.modal,
            Rect::new(20, 12, 160, 36),
            "content under the recommended size leaves the box at 160x36, centered"
        );
    }

    /// The box has to carry exactly the chrome its body sits under, or a list
    /// sized to fit would still scroll by however far the count is off.
    #[test]
    fn a_long_entry_list_grows_the_box_to_show_every_row() {
        let laid = help_layout(Rect::new(0, 0, 200, 60), 50, 0).expect("the area hosts help");
        assert_eq!(laid.modal.height, 55, "fifty entries plus five chrome rows");
        assert_eq!(
            laid.list.height, 50,
            "and the body then holds all fifty without scrolling"
        );
    }

    #[test]
    fn a_list_larger_than_the_area_stops_at_the_margin() {
        let laid = help_layout(Rect::new(0, 0, 200, 60), u16::MAX, 0).expect("the area hosts help");
        assert_eq!(
            laid.modal.height, 56,
            "growth stops at the area less its margin"
        );
    }

    #[test]
    fn an_area_too_small_for_the_minimum_hosts_nothing() {
        assert!(
            help_layout(Rect::new(0, 0, 41, 60), 5, 0).is_none(),
            "a width under the 40-column minimum plus its thinnest margin fails"
        );
        assert!(
            help_layout(Rect::new(0, 0, 200, 13), 5, 0).is_none(),
            "so does a height under the 12-row minimum plus its thinnest margin"
        );
    }

    /// The footer row ends in its note, which names the mode help opened from and
    /// then the context it captured there.
    #[test]
    fn the_footer_ends_in_the_mode_and_context_note() {
        let mut h = crate::test_harness::TestHarness::with_size(200, 40);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenHelp);
        let help = h.stoat.help.take().expect("help is open");
        let theme = h.stoat.theme.clone();
        let chrome = crate::render::editor::ResolvedChrome::resolve(&theme);
        let area = Rect::new(0, 0, 200, 40);
        let mut buf = Buffer::empty(area);
        render_help(
            &help,
            "insert",
            h.stoat.active_workspace_mut(),
            &theme,
            &chrome,
            area,
            0,
            &mut buf,
            &mut stoat_widgets::ApcScene::new(),
        );

        let footer = help_layout(area, help_content_rows(&help), 0)
            .expect("help fits")
            .footer;
        h.stoat.help = Some(help);
        let text: String = (footer.x..footer.x + footer.width)
            .map(|x| buf[(x, footer.y)].symbol())
            .collect();
        assert!(
            text.ends_with("normal \u{00B7} view file \u{00B7} pane editor "),
            "the footer reads {text:?}"
        );
    }
}
