use gpui::{
    App, Application, Bounds, Context, Element, ElementId, Font, FontStyle, FontWeight,
    GlobalElementId, Hsla, InspectorElementId, IntoElement, LayoutId, Pixels, Render, SharedString,
    TextRun, Window, WindowBounds, WindowOptions, div, prelude::*, px, rgb, size,
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
    // Visible range (will be used for viewport rendering)
    visible_range: std::ops::Range<usize>,
}

impl EditorElement {
    fn new(stoat: Stoat) -> Self {
        Self {
            stoat,
            cached_lines: Vec::new(),
            cached_buffer_version: None,
            visible_range: 0..100, // Default to first 100 lines
        }
    }

    fn needs_update(&self, buffer_version: &clock::Global) -> bool {
        self.cached_buffer_version.as_ref() != Some(buffer_version)
    }

    fn build_lines(&mut self, cx: &App) -> Vec<SharedString> {
        let buffer_snapshot = self.stoat.buffer_snapshot(cx);
        let _text_style = TextStyle::new();

        // For now, process all text - viewport optimization coming next
        let chunks = buffer_snapshot.text_for_range(0..buffer_snapshot.len());
        let mut current_line = String::new();
        let mut lines = Vec::new();

        for chunk in chunks {
            let mut last_pos = 0;
            for (pos, ch) in chunk.char_indices() {
                if ch == '\n' {
                    current_line.push_str(&chunk[last_pos..pos]);

                    if current_line.is_empty() {
                        lines.push(SharedString::from(" "));
                    } else {
                        lines.push(SharedString::from(current_line.clone()));
                    }

                    current_line.clear();
                    last_pos = pos + 1;
                }
            }
            if last_pos < chunk.len() {
                current_line.push_str(&chunk[last_pos..]);
            }
        }

        if !current_line.is_empty() || lines.is_empty() {
            lines.push(SharedString::from(current_line));
        }

        lines
    }
}

impl Element for EditorElement {
    type RequestLayoutState = Vec<SharedString>;
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
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _window: &mut Window,
        _cx: &mut App,
    ) {
        // No prepaint needed for now
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        request_layout: &mut Self::RequestLayoutState,
        _: &mut (),
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
        if request_layout.is_empty() {
            let empty_msg = div().child(SharedString::from("Empty buffer - ready for input"));
            empty_msg.into_any().paint(window, cx);
            return;
        }

        // Render the cached styled lines
        let lines_element = request_layout
            .iter()
            .fold(container, |container, styled_line| {
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
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<'_, Self>) -> impl IntoElement {
        // Simply return the editor element - it handles its own rendering
        self.editor_element.clone()
    }
}

// TODO: Implement zero-allocation tokenized chunks iterator
// This will combine text chunks from the rope with syntax information from TokenMap
// For now, we're just rendering plain text until we properly implement the iterator
