//! File finder with async preview loading and UI rendering.
//!
//! Combines data types, async loading logic, and rendering components for the file/buffer finder.
//! Following Zed's pattern where a feature combines state and UI in one module.

// Re-export syntax module types
use crate::syntax::{HighlightMap, HighlightedChunks, SyntaxTheme};
use gpui::{
    div, point, prelude::FluentBuilder, px, relative, rgb, rgba, App, Bounds, Element, Font,
    FontStyle, FontWeight, GlobalElementId, InspectorElementId, InteractiveElement, IntoElement,
    LayoutId, PaintQuad, ParentElement, Pixels, RenderOnce, ScrollHandle, ShapedLine, SharedString,
    StatefulInteractiveElement, Style, Styled, TextRun, Window,
};
use std::{
    path::{Path, PathBuf},
    sync::OnceLock,
};
use stoat_rope::TokenSnapshot;
use stoat_text::{Language, Parser};
use text::{Buffer, BufferId};

/// Preview data for file finder.
///
/// Enum supports progressive enhancement: show plain text immediately,
/// upgrade to syntax-highlighted version when ready.
#[derive(Clone)]
pub enum PreviewData {
    /// Plain text preview (fast, shown immediately)
    Plain(String),
    /// Syntax-highlighted preview (slower, shown after parsing)
    Highlighted { text: String, tokens: TokenSnapshot },
}

impl PreviewData {
    /// Get the text content of this preview
    pub fn text(&self) -> &str {
        match self {
            PreviewData::Plain(text) => text,
            PreviewData::Highlighted { text, .. } => text,
        }
    }

    /// Get the token snapshot if this is a highlighted preview
    pub fn tokens(&self) -> Option<&TokenSnapshot> {
        match self {
            PreviewData::Plain(_) => None,
            PreviewData::Highlighted { tokens, .. } => Some(tokens),
        }
    }
}

/// Load plain text preview without syntax highlighting.
///
/// Fast operation suitable for immediate display. Reads up to 100KB.
/// Uses `smol::unblock` to avoid blocking async executor.
pub async fn load_text_only(path: &Path) -> Option<String> {
    let path = path.to_path_buf();

    smol::unblock(move || {
        const MAX_BYTES: usize = 100 * 1024; // 100KB

        // Read only first MAX_BYTES
        let mut file = std::fs::File::open(&path).ok()?;
        let mut buffer = vec![0; MAX_BYTES];
        let bytes_read = std::io::Read::read(&mut file, &mut buffer).ok()?;
        buffer.truncate(bytes_read);

        // Check for binary content
        let check_size = buffer.len().min(1024);
        if buffer[..check_size].contains(&0) {
            return None; // Binary file
        }

        // Decode as UTF-8
        String::from_utf8(buffer).ok()
    })
    .await
}

/// Load syntax-highlighted file preview.
///
/// Reads file and parses for syntax highlighting. Both file I/O and parsing
/// run on thread pool via `smol::unblock` to avoid blocking executor.
pub async fn load_file_preview(path: &Path) -> Option<PreviewData> {
    let path = path.to_path_buf();

    // Phase 1: File I/O on thread pool
    let (text, language) = smol::unblock(move || {
        const MAX_BYTES: usize = 100 * 1024;

        let mut file = std::fs::File::open(&path).ok()?;
        let mut buffer = vec![0; MAX_BYTES];
        let bytes_read = std::io::Read::read(&mut file, &mut buffer).ok()?;
        buffer.truncate(bytes_read);

        let check_size = buffer.len().min(1024);
        if buffer[..check_size].contains(&0) {
            return None;
        }

        let text = String::from_utf8(buffer).ok()?;
        let language = path
            .extension()
            .and_then(|ext| ext.to_str())
            .map(Language::from_extension)
            .unwrap_or(Language::PlainText);

        Some((text, language))
    })
    .await?;

    // Phase 2: CPU-intensive parsing on thread pool
    smol::unblock(move || {
        let mut parser = Parser::new(language).ok()?;
        let buffer = Buffer::new(0, BufferId::new(1).ok()?, text.clone());
        let snapshot = buffer.snapshot();
        let parsed_tokens = parser.parse(&text, &snapshot).ok()?;

        // Build token snapshot
        let mut token_map = stoat_rope::TokenMap::new(&snapshot);
        token_map.replace_tokens(parsed_tokens, &snapshot);
        let tokens = token_map.snapshot();

        Some(PreviewData::Highlighted { text, tokens })
    })
    .await
}

