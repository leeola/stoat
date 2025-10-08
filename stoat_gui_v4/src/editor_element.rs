//! Minimal EditorElement for stoat_v4
//!
//! Simplified version that just renders text with syntax highlighting.
//! No gutter, no mouse handling, no complex layout - just get text visible.

use crate::{
    editor_style::EditorStyle,
    editor_view::EditorView,
    syntax::{HighlightMap, HighlightedChunks, SyntaxTheme},
};
use gpui::{
    point, px, relative, size, App, Bounds, Element, ElementId, Entity, Font, FontStyle,
    FontWeight, GlobalElementId, InspectorElementId, IntoElement, LayoutId, Pixels, SharedString,
    Style, TextRun, Window,
};

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
    type PrepaintState = ();

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
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Self::PrepaintState {
        // No prepaint needed for minimal version
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        // Paint background
        window.paint_quad(gpui::PaintQuad {
            bounds,
            corner_radii: 0.0.into(),
            background: self.style.background.into(),
            border_color: gpui::transparent_black(),
            border_widths: 0.0.into(),
            border_style: gpui::BorderStyle::default(),
        });

        // Get buffer and tokens - clone snapshots to avoid holding borrows of cx
        let buffer_snapshot = {
            let stoat = self.view.read(cx).stoat.read(cx);
            let buffer_item = stoat.buffer_item();
            buffer_item.read(cx).buffer().read(cx).snapshot()
        };
        let token_snapshot = {
            let stoat = self.view.read(cx).stoat.read(cx);
            let buffer_item = stoat.buffer_item();
            buffer_item.read(cx).token_snapshot()
        };

        // Create font
        let font = Font {
            family: SharedString::from("Menlo"),
            features: Default::default(),
            weight: FontWeight::NORMAL,
            style: FontStyle::Normal,
            fallbacks: None,
        };

        // Calculate visible range
        let max_point = buffer_snapshot.max_point();
        let visible_lines =
            ((bounds.size.height - self.style.padding * 2.0) / self.style.line_height).floor();
        let max_lines = visible_lines as u32;

        // Set viewport lines on Stoat and get scroll position
        let stoat_entity = self.view.read(cx).stoat.clone();
        let scroll_y = stoat_entity.update(cx, |stoat, _cx| {
            stoat.set_viewport_lines(visible_lines as f32);
            stoat.scroll_position().y
        });

        // Calculate gutter width (for line numbers)
        let gutter_width = self.calculate_gutter_width(max_point.row + 1, window);

        // Use scroll position as offset
        let scroll_offset = scroll_y.floor() as u32;

        // Track line positions for cursor rendering
        let mut line_positions: Vec<(u32, Pixels)> = Vec::new();

        // Render visible lines starting from scroll_offset
        let mut y = bounds.origin.y + self.style.padding;

        let start_line = scroll_offset;
        let end_line = (start_line + max_lines).min(max_point.row + 1);

        for line_idx in start_line..end_line {
            // Store position of this line
            line_positions.push((line_idx, y));
            let line_start = buffer_snapshot.point_to_offset(text::Point::new(line_idx, 0));
            let line_end_row = if line_idx == max_point.row {
                buffer_snapshot.len()
            } else {
                buffer_snapshot.point_to_offset(text::Point::new(line_idx + 1, 0))
            };

            // Get highlighted chunks for this line
            let chunks = HighlightedChunks::new(
                line_start..line_end_row,
                &buffer_snapshot,
                &token_snapshot,
                &self.highlight_map,
            );

            // Build complete line text and runs
            let mut line_text = String::new();
            let mut runs = Vec::new();

            for chunk in chunks {
                let text = chunk.text;
                if text.is_empty() {
                    continue;
                }

                // Get color for this chunk
                let color = if let Some(highlight_id) = chunk.highlight_id {
                    self.syntax_theme
                        .highlights
                        .get(highlight_id.0 as usize)
                        .map(|(_name, style)| style.color.unwrap_or(self.style.text_color))
                        .unwrap_or(self.style.text_color)
                } else {
                    self.style.text_color
                };

                line_text.push_str(text);
                runs.push(TextRun {
                    len: text.len(),
                    font: font.clone(),
                    color,
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                });
            }

            // Skip empty lines (but still advance y)
            if !line_text.is_empty() {
                // Strip trailing newline (shape_line doesn't accept newlines)
                if line_text.ends_with('\n') {
                    line_text.pop();
                    // Adjust last run length if needed
                    if let Some(last_run) = runs.last_mut() {
                        if last_run.len > 0 {
                            last_run.len -= 1;
                        }
                    }
                }

                // Shape and paint the complete line (only if not empty after stripping newline)
                if !line_text.is_empty() {
                    let shaped = window.text_system().shape_line(
                        SharedString::from(line_text),
                        self.style.font_size,
                        &runs,
                        None,
                    );

                    let x = bounds.origin.x + gutter_width + self.style.padding;
                    if let Err(e) = shaped.paint(point(x, y), self.style.line_height, window, cx) {
                        tracing::warn!("Failed to paint line {}: {:?}", line_idx, e);
                    }
                }
            }

            y += self.style.line_height;

            // Stop if we've gone past the visible area
            if y > bounds.origin.y + bounds.size.height {
                break;
            }
        }

        // Paint line numbers in gutter
        self.paint_line_numbers(bounds, &line_positions, gutter_width, window, cx);

        // Paint cursor on top of text
        self.paint_cursor(bounds, &line_positions, gutter_width, window, cx);
    }
}

