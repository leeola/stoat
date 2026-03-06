use crate::git::status::DiffPreviewData;
use gpui::{
    div, point, prelude::FluentBuilder, px, rgb, rgba, App, Bounds, Element, Font, FontStyle,
    FontWeight, GlobalElementId, InspectorElementId, InteractiveElement, IntoElement, LayoutId,
    PaintQuad, ParentElement, Pixels, RenderOnce, ScrollHandle, ShapedLine, SharedString,
    StatefulInteractiveElement, Style, Styled, TextRun, Window,
};
use std::path::PathBuf;

/// A file entry displayed in the diff summary.
#[derive(Clone, Debug)]
pub struct DiffSummaryFileEntry {
    pub path: PathBuf,
    pub status: String,
    pub staged: Option<bool>,
}

/// Unified diff summary component shared by git status and blame commit diff.
#[derive(IntoElement)]
pub struct DiffSummary {
    title: String,
    metadata_lines: Vec<String>,
    files: Vec<DiffSummaryFileEntry>,
    selected: usize,
    preview: Option<DiffPreviewData>,
    scroll_handle: ScrollHandle,
}

impl DiffSummary {
    pub fn new(
        title: String,
        metadata_lines: Vec<String>,
        files: Vec<DiffSummaryFileEntry>,
        selected: usize,
        preview: Option<DiffPreviewData>,
        scroll_handle: ScrollHandle,
    ) -> Self {
        Self {
            title,
            metadata_lines,
            files,
            selected,
            preview,
            scroll_handle,
        }
    }

    fn render_header(&self) -> impl IntoElement {
        div()
            .p(px(8.0))
            .border_b_1()
            .border_color(rgb(0x3e3e42))
            .bg(rgb(0x252526))
            .text_color(rgb(0xd4d4d4))
            .font_weight(FontWeight::SEMIBOLD)
            .child(self.title.clone())
    }

    fn render_metadata(&self) -> Option<impl IntoElement> {
        if self.metadata_lines.is_empty() {
            return None;
        }

        Some(
            div()
                .p(px(12.0))
                .border_b_1()
                .border_color(rgb(0x3e3e42))
                .bg(rgb(0x1e1e1e))
                .text_color(rgb(0x808080))
                .text_size(px(12.0))
                .flex()
                .flex_col()
                .gap_1()
                .children(
                    self.metadata_lines
                        .iter()
                        .map(|line| div().child(line.clone())),
                ),
        )
    }

    fn render_file_list(&self) -> impl IntoElement {
        if self.files.is_empty() {
            return div()
                .id("diff-modal-list")
                .flex()
                .flex_col()
                .flex_1()
                .items_center()
                .justify_center()
                .child(
                    div()
                        .text_color(rgb(0x808080))
                        .text_size(px(13.0))
                        .child("no files"),
                );
        }

        let selected = self.selected;
        div()
            .id("diff-modal-list")
            .flex()
            .flex_col()
            .flex_1()
            .overflow_y_scroll()
            .track_scroll(&self.scroll_handle)
            .children(self.files.iter().enumerate().map(|(i, entry)| {
                let status_color = match entry.status.as_str() {
                    "M" => rgb(0x4ec9b0),
                    "A" => rgb(0x6a9955),
                    "D" => rgb(0xf14c4c),
                    "R" => rgb(0xc586c0),
                    "C" => rgb(0xc586c0),
                    "!" => rgb(0xf48771),
                    "??" => rgb(0x808080),
                    _ => rgb(0xd4d4d4),
                };

                let display = match entry.staged {
                    Some(true) => format!("{} ", entry.status),
                    Some(false) => format!(" {}", entry.status),
                    None => entry.status.clone(),
                };

                div()
                    .flex()
                    .gap_2()
                    .px(px(8.0))
                    .py(px(3.0))
                    .when(i == selected, |div| div.bg(rgb(0x3b4261)))
                    .child(
                        div()
                            .text_color(status_color)
                            .text_size(px(11.0))
                            .font_weight(FontWeight::BOLD)
                            .w(px(24.0))
                            .child(display),
                    )
                    .child(
                        div()
                            .text_color(rgb(0xd4d4d4))
                            .text_size(px(11.0))
                            .child(entry.path.to_string_lossy().to_string()),
                    )
            }))
    }

    fn render_preview(&self) -> DiffPreviewElement {
        DiffPreviewElement::new(self.preview.clone())
    }
}

