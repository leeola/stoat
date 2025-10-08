//! File finder modal for quick file navigation.
//!
//! Renders a modal overlay for fuzzy file finding. All state management and input handling
//! happens in [`stoat_v4::Stoat`] core - this is just the presentation layer.

use crate::syntax::{HighlightMap, HighlightedChunks, SyntaxTheme};
use gpui::{
    div, point, prelude::FluentBuilder, px, relative, rgb, rgba, App, Bounds, Element, Font,
    FontStyle, FontWeight, GlobalElementId, InspectorElementId, InteractiveElement, IntoElement,
    LayoutId, PaintQuad, ParentElement, Pixels, RenderOnce, ScrollHandle, ShapedLine, SharedString,
    StatefulInteractiveElement, Style, Styled, TextRun, Window,
};
use std::{path::PathBuf, sync::OnceLock};
use stoat_v4::PreviewData;

/// File finder modal renderer.
///
/// Stateless component that renders file finder UI. All interaction is handled through
/// the action system in file_finder mode.
#[derive(IntoElement)]
pub struct FileFinder {
    query: String,
    files: Vec<PathBuf>,
    selected: usize,
    preview: Option<PreviewData>,
    scroll_handle: ScrollHandle,
}

impl FileFinder {
    /// Create a new file finder renderer with the given state.
    pub fn new(
        query: String,
        files: Vec<PathBuf>,
        selected: usize,
        preview: Option<PreviewData>,
        scroll_handle: ScrollHandle,
    ) -> Self {
        Self {
            query,
            files,
            selected,
            preview,
            scroll_handle,
        }
    }

    /// Render the input box showing the current query.
    fn render_input(&self) -> impl IntoElement {
        let query = self.query.clone();

        div()
            .p(px(8.0))
            .border_b_1()
            .border_color(rgb(0x3e3e42))
            .bg(rgb(0x252526))
            .text_color(rgb(0xd4d4d4))
            .child(if query.is_empty() {
                "Type to search files...".to_string()
            } else {
                query
            })
    }

    /// Render the list of filtered files.
    fn render_file_list(&self) -> impl IntoElement {
        let files = &self.files;
        let selected = self.selected;

        div()
            .id("file-list")
            .flex()
            .flex_col()
            .flex_1()
            .overflow_y_scroll()
            .track_scroll(&self.scroll_handle)
            .children(files.iter().enumerate().map(|(i, path)| {
                div()
                    .px(px(8.0))
                    .py(px(3.0))
                    .when(i == selected, |div| {
                        div.bg(rgb(0x3b4261)) // Blue-gray highlight for selected file
                    })
                    .text_color(rgb(0xd4d4d4))
                    .text_size(px(11.0))
                    .child(
                        path.strip_prefix("./")
                            .unwrap_or(path)
                            .to_string_lossy()
                            .to_string(),
                    )
            }))
    }

    /// Render the file preview panel with syntax highlighting.
    fn render_preview(&self) -> PreviewElement {
        PreviewElement::new(self.preview.clone())
    }
}

/// Custom element for rendering syntax-highlighted file preview.
///
/// This implements GPUI's low-level Element trait to properly render colored text
/// using TextRun and ShapedLine APIs.
struct PreviewElement {
    preview: Option<PreviewData>,
    theme: SyntaxTheme,
    highlight_map: HighlightMap,
}

/// Layout state prepared during prepaint
struct PreviewLayout {
    lines: Vec<ShapedLineWithPosition>,
    bounds: Bounds<Pixels>,
}

struct ShapedLineWithPosition {
    shaped: ShapedLine,
    position: gpui::Point<Pixels>,
}

impl PreviewElement {
    fn new(preview: Option<PreviewData>) -> Self {
        let theme = SyntaxTheme::monokai_dark();
        let highlight_map = HighlightMap::new(&theme);

        Self {
            preview,
            theme,
            highlight_map,
        }
    }
}

impl Element for PreviewElement {
    type RequestLayoutState = ();
    type PrepaintState = PreviewLayout;

