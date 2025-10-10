//! Minimap view - a persistent entity that renders the file at tiny scale.
//!
//! Following Zed's architecture, the minimap is a separate Entity that persists
//! between frames, allowing GPUI's element system to cache text shaping automatically.

use crate::{
    editor_style::EditorStyle,
    minimap::{CachedLine, MinimapLayout, MINIMAP_FONT_SIZE, MINIMAP_LINE_HEIGHT},
    syntax::{HighlightMap, HighlightedChunks, SyntaxTheme},
};
use clock::Global;
use gpui::{
    point, px, size, App, Bounds, Context, Element, ElementId, Entity, Font, FontStyle, FontWeight,
    GlobalElementId, InspectorElementId, IntoElement, LayoutId, Pixels, Render, SharedString,
    Style, TextRun, Window,
};
use stoat::Stoat;

/// MinimapView is a persistent entity that renders the minimap.
///
/// Unlike EditorElement which is recreated every frame, this entity persists
/// and maintains its own state. GPUI's element system automatically caches
/// the shaped text between frames.
pub struct MinimapView {
    parent_stoat: Entity<Stoat>,
    style: EditorStyle,
    syntax_theme: SyntaxTheme,
    highlight_map: HighlightMap,
    /// Calculated scroll position for the minimap (set by parent)
    scroll_y: f64,
    /// Cached pre-shaped text lines for fast rendering
    cached_lines: Vec<CachedLine>,
    /// Buffer version when cache was built
    cached_buffer_version: Option<Global>,
    /// Token version when cache was built
    cached_token_version: Option<Global>,
}

impl MinimapView {
    pub fn new(parent_stoat: Entity<Stoat>, _cx: &mut Context<Self>) -> Self {
        let syntax_theme = SyntaxTheme::default();
        let highlight_map = HighlightMap::new(&syntax_theme);

        Self {
            parent_stoat,
            style: EditorStyle::default(),
            syntax_theme,
            highlight_map,
            scroll_y: 0.0,
            cached_lines: Vec::new(),
            cached_buffer_version: None,
            cached_token_version: None,
        }
    }

    /// Update minimap scroll based on parent editor's scroll position
    pub fn set_scroll_from_parent(
        &mut self,
        editor_scroll_y: f64,
        visible_editor_lines: f64,
        total_lines: f64,
        visible_minimap_lines: f64,
    ) {
        self.scroll_y = MinimapLayout::calculate_minimap_scroll(
            total_lines,
            visible_editor_lines,
            visible_minimap_lines,
            editor_scroll_y,
        ) as f64;
    }

    /// Rebuild the shaped text cache for minimap lines.
    ///
    /// This shapes all lines in the buffer (up to MAX_MINIMAP_LINES) and stores
    /// the ShapedLines for reuse. Text shaping is expensive, so we only do it when
    /// buffer content or syntax highlighting changes - NOT on scroll!
    fn rebuild_cache(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Get buffer and token snapshots
        let stoat = self.parent_stoat.read(cx);
        let buffer_item = stoat.active_buffer(cx);
        let buffer = buffer_item.read(cx).buffer().read(cx);
        let buffer_snapshot = buffer.snapshot();
        let token_snapshot = buffer_item.read(cx).token_snapshot();
        let max_point = buffer_snapshot.max_point();

        // Store versions for cache validation
        self.cached_buffer_version = Some(buffer.version());
        self.cached_token_version = Some(token_snapshot.version.clone());

        // Font for minimap
        let minimap_font = Font {
            family: SharedString::from("Menlo"),
            features: Default::default(),
            weight: FontWeight::NORMAL,
            style: FontStyle::Normal,
            fallbacks: None,
        };

        // Shape all lines (or limit to reasonable number)
        self.cached_lines.clear();
        let total_lines = max_point.row + 1;
        let lines_to_cache = total_lines.min(500); // Cache up to 500 lines

        for line_idx in 0..lines_to_cache {
            let line_start = buffer_snapshot.point_to_offset(text::Point::new(line_idx, 0));
            let line_end = if line_idx == max_point.row {
                buffer_snapshot.len()
            } else {
                buffer_snapshot.point_to_offset(text::Point::new(line_idx + 1, 0))
            };

            // Get highlighted chunks
            let chunks = HighlightedChunks::new(
                line_start..line_end,
                &buffer_snapshot,
                &token_snapshot,
                &self.highlight_map,
            );

            // Build line text and runs
            let mut line_text = String::new();
            let mut runs = Vec::new();

            for chunk in chunks {
                let text = chunk.text;
                if text.is_empty() {
                    continue;
                }

                let color = if let Some(highlight_id) = chunk.highlight_id {
                    self.syntax_theme
                        .highlights
                        .get(highlight_id.0 as usize)
                        .map(|(_name, hl_style)| hl_style.color.unwrap_or(self.style.text_color))
                        .unwrap_or(self.style.text_color)
                } else {
                    self.style.text_color
                };

                line_text.push_str(text);
                runs.push(TextRun {
                    len: text.len(),
                    font: minimap_font.clone(),
                    color,
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                });
            }

            // Strip trailing newline
            if line_text.ends_with('\n') {
                line_text.pop();
                if let Some(last_run) = runs.last_mut() {
                    if last_run.len > 0 {
                        last_run.len -= 1;
                    }
                }
            }

            // Shape the line
            let shaped = if !line_text.is_empty() {
                window.text_system().shape_line(
                    SharedString::from(line_text),
                    px(MINIMAP_FONT_SIZE),
                    &runs,
                    None,
                )
            } else {
                // Empty line - shape empty string for consistent layout
                window.text_system().shape_line(
                    SharedString::from(""),
                    px(MINIMAP_FONT_SIZE),
                    &[],
                    None,
                )
            };

            self.cached_lines.push(CachedLine { line_idx, shaped });
        }
    }
}

