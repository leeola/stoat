use gpui::{
    App, Application, Bounds, Context, DispatchPhase, Element, ElementId, Font, FontStyle,
    FontWeight, GlobalElementId, Hsla, InspectorElementId, InteractiveElement, IntoElement,
    LayoutId, Pixels, Point, Render, ScrollDelta, ScrollWheelEvent, SharedString, TextRun, Window,
    WindowBounds, WindowOptions, div, point, prelude::*, px, rgb, size,
};
use stoat::Stoat;

pub fn run_with_stoat(stoat: Option<Stoat>) -> Result<(), Box<dyn std::error::Error>> {
    Application::new().run(move |cx: &mut App| {
        let stoat = stoat.unwrap_or_else(|| Stoat::new(cx));
        let bounds = Bounds::centered(None, size(px(800.0), px(600.0)), cx);

        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_, cx| {
                cx.new(|_| {
                    let editor_element = EditorElement::new(stoat.clone());
                    EditorView {
                        stoat,
                        editor_element,
                    }
                })
            },
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
        }

        let bounds = Bounds::centered(None, size(px(800.0), px(600.0)), cx);

        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_, cx| {
                cx.new(|_| {
                    let editor_element = EditorElement::new(stoat.clone());
                    EditorView {
                        stoat,
                        editor_element,
                    }
                })
            },
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
    editor_element: EditorElement,
}

/// Custom element that handles efficient text rendering with caching
#[derive(Clone)]
struct EditorElement {
    stoat: Stoat,
    // Cache for rendered lines
    cached_lines: Vec<SharedString>,
    // Version of the buffer when cache was built
    cached_buffer_version: Option<clock::Global>,
    // Visible range in display rows
    visible_range: std::ops::Range<u32>,
    // Scroll position (y = top row as float)
    scroll_position: Point<f32>,
    // Number of visible lines (calculated from bounds)
    visible_line_count: Option<f32>,
    // Line height in pixels
    line_height: Pixels,
}

impl EditorElement {
    fn new(stoat: Stoat) -> Self {
        Self {
            stoat,
            cached_lines: Vec::new(),
            cached_buffer_version: None,
            visible_range: 0..50, // Default visible range
            scroll_position: point(0.0, 0.0),
            visible_line_count: None,
            line_height: px(20.0), // Default line height
        }
    }

    fn needs_update(&self, buffer_version: &clock::Global) -> bool {
        self.cached_buffer_version.as_ref() != Some(buffer_version)
    }

    fn build_lines(&mut self, cx: &App) -> Vec<SharedString> {
        let buffer_snapshot = self.stoat.buffer_snapshot(cx);
        let _text_style = TextStyle::new();

        // Calculate byte range for visible lines
        // For now, we'll process all text but only return visible lines
        // TODO: Calculate actual byte offsets for start_row and end_row
        let chunks = buffer_snapshot.text_for_range(0..buffer_snapshot.len());
        let mut current_line = String::new();
        let mut all_lines = Vec::new();
        let mut line_count = 0u32;

        for chunk in chunks {
            let mut last_pos = 0;
            for (pos, ch) in chunk.char_indices() {
                if ch == '\n' {
                    current_line.push_str(&chunk[last_pos..pos]);

                    // Only store lines in visible range
                    if line_count >= self.visible_range.start && line_count < self.visible_range.end
                    {
                        if current_line.is_empty() {
                            all_lines.push(SharedString::from(" "));
                        } else {
                            all_lines.push(SharedString::from(current_line.clone()));
                        }
                    }

                    current_line.clear();
                    last_pos = pos + 1;
                    line_count += 1;

                    // Stop processing if we've passed the visible range
                    if line_count >= self.visible_range.end {
                        return all_lines;
                    }
                }
            }
            if last_pos < chunk.len() {
                current_line.push_str(&chunk[last_pos..]);
            }
        }

        // Handle last line if in visible range
        if line_count >= self.visible_range.start && line_count < self.visible_range.end {
            if !current_line.is_empty() || all_lines.is_empty() {
                all_lines.push(SharedString::from(current_line));
            }
        }

        all_lines
    }
}

impl Element for EditorElement {
    type RequestLayoutState = Vec<SharedString>;
    type PrepaintState = Vec<SharedString>;

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
        // Check if we need to rebuild the cache
        let buffer_snapshot = self.stoat.buffer_snapshot(cx);
        let buffer_version = buffer_snapshot.version();

        if self.needs_update(buffer_version) {
            // Rebuild the cache
            self.cached_lines = self.build_lines(cx);
            self.cached_buffer_version = Some(buffer_version.clone());
        }

        // For now, return a simple layout - proper measurement coming later
        let layout_id = window.request_layout(gpui::Style::default(), [], cx);

        (layout_id, self.cached_lines.clone())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        request_layout: &mut Self::RequestLayoutState,
        _window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        // Calculate visible lines based on bounds and scroll position
        let height_in_pixels = bounds.size.height;
        let visible_lines = (height_in_pixels / self.line_height).floor();
        self.visible_line_count = Some(visible_lines);

        // Calculate visible range based on scroll position
        let start_row = self.scroll_position.y.max(0.0) as u32;

        // Get total line count from buffer
        let buffer_snapshot = self.stoat.buffer_snapshot(cx);
        let text = buffer_snapshot.text_for_range(0..buffer_snapshot.len());
        let total_lines = text.fold(0u32, |count, chunk| {
            count + chunk.matches('\n').count() as u32
        }) + 1; // +1 for last line if no trailing newline

        let end_row = ((self.scroll_position.y + visible_lines).ceil() as u32).min(total_lines);

        // Update visible range
        self.visible_range = start_row..end_row;

        // Check if we need to rebuild the cache
        let buffer_version = buffer_snapshot.version();
        if self.needs_update(buffer_version) {
            self.cached_lines = self.build_lines(cx);
            self.cached_buffer_version = Some(buffer_version.clone());
        }

        // Return the cached lines for painting
        self.cached_lines.clone()
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
        // Paint the cached lines within the bounds
        let container = div()
            .bg(rgb(0x1e1e1e))
            .size_full()
            .p(px(20.0))
            .flex()
            .flex_col();

        // Check if buffer is empty
        if prepaint.is_empty() {
            let empty_msg = div().child(SharedString::from("Empty buffer - ready for input"));
            empty_msg.into_any().paint(window, cx);
            return;
        }

        // Render the cached styled lines from prepaint state
        let lines_element = prepaint.iter().fold(container, |container, styled_line| {
            container.child(div().child(styled_line.clone()))
        });

        lines_element.into_any().paint(window, cx);
    }
}

