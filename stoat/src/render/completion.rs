use crate::{
    app::Stoat,
    completion::{CompletionItem, CompletionItemKind, CompletionPopup},
    fuzzy,
    pane::{FocusTarget, View},
    render::{
        cursor_popup,
        paint::style_rgb,
        text::{clip_to_width, write_str, write_str_clipped},
    },
    theme::scope,
};
use nucleo::Utf32Str;
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::Style,
    widgets::{Block, Borders},
};
use std::ops::Range;
use stoat_widgets::panel::Panel;
use stoatty_protocol::command::Edge;

/// Maximum number of completion rows visible at once. Larger lists
/// scroll so the selected row stays in view.
pub(crate) const MAX_VISIBLE_ROWS: usize = 10;

/// The column a row's label starts in, past the kind badge and a space.
const LABEL_COLUMN: usize = 2;

/// The columns that keep the longest label apart from the kind names.
const KIND_GAP: usize = 2;

/// Rows the footer takes under the list, which are the hairline and the detail
/// line.
const FOOTER_ROWS: u16 = 2;

/// The corner radius of the live popup's selected row, in logical pixels.
const SELECTED_ROW_RADIUS: u8 = 4;

/// The on-screen rectangles of the completion popup, derived from `stoat` by
/// [`completion_popup_layout`], plus the scroll offset of the first visible row.
///
/// The popup is cursor-anchored, so the rects move with the cursor each frame.
/// Shared by the renderer and the smooth-scroll emit so the pooled list region
/// matches the painted one exactly.
pub(crate) struct CompletionLayout {
    /// The bordered popup rect.
    pub(crate) popup_area: Rect,
    /// The scrolling row region inside the border, which is also the pool
    /// region. It leaves out the footer's hairline and detail rows when a
    /// detail shows.
    pub(crate) inner: Rect,
    /// Index of the first visible item, the list's scroll offset.
    pub(crate) viewport_top: usize,
    /// The detail and documentation line of the selected row, painted on the
    /// footer row under the hairline below [`Self::inner`].
    ///
    /// `None` when no row is selected, when the selected row has neither, or
    /// when the box leaves no list row above the footer.
    pub(crate) detail: Option<String>,
}

/// What a computed completion geometry is only valid for.
///
/// The frame tick carries the cursor and the pane rects, which move once a
/// frame at most. The rest is the popup itself, because a caller that changes
/// the popup and emits again without a repaint asks for geometry the paint
/// never produced, and the tick alone hands it the previous answer.
///
/// The labels are not hashed. A re-query is what replaces them, and it bumps
/// the generation.
#[derive(PartialEq)]
struct LayoutKey {
    tick: u64,
    generation: u64,
    /// The popup's own geometry inputs, or `None` when no popup is open.
    popup: Option<(usize, Option<usize>, Range<usize>, usize)>,
}

/// One frame's completion geometry, so the frame's two consumers compute it
/// once between them.
///
/// The paint and the pool emit both need the same rects, and both run inside a
/// single Redraw. Producing them locks the focused buffer for the match prefix
/// and measures every visible label, which is what makes computing them twice
/// worth avoiding.
///
/// `layout` is `None` when no popup shows that frame. That answer is kept
/// rather than treated as an empty memo, since establishing it costs the second
/// consumer the same work all over again.
pub(crate) struct CompletionLayoutMemo {
    key: LayoutKey,
    pub(crate) layout: Option<(String, CompletionLayout)>,
}

/// This frame's completion geometry, computed here or handed back from the memo
/// the frame's other consumer parked.
///
/// Handed over by value rather than borrowed, because the pool emit holds `&mut`
/// to the pool state while it reads the rects. Park it back with
/// [`park_layout`] when a later consumer of the same frame still needs it.
pub(crate) fn frame_layout(stoat: &mut Stoat) -> CompletionLayoutMemo {
    let key = layout_key(stoat);
    match stoat.completion_layout.take() {
        Some(memo) if memo.key == key => memo,
        _ => CompletionLayoutMemo {
            key,
            layout: completion_popup_layout(stoat),
        },
    }
}

/// Leave `memo` for the rest of the frame to read.
pub(crate) fn park_layout(stoat: &mut Stoat, memo: CompletionLayoutMemo) {
    stoat.completion_layout = Some(memo);
}

fn layout_key(stoat: &Stoat) -> LayoutKey {
    LayoutKey {
        tick: stoat.render_tick,
        generation: stoat.completion_generation,
        popup: stoat.pending_completion.as_ref().map(|p| {
            (
                p.anchor_offset,
                p.selected_idx,
                p.prefix_range.clone(),
                p.len(),
            )
        }),
    }
}

