use gpui::{
    App, Application, Bounds, Context, Element, ElementId, Font, FontStyle, FontWeight,
    GlobalElementId, Hsla, InspectorElementId, IntoElement, LayoutId, PaintQuad, Pixels, Render,
    ShapedLine, SharedString, Style, TextRun, Window, WindowBounds, WindowOptions, point,
    prelude::*, px, relative, rgb, size,
};
use smallvec::SmallVec;
use stoat::Stoat;
use text;

pub fn run_with_stoat(stoat: Option<Stoat>) -> Result<(), Box<dyn std::error::Error>> {
    Application::new().run(move |cx: &mut App| {
        let stoat = stoat.unwrap_or_else(|| Stoat::new(cx));

        // Add some test content to see if rendering works
        stoat.buffer().update(cx, |buffer, _| {
            buffer.edit([(
                0..0,
                "Hello, World!\nThis is a test\nLine 3\nLine 4\nLine 5",
            )]);
        });

        let bounds = Bounds::centered(None, size(px(800.0), px(600.0)), cx);

        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_, cx| cx.new(|_| EditorView { stoat }),
        )
        .unwrap();

        cx.on_window_closed(|cx| {
            cx.quit();
        })
        .detach();

        cx.activate(true);
    });

    Ok(())
}

pub fn run_with_paths(paths: Vec<std::path::PathBuf>) -> Result<(), Box<dyn std::error::Error>> {
    Application::new().run(move |cx: &mut App| {
        let mut stoat = Stoat::new(cx);

        // Load files if provided
        if !paths.is_empty() {
            let path_refs: Vec<&std::path::Path> = paths.iter().map(|p| p.as_ref()).collect();
            stoat.load_files(&path_refs, cx);
        } else {
            // Add test content if no files provided
            stoat.buffer().update(cx, |buffer, _| {
                buffer.edit([(
                    0..0,
                    "Hello, World!\nThis is a test\nLine 3\nLine 4\nLine 5",
                )]);
            });
        }

        let bounds = Bounds::centered(None, size(px(800.0), px(600.0)), cx);

        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_, cx| cx.new(|_| EditorView { stoat }),
        )
        .unwrap();

        cx.on_window_closed(|cx| {
            cx.quit();
        })
        .detach();

        cx.activate(true);
    });

    Ok(())
}

struct EditorView {
    stoat: Stoat,
}

/// Minimal EditorElement following Zed's pattern
struct EditorElement {
    stoat: Stoat,
    style: EditorStyle,
}

/// Style configuration for the editor
struct EditorStyle {
    text_color: Hsla,
    background: Hsla,
    line_height: Pixels,
    font_size: Pixels,
    padding: Pixels,
}

impl Default for EditorStyle {
    fn default() -> Self {
        Self {
            text_color: rgb(0xcccccc).into(),
            background: rgb(0x1e1e1e).into(),
            line_height: px(20.0),
            font_size: px(14.0),
            padding: px(20.0),
        }
    }
}

/// Layout state computed in prepaint
struct EditorLayout {
    /// The shaped lines ready to paint
    lines: SmallVec<[PositionedLine; 32]>,
    /// Total bounds of the editor
    bounds: Bounds<Pixels>,
    /// Content area (excluding padding)
    _content_bounds: Bounds<Pixels>,
    /// Line height for positioning
    _line_height: Pixels,
}

/// A shaped line with its rendering position
struct PositionedLine {
    shaped: ShapedLine,
    position: gpui::Point<Pixels>,
}

impl EditorElement {
    fn new(stoat: Stoat) -> Self {
        Self {
            stoat,
            style: EditorStyle::default(),
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

        // Get buffer content and create shaped lines
        let buffer_snapshot = self.stoat.buffer_snapshot(cx);
        let visible_lines = (content_bounds.size.height / self.style.line_height) as usize;
        let row_count = buffer_snapshot.row_count() as usize;
        let rows_to_render = row_count.min(visible_lines);

        let mut lines = SmallVec::new();
        let mut y_offset = 0.0;

        // Reuse a single String allocation for all lines (like Zed does)
        let mut line_text = String::new();

        // Iterate through rows efficiently
        for row in 0..rows_to_render {
            let row = row as u32;
            let line_start = text::Point::new(row, 0);
            let line_len = buffer_snapshot.line_len(row);
            let line_end = text::Point::new(row, line_len);

            // Clear and reuse the String allocation
            line_text.clear();

            // Build up the line from chunks
            let chunks = buffer_snapshot.text_for_range(line_start..line_end);
            for chunk in chunks {
                line_text.push_str(chunk);
            }

            // Create SharedString - empty lines use static string
            let text = if line_text.is_empty() {
                EMPTY_LINE.clone()
            } else {
                SharedString::from(line_text.clone())
            };

            // Shape the line using GPUI's text system
            let text_run = TextRun {
                len: text.len(),
                font: font.clone(),
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
                position: point(
                    content_bounds.origin.x,
                    content_bounds.origin.y + px(y_offset),
                ),
            });
            y_offset += self.style.line_height.0;
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
    }
}

impl IntoElement for EditorElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Render for EditorView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<'_, Self>) -> impl IntoElement {
        // Create a fresh element each time, following Zed's pattern
        EditorElement::new(self.stoat.clone())
    }
}

// TODO: Implement zero-allocation tokenized chunks iterator
// This will combine text chunks from the rope with syntax information from TokenMap
// For now, we're just rendering plain text until we properly implement the iterator