impl EditorElement {
    /// Paint line numbers in the gutter
    fn paint_line_numbers(
        &self,
        bounds: Bounds<Pixels>,
        line_positions: &[(u32, Pixels)],
        gutter_width: Pixels,
        window: &mut Window,
        cx: &mut App,
    ) {
        if !self.style.show_line_numbers || gutter_width == Pixels::ZERO {
            return;
        }

        // Create font for line numbers
        let font = Font {
            family: SharedString::from("Menlo"),
            features: Default::default(),
            weight: FontWeight::NORMAL,
            style: FontStyle::Normal,
            fallbacks: None,
        };

        // Dimmed color for line numbers (60% opacity)
        let line_number_color = gpui::Hsla {
            h: self.style.text_color.h,
            s: self.style.text_color.s,
            l: self.style.text_color.l,
            a: self.style.text_color.a * 0.6,
        };

        // Render each visible line number
        for (line_idx, y) in line_positions {
            let line_number = format!("{}", line_idx + 1); // 1-indexed line numbers
            let line_number_shared = SharedString::from(line_number);

            let text_run = TextRun {
                len: line_number_shared.len(),
                font: font.clone(),
                color: line_number_color,
                background_color: None,
                underline: None,
                strikethrough: None,
            };

            let shaped = window.text_system().shape_line(
                line_number_shared,
                self.style.font_size * 0.9, // Slightly smaller
                &[text_run],
                None,
            );

            // Right-align within gutter (subtract text width from gutter width)
            let x = bounds.origin.x + gutter_width - shaped.width - px(8.0);

            if let Err(e) = shaped.paint(point(x, *y), self.style.line_height, window, cx) {
                tracing::warn!("Failed to paint line number {}: {:?}", line_idx + 1, e);
            }
        }
    }

    /// Calculate gutter width based on line numbers to display
    fn calculate_gutter_width(&self, max_line_number: u32, window: &mut Window) -> Pixels {
        if !self.style.show_line_numbers {
            return Pixels::ZERO;
        }

        // Format the maximum line number to measure its width
        let max_line_text = format!("{}", max_line_number);

        // Create font for line numbers (same as code font)
        let font = Font {
            family: SharedString::from("Menlo"),
            features: Default::default(),
            weight: FontWeight::NORMAL,
            style: FontStyle::Normal,
            fallbacks: None,
        };

        // Measure the width of the maximum line number
        let line_number_shared = SharedString::from(max_line_text);
        let text_run = TextRun {
            len: line_number_shared.len(),
            font,
            color: self.style.text_color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };

        let shaped = window.text_system().shape_line(
            line_number_shared,
            self.style.font_size * 0.9, // Slightly smaller font for line numbers
            &[text_run],
            None,
        );

        // Add padding on both sides for spacing
        shaped.width + px(16.0) // 8px padding on each side
    }

    /// Paint the cursor at the current position
    fn paint_cursor(
        &self,
        bounds: Bounds<Pixels>,
        line_positions: &[(u32, Pixels)],
        gutter_width: Pixels,
        window: &mut Window,
        cx: &mut App,
    ) {
        // Only paint cursor if the editor view is focused
        if !self.view.read(cx).is_focused(window) {
            return;
        }

        // Get cursor position from stoat
        let stoat = self.view.read(cx).stoat.read(cx);
        let cursor_position = stoat.cursor_position();

        // Find the y position for the cursor's line
        let cursor_y = line_positions
            .iter()
            .find(|(line_idx, _)| *line_idx == cursor_position.row)
            .map(|(_, y)| *y);

        let Some(cursor_y) = cursor_y else {
            // Cursor not in visible range
            return;
        };

        // Get the buffer snapshot to measure text before cursor
        let buffer_item = stoat.buffer_item();
        let buffer = buffer_item.read(cx).buffer().read(cx);
        let buffer_snapshot = buffer.snapshot();

        // Calculate cursor x position by measuring text before cursor
        let text_before_cursor = if cursor_position.column > 0 {
            let line_start = text::Point::new(cursor_position.row, 0);
            let cursor_point = text::Point::new(cursor_position.row, cursor_position.column);

            let mut text_before = String::new();
            for chunk in buffer_snapshot.text_for_range(line_start..cursor_point) {
                text_before.push_str(chunk);
            }
            text_before
        } else {
            String::new()
        };

        // Measure text width
        let text_width = if !text_before_cursor.is_empty() {
            let font = Font {
                family: SharedString::from("Menlo"),
                features: Default::default(),
                weight: FontWeight::NORMAL,
                style: FontStyle::Normal,
                fallbacks: None,
            };

            let text_shared = SharedString::from(text_before_cursor);
            let text_run = TextRun {
                len: text_shared.len(),
                font,
                color: self.style.text_color,
                background_color: None,
                underline: None,
                strikethrough: None,
            };

            let shaped = window.text_system().shape_line(
                text_shared,
                self.style.font_size,
                &[text_run],
                None,
            );

            shaped.width
        } else {
            Pixels::ZERO
        };

        // Paint cursor as 2px vertical bar
        let cursor_x = bounds.origin.x + gutter_width + self.style.padding + text_width;
        let cursor_bounds = Bounds {
            origin: point(cursor_x, cursor_y),
            size: size(px(2.0), self.style.line_height),
        };

        window.paint_quad(gpui::PaintQuad {
            bounds: cursor_bounds,
            corner_radii: 0.0.into(),
            background: self.style.text_color.into(),
            border_color: gpui::transparent_black(),
            border_widths: 0.0.into(),
            border_style: gpui::BorderStyle::default(),
        });
    }
}

impl IntoElement for EditorElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}
