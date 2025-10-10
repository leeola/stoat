//! Git status modal for viewing modified files.
//!
//! Renders a modal overlay for git status viewing with diff preview. All state management
//! and input handling happens in [`stoat::Stoat`] core - this is just the presentation layer.
//!
//! Layout matches [`FileFinder`] with two panels: file list on left, diff preview on right.

use gpui::{
    App, Bounds, Element, Font, FontStyle, FontWeight, GlobalElementId, InspectorElementId,
    InteractiveElement, IntoElement, LayoutId, PaintQuad, ParentElement, Pixels, RenderOnce,
    ScrollHandle, ShapedLine, SharedString, StatefulInteractiveElement, Style, Styled, TextRun,
    Window, div, point, prelude::FluentBuilder, px, rgb, rgba,
};
use stoat::git_status::{DiffPreviewData, GitBranchInfo, GitStatusEntry};

/// Git status modal renderer.
///
/// Stateless component that renders git status UI similar to FileFinder.
/// Two-panel layout: file list on left (45%), diff preview on right (55%).
#[derive(IntoElement)]
pub struct GitStatus {
    files: Vec<GitStatusEntry>,
    selected: usize,
    preview: Option<DiffPreviewData>,
    branch_info: Option<GitBranchInfo>,
    scroll_handle: ScrollHandle,
}

impl GitStatus {
    /// Create a new git status renderer with the given state.
    pub fn new(
        files: Vec<GitStatusEntry>,
        selected: usize,
        preview: Option<DiffPreviewData>,
        branch_info: Option<GitBranchInfo>,
        scroll_handle: ScrollHandle,
    ) -> Self {
        Self {
            files,
            selected,
            preview,
            branch_info,
            scroll_handle,
        }
    }

    /// Render the header bar showing title.
    fn render_header(&self) -> impl IntoElement {
        div()
            .p(px(8.0))
            .border_b_1()
            .border_color(rgb(0x3e3e42))
            .bg(rgb(0x252526))
            .text_color(rgb(0xd4d4d4))
            .font_weight(FontWeight::SEMIBOLD)
            .child("Git Status")
    }

    /// Render branch information section (git-style formatting).
    fn render_branch_info(&self) -> Option<impl IntoElement> {
        let branch_info = self.branch_info.as_ref()?;

        let mut lines = vec![format!("On branch {}", branch_info.branch_name)];

        if branch_info.ahead > 0 && branch_info.behind > 0 {
            lines.push(format!(
                "Your branch is ahead by {} and behind by {} commits.",
                branch_info.ahead, branch_info.behind
            ));
        } else if branch_info.ahead > 0 {
            lines.push(format!(
                "Your branch is ahead of 'origin/{}' by {} commit{}.",
                branch_info.branch_name,
                branch_info.ahead,
                if branch_info.ahead == 1 { "" } else { "s" }
            ));
        } else if branch_info.behind > 0 {
            lines.push(format!(
                "Your branch is behind 'origin/{}' by {} commit{}.",
                branch_info.branch_name,
                branch_info.behind,
                if branch_info.behind == 1 { "" } else { "s" }
            ));
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
                .children(lines.into_iter().map(|line| div().child(line))),
        )
    }