impl IntoElement for EditorElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

struct TextStyle {
    font_family: SharedString,
    font_size: Pixels,
    font_weight: FontWeight,
    font_style: FontStyle,
    color: Hsla,
}

impl TextStyle {
    fn new() -> Self {
        Self {
            font_family: SharedString::from("monospace"),
            font_size: px(14.0),
            font_weight: FontWeight::NORMAL,
            font_style: FontStyle::Normal,
            color: rgb(0xcccccc).into(),
        }
    }

    fn to_run(&self, len: usize) -> TextRun {
        TextRun {
            len,
            font: Font {
                family: self.font_family.clone(),
                features: Default::default(),
                weight: self.font_weight,
                style: self.font_style,
                fallbacks: Default::default(),
            },
            color: self.color,
            background_color: None,
            underline: None,
            strikethrough: None,
        }
    }
}

impl Render for EditorView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        // Wrap the editor element in a scrollable container
        let editor_element = self.editor_element.clone();

        div()
            .size_full()
            .on_scroll_wheel(cx.listener(
                move |this: &mut EditorView, event: &ScrollWheelEvent, _window, cx| {
                    // Calculate scroll delta
                    let (delta_y, _delta_x) = match event.delta {
                        ScrollDelta::Pixels(pixels) => (pixels.y.0 as f32, pixels.x.0 as f32),
                        ScrollDelta::Lines(lines) => {
                            // Convert lines to pixels (assuming 20px line height)
                            (
                                lines.y * this.editor_element.line_height.0 as f32,
                                lines.x * 20.0,
                            )
                        },
                    };

                    // Update scroll position
                    this.editor_element.scroll_position.y = (this.editor_element.scroll_position.y
                        - delta_y / this.editor_element.line_height.0 as f32)
                        .max(0.0);

                    // Notify to trigger re-render
                    cx.notify();
                },
            ))
            .child(editor_element)
    }
}

// TODO: Implement zero-allocation tokenized chunks iterator
// This will combine text chunks from the rope with syntax information from TokenMap
// For now, we're just rendering plain text until we properly implement the iterator
