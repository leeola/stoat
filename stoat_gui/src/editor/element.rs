use super::{
    gutter::GutterLayout,
    layout::{EditorLayout, PositionedLine},
    style::EditorStyle,
    view::EditorView,
};
use crate::syntax::{HighlightMap, HighlightedChunks, SyntaxTheme};
use gpui::{
    App, Bounds, DispatchPhase, Element, ElementId, Entity, Font, FontStyle, FontWeight,
    GlobalElementId, InspectorElementId, IntoElement, LayoutId, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, SharedString, Style, TextRun, Window, point,
    px, relative, size,
};
use smallvec::SmallVec;
use std::rc::Rc;
use text::ToOffset;
use tracing::info;

pub struct EditorElement {
    view: Entity<EditorView>,
    style: EditorStyle,
    syntax_theme: SyntaxTheme,
    highlight_map: HighlightMap,
}

impl EditorElement {
    pub fn new(view: Entity<EditorView>) -> Self {
        let syntax_theme = SyntaxTheme::default();
        let highlight_map = HighlightMap::new(&syntax_theme);

        Self {
            view,
            style: EditorStyle::default(),
            syntax_theme,
            highlight_map,
        }
    }
}

impl Element for EditorElement {
    type RequestLayoutState = ();
    type PrepaintState = Rc<EditorLayout>;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        // Request a simple full-size layout
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = relative(1.).into();
        let layout_id = window.request_layout(style, [], cx);
        (layout_id, ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        // Calculate gutter bounds
        let (gutter_width, gutter_right_padding) = self.compute_gutter_width(cx);
        let gutter_bounds = Bounds {
            origin: bounds.origin + point(self.style.padding, self.style.padding),
            size: size(gutter_width, bounds.size.height - self.style.padding * 2.0),
        };

        // Calculate content bounds (shifted right by gutter width)
        let content_bounds = Bounds {
            origin: bounds.origin + point(self.style.padding + gutter_width, self.style.padding),
            size: size(
                bounds.size.width - self.style.padding * 2.0 - gutter_width,
                bounds.size.height - self.style.padding * 2.0,
            ),
        };

        // Create the text style for shaping
        let font = Font {
            family: SharedString::from("Menlo"),
            features: Default::default(),
            weight: FontWeight::NORMAL,
            style: FontStyle::Normal,
            fallbacks: None,
        };

        // Static empty line to avoid repeated allocations
        static EMPTY_LINE: SharedString = SharedString::new_static(" ");

        // Get buffer content and scroll position
        let stoat_entity = self.view.read(cx).stoat();
        let stoat = stoat_entity.read(cx);
        let buffer_snapshot = stoat.buffer_snapshot(cx);
        let scroll_position = stoat.scroll_position();

        // Calculate visible row range based on scroll position and viewport height
        let height_in_lines = content_bounds.size.height / self.style.line_height;
        let start_row = scroll_position.y as u32;
        let max_row = buffer_snapshot.row_count();
        let end_row = ((scroll_position.y + height_in_lines).ceil() as u32).min(max_row);

        let mut lines = SmallVec::new();
        let mut line_lengths = SmallVec::new();

        // Get token snapshot for syntax highlighting
        let token_snapshot = stoat.token_snapshot(cx);

        // Only iterate through visible rows
        for row in start_row..end_row {
            let line_start = text::Point::new(row, 0);
            let line_len = buffer_snapshot.line_len(row);

            // Store buffer line length for position clamping
            line_lengths.push(line_len);
            let line_end = text::Point::new(row, line_len);

            // Convert line range to byte offsets for highlighting
            let line_start_offset = line_start.to_offset(&buffer_snapshot);
            let line_end_offset = line_end.to_offset(&buffer_snapshot);

            // Create highlighted chunks iterator for this line
            let highlighted_chunks = HighlightedChunks::new(
                line_start_offset..line_end_offset,
                &buffer_snapshot,
                &token_snapshot,
                &self.highlight_map,
            );

            // Build text runs with proper highlighting
            let mut text_runs = Vec::new();
            let mut line_text = String::new();
            let mut current_offset = 0;

            for chunk in highlighted_chunks {
                // Expand tabs to spaces before adding to line text
                let expanded_text = if chunk.text.contains('\t') {
                    let mut expanded = String::with_capacity(chunk.text.len() * 2);
                    let mut column = current_offset;
                    for ch in chunk.text.chars() {
                        if ch == '\t' {
                            let tab_stop = 4; // TODO: Make this configurable
                            let spaces_to_add = tab_stop - (column % tab_stop);
                            for _ in 0..spaces_to_add {
                                expanded.push(' ');
                                column += 1;
                            }
                        } else {
                            expanded.push(ch);
                            column += 1;
                            if ch == '\n' {
                                column = 0;
                            }
                        }
                    }
                    expanded
                } else {
                    chunk.text.to_string()
                };

                // Add text to line
                line_text.push_str(&expanded_text);

                // Create text run with appropriate styling
                let text_len = expanded_text.len();
                if text_len > 0 {
                    let highlight_style = chunk
                        .highlight_id
                        .and_then(|id| id.style(&self.syntax_theme))
                        .unwrap_or_default();

                    let color = highlight_style
                        .color
                        .unwrap_or(self.syntax_theme.default_text_color);

                    let text_run = TextRun {
                        len: text_len,
                        font: font.clone(),
                        color,
                        background_color: highlight_style.background_color,
                        underline: None,
                        strikethrough: None,
                    };

                    text_runs.push(text_run);
                }
                current_offset += text_len;
            }

            // Create SharedString - empty lines use static string
            let text = if line_text.is_empty() {
                EMPTY_LINE.clone()
            } else {
                SharedString::from(line_text)
            };

            // Shape the line using GPUI's text system with multiple text runs
            let shaped = if text_runs.is_empty() {
                // Fallback for empty lines
                let text_run = TextRun {
                    len: text.len(),
                    font: font.clone(),
                    color: self.syntax_theme.default_text_color,
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                };
                window
                    .text_system()
                    .shape_line(text, self.style.font_size, &[text_run], None)
            } else {
                window
                    .text_system()
                    .shape_line(text, self.style.font_size, &text_runs, None)
            };

            // Position lines relative to viewport (accounting for scroll offset)
            let relative_row = row - start_row;
            lines.push(PositionedLine {
                shaped,
                position: point(
                    content_bounds.origin.x,
                    content_bounds.origin.y
                        + px(relative_row as f32 * f32::from(self.style.line_height)),
                ),
            });
        }

        // If no content, add a placeholder
        if lines.is_empty() {
            static PLACEHOLDER: SharedString =
                SharedString::new_static("Empty buffer - ready for input");
            let text = PLACEHOLDER.clone();
            let text_run = TextRun {
                len: text.len(),
                font,
                color: self.syntax_theme.default_text_color,
                background_color: None,
                underline: None,
                strikethrough: None,
            };

            let shaped =
                window
                    .text_system()
                    .shape_line(text, self.style.font_size, &[text_run], None);

            lines.push(PositionedLine {
                shaped,
                position: content_bounds.origin,
            });
            line_lengths.push(0); // Empty buffer has zero length
        }

        // Compute gutter layout with diff indicators (if enabled)
        let gutter = if self.style.show_diff_indicators {
            let stoat_entity = self.view.read(cx).stoat();
            let stoat = stoat_entity.read(cx);
            let buffer_item = stoat.active_buffer_item(cx);
            let diff = buffer_item.read(cx).diff();

            Some(GutterLayout::new(
                gutter_bounds,
                start_row..end_row,
                diff,
                &buffer_snapshot,
                gutter_width,
                gutter_right_padding,
                self.style.line_height,
            ))
        } else {
            None
        };

        let layout = EditorLayout {
            lines,
            line_lengths,
            bounds,
            content_bounds,
            line_height: self.style.line_height,
            scroll_position,
            start_row,
            gutter,
        };

        Rc::new(layout)
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        layout: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        // Register mouse down handler to position cursor
        window.on_mouse_event({
            let layout = layout.clone();
            let view = self.view.clone();
            move |event: &MouseDownEvent, phase, _window, cx| {
                if phase == DispatchPhase::Bubble && event.button == MouseButton::Left {
                    if let Some(text_pos) = layout.position_for_pixel(event.position) {
                        info!(
                            "Mouse down at pixel {:?} -> text position {:?}",
                            event.position, text_pos
                        );
                        view.update(cx, |view, cx| {
                            view.set_cursor_position(text_pos, cx);
                        });
                    }
                }
            }
        });

        // Register mouse move handler for drag selection
        window.on_mouse_event({
            let layout = layout.clone();
            let view = self.view.clone();
            move |event: &MouseMoveEvent, phase, _window, cx| {
                if phase == DispatchPhase::Bubble && event.pressed_button == Some(MouseButton::Left)
                {
                    if let Some(text_pos) = layout.position_for_pixel(event.position) {
                        view.update(cx, |view, cx| {
                            // Start selection mode on first drag
                            if !view.is_selecting(cx) {
                                view.start_selection(cx);
                            }
                            // Extend selection to current position
                            view.extend_selection_to(text_pos, cx);
                            // Notify to trigger re-render
                            cx.notify();
                        });
                    }
                }
            }
        });

        // Register mouse up handler to end selection
        window.on_mouse_event({
            let view = self.view.clone();
            move |event: &MouseUpEvent, phase, _window, cx| {
                if phase == DispatchPhase::Bubble && event.button == MouseButton::Left {
                    view.update(cx, |view, cx| {
                        view.end_selection(cx);
                    });
                }
            }
        });

        // Paint background using theme color
        window.paint_quad(PaintQuad {
            bounds: layout.bounds,
            corner_radii: Default::default(),
            background: self.syntax_theme.background_color.into(),
            border_color: Default::default(),
            border_widths: Default::default(),
            border_style: Default::default(),
        });

        // Paint git diff gutter (after background, before selection)
        self.paint_gutter(layout, window, cx);

        // Paint selection (behind text)
        self.paint_selection(layout, window, cx);

        // Paint each shaped line
        for line in &layout.lines {
            // Paint the shaped text - this is how Zed does it
            line.shaped
                .paint(line.position, self.style.line_height, window, cx)
                .unwrap_or_else(|err| {
                    eprintln!("Failed to paint line: {err:?}");
                });
        }

        // Paint cursor
        self.paint_cursor(layout, window, cx);
    }
}

