//! Minimal EditorElement for stoat
//!
//! Simplified version that just renders text with syntax highlighting.
//! No gutter, no mouse handling, no complex layout - just get text visible.

use crate::{
    editor_style::EditorStyle, editor_view::EditorView, gutter::GutterLayout,
    syntax::HighlightedChunks,
};
use gpui::{
    point, px, relative, size, App, Bounds, Element, ElementId, Entity, Font, FontStyle,
    FontWeight, GlobalElementId, InspectorElementId, IntoElement, LayoutId, Pixels, SharedString,
    Style, TextRun, Window,
};
use std::sync::Arc;
use text::ToPoint;

pub struct EditorElement {
    view: Entity<EditorView>,
    style: Arc<EditorStyle>,
}

impl EditorElement {
    pub fn new(view: Entity<EditorView>, style: Arc<EditorStyle>) -> Self {
        Self { view, style }
    }
}

impl Element for EditorElement {
    type RequestLayoutState = ();
    type PrepaintState = EditorPrepaintState;

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
        // Request a simple full-size layout for the main editor
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
        let prepaint_start = std::time::Instant::now();

        // Detect if this EditorElement is rendering a minimap (for conditional gutter rendering)
        let is_minimap = self.view.read(cx).stoat.read(cx).is_minimap();

        // Get font and sizing from style (persistent across frames for GPUI's LineLayoutCache)
        // Using cached font ensures stable font ID for cache hits
        let font = self.style.font.clone();
        let font_size = self.style.font_size;
        let line_height = self.style.line_height;

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

        // Calculate visible range
        let max_point = buffer_snapshot.max_point();
        // Calculate ACTUAL visible lines including fractional lines for accurate thumb sizing
        // Don't floor here - use the precise fractional value for viewport_lines
        let visible_lines_precise = (bounds.size.height - self.style.padding * 2.0) / line_height;
        let max_lines = visible_lines_precise.floor() as u32;

        // Set viewport lines on Stoat and get scroll position
        let stoat_entity = self.view.read(cx).stoat.clone();
        let scroll_y = stoat_entity.update(cx, |stoat, _cx| {
            stoat.set_viewport_lines(visible_lines_precise);
            stoat.update_scroll_animation();
            stoat.scroll_position().y
        });

        // Calculate gutter width (minimap has no gutter)
        let gutter_width = if is_minimap {
            Pixels::ZERO
        } else {
            self.calculate_gutter_width(max_point.row + 1, window)
        };

        // Use scroll position as offset
        let scroll_offset = scroll_y.floor() as u32;

        // Calculate visible line range
        let start_line = scroll_offset;
        let end_line = (start_line + max_lines).min(max_point.row + 1);

        // ===== EXPENSIVE WORK: Syntax highlighting + text shaping =====
        // Do this ONCE in prepaint, cache results for fast paint()
        // Following Zed's architecture: ONE iterator for all visible lines
        let mut line_layouts = Vec::with_capacity((end_line - start_line) as usize);

        // Detailed timing to diagnose cache effectiveness
        let mut total_highlight_time = std::time::Duration::ZERO;
        let mut total_shape_time = std::time::Duration::ZERO;

        let highlight_start = std::time::Instant::now();

        // Calculate byte offset range for ENTIRE visible region (not per-line)
        let start_offset = buffer_snapshot.point_to_offset(text::Point::new(start_line, 0));
        let end_offset = if end_line > max_point.row {
            buffer_snapshot.len()
        } else {
            buffer_snapshot.point_to_offset(text::Point::new(end_line, 0))
        };

        // Create ONE iterator for ALL visible lines (Zed's approach)
        let chunks = HighlightedChunks::new(
            start_offset..end_offset,
            &buffer_snapshot,
            &token_snapshot,
            &self.style.highlight_map,
        );

        // Process all chunks, detecting line boundaries via newlines
        let mut line_text = String::new();
        let mut runs = Vec::new();
        let mut current_line_idx = start_line;
        let mut y = bounds.origin.y + self.style.padding;

        for chunk in chunks {
            // Get color for this chunk (outside the split loop to avoid recomputation)
            let color = if let Some(highlight_id) = chunk.highlight_id {
                self.style
                    .syntax_theme
                    .highlights
                    .get(highlight_id.0 as usize)
                    .map(|(_name, style)| style.color.unwrap_or(self.style.text_color))
                    .unwrap_or(self.style.text_color)
            } else {
                self.style.text_color
            };

            // Split chunk on '\n' to detect line boundaries (following Zed)
            for (split_ix, line_chunk) in chunk.text.split('\n').enumerate() {
                if split_ix > 0 {
                    // We hit a newline - shape the completed line (including empty lines)
                    let shape_start = std::time::Instant::now();
                    let shaped = window.text_system().shape_line(
                        SharedString::from(std::mem::take(&mut line_text)),
                        font_size,
                        &runs,
                        None,
                    );
                    total_shape_time += shape_start.elapsed();

                    line_layouts.push(ShapedLineLayout {
                        line_idx: current_line_idx,
                        shaped,
                        y_position: y,
                    });

                    // Reset for next line
                    line_text.clear();
                    runs.clear();
                    current_line_idx += 1;
                    y += line_height;

                    // Early exit if beyond visible area
                    if y > bounds.origin.y + bounds.size.height {
                        break;
                    }
                }

                // Accumulate text for current line
                if !line_chunk.is_empty() {
                    line_text.push_str(line_chunk);
                    runs.push(TextRun {
                        len: line_chunk.len(),
                        font: font.clone(),
                        color,
                        background_color: None,
                        underline: None,
                        strikethrough: None,
                    });
                }
            }
        }