/// Compute the anchored completion popup geometry, returning its match prefix
/// and the [`CompletionLayout`], or `None` when no popup should show.
///
/// The popup itself is not returned. Callers re-borrow
/// [`Stoat::pending_completion`] for the item rows, so the list is never cloned
/// here. `None` mirrors every [`render_completion`] bail: no pending completion
/// or empty items, the focused pane is not an editor, the cursor is off-screen,
/// or the interior width collapses to zero.
pub(crate) fn completion_popup_layout(stoat: &mut Stoat) -> Option<(String, CompletionLayout)> {
    #[cfg(test)]
    stoat
        .completion_layouts
        .set(stoat.completion_layouts.get() + 1);

    // Read the scalars the geometry needs and drop the popup borrow before the
    // mutable workspace access below. The item list is re-borrowed for the width
    // scan once that work is done, so the popup is never cloned.
    let (anchor_offset, selected_idx, prefix_range) = match &stoat.pending_completion {
        Some(p) if !p.items.is_empty() => (p.anchor_offset, p.selected_idx, p.prefix_range.clone()),
        _ => return None,
    };

    let prefix = extract_prefix(stoat, prefix_range);

    let (content_area, cursor_screen) =
        cursor_popup::focused_editor_popup_ctx(stoat, anchor_offset)?;

    let interior_width = content_area.width.saturating_sub(2);
    if interior_width == 0 {
        return None;
    }

    let popup = stoat.pending_completion.as_ref()?;
    let total = popup.len();
    let viewport_top = viewport_top_for(selected_idx.unwrap_or(0), total, MAX_VISIBLE_ROWS);
    let visible_count = total.saturating_sub(viewport_top).min(MAX_VISIBLE_ROWS);

    // Counted rather than truncated, since the width is all these want and a
    // truncation allocates a string per visible row to hand it over.
    let clipped_width = |text: &str| text.chars().take(interior_width as usize).count();

    let (label_width, name_width) =
        popup
            .rows()
            .skip(viewport_top)
            .take(visible_count)
            .fold((0, 0), |(label, name), item| {
                let item_name = kind_name(item).chars().count();
                (label.max(clipped_width(&item.label)), name.max(item_name))
            });
    let name_room = match name_width {
        0 => 0,
        width => KIND_GAP + width,
    };
    let row_width = (LABEL_COLUMN + label_width + name_room).min(interior_width as usize) as u16;
    let detail = selected_idx
        .and_then(|row| popup.row(row))
        .and_then(detail_footer);
    let detail_width = detail.as_deref().map(clipped_width).unwrap_or(0) as u16;
    let footer_rows = if detail.is_some() { FOOTER_ROWS } else { 0 };

    // Completion prefers below, unlike the other cursor popups. It appears
    // mid-typing, so covering the line above the cursor hides what was just
    // written.
    let popup_area = cursor_popup::popup_rect(
        content_area,
        cursor_screen,
        (
            row_width.max(detail_width),
            visible_count as u16 + footer_rows,
        ),
        false,
    );
    let full_inner = Block::default().borders(Borders::ALL).inner(popup_area);
    // A box clamped to the footer's height or less keeps its rows for the list.
    let detail = detail.filter(|_| full_inner.height > FOOTER_ROWS);
    let footer_rows = if detail.is_some() { FOOTER_ROWS } else { 0 };
    let inner = Rect {
        height: full_inner.height - footer_rows,
        ..full_inner
    };

    Some((
        prefix,
        CompletionLayout {
            popup_area,
            inner,
            viewport_top,
            detail,
        },
    ))
}

/// The footer line for a completion row, joining its detail with the first
/// line of its documentation. `None` when it has neither. Painted under the
/// hairline below the list by [`render_completion`].
fn detail_footer(item: &CompletionItem) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    if let Some(detail) = item.detail.as_deref().filter(|d| !d.is_empty()) {
        parts.push(detail);
    }
    if let Some(first) = item
        .documentation
        .as_deref()
        .and_then(|doc| doc.lines().next())
        .filter(|line| !line.is_empty())
    {
        parts.push(first);
    }
    (!parts.is_empty()).then(|| parts.join("  "))
}