impl EditorElement {
    /// Paint the cursor at the current position
    fn paint_cursor(&self, layout: &EditorLayout, window: &mut Window, cx: &mut App) {
        let stoat_entity = self.view.read(cx).stoat();
        let stoat = stoat_entity.read(cx);
        let cursor_position = stoat.cursor_position();
        let buffer_snapshot = stoat.buffer_snapshot(cx);

        // Only render cursor if it's in the visible range
        let scroll_position = stoat.scroll_position();
        let start_row = scroll_position.y as u32;
        let visible_rows = layout.lines.len() as u32;
        let end_row = start_row + visible_rows;

        if cursor_position.row >= start_row && cursor_position.row < end_row {
            let relative_row = cursor_position.row - start_row;

            if let Some(line) = layout.lines.get(relative_row as usize) {
                // Calculate cursor x position within the line
                let line_start = text::Point::new(cursor_position.row, 0);
                let cursor_offset_in_line = cursor_position.column;

                // Use GPUI's text measurement to get precise cursor position
                let text_before_cursor = if cursor_offset_in_line > 0 {
                    let line_len = buffer_snapshot.line_len(cursor_position.row);
                    let end_col = cursor_offset_in_line.min(line_len);
                    let text_range = line_start..text::Point::new(cursor_position.row, end_col);

                    let mut text_before = String::new();
                    for chunk in buffer_snapshot.text_for_range(text_range) {
                        text_before.push_str(chunk);
                    }

                    // Expand tabs to spaces for cursor positioning too
                    if text_before.contains('\t') {
                        let mut expanded = String::with_capacity(text_before.len() * 2);
                        let mut column = 0;
                        for ch in text_before.chars() {
                            if ch == '\t' {
                                let tab_stop = 4; // TODO: Make this configurable
                                let spaces_to_add = tab_stop - (column % tab_stop);
                                for _ in 0..spaces_to_add {
                                    expanded.push(' ');
                                    column += 1;
                                }
                            } else {
                                expanded.push(ch);
                                column += 1;
                            }
                        }
                        expanded
                    } else {
                        text_before
                    }
                } else {
                    String::new()
                };

                // Measure the text width to position cursor
                let text_width = if !text_before_cursor.is_empty() {
                    let font = Font {
                        family: SharedString::from("Menlo"),
                        features: Default::default(),
                        weight: FontWeight::NORMAL,
                        style: FontStyle::Normal,
                        fallbacks: None,
                    };

                    let text_before_shared = SharedString::from(text_before_cursor.clone());
                    let text_run_len = text_before_shared.len();
                    let text_run = TextRun {
                        len: text_run_len,
                        font,
                        color: self.syntax_theme.default_text_color,
                        background_color: None,
                        underline: None,
                        strikethrough: None,
                    };

                    let shaped = window.text_system().shape_line(
                        text_before_shared,
                        self.style.font_size,
                        &[text_run],
                        None,
                    );

                    shaped.width
                } else {
                    px(0.0)
                };

                // Paint cursor as a vertical line
                let cursor_x = line.position.x + text_width;
                let cursor_y = line.position.y;
                let cursor_bounds = Bounds {
                    origin: point(cursor_x, cursor_y),
                    size: size(px(2.0), self.style.line_height),
                };

                window.paint_quad(PaintQuad {
                    bounds: cursor_bounds,
                    corner_radii: Default::default(),
                    background: self.syntax_theme.default_text_color.into(),
                    border_color: Default::default(),
                    border_widths: Default::default(),
                    border_style: Default::default(),
                });
            }
        }
    }