    /// Render the list of modified files with status indicators.
    fn render_file_list(&self) -> impl IntoElement {
        let files = &self.files;
        let selected = self.selected;

        if files.is_empty() {
            return div()
                .id("git-status-list")
                .flex()
                .flex_col()
                .flex_1()
                .items_center()
                .justify_center()
                .child(
                    div()
                        .text_color(rgb(0x808080))
                        .text_size(px(13.0))
                        .child("nothing to commit, working tree clean"),
                );
        }

        div()
            .id("git-status-list")
            .flex()
            .flex_col()
            .flex_1()
            .overflow_y_scroll()
            .track_scroll(&self.scroll_handle)
            .children(files.iter().enumerate().map(|(i, entry)| {
                let status_color = match entry.status.as_str() {
                    "M" => rgb(0x4ec9b0),  // Teal for modified
                    "A" => rgb(0x6a9955),  // Green for added
                    "D" => rgb(0xf14c4c),  // Red for deleted
                    "R" => rgb(0xc586c0),  // Purple for renamed
                    "!" => rgb(0xf48771),  // Orange for conflicted
                    "??" => rgb(0x808080), // Gray for untracked
                    _ => rgb(0xd4d4d4),    // White for unknown
                };

                div()
                    .flex()
                    .gap_2()
                    .px(px(8.0))
                    .py(px(3.0))
                    .when(i == selected, |div| {
                        div.bg(rgb(0x3b4261)) // Blue-gray highlight for selected file
                    })
                    .child(
                        div()
                            .text_color(status_color)
                            .text_size(px(11.0))
                            .font_weight(FontWeight::BOLD)
                            .w(px(16.0))
                            .child(entry.status_display()),
                    )
                    .child(
                        div()
                            .text_color(rgb(0xd4d4d4))
                            .text_size(px(11.0))
                            .child(entry.path.to_string_lossy().to_string()),
                    )
            }))
    }

    /// Render the diff preview panel.
    fn render_preview(&self) -> DiffPreviewElement {
        DiffPreviewElement::new(self.preview.clone())
    }
}

/// Custom element for rendering git diff preview with colored lines.
///
/// Implements GPUI's low-level Element trait to render diff text with proper coloring:
/// - Lines starting with '+' in green
/// - Lines starting with '-' in red
/// - Hunk headers (@@) in gray
/// - Context lines in default color
struct DiffPreviewElement {
    preview: Option<DiffPreviewData>,
}

struct DiffPreviewLayout {
    lines: Vec<ShapedLineWithPosition>,
    bounds: Bounds<Pixels>,
}

struct ShapedLineWithPosition {
    shaped: ShapedLine,
    position: gpui::Point<Pixels>,
}

impl DiffPreviewElement {
    fn new(preview: Option<DiffPreviewData>) -> Self {
        Self { preview }
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
        // Request full-size layout
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
            return DiffPreviewLayout {
                lines: Vec::new(),
                bounds,
            };
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

        // Calculate viewport culling
        let visible_height = f32::from(bounds.size.height);
        let max_visible_lines = (visible_height / f32::from(line_height)).ceil() as usize + 2;

        // Diff colors
        let color_added = rgb(0x6a9955); // Green for +
        let color_removed = rgb(0xf14c4c); // Red for -
        let color_hunk = rgb(0x808080); // Gray for @@
        let color_default = rgb(0xd4d4d4); // White for context

        // Shape each line with appropriate color
        let mut lines = Vec::new();
        let mut y_offset = bounds.origin.y + px(12.0);

        for (line_idx, line_text) in preview.text().lines().enumerate() {
            if line_idx >= max_visible_lines {
                break;
            }

            // Determine color based on line prefix
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

        DiffPreviewLayout { lines, bounds }
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
            background: rgb(0x1e1e1e).into(),
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

impl RenderOnce for GitStatus {
    fn render(self, window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let viewport_width = f32::from(window.viewport_size().width);
        let viewport_height = f32::from(window.viewport_size().height);
        let show_preview = viewport_width > 1000.0 && self.preview.is_some();
        let is_clean = self.files.is_empty();

        let branch_info_elem = self.render_branch_info();

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
                    .when(is_clean, |div| div.w(px(500.0)).h(px(200.0)))
                    .when(!is_clean, |div| div.w_3_4().h(px(viewport_height * 0.85)))
                    .bg(rgb(0x1e1e1e))
                    .border_1()
                    .border_color(rgb(0x3e3e42))
                    .rounded(px(8.0))
                    .overflow_hidden()
                    .child(self.render_header())
                    .when_some(branch_info_elem, |div, elem| div.child(elem))
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
                                // Right panel: diff preview (55%)
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