impl Render for MinimapView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity().clone();
        MinimapElement { view: entity }
    }
}

/// The element that renders the minimap.
///
/// This is recreated each frame, but it reads from the persistent MinimapView.
/// GPUI caches the shaped text automatically in its element system.
struct MinimapElement {
    view: Entity<MinimapView>,
}

impl Element for MinimapElement {
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
        let mut style = Style::default();
        style.size.width = gpui::relative(1.).into();
        style.size.height = gpui::relative(1.).into();
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
        ()
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
        // Check if cache needs rebuilding
        let needs_rebuild = {
            let view = self.view.read(cx);
            let stoat = view.parent_stoat.read(cx);
            let buffer_item = stoat.active_buffer(cx);
            let buffer = buffer_item.read(cx).buffer().read(cx);
            let token_snapshot = buffer_item.read(cx).token_snapshot();

            // Cache is invalid if versions don't match
            view.cached_buffer_version.as_ref() != Some(&buffer.version())
                || view.cached_token_version.as_ref() != Some(&token_snapshot.version)
        };

        // Rebuild cache if needed (only on buffer/token changes, NOT on scroll!)
        if needs_rebuild {
            self.view.update(cx, |view, cx| {
                view.rebuild_cache(window, cx);
            });
        }

        // Read minimap view data for painting
        let (editor_style, scroll_y, cached_lines) = {
            let view = self.view.read(cx);
            (view.style.clone(), view.scroll_y, view.cached_lines.clone())
        };

        // Paint minimap background
        window.paint_quad(gpui::PaintQuad {
            bounds,
            corner_radii: 0.0.into(),
            background: editor_style.background.into(),
            border_color: gpui::transparent_black(),
            border_widths: 0.0.into(),
            border_style: gpui::BorderStyle::default(),
        });

        // Get editor state for thumb rendering
        let (total_lines, editor_scroll_y, visible_editor_lines) = {
            let view = self.view.read(cx);
            let stoat = view.parent_stoat.read(cx);
            let buffer_item = stoat.active_buffer(cx);
            let buffer = buffer_item.read(cx).buffer().read(cx);
            let max_point = buffer.snapshot().max_point();
            let total_lines = (max_point.row + 1) as f64;
            let scroll_y = stoat.scroll_position().y as f64;
            let visible_lines = stoat.viewport_lines().unwrap_or(1.0) as f64;
            (total_lines, scroll_y, visible_lines)
        };

        // Calculate visible minimap lines
        let visible_minimap_lines = (bounds.size.height / px(MINIMAP_LINE_HEIGHT)) as f64;

        // Calculate which lines to render
        let start_line = scroll_y.floor() as u32;
        let end_line = (start_line + visible_minimap_lines.ceil() as u32).min(total_lines as u32);

        // Paint cached lines (no text shaping during scroll!)
        let mut y = bounds.origin.y;
        for line_idx in start_line..end_line {
            // Find cached line
            if let Some(cached_line) = cached_lines.iter().find(|cl| cl.line_idx == line_idx) {
                let x = bounds.origin.x + px(2.0);
                if let Err(e) =
                    cached_line
                        .shaped
                        .paint(point(x, y), px(MINIMAP_LINE_HEIGHT), window, cx)
                {
                    tracing::error!("Failed to paint minimap line {}: {:?}", line_idx, e);
                }
            }

            y += px(MINIMAP_LINE_HEIGHT);

            if y > bounds.origin.y + bounds.size.height {
                break;
            }
        }

        // Paint viewport thumb overlay
        if let Some(thumb_bounds) = MinimapLayout::calculate_thumb_bounds(
            bounds,
            total_lines,
            visible_editor_lines,
            editor_scroll_y,
        ) {
            // Paint thumb fill
            window.paint_quad(gpui::PaintQuad {
                bounds: thumb_bounds,
                corner_radii: 0.0.into(),
                background: editor_style.minimap_thumb_color.into(),
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
                background: editor_style.minimap_thumb_border_color.into(),
                border_color: gpui::transparent_black(),
                border_widths: 0.0.into(),
                border_style: gpui::BorderStyle::default(),
            });
        }
    }
}

impl IntoElement for MinimapElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}