    /// Paint the selection highlight
    fn paint_selection(&self, layout: &EditorLayout, window: &mut Window, cx: &mut App) {
        let stoat_entity = self.view.read(cx).stoat();
        let stoat = stoat_entity.read(cx);
        let selection = stoat.cursor_manager().selection();

        // Only paint if there's an actual selection (not just a cursor)
        if selection.is_empty() {
            return;
        }

        let buffer_snapshot = stoat.buffer_snapshot(cx);
        let scroll_position = stoat.scroll_position();
        let start_row = scroll_position.y as u32;
        let visible_rows = layout.lines.len() as u32;
        let end_row = start_row + visible_rows;

        // Selection color - light blue with 30% transparency (0xRRGGBBAA format)
        let selection_color = gpui::rgba(0x3366FF4D);

        // Convert selection points to offsets for easier comparison
        let selection_start_offset = selection.start.to_offset(&buffer_snapshot);
        let selection_end_offset = selection.end.to_offset(&buffer_snapshot);

        // Paint selection for each visible line that intersects the selection
        for row in start_row..end_row {
            let line_start = text::Point::new(row, 0);
            let line_len = buffer_snapshot.line_len(row);
            let line_end = text::Point::new(row, line_len);

            let line_start_offset = line_start.to_offset(&buffer_snapshot);
            let line_end_offset = line_end.to_offset(&buffer_snapshot);

            // Skip lines that don't intersect the selection
            if line_end_offset <= selection_start_offset
                || line_start_offset >= selection_end_offset
            {
                continue;
            }

            // Calculate selection range within this line
            let sel_start_in_line = if selection_start_offset > line_start_offset {
                buffer_snapshot
                    .offset_to_point(selection_start_offset)
                    .column
            } else {
                0
            };

            let sel_end_in_line = if selection_end_offset < line_end_offset {
                buffer_snapshot.offset_to_point(selection_end_offset).column
            } else {
                line_len
            };

            if sel_start_in_line >= sel_end_in_line {
                continue;
            }

            let relative_row = row - start_row;
            if let Some(line) = layout.lines.get(relative_row as usize) {
                // Measure text width before selection
                let font = Font {
                    family: SharedString::from("Menlo"),
                    features: Default::default(),
                    weight: FontWeight::NORMAL,
                    style: FontStyle::Normal,
                    fallbacks: None,
                };

                // Get text before selection start
                let text_before_range = line_start..text::Point::new(row, sel_start_in_line);
                let mut text_before = String::new();
                for chunk in buffer_snapshot.text_for_range(text_before_range) {
                    text_before.push_str(chunk);
                }

                // Get selected text
                let selected_text_range = text::Point::new(row, sel_start_in_line)
                    ..text::Point::new(row, sel_end_in_line);
                let mut selected_text = String::new();
                for chunk in buffer_snapshot.text_for_range(selected_text_range) {
                    selected_text.push_str(chunk);
                }

                // Expand tabs for both
                let expand_tabs = |text: &str| -> String {
                    if !text.contains('\t') {
                        return text.to_string();
                    }
                    let mut expanded = String::with_capacity(text.len() * 2);
                    let mut column = 0;
                    for ch in text.chars() {
                        if ch == '\t' {
                            let tab_stop = 4;
                            let spaces_to_add = tab_stop - (column % tab_stop);
                            for _ in 0..spaces_to_add {
                                expanded.push(' ');
                                column += 1;
                            }
                        } else {
                            expanded.push(ch);
                            column += 1;
                        }
                    }
                    expanded
                };

                let text_before_expanded = expand_tabs(&text_before);
                let selected_text_expanded = expand_tabs(&selected_text);

                // Measure widths
                let measure_width = |text: &str| -> Pixels {
                    if text.is_empty() {
                        return px(0.0);
                    }
                    let shared_text = SharedString::from(text.to_string());
                    let text_run = TextRun {
                        len: shared_text.len(),
                        font: font.clone(),
                        color: self.syntax_theme.default_text_color,
                        background_color: None,
                        underline: None,
                        strikethrough: None,
                    };
                    let shaped = window.text_system().shape_line(
                        shared_text,
                        self.style.font_size,
                        &[text_run],
                        None,
                    );
                    shaped.width
                };

                let offset_x = measure_width(&text_before_expanded);
                let selection_width = measure_width(&selected_text_expanded);

                // Paint selection background
                let selection_bounds = Bounds {
                    origin: point(line.position.x + offset_x, line.position.y),
                    size: size(selection_width, self.style.line_height),
                };

                window.paint_quad(PaintQuad {
                    bounds: selection_bounds,
                    corner_radii: Default::default(),
                    background: selection_color.into(),
                    border_color: Default::default(),
                    border_widths: Default::default(),
                    border_style: Default::default(),
                });
            }
        }
    }

