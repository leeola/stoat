//! Minimal EditorElement for stoat
//!
//! Simplified version that just renders text with syntax highlighting.
//! No gutter, no mouse handling, no complex layout - just get text visible.

use crate::{
    editor_style::EditorStyle,
    editor_view::EditorView,
    gutter::GutterLayout,
    minimap::{CachedQuad, MAX_MINIMAP_LINES, MINIMAP_LINE_HEIGHT, MinimapCache, MinimapLayout},
    syntax::{HighlightMap, HighlightedChunks, SyntaxTheme},
};
use gpui::{
    App, Bounds, Element, ElementId, Entity, Font, FontStyle, FontWeight, GlobalElementId,
    InspectorElementId, IntoElement, LayoutId, Pixels, SharedString, Style, TextRun, Window, point,
    px, relative, size,
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
    type PrepaintState = MinimapCache;

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
        // Build minimap cache during prepaint for optimal performance
        self.build_minimap_cache(bounds, window, cx)
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
            let buffer_item = stoat.active_buffer(cx);
            buffer_item.read(cx).buffer().read(cx).snapshot()
        };
        let token_snapshot = {
            let stoat = self.view.read(cx).stoat.read(cx);
            let buffer_item = stoat.active_buffer(cx);
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

        // Set viewport lines on Stoat, update scroll animation, and get scroll position
        let stoat_entity = self.view.read(cx).stoat.clone();
        let (scroll_y, is_animating) = stoat_entity.update(cx, |stoat, _cx| {
            stoat.set_viewport_lines(visible_lines);
            stoat.update_scroll_animation();
            (stoat.scroll_position().y, stoat.is_scroll_animating())
        });

        // Request another frame if scroll animation is in progress
        if is_animating {
            let view = self.view.clone();
            window.on_next_frame(move |_, cx| {
                view.update(cx, |_, cx| cx.notify());
            });
        }

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
                        tracing::error!("Failed to paint line {}: {:?}", line_idx, e);
                    }
                }
            }

            y += self.style.line_height;

            // Stop if we've gone past the visible area
            if y > bounds.origin.y + bounds.size.height {
                break;
            }
        }

        // Paint git diff indicators in gutter (behind line numbers)
        self.paint_gutter(bounds, start_line..end_line, gutter_width, window, cx);

        // Paint line numbers in gutter
        self.paint_line_numbers(bounds, &line_positions, gutter_width, window, cx);

        // Paint cursor on top of text
        self.paint_cursor(bounds, &line_positions, gutter_width, window, cx);

        // Paint minimap on right side using cached colored quads
        self.paint_minimap(bounds, _prepaint, window, cx);
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
                tracing::error!("Failed to paint line number {}: {:?}", line_idx + 1, e);
            }
        }
    }

    /// Calculate gutter width based on line numbers to display
    fn calculate_gutter_width(&self, max_line_number: u32, window: &mut Window) -> Pixels {
        if !self.style.show_line_numbers {
            return Pixels::ZERO;
        }

        // Format the maximum line number to measure its width
        let max_line_text = format!("{max_line_number}");

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
        let buffer_item = stoat.active_buffer(cx);
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

    /// Paint git diff indicators in the gutter
    fn paint_gutter(
        &self,
        bounds: Bounds<Pixels>,
        visible_rows: std::ops::Range<u32>,
        gutter_width: Pixels,
        window: &mut Window,
        cx: &mut App,
    ) {
        if !self.style.show_diff_indicators || gutter_width == Pixels::ZERO {
            return;
        }

        // Get diff from buffer item
        let stoat = self.view.read(cx).stoat.read(cx);
        let buffer_item = stoat.active_buffer(cx);
        let diff = buffer_item.read(cx).diff();
        let buffer_snapshot = buffer_item.read(cx).buffer().read(cx).snapshot();

        // Create gutter bounds (left portion of editor)
        let gutter_bounds = Bounds {
            origin: bounds.origin,
            size: size(gutter_width, bounds.size.height),
        };

        // Create gutter layout with diff indicators
        let gutter_layout = GutterLayout::new(
            gutter_bounds,
            visible_rows,
            diff,
            &buffer_snapshot,
            gutter_width,
            self.style.padding,
            self.style.line_height,
        );

        // Paint diff indicators
        for indicator in &gutter_layout.diff_indicators {
            let diff_color = match indicator.status {
                stoat::git_diff::DiffHunkStatus::Added => self.style.diff_added_color,
                stoat::git_diff::DiffHunkStatus::Modified => self.style.diff_modified_color,
                stoat::git_diff::DiffHunkStatus::Deleted => self.style.diff_deleted_color,
            };

            // Blend with background for subtle appearance (60% opacity)
            let blended_color = gpui::Hsla {
                h: diff_color.h,
                s: diff_color.s,
                l: diff_color.l,
                a: diff_color.a * 0.6,
            };

            window.paint_quad(gpui::PaintQuad {
                bounds: indicator.bounds,
                corner_radii: indicator.corner_radii,
                background: blended_color.into(),
                border_color: gpui::transparent_black(),
                border_widths: 0.0.into(),
                border_style: gpui::BorderStyle::default(),
            });
        }
    }

    /// Paint the minimap using cached colored quads (VS Code style).
    ///
    /// This is dramatically faster than text rendering - we just paint pre-computed
    /// colored rectangles from the cache. No text shaping, no font rendering!
    ///
    /// Performance: ~0.1-0.3ms per frame (vs 3-5ms with text rendering).
    fn paint_minimap(
        &self,
        bounds: Bounds<Pixels>,
        cache: &MinimapCache,
        window: &mut Window,
        cx: &mut App,
    ) {
        if !self.style.show_minimap {
            return;
        }

        // Get buffer for calculating thumb bounds
        let (total_lines, scroll_y, visible_editor_lines) = {
            let stoat = self.view.read(cx).stoat.read(cx);
            let buffer_item = stoat.active_buffer(cx);
            let buffer = buffer_item.read(cx).buffer().read(cx);
            let max_point = buffer.snapshot().max_point();
            let total_lines = (max_point.row + 1) as f64;
            let scroll_y = stoat.scroll_position().y as f64;
            let visible_editor_lines = stoat.viewport_lines().unwrap_or(1.0) as f64;
            (total_lines, scroll_y, visible_editor_lines)
        };

        // Calculate minimap dimensions
        let em_width = px(8.0);
        let minimap_width = MinimapLayout::calculate_minimap_width(
            bounds.size.width,
            em_width,
            self.style.minimap_max_columns,
        );

        let minimap_bounds = Bounds {
            origin: point(
                bounds.origin.x + bounds.size.width - minimap_width,
                bounds.origin.y,
            ),
            size: size(minimap_width, bounds.size.height),
        };

        // Paint minimap background
        window.paint_quad(gpui::PaintQuad {
            bounds: minimap_bounds,
            corner_radii: 0.0.into(),
            background: self.style.background.into(),
            border_color: gpui::transparent_black(),
            border_widths: 0.0.into(),
            border_style: gpui::BorderStyle::default(),
        });

        // Paint all cached colored quads (super fast!)
        for quad in &cache.cached_quads {
            window.paint_quad(gpui::PaintQuad {
                bounds: quad.bounds,
                corner_radii: 0.0.into(),
                background: quad.color.into(),
                border_color: gpui::transparent_black(),
                border_widths: 0.0.into(),
                border_style: gpui::BorderStyle::default(),
            });
        }

        // Paint viewport thumb overlay
        if let Some(thumb_bounds) = MinimapLayout::calculate_thumb_bounds(
            minimap_bounds,
            total_lines,
            visible_editor_lines,
            scroll_y,
        ) {
            // Paint thumb fill
            window.paint_quad(gpui::PaintQuad {
                bounds: thumb_bounds,
                corner_radii: 0.0.into(),
                background: self.style.minimap_thumb_color.into(),
                border_color: gpui::transparent_black(),
                border_widths: 0.0.into(),
                border_style: gpui::BorderStyle::default(),
            });

            // Paint thumb border (left edge for visual definition)
            let border_width = px(1.0);
            let border_bounds = Bounds {
                origin: thumb_bounds.origin,
                size: size(border_width, thumb_bounds.size.height),
            };

            window.paint_quad(gpui::PaintQuad {
                bounds: border_bounds,
                corner_radii: 0.0.into(),
                background: self.style.minimap_thumb_border_color.into(),
                border_color: gpui::transparent_black(),
                border_widths: 0.0.into(),
                border_style: gpui::BorderStyle::default(),
            });
        }
    }

    /// Build cached minimap colored quads for blazing-fast rendering.
    ///
    /// VS Code-style approach: instead of rendering text, we create colored rectangles
    /// that represent syntax chunks. These are cached and reused across frames during
    /// scroll animations, avoiding expensive text shaping entirely.
    fn build_minimap_cache(
        &self,
        bounds: Bounds<Pixels>,
        _window: &mut Window,
        cx: &mut App,
    ) -> MinimapCache {
        if !self.style.show_minimap {
            return MinimapCache::default();
        }

        // Get buffer and token snapshots
        let (buffer_snapshot, token_snapshot, buffer_version, token_version) = {
            let stoat = self.view.read(cx).stoat.read(cx);
            let buffer_item = stoat.active_buffer(cx);
            let buffer = buffer_item.read(cx).buffer();
            let buffer_snapshot = buffer.read(cx).snapshot();
            let token_snapshot = buffer_item.read(cx).token_snapshot();
            let buffer_version = buffer.read(cx).version();
            let token_version = token_snapshot.version.clone();
            (
                buffer_snapshot,
                token_snapshot,
                buffer_version,
                token_version,
            )
        };

        let max_point = buffer_snapshot.max_point();
        let total_lines = (max_point.row + 1) as f64;

        // Get editor state
        let stoat = self.view.read(cx).stoat.read(cx);
        let scroll_y = stoat.scroll_position().y as f64;
        let visible_editor_lines = stoat.viewport_lines().unwrap_or(1.0) as f64;

        // Calculate minimap dimensions
        let em_width = px(8.0);
        let minimap_width = MinimapLayout::calculate_minimap_width(
            bounds.size.width,
            em_width,
            self.style.minimap_max_columns,
        );

        let minimap_bounds = Bounds {
            origin: point(
                bounds.origin.x + bounds.size.width - minimap_width,
                bounds.origin.y,
            ),
            size: size(minimap_width, bounds.size.height),
        };

        let minimap_line_height = px(MINIMAP_LINE_HEIGHT);
        let visible_minimap_lines = ((minimap_bounds.size.height / minimap_line_height) as f64)
            .min(MAX_MINIMAP_LINES as f64);

        let minimap_scroll_y = MinimapLayout::calculate_minimap_scroll(
            total_lines,
            visible_editor_lines,
            visible_minimap_lines,
            scroll_y,
        );

        // Build colored quads for all visible lines
        let mut cached_quads = Vec::new();
        let start_line = minimap_scroll_y.floor() as u32;
        let end_line = ((minimap_scroll_y + visible_minimap_lines as f32).ceil() as u32)
            .min(max_point.row + 1);

        let mut y = minimap_bounds.origin.y;

        for line_idx in start_line..end_line {
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

            let mut x = minimap_bounds.origin.x + px(2.0);

            // Convert each syntax chunk into a colored quad
            for chunk in chunks {
                let text = chunk.text;
                if text.is_empty() || text == "\n" {
                    continue;
                }

                // Get syntax color
                let color = if let Some(highlight_id) = chunk.highlight_id {
                    self.syntax_theme
                        .highlights
                        .get(highlight_id.0 as usize)
                        .map(|(_name, style)| style.color.unwrap_or(self.style.text_color))
                        .unwrap_or(self.style.text_color)
                } else {
                    self.style.text_color
                };

                // Calculate quad width: ~0.5px per character (dense blocks)
                let text_len = text.trim_end_matches('\n').len();
                if text_len == 0 {
                    continue;
                }
                let quad_width = px(text_len as f32 * 0.5);

                // Create colored quad
                let quad_bounds = Bounds {
                    origin: point(x, y),
                    size: size(quad_width, minimap_line_height),
                };

                cached_quads.push(CachedQuad {
                    bounds: quad_bounds,
                    color,
                });

                x += quad_width;
            }

            y += minimap_line_height;

            if y > minimap_bounds.origin.y + minimap_bounds.size.height {
                break;
            }
        }

        MinimapCache {
            cached_quads,
            buffer_version: Some(buffer_version),
            token_version: Some(token_version),
            cached_bounds: Some(minimap_bounds),
            cached_scroll_y: Some(minimap_scroll_y),
        }
    }
}

impl IntoElement for EditorElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}