        // Shape final line if we have accumulated text (no trailing newline case)
        if !line_text.is_empty() {
            let shape_start = std::time::Instant::now();
            let shaped = window.text_system().shape_line(
                SharedString::from(std::mem::take(&mut line_text)),
                font_size,
                &runs,
                None,
            );
            total_shape_time += shape_start.elapsed();

            line_layouts.push(ShapedLineLayout {
                line_idx: current_line_idx,
                shaped,
                y_position: y,
            });
        }

        // Don't update viewport_lines here - we already set it with the precise fractional value
        // at the start of prepaint(). This ensures the thumb accurately represents the viewport
        // including any fractional line space.

        // Collect expanded diff blocks for visible range (skip for minimap)
        let diff_blocks = if is_minimap {
            Vec::new()
        } else {
            self.collect_diff_blocks(cx, &buffer_snapshot)
        };

        EditorPrepaintState {
            line_layouts,
            gutter_width,
            diff_blocks,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        // Detect if this EditorElement is rendering a minimap (for conditional gutter rendering)
        let is_minimap = self.view.read(cx).stoat.read(cx).is_minimap();

        // Get line height from style (persistent across frames for cache stability)
        let line_height = self.style.line_height;

        // Paint background
        window.paint_quad(gpui::PaintQuad {
            bounds,
            corner_radii: 0.0.into(),
            background: self.style.background.into(),
            border_color: gpui::transparent_black(),
            border_widths: 0.0.into(),
            border_style: gpui::BorderStyle::default(),
        });

        // Check if scroll animation is in progress (for requesting next frame)
        let stoat_entity = self.view.read(cx).stoat.clone();
        let is_animating = stoat_entity.read(cx).is_scroll_animating();
        if is_animating {
            let view = self.view.clone();
            window.on_next_frame(move |_, cx| {
                view.update(cx, |_, cx| cx.notify());
            });
        }

        // ===== FAST PATH: Just paint the pre-shaped lines from prepaint =====
        // All expensive work (syntax highlighting + text shaping) was done in prepaint()

        // Collect line positions for cursor/gutter rendering
        let mut line_positions: Vec<(u32, Pixels)> =
            Vec::with_capacity(prepaint.line_layouts.len());

        // Paint all pre-shaped lines (FAST: just drawing, no computation!)
        for layout in &prepaint.line_layouts {
            line_positions.push((layout.line_idx, layout.y_position));

            let x = bounds.origin.x + prepaint.gutter_width + self.style.padding;
            if let Err(e) =
                layout
                    .shaped
                    .paint(point(x, layout.y_position), line_height, window, cx)
            {
                tracing::error!("Failed to paint line {}: {:?}", layout.line_idx, e);
            }
        }

        // Skip gutter and cursor rendering for minimap
        if !is_minimap {
            // Calculate visible range for gutter
            let start_line = line_positions.first().map(|(idx, _)| *idx).unwrap_or(0);
            let end_line = line_positions.last().map(|(idx, _)| *idx + 1).unwrap_or(0);

            // Paint git diff indicators in gutter (behind line numbers)
            self.paint_gutter(
                bounds,
                start_line..end_line,
                prepaint.gutter_width,
                window,
                cx,
            );

            // Paint expanded diff blocks inline (before line numbers, behind text)
            self.paint_diff_blocks(
                bounds,
                &prepaint.diff_blocks,
                &line_positions,
                prepaint.gutter_width,
                line_height,
                window,
                cx,
            );

            // Paint line numbers in gutter
            self.paint_line_numbers(bounds, &line_positions, prepaint.gutter_width, window, cx);

            // Paint cursor on top of text
            self.paint_cursor(bounds, &line_positions, prepaint.gutter_width, window, cx);
        }
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

    /// Paint expanded diff blocks inline.
    ///
    /// Renders deleted content from git HEAD as dimmed text blocks positioned before
    /// their corresponding hunks. Uses dark red background and subtle styling to
    /// distinguish from normal code.
    fn paint_diff_blocks(
        &self,
        bounds: Bounds<Pixels>,
        diff_blocks: &[DiffBlock],
        line_positions: &[(u32, Pixels)],
        gutter_width: Pixels,
        line_height: Pixels,
        window: &mut Window,
        _cx: &mut App,
    ) {
        for block in diff_blocks {
            // Find the Y position where this block should be inserted
            // (before the line at insert_before_row)
            let insert_y = line_positions
                .iter()
                .find(|(row, _)| *row == block.insert_before_row)
                .map(|(_, y)| *y);

            let Some(base_y) = insert_y else {
                // Block's insertion point not in visible range
                continue;
            };

            // Calculate block height and position
            let block_height = line_height * (block.deleted_lines.len() as f32);
            let block_y = base_y - block_height;

            // Paint background for the deleted block (dark red with transparency)
            let block_bounds = Bounds {
                origin: point(bounds.origin.x, block_y),
                size: size(bounds.size.width, block_height),
            };

            window.paint_quad(gpui::PaintQuad {
                bounds: block_bounds,
                corner_radii: 0.0.into(),
                background: gpui::Hsla {
                    h: 0.0,  // Red hue
                    s: 0.3,  // Subtle saturation
                    l: 0.15, // Dark
                    a: 0.3,  // Transparent
                }
                .into(),
                border_color: gpui::transparent_black(),
                border_widths: 0.0.into(),
                border_style: gpui::BorderStyle::default(),
            });

            // Create font for diff text
            let font = Font {
                family: SharedString::from("Menlo"),
                features: Default::default(),
                weight: FontWeight::NORMAL,
                style: FontStyle::Normal,
                fallbacks: None,
            };

            // Dimmed color for deleted text
            let deleted_text_color = gpui::Hsla {
                h: 0.0, // Red hue
                s: 0.3, // Some saturation
                l: 0.5, // Medium lightness
                a: 0.6, // Dimmed
            };

            // Paint each deleted line with "- " prefix
            let mut y = block_y;
            for line in &block.deleted_lines {
                let line_text = format!("- {line}");
                let line_shared = SharedString::from(line_text);

                let text_run = TextRun {
                    len: line_shared.len(),
                    font: font.clone(),
                    color: deleted_text_color,
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                };

                let shaped = window.text_system().shape_line(
                    line_shared,
                    self.style.font_size,
                    &[text_run],
                    None,
                );

                let x = bounds.origin.x + gutter_width + self.style.padding;
                if let Err(e) = shaped.paint(point(x, y), line_height, window, _cx) {
                    tracing::error!("Failed to paint diff line: {:?}", e);
                }

                y += line_height;
            }
        }
    }

    /// Collect expanded diff blocks for rendering.
    ///
    /// Iterates over all expanded hunks in the buffer item and constructs [`DiffBlock`]
    /// structures containing the deleted text to display inline.
    fn collect_diff_blocks(
        &self,
        cx: &App,
        buffer_snapshot: &text::BufferSnapshot,
    ) -> Vec<DiffBlock> {
        let stoat = self.view.read(cx).stoat.read(cx);
        let buffer_item = stoat.active_buffer(cx);
        let buffer_item_ref = buffer_item.read(cx);

        let blocks: Vec<_> = buffer_item_ref
            .expanded_hunks()
            .filter_map(|(hunk_idx, hunk)| {
                // Skip if no deleted content
                if hunk.diff_base_byte_range.is_empty() {
                    return None;
                }

                // Get the row where this block should be inserted (before the hunk start)
                let insert_before_row = hunk.buffer_range.start.to_point(buffer_snapshot).row;

                // Get deleted text and split into lines
                let deleted_text = buffer_item_ref.base_text_for_hunk(hunk_idx);
                let deleted_lines: Vec<String> =
                    deleted_text.lines().map(|line| line.to_string()).collect();

                // Skip empty deleted sections
                if deleted_lines.is_empty() {
                    return None;
                }

                Some(DiffBlock {
                    insert_before_row,
                    deleted_lines,
                    status: hunk.status,
                })
            })
            .collect();

        tracing::info!("collect_diff_blocks: collected {} blocks", blocks.len());
        blocks
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
}

/// Block of deleted text to show inline above a diff hunk.
///
/// Represents deleted content from git HEAD that can be expanded inline to show
/// what was removed. Each block is positioned before a specific row and contains
/// the formatted deleted lines.
#[derive(Debug, Clone)]
pub struct DiffBlock {
    /// Row where block should be inserted (before this row)
    pub insert_before_row: u32,
    /// Lines of deleted text from HEAD
    pub deleted_lines: Vec<String>,
    /// Original hunk status for styling
    pub status: stoat::git_diff::DiffHunkStatus,
}

/// Prepaint state for editor rendering (following Zed's architecture).
///
/// Caches expensive computations (syntax highlighting, text shaping) done in prepaint
/// so that paint() can be fast and just draw the pre-computed results.
pub struct EditorPrepaintState {
    /// Pre-shaped line layouts for visible lines
    pub line_layouts: Vec<ShapedLineLayout>,
    /// Gutter width for positioning
    pub gutter_width: Pixels,
    /// Expanded diff blocks to render inline
    pub diff_blocks: Vec<DiffBlock>,
}

/// A single line that has been shaped and is ready to paint.
///
/// Contains the line index, pre-shaped text, and Y position for fast painting.
pub struct ShapedLineLayout {
    /// Line index in the buffer
    pub line_idx: u32,
    /// Pre-shaped text from GPUI (already has syntax highlighting colors)
    pub shaped: gpui::ShapedLine,
    /// Y position where this line should be painted
    pub y_position: Pixels,
}

impl IntoElement for EditorElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}