    /// Paint the git diff gutter with colored indicators.
    ///
    /// Renders the gutter area on the left side of the editor with:
    /// - Background fill in gutter area
    /// - Colored bars for each changed line (green=added, blue=modified, red=deleted)
    ///
    /// # Arguments
    ///
    /// * `layout` - Editor layout containing gutter layout with diff indicators
    /// * `window` - GPUI window for painting operations
    /// * `cx` - App context
    fn paint_gutter(&self, layout: &EditorLayout, window: &mut Window, _cx: &mut App) {
        let Some(gutter) = &layout.gutter else {
            return;
        };

        // Paint gutter background (same as editor background)
        window.paint_quad(PaintQuad {
            bounds: gutter.dimensions.bounds,
            corner_radii: Default::default(),
            background: self.syntax_theme.background_color.into(),
            border_color: Default::default(),
            border_widths: Default::default(),
            border_style: Default::default(),
        });

        // Paint diff indicators with blended colors
        for indicator in &gutter.diff_indicators {
            let diff_color = match indicator.status {
                stoat::git_diff::DiffHunkStatus::Added => self.style.diff_added_color,
                stoat::git_diff::DiffHunkStatus::Modified => self.style.diff_modified_color,
                stoat::git_diff::DiffHunkStatus::Deleted => self.style.diff_deleted_color,
            };

            // Blend diff color with editor background to prevent transparency artifacts
            let blended_color = self.syntax_theme.background_color.blend(diff_color);

            window.paint_quad(PaintQuad {
                bounds: indicator.bounds,
                corner_radii: indicator.corner_radii,
                background: blended_color.into(),
                border_color: Default::default(),
                border_widths: Default::default(),
                border_style: Default::default(),
            });
        }
    }