/// Paint the completion popup, if any, anchored to the focused
/// editor's primary cursor. Renders below the cursor when there is
/// room below, otherwise above. Truncates labels that exceed the
/// popup's interior width and clamps height to the focused pane.
///
/// No-op when [`Stoat::pending_completion`] is `None` or empty,
/// when the focused pane is not an editor, when the cursor is
/// off-screen, or when neither the popup width nor height fits.
/// Returns whether it painted the popup.
pub(crate) fn render_completion(
    stoat: &mut Stoat,
    buf: &mut Buffer,
    scene: &mut stoat_widgets::ApcScene,
) -> bool {
    // Parked back before every return, since the pool emit runs after this and
    // reads what the frame's first consumer left rather than laying out again.
    let memo = frame_layout(stoat);
    let Some((prefix, layout)) = &memo.layout else {
        park_layout(stoat, memo);
        return false;
    };
    // The layout confirmed a non-empty popup. Re-borrow it for the rows instead
    // of cloning the item list through the layout call.
    let Some(popup) = stoat.pending_completion.as_ref() else {
        park_layout(stoat, memo);
        return false;
    };

    let modal_style = stoat.theme.get(scope::UI_MODAL_HINTS);
    crate::render::clear_modal(layout.popup_area, buf, &stoat.theme, modal_style);
    crate::render::chrome::modal_frame(
        buf,
        layout.popup_area,
        None,
        modal_style,
        &stoat.theme,
        scene,
    );

    paint_completion_rows(
        popup,
        popup.selected_idx,
        prefix,
        layout.viewport_top,
        layout.inner,
        &stoat.theme,
        true,
        buf,
        scene,
    );

    if let Some(detail) = &layout.detail {
        let inner = layout.inner;
        let rule_y = inner.y + inner.height;
        let rule_style = stoat.theme.get(scope::UI_BORDER_INACTIVE);
        crate::render::chrome::hline(buf, inner.x, rule_y, inner.width, rule_style, scene);

        let footer_y = rule_y + 1;
        let footer_style = modal_style.patch(stoat.theme.get(scope::UI_MODAL_FOOTER));
        let end = inner.x + inner.width;
        for col in inner.x..end {
            buf[(col, footer_y)].set_char(' ').set_style(footer_style);
        }
        let text_style = footer_style.patch(stoat.theme.get(scope::UI_TEXT_DIM));
        write_str_clipped(buf, inner.x, footer_y, detail, text_style, end);
    }

    park_layout(stoat, memo);
    true
}

/// Paint completion rows into `area` starting at item `start_row`, one row per
/// item, highlighting the selected row and the characters matching `prefix`.
///
/// A row leads with its kind badge from [`kind_glyph`] and sets its label from
/// [`LABEL_COLUMN`], clipped to the area. Its [`kind_name`] ends the row in
/// `ui.text.muted` when the label leaves room for it.
///
/// Shared by the live popup, which derives `start_row` from the viewport, and
/// the smooth-scroll pool, which paints absolute pages, so both render
/// identical rows.
///
/// With `panels`, a live scene, and an RGB selection fill, the selected row is
/// a box with rounded corners across the row in that fill, and a badge's box
/// goes through [`chip_cells`](crate::render::chrome::chip_cells) over it.
/// Otherwise the selected row's cells take the selection fill and the accent
/// bar marks it, as a plain terminal draws it. A pool page captures no panel,
/// so a page passes `false` and paints the badge's box into its cell.
#[allow(clippy::too_many_arguments)]
pub(crate) fn paint_completion_rows(
    popup: &CompletionPopup,
    selected_idx: Option<usize>,
    prefix: &str,
    start_row: usize,
    area: Rect,
    theme: &crate::theme::Theme,
    panels: bool,
    buf: &mut Buffer,
    scene: &mut stoat_widgets::ApcScene,
) {
    // The box style's foreground is its border color, so an unselected row
    // takes the text color over it.
    let text_style = theme
        .get(scope::UI_MODAL_HINTS)
        .patch(theme.get(scope::UI_TEXT));
    let selected_style = theme.list_selected(scope::UI_SELECTION);
    let match_style = theme.list_match();
    let name_style = theme.get(scope::UI_TEXT_MUTED);

    let pattern = fuzzy::parse_query(prefix);
    let mut hay_buf: Vec<char> = Vec::new();
    let mut indices_buf: Vec<u32> = Vec::new();

    // Taken once for the whole window rather than per row, the matcher's
    // scratch buffers being what makes reuse worth anything.
    fuzzy::with_matcher(|matcher| {
        let width = area.width as usize;
        for row_idx in 0..area.height {
            let item_idx = start_row + row_idx as usize;
            let Some(item) = popup.row(item_idx) else {
                break;
            };
            let row = area.y + row_idx;
            let selected = selected_idx == Some(item_idx);
            let row_fill =
                style_rgb(selected_style.bg).filter(|_| selected && panels && scene.live());
            let (cell_style, row_style) = match (selected, row_fill) {
                (true, Some(_)) => (
                    text_style,
                    Style {
                        bg: text_style.bg,
                        ..selected_style
                    },
                ),
                (true, None) => (selected_style, selected_style),
                (false, _) => (text_style, text_style),
            };
            for col in area.x..area.x + area.width {
                buf[(col, row)].set_char(' ').set_style(cell_style);
            }
            // A box hides the chrome sent before it in its rect, so the row's
            // box goes out first, then the badge's box over it, and then the
            // selection bar that shares the badge's cell.
            if let Some(fill) = row_fill {
                Panel {
                    corner_radius: SELECTED_ROW_RADIUS,
                    edges: Edge::symmetric(0, 0),
                    ..Panel::chip(fill)
                }
                .draw_components(Rect::new(area.x, row, area.width, 1), scene);
            }
            if let Some((glyph, kind_scope)) = kind_glyph(item.kind).filter(|_| width > 0) {
                let badge = match panels {
                    true => crate::render::chrome::chip_cells(
                        Rect::new(area.x, row, 1, 1),
                        theme.get(kind_scope),
                        scene,
                    ),
                    false => theme.get(kind_scope),
                };
                write_str(buf, area.x, row, glyph, row_style.patch(badge));
            }
            if selected && row_fill.is_none() {
                crate::render::chrome::selection_bar(area.x, row, theme, buf, scene);
            }

            let label = clip_to_width(&item.label, width.saturating_sub(LABEL_COLUMN));
            indices_buf.clear();
            if let Some(p) = &pattern {
                let hay = Utf32Str::new(label, &mut hay_buf);
                p.indices(hay, matcher, &mut indices_buf);
            }
            for (col_idx, ch) in label.chars().enumerate() {
                let col = area.x + (LABEL_COLUMN + col_idx) as u16;
                // A match style with no background keeps the row's own fill, so
                // the selected row stays whole under a matched char.
                let style = if indices_buf.contains(&(col_idx as u32)) {
                    row_style.patch(match_style)
                } else {
                    row_style
                };
                buf[(col, row)].set_char(ch).set_style(style);
            }

            let name = kind_name(item);
            let name_width = name.chars().count();
            let label_end = LABEL_COLUMN + label.chars().count();
            if name_width > 0 && label_end + KIND_GAP + name_width <= width {
                let name_x = area.x + (width - name_width) as u16;
                write_str(buf, name_x, row, name, row_style.patch(name_style));
            }
        }
    });
}