/// Finder mode - determines what the finder searches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinderMode {
    /// Search for files in worktree
    Files,
    /// Search for open buffers
    Buffers,
}

/// Unified finder modal renderer for files and buffers.
///
/// Stateless component that renders finder UI. All interaction is handled through
/// the action system in file_finder or buffer_finder mode.
#[derive(IntoElement)]
pub struct Finder {
    mode: FinderMode,
    query: String,
    items: Vec<String>,
    selected: usize,
    preview: Option<PreviewData>,
    scroll_handle: ScrollHandle,
}

impl Finder {
    /// Create a new file finder renderer with the given state.
    pub fn new_file_finder(
        query: String,
        files: Vec<PathBuf>,
        selected: usize,
        preview: Option<PreviewData>,
        scroll_handle: ScrollHandle,
    ) -> Self {
        let items = files.iter().map(|p| p.display().to_string()).collect();

        Self {
            mode: FinderMode::Files,
            query,
            items,
            selected,
            preview,
            scroll_handle,
        }
    }

    /// Create a new buffer finder renderer with the given state.
    ///
    /// Formats each buffer entry with status flags in Helix style:
    /// - `*` for active buffer
    /// - `o` for visible buffer (not active)
    /// - `+` for modified buffer
    pub fn new_buffer_finder(
        query: String,
        buffers: Vec<crate::BufferListEntry>,
        selected: usize,
        scroll_handle: ScrollHandle,
    ) -> Self {
        let items = buffers
            .iter()
            .map(|entry| {
                // Build flags string: active (*), visible (o), modified (+)
                let mut flags = String::with_capacity(3);
                flags.push(if entry.is_active { '*' } else { ' ' });
                flags.push(if entry.is_visible && !entry.is_active {
                    'o'
                } else {
                    ' '
                });
                flags.push(if entry.is_modified { '+' } else { ' ' });

                // Combine flags and name with separator
                format!("{}  {}", flags, entry.display_name)
            })
            .collect();

        Self {
            mode: FinderMode::Buffers,
            query,
            items,
            selected,
            preview: None,
            scroll_handle,
        }
    }

    /// Render the input box showing the current query.
    fn render_input(&self) -> impl IntoElement {
        let query = self.query.clone();
        let placeholder = match self.mode {
            FinderMode::Files => "Type to search files...",
            FinderMode::Buffers => "Type to search buffers...",
        };

        div()
            .p(px(8.0))
            .border_b_1()
            .border_color(rgb(0x3e3e42))
            .bg(rgb(0x252526))
            .text_color(rgb(0xd4d4d4))
            .child(if query.is_empty() {
                placeholder.to_string()
            } else {
                query
            })
    }

    /// Render column header for buffer finder.
    ///
    /// Shows column labels explaining the flags:
    /// - First column: status flags (*=active, o=visible, +=modified)
    /// - Second column: buffer name
    fn render_buffer_header(&self) -> impl IntoElement {
        div()
            .px(px(8.0))
            .py(px(3.0))
            .border_b_1()
            .border_color(rgb(0x3e3e42))
            .bg(rgb(0x2a2a2a))
            .text_color(rgb(0x888888))
            .text_size(px(10.0))
            .child("*o+  Name")
    }