    /// Compute gutter width dynamically based on enabled features.
    ///
    /// Returns `(total_width, right_padding)` where:
    /// - `total_width` - Full gutter width including padding
    /// - `right_padding` - Spacing between gutter content and editor text
    ///
    /// Width accounts for:
    /// - Line numbers (if enabled)
    /// - Diff indicators (if enabled) with [`EditorStyle::diff_indicator_padding`]
    /// - Right padding from [`EditorStyle::gutter_right_padding`]
    /// - Minimum width of 8px if any feature is enabled
    ///
    /// All padding values are configurable via [`EditorStyle`].
    fn compute_gutter_width(&self, _cx: &App) -> (Pixels, Pixels) {
        let mut content_width = px(0.0);

        // Add width for line numbers (if enabled)
        if self.style.show_line_numbers {
            // Future: compute based on max line number digits
            // For now, placeholder width:
            content_width = content_width + px(40.0);
        }

        // Add width for diff indicators (if enabled)
        if self.style.show_diff_indicators {
            // Widest diff indicator is deleted hunk: 0.35 * line_height
            let diff_width = (0.35 * self.style.line_height).floor();
            // Add configured padding around indicator
            let diff_total = diff_width + self.style.diff_indicator_padding;

            content_width = content_width.max(diff_total);
        }

        // Add right padding for spacing between gutter and content
        if content_width > px(0.0) {
            let min_width = content_width.max(px(8.0));
            (
                min_width + self.style.gutter_right_padding,
                self.style.gutter_right_padding,
            )
        } else {
            (px(0.0), px(0.0)) // No gutter if no features enabled
        }
    }
}

impl IntoElement for EditorElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}