/// The number of rows a half-page key moves the selection of `popup`.
///
/// Half of the rows the popup shows at once, and at least one, as the Helix
/// menu moves half of its own height.
pub(crate) fn half_page(popup: &CompletionPopup) -> usize {
    (popup.len().min(MAX_VISIBLE_ROWS) / 2).max(1)
}

/// The glyph and the theme scope of the badge that leads a row of `kind`.
///
/// Returns [`None`] for a kind with no badge, which are files, folders,
/// snippets, the catch-all kind, and an item of no kind. The row then leaves
/// its badge cell blank.
fn kind_glyph(kind: Option<CompletionItemKind>) -> Option<(&'static str, &'static str)> {
    let badge = match kind? {
        CompletionItemKind::Function | CompletionItemKind::Method => {
            ("f", scope::UI_COMPLETION_KIND_FUNCTION)
        },
        CompletionItemKind::Constant
        | CompletionItemKind::Enum
        | CompletionItemKind::Class
        | CompletionItemKind::Struct => ("c", scope::UI_COMPLETION_KIND_CONSTANT),
        CompletionItemKind::Module => ("m", scope::UI_COMPLETION_KIND_MODULE),
        CompletionItemKind::Variable | CompletionItemKind::Field => {
            ("v", scope::UI_COMPLETION_KIND_VARIABLE)
        },
        CompletionItemKind::Keyword => ("k", scope::UI_COMPLETION_KIND_KEYWORD),
        CompletionItemKind::File
        | CompletionItemKind::Folder
        | CompletionItemKind::Snippet
        | CompletionItemKind::Other => return None,
    };
    Some(badge)
}

/// The word that ends a row of `item`, such as `fn` for a function or the type
/// of a constant.
///
/// Most servers lead an item's detail with that word, so the word is the
/// detail's leading run of letters, digits, and underscores. A detail of
/// `fn(&self) -> Rect` gives `fn`. An item whose detail leads with no such run
/// takes its kind's name. The catch-all kind and an item of no kind give an
/// empty word.
fn kind_name(item: &CompletionItem) -> &str {
    let detail = item.detail.as_deref().unwrap_or_default();
    let word_end = detail
        .find(|ch: char| !(ch.is_alphanumeric() || ch == '_'))
        .unwrap_or(detail.len());
    if word_end > 0 {
        return &detail[..word_end];
    }

    match item.kind {
        Some(CompletionItemKind::Function) => "function",
        Some(CompletionItemKind::Method) => "method",
        Some(CompletionItemKind::Variable) => "variable",
        Some(CompletionItemKind::Field) => "field",
        Some(CompletionItemKind::Module) => "module",
        Some(CompletionItemKind::Class) => "class",
        Some(CompletionItemKind::Struct) => "struct",
        Some(CompletionItemKind::Enum) => "enum",
        Some(CompletionItemKind::Constant) => "constant",
        Some(CompletionItemKind::Keyword) => "keyword",
        Some(CompletionItemKind::File) => "file",
        Some(CompletionItemKind::Folder) => "folder",
        Some(CompletionItemKind::Snippet) => "snippet",
        Some(CompletionItemKind::Other) | None => "",
    }
}