    /// Render the list of filtered items (files or buffers).
    ///
    /// For buffer mode, includes a header row with column labels.
    fn render_item_list(&self) -> impl IntoElement {
        let items = &self.items;
        let selected = self.selected;

        div()
            .id("item-list")
            .flex()
            .flex_col()
            .flex_1()
            .overflow_y_scroll()
            .track_scroll(&self.scroll_handle)
            .when(self.mode == FinderMode::Buffers, |div| {
                div.child(self.render_buffer_header())
            })
            .children(items.iter().enumerate().map(|(i, display_name)| {
                // Strip "./" prefix from file paths for cleaner display
                let display_text = display_name.strip_prefix("./").unwrap_or(display_name);

                div()
                    .px(px(8.0))
                    .py(px(3.0))
                    .when(i == selected, |div| {
                        div.bg(rgb(0x3b4261)) // Blue-gray highlight for selected item
                    })
                    .text_color(rgb(0xd4d4d4))
                    .text_size(px(11.0))
                    .child(display_text.to_string())
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
    cached_text_ptr: Option<*const str>,
    cached_layout: Option<PreviewLayout>,
}

/// Layout state prepared during prepaint
#[derive(Clone)]
struct PreviewLayout {
    lines: Vec<ShapedLineWithPosition>,
    bounds: Bounds<Pixels>,
}

#[derive(Clone)]
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
            cached_text_ptr: None,
            cached_layout: None,
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
            self.cached_text_ptr = None;
            self.cached_layout = None;
            return PreviewLayout {
                lines: Vec::new(),
                bounds,
            };
        };

        let preview_text = preview.text();
        let text_ptr = preview_text as *const str;

        // Return cached layout if preview hasn't changed and bounds match
        if self.cached_text_ptr == Some(text_ptr) {
            if let Some(ref cached) = self.cached_layout {
                if cached.bounds == bounds {
                    return cached.clone();
                }
            }
        }
        let buffer = text::Buffer::new(
            0,
            text::BufferId::new(1).expect("BufferId::new(1) should never fail"),
            preview_text.to_string(),
        );
        let snapshot = buffer.snapshot();

        // Get tokens - use cached empty snapshot for Plain preview
        // This avoids expensive TokenMap creation on every frame
        static EMPTY_TOKENS: OnceLock<stoat_rope::TokenSnapshot> = OnceLock::new();
        let tokens = match preview {
            PreviewData::Highlighted { tokens, .. } => tokens,
            PreviewData::Plain(_) => EMPTY_TOKENS.get_or_init(|| {
                // Create minimal empty snapshot once - HighlightedChunks will return
                // chunks with no highlighting (highlight_id = None)
                let empty_buf = text::Buffer::new(
                    0,
                    text::BufferId::new(1).expect("BufferId::new(1) should never fail"),
                    String::new(),
                );
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

        let layout = PreviewLayout { lines, bounds };
        self.cached_text_ptr = Some(text_ptr);
        self.cached_layout = Some(layout.clone());
        layout
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

impl RenderOnce for Finder {
    fn render(self, window: &mut Window, _cx: &mut App) -> impl IntoElement {
        // Check window width to determine if we should show preview
        // Only show preview for file finder mode, not buffer finder
        let viewport_width = f32::from(window.viewport_size().width);
        let viewport_height = f32::from(window.viewport_size().height);
        let show_preview =
            self.mode == FinderMode::Files && viewport_width > 1000.0 && self.preview.is_some();

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
                                // Left panel: item list (45%)
                                div()
                                    .flex()
                                    .flex_col()
                                    .w(px(viewport_width * 0.75 * 0.45))
                                    .border_r_1()
                                    .border_color(rgb(0x3e3e42))
                                    .child(self.render_item_list()),
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
                        // Single panel: just item list
                        div().flex().flex_row().flex_1().overflow_hidden().child(
                            div()
                                .flex()
                                .flex_col()
                                .flex_1()
                                .child(self.render_item_list()),
                        )
                    }),
            )
    }
}
