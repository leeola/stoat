//! Minimal EditorElement for stoat
//!
//! Simplified version that just renders text with syntax highlighting.
//! No gutter, no mouse handling, no complex layout - just get text visible.

use crate::{
    editor_style::EditorStyle, editor_view::EditorView, gutter::GutterLayout,
    syntax::HighlightedChunks,
};
use gpui::{
    point, px, relative, size, App, Bounds, CursorStyle, Element, ElementId, Entity, Font,
    FontStyle, FontWeight, GlobalElementId, Hitbox, HitboxBehavior, InspectorElementId,
    IntoElement, LayoutId, ParentElement, Pixels, SharedString, Style, Styled, TextRun, Window,
};

pub struct EditorElement {
    view: Entity<EditorView>,
    style: EditorStyle,
}

impl EditorElement {
    pub fn new(view: Entity<EditorView>, style: EditorStyle) -> Self {
        eprintln!(
            "[PERF] EditorElement::new() called - entity_id={:?}",
            view.entity_id()
        );

        Self { view, style }
    }
}

impl Element for EditorElement {
    type RequestLayoutState = ();
    type PrepaintState = Option<MinimapLayout>;

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
        // Detect if this EditorElement is rendering a minimap
        let is_minimap = self.view.read(cx).stoat.read(cx).is_minimap();

        eprintln!(
            "[PERF] EditorElement::prepaint() - is_minimap={}, entity_id={:?}",
            is_minimap,
            self.view.entity_id()
        );

        // Minimap should not render a nested minimap
        if is_minimap {
            return None;
        }

        // Get font size for measurements
        let font_size = self.style.font_size;

        // Create font
        let font = Font {
            family: SharedString::from("Menlo"),
            features: Default::default(),
            weight: FontWeight::NORMAL,
            style: FontStyle::Normal,
            fallbacks: None,
        };

        // Measure em_width for minimap calculations
        let em_width = {
            let text_run = TextRun {
                len: 1,
                font: font.clone(),
                color: self.style.text_color,
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            let shaped = window.text_system().shape_line(
                SharedString::from("M"),
                font_size,
                &[text_run],
                None,
            );
            shaped.width
        };

        // Get buffer snapshot to calculate total lines
        let max_point = {
            let stoat = self.view.read(cx).stoat.read(cx);
            let buffer_item = stoat.active_buffer(cx);
            buffer_item
                .read(cx)
                .buffer()
                .read(cx)
                .snapshot()
                .max_point()
        };

        // Calculate visible lines based on bounds
        let visible_lines =
            ((bounds.size.height - self.style.padding * 2.0) / self.style.line_height).floor();

        // Get scroll position
        let scroll_y = self.view.read(cx).stoat.read(cx).scroll_position().y;

        // Calculate gutter width
        let gutter_width = self.calculate_gutter_width(max_point.row + 1, window);

        // Layout minimap (only allowed during prepaint phase!)
        self.layout_minimap(
            bounds,
            gutter_width,
            max_point.row + 1,
            visible_lines,
            scroll_y,
            em_width,
            window,
            cx,
        )
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
        // Detect if this EditorElement is rendering a minimap
        let is_minimap = self.view.read(cx).stoat.read(cx).is_minimap();

        // Apply minimap-specific styling (following Zed's approach)
        let (font_size, line_height, font_weight) = if is_minimap {
            // Minimap uses tiny font with BLACK weight (900) to make 2px text visible
            (
                px(crate::minimap::MINIMAP_FONT_SIZE),
                px(crate::minimap::MINIMAP_LINE_HEIGHT),
                FontWeight(crate::minimap::MINIMAP_FONT_WEIGHT),
            )
        } else {
            (
                self.style.font_size,
                self.style.line_height,
                FontWeight::NORMAL,
            )
        };

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

        // Create font (use BLACK weight for minimap to make tiny text visible)
        let font = Font {
            family: SharedString::from("Menlo"),
            features: Default::default(),
            weight: font_weight,
            style: FontStyle::Normal,
            fallbacks: None,
        };

        // Calculate visible range
        let max_point = buffer_snapshot.max_point();
        let visible_lines = ((bounds.size.height - self.style.padding * 2.0) / line_height).floor();
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

        // Calculate gutter width (minimap has no gutter)
        let gutter_width = if is_minimap {
            Pixels::ZERO
        } else {
            self.calculate_gutter_width(max_point.row + 1, window)
        };

        // Use scroll position as offset
        let scroll_offset = scroll_y.floor() as u32;

        // Track line positions for cursor rendering
        let mut line_positions: Vec<(u32, Pixels)> = Vec::new();

        // Render visible lines starting from scroll_offset
        let mut y = bounds.origin.y + self.style.padding;

        let start_line = scroll_offset;
        let end_line = (start_line + max_lines).min(max_point.row + 1);

        eprintln!(
            "[PERF] EditorElement::paint() - is_minimap={}, rendering lines {}..{} ({} lines)",
            is_minimap,
            start_line,
            end_line,
            end_line - start_line
        );

        let mut highlighted_lines = 0;
        let mut shaped_lines = 0;

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
            highlighted_lines += 1;
            let chunks = HighlightedChunks::new(
                line_start..line_end_row,
                &buffer_snapshot,
                &token_snapshot,
                &self.style.highlight_map,
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
                    self.style
                        .syntax_theme
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
                    shaped_lines += 1;
                    let shaped = window.text_system().shape_line(
                        SharedString::from(line_text),
                        font_size,
                        &runs,
                        None,
                    );

                    let x = bounds.origin.x + gutter_width + self.style.padding;
                    if let Err(e) = shaped.paint(point(x, y), line_height, window, cx) {
                        tracing::error!("Failed to paint line {}: {:?}", line_idx, e);
                    }
                }
            }

