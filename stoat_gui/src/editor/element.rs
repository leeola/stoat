use super::{
    layout::{EditorLayout, PositionedLine},
    style::EditorStyle,
};
use gpui::{
    point, px, relative, size, App, Bounds, Element, ElementId, Font, FontStyle, FontWeight,
    GlobalElementId, InspectorElementId, IntoElement, LayoutId, PaintQuad, Pixels, SharedString,
    Style, TextRun, Window,
};
use smallvec::SmallVec;
use stoat::Stoat;

pub struct EditorElement {
    stoat: Stoat,
    style: EditorStyle,
}

impl EditorElement {
    pub fn new(stoat: Stoat) -> Self {
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

        // Get buffer content and scroll position
        let buffer_snapshot = self.stoat.buffer_snapshot(cx);
        let scroll_position = self.stoat.scroll_position();

        // Calculate visible row range based on scroll position and viewport height
        let height_in_lines = content_bounds.size.height / self.style.line_height;
        let start_row = scroll_position.y as u32;
        let max_row = buffer_snapshot.row_count();
        let end_row = ((scroll_position.y + height_in_lines).ceil() as u32).min(max_row);

        let mut lines = SmallVec::new();

        // Reuse a single String allocation for all lines (like Zed does)
        let mut line_text = String::new();

        // Only iterate through visible rows
        for row in start_row..end_row {
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

            // Position lines relative to viewport (accounting for scroll offset)
            let relative_row = row - start_row;
            lines.push(PositionedLine {
                shaped,
                position: point(
                    content_bounds.origin.x,
                    content_bounds.origin.y + px(relative_row as f32 * self.style.line_height.0),
                ),
            });
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
