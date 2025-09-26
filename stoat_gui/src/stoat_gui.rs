use gpui::{
    App, Application, Bounds, Context, Element, ElementId, Font, FontStyle, FontWeight,
    GlobalElementId, Hsla, InspectorElementId, IntoElement, LayoutId, PaintQuad, Pixels, Render,
    ShapedLine, SharedString, Style, TextRun, Window, WindowBounds, WindowOptions, point,
    prelude::*, px, relative, rgb, size,
};
use stoat::Stoat;

pub fn run_with_stoat(stoat: Option<Stoat>) -> Result<(), Box<dyn std::error::Error>> {
    Application::new().run(move |cx: &mut App| {
        let mut stoat = stoat.unwrap_or_else(|| Stoat::new(cx));

        // Add some test content to see if rendering works
        stoat.buffer().update(cx, |buffer, _| {
            buffer.edit([(
                0..0,
                "Hello, World!\nThis is a test\nLine 3\nLine 4\nLine 5",
            )]);
        });

        // Check if buffer actually has content after edit
        let check_snapshot = stoat.buffer_snapshot(cx);
        eprintln!("Right after edit, buffer len: {}", check_snapshot.len());
        if check_snapshot.len() > 0 {
            let content: String = check_snapshot
                .text_for_range(0..check_snapshot.len())
                .collect();
            eprintln!("Buffer content: {:?}", &content[..50.min(content.len())]);
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

            // Check if buffer actually has content after edit
            let check_snapshot = stoat.buffer_snapshot(cx);
            eprintln!("Right after edit, buffer len: {}", check_snapshot.len());
            if check_snapshot.len() > 0 {
                let content: String = check_snapshot
                    .text_for_range(0..check_snapshot.len())
                    .collect();
                eprintln!("Buffer content: {:?}", &content[..50.min(content.len())]);
            }
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
    lines: Vec<PositionedLine>,
    /// Total bounds of the editor
    bounds: Bounds<Pixels>,
    /// Content area (excluding padding)
    content_bounds: Bounds<Pixels>,
    /// Line height for positioning
    line_height: Pixels,
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

        // Get buffer content and create shaped lines
        let buffer_snapshot = self.stoat.buffer_snapshot(cx);
        let buffer_len = buffer_snapshot.len();
        let visible_lines = (content_bounds.size.height / self.style.line_height) as usize;

        let mut lines = Vec::new();
        let mut current_line = String::new();
        let chunks = buffer_snapshot.text_for_range(0..buffer_len);
        let mut y_offset = 0.0;

        eprintln!("Buffer len: {}, iterating chunks...", buffer_len);
        for chunk in chunks {
            eprintln!("Got chunk: {:?}", chunk);
            for ch in chunk.chars() {
                if ch == '\n' {
                    if lines.len() < visible_lines {
                        let text = if current_line.is_empty() {
                            SharedString::from(" ")
                        } else {
                            SharedString::from(current_line.clone())
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

                        let shaped = window.text_system().shape_line(
                            text,
                            self.style.font_size,
                            &[text_run],
                            None,
                        );

                        lines.push(PositionedLine {
                            shaped,
                            position: point(
                                content_bounds.origin.x,
                                content_bounds.origin.y + px(y_offset),
                            ),
                        });
                        y_offset += self.style.line_height.0;
                    }
                    current_line.clear();
                    if lines.len() >= visible_lines {
                        break;
                    }
                } else {
                    current_line.push(ch);
                }
            }
            if lines.len() >= visible_lines {
                break;
            }
        }

        // Add last line if needed
        if !current_line.is_empty() && lines.len() < visible_lines {
            let text = SharedString::from(current_line);
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
        }

        // If no content, add a placeholder
        if lines.is_empty() {
            let text = SharedString::from("Empty buffer - ready for input");
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
            content_bounds,
            line_height: self.style.line_height,
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