impl RenderOnce for DiffSummary {
    fn render(self, window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let viewport_width = f32::from(window.viewport_size().width);
        let viewport_height = f32::from(window.viewport_size().height);
        let show_preview = viewport_width > 1000.0 && self.preview.is_some();
        let is_empty = self.files.is_empty();

        let metadata_elem = self.render_metadata();

        div()
            .absolute()
            .top_0()
            .left_0()
            .right_0()
            .bottom_0()
            .bg(rgba(0x00000030))
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .when(is_empty, |div| div.w(px(500.0)).h(px(200.0)))
                    .when(!is_empty, |div| div.w_3_4().h(px(viewport_height * 0.85)))
                    .bg(rgb(0x1e1e1e))
                    .border_1()
                    .border_color(rgb(0x3e3e42))
                    .rounded(px(8.0))
                    .overflow_hidden()
                    .child(self.render_header())
                    .when_some(metadata_elem, |div, elem| div.child(elem))
                    .child(if show_preview {
                        div()
                            .flex()
                            .flex_row()
                            .flex_1()
                            .overflow_hidden()
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .w(px(viewport_width * 0.75 * 0.45))
                                    .border_r_1()
                                    .border_color(rgb(0x3e3e42))
                                    .child(self.render_file_list()),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .flex_1()
                                    .child(self.render_preview()),
                            )
                    } else {
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

/// Custom element for rendering git diff preview with colored lines.
pub(crate) struct DiffPreviewElement {
    preview: Option<DiffPreviewData>,
    cached_text_ptr: Option<*const str>,
    cached_layout: Option<DiffPreviewLayout>,
}

#[derive(Clone)]
pub(crate) struct DiffPreviewLayout {
    lines: Vec<ShapedLineWithPosition>,
    bounds: Bounds<Pixels>,
}

#[derive(Clone)]
struct ShapedLineWithPosition {
    shaped: ShapedLine,
    position: gpui::Point<Pixels>,
}

impl DiffPreviewElement {
    pub(crate) fn new(preview: Option<DiffPreviewData>) -> Self {
        Self {
            preview,
            cached_text_ptr: None,
            cached_layout: None,
        }
    }
}

impl Element for DiffPreviewElement {
    type RequestLayoutState = ();
    type PrepaintState = DiffPreviewLayout;

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
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        _cx: &mut App,
    ) -> Self::PrepaintState {
        let Some(preview) = &self.preview else {
            self.cached_text_ptr = None;
            self.cached_layout = None;
            return DiffPreviewLayout {
                lines: Vec::new(),
                bounds,
            };
        };

        let preview_text = preview.text();
        let text_ptr = preview_text as *const str;

        if self.cached_text_ptr == Some(text_ptr) {
            if let Some(ref cached) = self.cached_layout {
                if cached.bounds == bounds {
                    return cached.clone();
                }
            }
        }

        let font = Font {
            family: ".AppleSystemUIFontMonospaced".into(),
            features: Default::default(),
            weight: FontWeight::NORMAL,
            style: FontStyle::Normal,
            fallbacks: None,
        };
        let font_size = px(12.0);
        let line_height = px(18.0);

        let visible_height = f32::from(bounds.size.height);
        let max_visible_lines = (visible_height / f32::from(line_height)).ceil() as usize + 2;

        let color_added = rgb(0x6a9955);
        let color_removed = rgb(0xf14c4c);
        let color_hunk = rgb(0x808080);
        let color_default = rgb(0xd4d4d4);

        let mut lines = Vec::new();
        let mut y_offset = bounds.origin.y + px(12.0);

        for (line_idx, line_text) in preview_text.lines().enumerate() {
            if line_idx >= max_visible_lines {
                break;
            }

            let color = if line_text.starts_with('+') {
                color_added
            } else if line_text.starts_with('-') {
                color_removed
            } else if line_text.starts_with("@@") {
                color_hunk
            } else {
                color_default
            };

            let text = if line_text.is_empty() {
                SharedString::from(" ")
            } else {
                SharedString::from(line_text.to_string())
            };

            let text_run = TextRun {
                len: text.len(),
                font: font.clone(),
                color: color.into(),
                background_color: None,
                underline: None,
                strikethrough: None,
            };

            let shaped = window
                .text_system()
                .shape_line(text, font_size, &[text_run], None);

            lines.push(ShapedLineWithPosition {
                shaped,
                position: point(bounds.origin.x + px(12.0), y_offset),
            });

            y_offset += line_height;
        }

        let layout = DiffPreviewLayout { lines, bounds };
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
        window.paint_quad(PaintQuad {
            bounds: layout.bounds,
            corner_radii: Default::default(),
            background: rgb(0x1e1e1e).into(),
            border_color: Default::default(),
            border_widths: Default::default(),
            border_style: Default::default(),
        });

        let line_height = px(18.0);
        for line in &layout.lines {
            line.shaped
                .paint(line.position, line_height, window, cx)
                .unwrap_or_else(|err| {
                    eprintln!("Failed to paint diff line: {err:?}");
                });
        }
    }
}

impl IntoElement for DiffPreviewElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}
