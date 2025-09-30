use super::{
    layout::{EditorLayout, PositionedLine},
    style::EditorStyle,
};
use crate::syntax::{HighlightMap, HighlightedChunks, SyntaxTheme};
use gpui::{
    App, Bounds, Element, ElementId, Font, FontStyle, FontWeight, GlobalElementId,
    InspectorElementId, IntoElement, LayoutId, PaintQuad, Pixels, SharedString, Style, TextRun,
    Window, point, px, relative, size,
};
use smallvec::SmallVec;
use stoat::Stoat;
use text::ToOffset;

pub struct EditorElement {
    stoat: Stoat,
    style: EditorStyle,
    syntax_theme: SyntaxTheme,
    highlight_map: HighlightMap,
}

impl EditorElement {
    pub fn new(stoat: Stoat) -> Self {
        let syntax_theme = SyntaxTheme::default();
        let highlight_map = HighlightMap::new(&syntax_theme);

        Self {
            stoat,
            style: EditorStyle::default(),
            syntax_theme,
            highlight_map,
        }
    }
}

impl Element for EditorElement {
    type RequestLayoutState = ();
    type PrepaintState = EditorLayout;

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
        // Calculate content bounds (with padding)
        let content_bounds = Bounds {
            origin: bounds.origin + point(self.style.padding, self.style.padding),
            size: size(
                bounds.size.width - self.style.padding * 2.0,
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
        let buffer_snapshot = self.stoat.buffer_snapshot(cx);
        let scroll_position = self.stoat.scroll_position();

        // Calculate visible row range based on scroll position and viewport height
        let height_in_lines = content_bounds.size.height / self.style.line_height;
        let start_row = scroll_position.y as u32;
        let max_row = buffer_snapshot.row_count();
        let end_row = ((scroll_position.y + height_in_lines).ceil() as u32).min(max_row);

        let mut lines = SmallVec::new();

        // Get token snapshot for syntax highlighting
        let token_snapshot = self.stoat.token_snapshot();

        // Only iterate through visible rows
        for row in start_row..end_row {
            let line_start = text::Point::new(row, 0);
            let line_len = buffer_snapshot.line_len(row);
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

                    let text_run = TextRun {
                        len: text_len,
                        font: font.clone(),
                        color: highlight_style.color.unwrap_or(self.style.text_color),
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
                    color: self.style.text_color,
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
                    content_bounds.origin.y + px(relative_row as f32 * self.style.line_height.0),
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
                color: self.style.text_color,
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
        }

        EditorLayout {
            lines,
            bounds,
            _content_bounds: content_bounds,
            _line_height: self.style.line_height,
        }
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
        // Paint background
        window.paint_quad(PaintQuad {
            bounds: layout.bounds,
            corner_radii: Default::default(),
            background: self.style.background.into(),
            border_color: Default::default(),
            border_widths: Default::default(),
            border_style: Default::default(),
        });

        // Paint each shaped line
        for line in &layout.lines {
            // Paint the shaped text - this is how Zed does it
            line.shaped
                .paint(line.position, self.style.line_height, window, cx)
                .unwrap_or_else(|err| {
                    eprintln!("Failed to paint line: {:?}", err);
                });
        }

        // Paint cursor
        self.paint_cursor(layout, window, cx);
    }
}

impl EditorElement {
    /// Paint the cursor at the current position
    fn paint_cursor(&self, layout: &EditorLayout, window: &mut Window, cx: &mut App) {
        let cursor_position = self.stoat.cursor_position();
        let buffer_snapshot = self.stoat.buffer_snapshot(cx);

        // Only render cursor if it's in the visible range
        let scroll_position = self.stoat.scroll_position();
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
                        color: self.style.text_color,
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
                    background: self.style.text_color.into(),
                    border_color: Default::default(),
                    border_widths: Default::default(),
                    border_style: Default::default(),
                });
            }
        }
    }
}

impl IntoElement for EditorElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}