fn extract_prefix(stoat: &Stoat, prefix_range: Range<usize>) -> String {
    let ws = stoat.active_workspace();
    let FocusTarget::SplitPane = ws.focus else {
        return String::new();
    };
    let pane_id = ws.panes.focus();
    let pane = ws.panes.pane(pane_id);
    let View::Editor(editor_id) = pane.view else {
        return String::new();
    };
    let Some(editor) = ws.editors.get(editor_id) else {
        return String::new();
    };
    let Some(buffer) = ws.buffers.get(editor.buffer_id) else {
        return String::new();
    };
    let guard = match buffer.read() {
        Ok(g) => g,
        Err(_) => return String::new(),
    };
    let rope = guard.rope();
    let len = rope.len();
    let start = prefix_range.start.min(len);
    let end = prefix_range.end.min(len);
    if start >= end {
        return String::new();
    }
    rope.slice(start..end).to_string()
}

fn viewport_top_for(selected: usize, total: usize, window: usize) -> usize {
    if total <= window {
        return 0;
    }
    let max_top = total - window;
    if selected < window {
        0
    } else {
        (selected + 1).saturating_sub(window).min(max_top)
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        apc_emit,
        completion::{CompletionItem, CompletionItemKind, CompletionPopup, CompletionSource},
        test_fixture::open_scratch_file,
        test_harness::TestHarness,
        theme::{scope, Theme},
    };
    use ratatui::{buffer::Buffer, layout::Rect, style::Color};
    use stoat_widgets::ApcScene;
    use stoatty_protocol::command::{
        decode_stream, BorderStyle, Command, Edge, PanelCommand, PanelShadow,
    };

    fn make_item(label: &str) -> CompletionItem {
        CompletionItem {
            label: label.into(),
            source: CompletionSource::Lsp,
            kind: None,
            detail: None,
            replace_range: crate::completion::unused_replace_range(),
            insert_text: label.into(),
            is_snippet: false,
            documentation: None,
            lsp_item: None,
            server: None,
        }
    }

    /// The paint and the pool emit both need the popup's rects, and producing
    /// them locks the focused buffer to read the match prefix and measures
    /// every visible label. Both run inside one Redraw, so the second reads
    /// what the first left rather than repeating that work.
    #[test]
    fn a_frame_lays_the_popup_out_once() {
        let mut h = TestHarness::with_size(40, 24);
        let _path = open_scratch_file(&mut h, "");
        h.type_keys("i");
        h.stoat.pending_completion = Some(CompletionPopup {
            selected_idx: Some(0),
            ..CompletionPopup::showing(vec![make_item("alpha"), make_item("beta")])
        });

        // The pool emit is the second consumer, and it ships nothing without a
        // stoatty listening, so without these it never asks for a layout at all.
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_apc_tx(tx);
        h.stoat.stoatty = true;

        h.stoat.completion_layouts.set(0);
        h.stoat.render();
        apc_emit::emit_smooth_scroll(&mut h.stoat);

        assert_eq!(
            h.stoat.completion_layouts.get(),
            1,
            "the paint and the emit share one layout",
        );

        h.stoat.render();
        apc_emit::emit_smooth_scroll(&mut h.stoat);

        assert_eq!(
            h.stoat.completion_layouts.get(),
            2,
            "and the next frame lays out again rather than reusing a stale one",
        );
    }

    /// Every other cursor popup prefers the space above the cursor, and
    /// completion is the one that prefers below. The snapshots all anchor at
    /// offset 0, where there is no room above and both preferences agree, so
    /// none of them would notice the difference.
    #[test]
    fn the_popup_opens_below_the_cursor_when_either_side_would_fit() {
        let mut h = TestHarness::with_size(40, 24);
        let _path = open_scratch_file(&mut h, &"line\n".repeat(20));
        h.type_keys("i");
        // Row 10 of a 20-line buffer on a 24-row terminal, so a four-row popup
        // has room on either side and only the preference decides.
        let anchor_offset = "line\n".len() * 10;
        h.stoat.pending_completion = Some(CompletionPopup {
            anchor_offset,
            prefix_range: anchor_offset..anchor_offset,
            ..CompletionPopup::showing(vec![make_item("alpha"), make_item("beta")])
        });

        let (_, cursor) =
            crate::render::cursor_popup::focused_editor_popup_ctx(&mut h.stoat, anchor_offset)
                .expect("the anchor resolves to a cell on screen");
        let (_, layout) = super::completion_popup_layout(&mut h.stoat).expect("the popup lays out");

        assert!(
            layout.popup_area.y > cursor.1,
            "the popup starts under cursor row {}, not at {}",
            cursor.1,
            layout.popup_area.y
        );
    }

    #[test]
    fn snapshot_popup_basic_three_items_selected_middle() {
        let mut h = TestHarness::with_size(40, 12);
        let _path = open_scratch_file(&mut h, "");
        h.type_keys("i");
        h.stoat.pending_completion = Some(CompletionPopup {
            selected_idx: Some(1),
            anchor_offset: 0,
            prefix_range: 0..0,
            ..CompletionPopup::showing(vec![
                make_item("println"),
                make_item("print"),
                make_item("panic"),
            ])
        });
        h.assert_snapshot("snapshot_completion_popup_basic");
    }

    #[test]
    fn snapshot_popup_shows_detail_footer_for_selected_row() {
        let mut h = TestHarness::with_size(40, 12);
        let _path = open_scratch_file(&mut h, "");
        h.type_keys("i");
        let mut foo = make_item("foo");
        foo.detail = Some("fn foo() -> u32".into());
        foo.documentation = Some("Returns the foo.\nMore details.".into());
        h.stoat.pending_completion = Some(CompletionPopup {
            selected_idx: Some(0),
            ..CompletionPopup::showing(vec![foo, make_item("bar")])
        });
        h.assert_snapshot("snapshot_completion_popup_detail_footer");
    }

    #[test]
    fn snapshot_popup_scrolls_when_selected_past_window() {
        let mut h = TestHarness::with_size(40, 16);
        let _path = open_scratch_file(&mut h, "");
        h.type_keys("i");
        let items: Vec<CompletionItem> = (0..15)
            .map(|i| make_item(&format!("item_{i:02}")))
            .collect();
        h.stoat.pending_completion = Some(CompletionPopup {
            selected_idx: Some(12),
            ..CompletionPopup::showing(items)
        });
        h.assert_snapshot("snapshot_completion_popup_scrolling");
    }

    #[test]
    fn snapshot_popup_highlights_matched_chars() {
        let mut h = TestHarness::with_size(40, 12);
        let _path = open_scratch_file(&mut h, "pri");
        h.type_keys("A");
        h.stoat.pending_completion = Some(CompletionPopup {
            selected_idx: Some(0),
            anchor_offset: 3,
            prefix_range: 0..3,
            ..CompletionPopup::showing(vec![
                make_item("println"),
                make_item("print"),
                make_item("panic"),
            ])
        });
        h.assert_snapshot("snapshot_completion_popup_with_match");
    }

    #[test]
    fn no_popup_state_renders_no_paint_block() {
        let mut h = TestHarness::with_size(40, 6);
        let _path = open_scratch_file(&mut h, "");
        h.type_keys("i");
        assert!(h.stoat.pending_completion.is_none());
        h.assert_snapshot("snapshot_completion_popup_absent");
    }

    /// The badge cell of each kind class as (glyph, foreground, background),
    /// with an item of a kind that has no badge last, and the panels the rows
    /// draw.
    ///
    /// A badge cell keeps the box's background, and its box is a panel in the
    /// kind's tint. The selected first row is a box across the row in the
    /// selection fill with 4 px corners and no accent bar. It goes out ahead of
    /// the first badge's box, which shows over it.
    #[test]
    fn each_kind_class_leads_its_row_with_its_badge() {
        let kinds = [
            CompletionItemKind::Function,
            CompletionItemKind::Struct,
            CompletionItemKind::Module,
            CompletionItemKind::Field,
            CompletionItemKind::Keyword,
            CompletionItemKind::Snippet,
        ];
        let items = kinds.map(|kind| CompletionItem {
            kind: Some(kind),
            ..make_item("x")
        });
        let (buf, inner, h) = painted(items.to_vec());

        let badges: Vec<(&str, Color, Color)> = (inner.y..inner.y + kinds.len() as u16)
            .map(|y| {
                let cell = &buf[(inner.x, y)];
                (cell.symbol(), cell.fg, cell.bg)
            })
            .collect();
        let scene = decode_stream(&h.stoat.apc_scene.all_bytes());
        let panels: Vec<PanelCommand> = scene
            .iter()
            .filter_map(|command| match command {
                Command::Panel(panel) if (panel.left, panel.height) == (inner.x, 1) => Some(*panel),
                _ => None,
            })
            .collect();
        let (x, y) = ((inner.x * 16) as i16, (inner.y * 16) as i16);
        let selection_bar = scene.iter().any(|command| {
            matches!(command, Command::Bar(bar) if (bar.x, bar.y, bar.height) == (x, y, 16))
        });
        let bytes = |hex: u32| [(hex >> 16) as u8, (hex >> 8) as u8, hex as u8];
        let rgb = |hex: u32| {
            let [r, g, b] = bytes(hex);
            Color::Rgb(r, g, b)
        };
        let panel = |row: u16, width: u16, hex: u32, edges: [Edge; 4]| PanelCommand {
            top: inner.y + row,
            left: inner.x,
            width,
            height: 1,
            style: BorderStyle::None_,
            border: bytes(hex),
            corner_radius: 4,
            fill: Some(bytes(hex)),
            shadow: PanelShadow::None_,
            edges,
            corners: 0,
            fillets: 0,
            fillet_radius: 2,
            hatch: None,
            above_pools: false,
            backdrop: false,
            anchor: None,
        };
        let chip = |row: u16, hex: u32| panel(row, 1, hex, Edge::symmetric(0, 1));
        assert_eq!(
            (badges, panels, selection_bar),
            (
                vec![
                    ("f", rgb(0x7fb0ff), rgb(0x252830)),
                    ("c", rgb(0x6cc7d6), rgb(0x252830)),
                    ("m", rgb(0xc59af0), rgb(0x252830)),
                    ("v", rgb(0xa3a9b5), rgb(0x252830)),
                    ("k", rgb(0xe8bd6a), rgb(0x252830)),
                    (" ", rgb(0xdfe2e8), rgb(0x252830)),
                ],
                vec![
                    panel(0, inner.width, 0x23324a, Edge::symmetric(0, 0)),
                    chip(0, 0x223049),
                    chip(1, 0x1e3237),
                    chip(2, 0x2a2236),
                    chip(3, 0x2c3039),
                    chip(4, 0x2c3039),
                ],
                false,
            ),
        );
    }

    /// A pool page captures no panel, so a completion page paints its badge's
    /// box into the badge's cell and emits no panel.
    #[test]
    fn a_completion_page_paints_the_badge_box_into_its_cell() {
        let theme = {
            let (config, _) = stoat_config::parse(
                r##"theme t { ui.completion.kind.function = { fg: "#010203", bg: "#040506" }; }"##,
            );
            Theme::from_config(&config.expect("theme parses"), "t").expect("theme loads")
        };
        let popup = CompletionPopup::showing(vec![CompletionItem {
            kind: Some(CompletionItemKind::Function),
            ..make_item("x")
        }]);
        let page =
            crate::smooth_scroll::render_completion_page(&popup, Some(0), "", 0, &theme, 10, 1);

        let panels = decode_stream(&page)
            .into_iter()
            .filter(|command| matches!(command, Command::Panel(_)))
            .count();
        let tinted = page.windows(10).any(|bytes| bytes == b"48;2;4;5;6");
        assert_eq!(
            (panels, tinted),
            (0, true),
            "the badge's cell carries the tint, and the page sends no panel"
        );
    }

    /// A row ends in the detail's leading word, or else in its kind's name, and
    /// the box holds the longest label and the longest word two columns apart.
    /// The footer under them fills its whole row, past a shorter detail.
    #[test]
    fn a_row_ends_in_the_word_that_names_its_kind() {
        let item = |label: &str, kind, detail: Option<&str>| CompletionItem {
            kind,
            detail: detail.map(str::to_owned),
            ..make_item(label)
        };
        let (buf, inner, _) = painted(vec![
            item(
                "modal_frame",
                Some(CompletionItemKind::Function),
                Some("fn(&self) -> Rect"),
            ),
            item(
                "MODAL_MARGIN",
                Some(CompletionItemKind::Constant),
                Some("size_t"),
            ),
            item("modals", Some(CompletionItemKind::Module), None),
            item("word", None, None),
        ]);

        let rows: Vec<String> = (inner.y..inner.y + inner.height)
            .map(|y| {
                (inner.x..inner.x + inner.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect()
            })
            .collect();
        assert_eq!(
            rows,
            [
                "f modal_frame       fn",
                "c MODAL_MARGIN  size_t",
                "m modals        module",
                "  word                ",
            ]
        );

        let footer_y = inner.y + inner.height + 1;
        let footer: Vec<Color> = (inner.x..inner.x + inner.width)
            .map(|x| buf[(x, footer_y)].bg)
            .collect();
        assert_eq!(
            footer,
            [Color::Rgb(0x1e, 0x21, 0x27); 22],
            "the raised footer fills the row past the shorter detail"
        );
    }

    /// Every kind's badge, and the name a row of it ends in when its detail
    /// gives no word.
    #[test]
    fn each_kind_takes_its_class_badge_and_its_name() {
        let kinds = [
            CompletionItemKind::Function,
            CompletionItemKind::Method,
            CompletionItemKind::Variable,
            CompletionItemKind::Field,
            CompletionItemKind::Module,
            CompletionItemKind::Class,
            CompletionItemKind::Struct,
            CompletionItemKind::Enum,
            CompletionItemKind::Constant,
            CompletionItemKind::Keyword,
            CompletionItemKind::File,
            CompletionItemKind::Folder,
            CompletionItemKind::Snippet,
            CompletionItemKind::Other,
        ];
        let items = kinds.map(|kind| CompletionItem {
            kind: Some(kind),
            ..make_item("x")
        });
        let table: Vec<(Option<(&str, &str)>, &str)> = items
            .iter()
            .map(|item| (super::kind_glyph(item.kind), super::kind_name(item)))
            .collect();

        let f = Some(("f", scope::UI_COMPLETION_KIND_FUNCTION));
        let v = Some(("v", scope::UI_COMPLETION_KIND_VARIABLE));
        let m = Some(("m", scope::UI_COMPLETION_KIND_MODULE));
        let c = Some(("c", scope::UI_COMPLETION_KIND_CONSTANT));
        let k = Some(("k", scope::UI_COMPLETION_KIND_KEYWORD));
        assert_eq!(
            table,
            [
                (f, "function"),
                (f, "method"),
                (v, "variable"),
                (v, "field"),
                (m, "module"),
                (c, "class"),
                (c, "struct"),
                (c, "enum"),
                (c, "constant"),
                (k, "keyword"),
                (None, "file"),
                (None, "folder"),
                (None, "snippet"),
                (None, ""),
            ]
        );
    }

    /// A row narrower than its content drops its name first, then clips its
    /// label, so the name never runs into the label.
    #[test]
    fn a_narrow_row_drops_its_name_and_then_clips_its_label() {
        let popup = CompletionPopup::showing(vec![CompletionItem {
            kind: Some(CompletionItemKind::Function),
            ..make_item("abcdefghij")
        }]);
        let theme = {
            let (config, _) = stoat_config::parse("theme t { }");
            Theme::from_config(&config.expect("theme parses"), "t").expect("theme loads")
        };
        let row = |width: u16| -> String {
            let area = Rect::new(0, 0, width, 1);
            let mut buf = Buffer::empty(area);
            let mut scene = ApcScene::new();
            super::paint_completion_rows(
                &popup,
                Some(0),
                "",
                0,
                area,
                &theme,
                true,
                &mut buf,
                &mut scene,
            );
            (0..width).map(|x| buf[(x, 0)].symbol()).collect()
        };
        assert_eq!(
            [row(8), row(20), row(22)],
            ["f abcdef", "f abcdefghij        ", "f abcdefghij  function"]
        );
    }

    /// A box clamped to the footer's height keeps its rows for the list.
    #[test]
    fn a_box_too_short_for_the_footer_drops_it() {
        let mut foo = make_item("foo");
        foo.detail = Some("fn foo() -> u32".into());
        let mut h = TestHarness::with_size(40, 5);
        let _path = open_scratch_file(&mut h, "");
        h.type_keys("i");
        h.stoat.pending_completion = Some(CompletionPopup {
            selected_idx: Some(0),
            ..CompletionPopup::showing(vec![foo])
        });

        let (_, layout) = super::completion_popup_layout(&mut h.stoat).expect("the popup lays out");
        assert_eq!(
            (layout.popup_area.height, layout.inner.height, layout.detail),
            (4, 2, None)
        );
    }

    /// The popup over `items`, the first selected, painted into a 40 by 12
    /// frame, alongside its row region and the harness that painted it.
    fn painted(items: Vec<CompletionItem>) -> (Buffer, Rect, TestHarness) {
        let mut h = TestHarness::with_size(40, 12);
        let _path = open_scratch_file(&mut h, "");
        h.type_keys("i");
        h.stoat.pending_completion = Some(CompletionPopup {
            selected_idx: Some(0),
            ..CompletionPopup::showing(items)
        });

        let buf = h.stoat.render();
        let (_, layout) = super::completion_popup_layout(&mut h.stoat).expect("the popup lays out");
        (buf, layout.inner, h)
    }
}