            y += line_height;

            // Stop if we've gone past the visible area
            if y > bounds.origin.y + bounds.size.height {
                break;
            }
        }

        eprintln!(
            "[PERF] EditorElement::paint() - is_minimap={}, highlighted {} lines, shaped {} lines",
            is_minimap, highlighted_lines, shaped_lines
        );

        // Skip gutter and cursor rendering for minimap
        if !is_minimap {
            // Paint git diff indicators in gutter (behind line numbers)
            self.paint_gutter(bounds, start_line..end_line, gutter_width, window, cx);

            // Paint line numbers in gutter
            self.paint_line_numbers(bounds, &line_positions, gutter_width, window, cx);

            // Paint cursor on top of text
            self.paint_cursor(bounds, &line_positions, gutter_width, window, cx);

            // Paint minimap and thumb overlay (if we have a layout from prepaint)
            if let Some(layout) = prepaint.take() {
                self.paint_minimap(layout, window, cx);
            }
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

    // ==== Minimap Methods (Following Zed's Implementation) ====

    /// Calculate minimap width based on editor dimensions.
    ///
    /// Following Zed's formula: 15% of text width, constrained by min/max columns.
    /// Returns `None` if minimap should be hidden.
    fn get_minimap_width(
        &self,
        text_width: Pixels,
        em_width: Pixels,
        max_columns: f32,
        cx: &App,
    ) -> Option<Pixels> {
        if !self.style.show_minimap {
            return None;
        }

        // Get minimap view (checking it exists)
        let _minimap_view = self.view.read(cx).minimap_view()?;

        // Minimap font size is tiny (2-3px) - calculate ratio from raw constants
        let font_ratio = crate::minimap::MINIMAP_FONT_SIZE / 14.0; // Assuming 14px base editor font

        // Scale em_width proportionally to font size ratio
        let minimap_em_width = em_width * font_ratio;

        // Width is 15% of text width, capped by max columns
        let minimap_width =
            (text_width * crate::minimap::MINIMAP_WIDTH_PCT).min(minimap_em_width * max_columns);

        // Must be at least min columns wide
        let min_width = minimap_em_width * crate::minimap::MINIMAP_MIN_WIDTH_COLUMNS;
        (minimap_width >= min_width).then_some(minimap_width)
    }

    /// Calculate minimap scroll position based on editor scroll.
    ///
    /// Zed's proportional scroll algorithm - minimap scrolls to keep
    /// the viewport indicator aligned with the visible editor region.
    fn calculate_minimap_scroll(
        total_lines: f64,
        visible_editor_lines: f64,
        visible_minimap_lines: f64,
        editor_scroll_y: f64,
    ) -> f64 {
        let non_visible_lines = (total_lines - visible_editor_lines).max(0.0);

        if non_visible_lines == 0.0 {
            // Entire document fits in viewport
            return 0.0;
        }

        // Calculate scroll percentage
        let scroll_percentage = (editor_scroll_y / non_visible_lines).clamp(0.0, 1.0);

        // Apply to minimap's scrollable range
        scroll_percentage * (total_lines - visible_minimap_lines).max(0.0)
    }

    /// Calculate thumb bounds for viewport indicator.
    ///
    /// The thumb shows which portion of the file is visible in the main editor.
    fn calculate_thumb_bounds(
        minimap_bounds: Bounds<Pixels>,
        total_lines: f64,
        visible_editor_lines: f64,
        editor_scroll_y: f64,
    ) -> Option<Bounds<Pixels>> {
        // No thumb if entire document is visible
        if total_lines <= visible_editor_lines {
            return None;
        }

        let minimap_height = minimap_bounds.size.height;

        // Thumb height as proportion of minimap height
        let thumb_height_ratio = (visible_editor_lines / total_lines).clamp(0.0, 1.0);
        let thumb_height = minimap_height * thumb_height_ratio as f32;

        // Thumb position based on scroll percentage
        let max_scroll = (total_lines - visible_editor_lines).max(0.0);
        let scroll_ratio = if max_scroll > 0.0 {
            (editor_scroll_y / max_scroll).clamp(0.0, 1.0)
        } else {
            0.0
        };

        let max_thumb_y = minimap_height - thumb_height;
        let thumb_y = minimap_bounds.origin.y + (max_thumb_y * scroll_ratio as f32);

        Some(Bounds {
            origin: point(minimap_bounds.origin.x, thumb_y),
            size: size(minimap_bounds.size.width, thumb_height),
        })
    }

    /// Layout the minimap editor and calculate thumb bounds (following Zed's architecture).
    ///
    /// Uses `layout_as_root` + `with_absolute_element_offset` instead of Element lifecycle.
    fn layout_minimap(
        &self,
        bounds: Bounds<Pixels>,
        gutter_width: Pixels,
        total_lines: u32,
        visible_editor_lines: f32,
        editor_scroll_y: f32,
        em_width: Pixels,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<MinimapLayout> {
        // Calculate text width (editor width minus gutter)
        let text_width = bounds.size.width - gutter_width - self.style.padding * 2.0;

        // Calculate minimap width
        let minimap_width =
            self.get_minimap_width(text_width, em_width, self.style.minimap_max_columns, cx)?;

        // Get minimap view
        let minimap_view = self.view.read(cx).minimap_view()?.clone();

        // Calculate minimap bounds (top-right corner, full height)
        let minimap_bounds = Bounds {
            origin: point(
                bounds.origin.x + bounds.size.width - minimap_width,
                bounds.origin.y,
            ),
            size: size(minimap_width, bounds.size.height),
        };

        // Create hitbox for mouse interaction
        let minimap_hitbox = window.insert_hitbox(minimap_bounds, HitboxBehavior::Normal);

        // Calculate minimap metrics
        let minimap_line_height = px(crate::minimap::MINIMAP_LINE_HEIGHT);
        let visible_minimap_lines =
            (minimap_bounds.size.height / minimap_line_height).floor() as f64;

        // Calculate minimap scroll using Zed's algorithm
        let minimap_scroll_top = Self::calculate_minimap_scroll(
            total_lines as f64,
            visible_editor_lines as f64,
            visible_minimap_lines,
            editor_scroll_y as f64,
        );

        // Set scroll position on minimap Stoat
        minimap_view.update(cx, |minimap_editor_view, cx| {
            minimap_editor_view.stoat.update(cx, |minimap_stoat, _cx| {
                minimap_stoat.set_scroll_position(point(0.0, minimap_scroll_top as f32));
                minimap_stoat.set_viewport_lines(visible_minimap_lines as f32);
            });
        });

        // Calculate thumb bounds
        let thumb_bounds = Self::calculate_thumb_bounds(
            minimap_bounds,
            total_lines as f64,
            visible_editor_lines as f64,
            editor_scroll_y as f64,
        );

        // Create minimap element (passing Entity<EditorView> directly)
        // Following Zed's approach: GPUI calls Render trait automatically
        let mut minimap = gpui::div()
            .size_full()
            .child(minimap_view)
            .into_any_element();

        // Layout with explicit size (Zed's approach - NOT using Element lifecycle)
        minimap.layout_as_root(minimap_bounds.size.into(), window, cx);

        // Prepaint with absolute positioning (Zed's approach)
        window.with_absolute_element_offset(minimap_bounds.origin, |window| {
            minimap.prepaint(window, cx)
        });

        Some(MinimapLayout {
            minimap,
            minimap_hitbox,
            thumb_bounds,
            minimap_line_height,
            minimap_scroll_top,
        })
    }

    /// Paint the minimap and viewport thumb overlay (following Zed's architecture).
    ///
    /// Uses `paint_layer` for proper compositing.
    fn paint_minimap(&mut self, mut layout: MinimapLayout, window: &mut Window, cx: &mut App) {
        // Paint in a layer for proper compositing (following Zed)
        window.paint_layer(layout.minimap_hitbox.bounds, |window| {
            // Paint the minimap element (already laid out and prepainted)
            layout.minimap.paint(window, cx);

            // Paint viewport thumb overlay on top
            if let Some(thumb_bounds) = layout.thumb_bounds {
                // Paint thumb fill
                window.paint_quad(gpui::PaintQuad {
                    bounds: thumb_bounds,
                    corner_radii: 0.0.into(),
                    background: self.style.minimap_thumb_color.into(),
                    border_color: gpui::transparent_black(),
                    border_widths: 0.0.into(),
                    border_style: gpui::BorderStyle::default(),
                });

                // Paint thumb border (left edge)
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
        });

        // Set cursor style when hovering over minimap (basic hover detection)
        window.set_cursor_style(CursorStyle::Arrow, &layout.minimap_hitbox);
    }
}

/// Layout information for the minimap (following Zed's architecture).
///
/// Contains the fully laid-out and prepainted minimap element along with
/// hitbox for mouse interaction and thumb overlay bounds.
pub struct MinimapLayout {
    /// The minimap editor element (already laid out and prepainted via layout_as_root)
    pub minimap: gpui::AnyElement,
    /// Hitbox for mouse interaction (hover, click, drag)
    pub minimap_hitbox: Hitbox,
    /// Bounds of the viewport thumb overlay (None if entire file visible)
    pub thumb_bounds: Option<Bounds<Pixels>>,
    /// Line height for minimap text rendering
    pub minimap_line_height: Pixels,
    /// Scroll position for the minimap (in lines)
    pub minimap_scroll_top: f64,
}

impl IntoElement for EditorElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}