    fn id(&self) -> Option<gpui::ElementId> {
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
        // Request full-size layout
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
        _cx: &mut App,
    ) -> Self::PrepaintState {
        let Some(preview) = &self.preview else {
            return PreviewLayout {
                lines: Vec::new(),
                bounds,
            };
        };

        // Create buffer snapshot from preview text (avoid cloning - use reference)
        let preview_text = preview.text();
        let buffer =
            text::Buffer::new(0, text::BufferId::new(1).unwrap(), preview_text.to_string());
        let snapshot = buffer.snapshot();

        // Get tokens - use cached empty snapshot for Plain preview
        // This avoids expensive TokenMap creation on every frame
        static EMPTY_TOKENS: OnceLock<stoat_rope::TokenSnapshot> = OnceLock::new();
        let tokens = match preview {
            PreviewData::Highlighted { tokens, .. } => tokens,
            PreviewData::Plain(_) => EMPTY_TOKENS.get_or_init(|| {
                // Create minimal empty snapshot once - HighlightedChunks will return
                // chunks with no highlighting (highlight_id = None)
                let empty_buf =
                    text::Buffer::new(0, text::BufferId::new(1).unwrap(), String::new());
                stoat_rope::TokenMap::new(&empty_buf.snapshot()).snapshot()
            }),
        };

        // Font configuration
        let font = Font {
            family: ".AppleSystemUIFontMonospaced".into(),
            features: Default::default(),
            weight: FontWeight::NORMAL,
            style: FontStyle::Normal,
            fallbacks: None,
        };
        let font_size = px(12.0);
        let line_height = px(18.0);

        // Calculate viewport culling - only render visible lines
        let visible_height = f32::from(bounds.size.height);
        let max_visible_lines = (visible_height / f32::from(line_height)).ceil() as usize + 2; // +2 for buffer

        // Shape each visible line with syntax highlighting
        let mut lines = Vec::new();
        let mut y_offset = bounds.origin.y + px(12.0); // Top padding
        let mut current_offset = 0; // Track offset incrementally - O(n) instead of O(n²)

        for (line_idx, line_text) in preview_text.lines().enumerate() {
            // Viewport culling: stop if we've rendered enough visible lines
            if line_idx >= max_visible_lines {
                break;
            }

            let line_start_offset = current_offset;
            let line_end_offset = current_offset + line_text.len();

            // Build text runs with highlighting
            let highlighted_chunks = HighlightedChunks::new(
                line_start_offset..line_end_offset,
                &snapshot,
                tokens,
                &self.highlight_map,
            );

            let mut text_runs = Vec::new();
            let mut full_line_text = String::new();

            for chunk in highlighted_chunks {
                full_line_text.push_str(chunk.text);

                let highlight_style = chunk
                    .highlight_id
                    .and_then(|id| id.style(&self.theme))
                    .unwrap_or_default();

                let color = highlight_style
                    .color
                    .unwrap_or(self.theme.default_text_color);

                text_runs.push(TextRun {
                    len: chunk.text.len(),
                    font: font.clone(),
                    color,
                    background_color: highlight_style.background_color,
                    underline: None,
                    strikethrough: None,
                });
            }

            // Shape the line
            let text = if full_line_text.is_empty() {
                SharedString::from(" ")
            } else {
                SharedString::from(full_line_text)
            };

            let shaped = if text_runs.is_empty() {
                let text_run = TextRun {
                    len: text.len(),
                    font: font.clone(),
                    color: self.theme.default_text_color,
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                };
                window
                    .text_system()
                    .shape_line(text, font_size, &[text_run], None)
            } else {
                window
                    .text_system()
                    .shape_line(text, font_size, &text_runs, None)
            };

            lines.push(ShapedLineWithPosition {
                shaped,
                position: point(bounds.origin.x + px(12.0), y_offset),
            });

            y_offset += line_height;
            current_offset = line_end_offset + 1; // +1 for newline character
        }

        PreviewLayout { lines, bounds }
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
            background: self.theme.background_color.into(),
            border_color: Default::default(),
            border_widths: Default::default(),
            border_style: Default::default(),
        });

        // Paint each shaped line
        let line_height = px(18.0);
        for line in &layout.lines {
            line.shaped
                .paint(line.position, line_height, window, cx)
                .unwrap_or_else(|err| {
                    eprintln!("Failed to paint preview line: {err:?}");
                });
        }
    }
}

impl IntoElement for PreviewElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl RenderOnce for FileFinder {
    fn render(self, window: &mut Window, _cx: &mut App) -> impl IntoElement {
        // Check window width to determine if we should show preview
        let viewport_width = f32::from(window.viewport_size().width);
        let viewport_height = f32::from(window.viewport_size().height);
        let show_preview = viewport_width > 1000.0 && self.preview.is_some();

        div()
            .absolute()
            .top_0()
            .left_0()
            .right_0()
            .bottom_0()
            .bg(rgba(0x00000030)) // Dimmed background overlay
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .w_3_4()
                    .h(px(viewport_height * 0.85))
                    .bg(rgb(0x1e1e1e))
                    .border_1()
                    .border_color(rgb(0x3e3e42))
                    .rounded(px(8.0))
                    .overflow_hidden()
                    .child(self.render_input())
                    .child(if show_preview {
                        // Two-panel layout: file list on left, preview on right
                        div()
                            .flex()
                            .flex_row()
                            .flex_1()
                            .overflow_hidden()
                            .child(
                                // Left panel: file list (45%)
                                div()
                                    .flex()
                                    .flex_col()
                                    .w(px(viewport_width * 0.75 * 0.45))
                                    .border_r_1()
                                    .border_color(rgb(0x3e3e42))
                                    .child(self.render_file_list()),
                            )
                            .child(
                                // Right panel: preview (55%)
                                div()
                                    .flex()
                                    .flex_col()
                                    .flex_1()
                                    .child(self.render_preview()),
                            )
                    } else {
                        // Single panel: just file list
                        div().flex().flex_row().flex_1().overflow_hidden().child(
                            div()
                                .flex()
                                .flex_col()
                                .flex_1()
                                .child(self.render_file_list()),
                        )
                    }),
            )
    }
}
