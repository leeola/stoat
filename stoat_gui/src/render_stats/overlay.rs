//! Frame time overlay visualization.
//!
//! Renders frame time metrics and a frame time graph in the top-left corner of the window.
//! Displays render time per frame rather than FPS, which is more accurate for event-driven UIs.

use crate::render_stats::tracker::{is_render_stats_enabled, FrameTimer};
use gpui::{
    point, px, size, App, Bounds, Element, Font, FontStyle, FontWeight, GlobalElementId, Hsla,
    IntoElement, Pixels, SharedString, TextRun, Window,
};
use std::{cell::RefCell, rc::Rc, time::Duration};

const OVERLAY_PADDING: Pixels = px(8.0);
const GRAPH_BAR_WIDTH: Pixels = px(3.0);
const GRAPH_BAR_SPACING: Pixels = px(1.0);
const GRAPH_HEIGHT: Pixels = px(40.0);
const TARGET_FRAME_TIME: Duration = Duration::from_micros(16667); // 60 FPS
const GRAPH_CEILING: Duration = Duration::from_millis(100); // Fixed Y-axis max at 100ms

/// Frame time overlay that displays render time and frame time graph.
///
/// Renders in the top-left corner with semi-transparent background.
/// Shows frame time in milliseconds, which is more meaningful than FPS
/// for event-driven UIs where frame rate varies with input.
/// Call [`RenderStatsOverlay::paint`] during the paint phase to render.
pub struct RenderStatsOverlay {
    frame_timer: Rc<RefCell<FrameTimer>>,
}

impl RenderStatsOverlay {
    /// Creates a new render stats overlay.
    pub fn new(frame_timer: Rc<RefCell<FrameTimer>>) -> Self {
        Self { frame_timer }
    }

    /// Paints the render stats overlay in the top-left corner.
    ///
    /// Should be called during the paint phase, after all other content is painted
    /// so the overlay appears on top.
    pub fn paint(&self, window: &mut Window, cx: &mut App) {
        let tracker = self.frame_timer.borrow();
        let avg_ms = tracker.avg_frame_time_ms();

        // Create frame time text - show ms as primary metric
        let frame_text = format!("Frame: {:.1}ms", avg_ms);
        let font = Font {
            family: SharedString::from("Menlo"),
            features: Default::default(),
            weight: FontWeight::NORMAL,
            style: FontStyle::Normal,
            fallbacks: None,
        };

        let text_color = Hsla {
            h: 0.0,
            s: 0.0,
            l: 0.9,
            a: 1.0,
        };

        let text_run = TextRun {
            len: frame_text.len(),
            font: font.clone(),
            color: text_color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };

        let shaped_text = window.text_system().shape_line(
            SharedString::from(frame_text),
            px(12.0),
            &[text_run],
            None,
        );

        // Calculate dimensions
        let frame_times = tracker.frame_times();
        let graph_width = if frame_times.is_empty() {
            px(0.0)
        } else {
            (GRAPH_BAR_WIDTH + GRAPH_BAR_SPACING) * frame_times.len() as f32 - GRAPH_BAR_SPACING
        };

        let content_width = shaped_text.width.max(graph_width) + OVERLAY_PADDING * 2.0;
        let content_height = px(16.0) + OVERLAY_PADDING * 3.0 + GRAPH_HEIGHT;

        let overlay_bounds = Bounds {
            origin: point(px(10.0), px(10.0)),
            size: size(content_width, content_height),
        };

        // Paint background
        window.paint_quad(gpui::PaintQuad {
            bounds: overlay_bounds,
            corner_radii: px(4.0).into(),
            background: Hsla {
                h: 0.0,
                s: 0.0,
                l: 0.1,
                a: 0.8,
            }
            .into(),
            border_color: Hsla {
                h: 0.0,
                s: 0.0,
                l: 0.3,
                a: 0.8,
            },
            border_widths: px(1.0).into(),
            border_style: gpui::BorderStyle::default(),
        });

        // Paint frame time text
        let text_origin = point(
            overlay_bounds.origin.x + OVERLAY_PADDING,
            overlay_bounds.origin.y + OVERLAY_PADDING,
        );
        let _ = shaped_text.paint(text_origin, px(16.0), window, cx);

        // Paint graph bars
        if !frame_times.is_empty() {
            // Use fixed ceiling for Y-axis scaling (8ms to 100ms range)
            let graph_origin_y = overlay_bounds.origin.y + px(16.0) + OVERLAY_PADDING * 2.0;
            let mut bar_x = overlay_bounds.origin.x + OVERLAY_PADDING;

            for &frame_time in frame_times.iter() {
                let height_ratio = frame_time.as_secs_f64() / GRAPH_CEILING.as_secs_f64();
                let bar_height = GRAPH_HEIGHT * height_ratio.min(1.0) as f32;

                // Color: green if under target, yellow if close, red if over
                let color = if frame_time <= TARGET_FRAME_TIME {
                    Hsla {
                        h: 120.0,
                        s: 0.8,
                        l: 0.5,
                        a: 0.9,
                    } // Green
                } else if frame_time <= TARGET_FRAME_TIME * 2 {
                    Hsla {
                        h: 60.0,
                        s: 0.8,
                        l: 0.5,
                        a: 0.9,
                    } // Yellow
                } else {
                    Hsla {
                        h: 0.0,
                        s: 0.8,
                        l: 0.5,
                        a: 0.9,
                    } // Red
                };

                let bar_bounds = Bounds {
                    origin: point(bar_x, graph_origin_y + (GRAPH_HEIGHT - bar_height)),
                    size: size(GRAPH_BAR_WIDTH, bar_height),
                };

                window.paint_quad(gpui::PaintQuad {
                    bounds: bar_bounds,
                    corner_radii: px(1.0).into(),
                    background: color.into(),
                    border_color: gpui::transparent_black(),
                    border_widths: 0.0.into(),
                    border_style: gpui::BorderStyle::default(),
                });

                bar_x += GRAPH_BAR_WIDTH + GRAPH_BAR_SPACING;
            }
        }
    }
}

/// GPUI element wrapper for rendering frame time overlay.
///
/// This element integrates with GPUI's rendering pipeline by calling
/// `record_frame()` during prepaint and rendering the overlay during paint.
/// Displays frame render time which is more accurate than FPS for event-driven UIs.
pub struct RenderStatsOverlayElement {
    frame_timer: Rc<RefCell<FrameTimer>>,
}

impl RenderStatsOverlayElement {
    /// Creates a new render stats overlay element.
    pub fn new(frame_timer: Rc<RefCell<FrameTimer>>) -> Self {
        Self { frame_timer }
    }
}

impl IntoElement for RenderStatsOverlayElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for RenderStatsOverlayElement {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<gpui::ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _global_id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (gpui::LayoutId, Self::RequestLayoutState) {
        // Request a zero-sized layout since we paint outside the layout system
        use gpui::Style;
        let style = Style::default();
        (window.request_layout(style, None, cx), ())
    }

    fn prepaint(
        &mut self,
        _global_id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _state: &mut Self::RequestLayoutState,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Self::PrepaintState {
        // Record frame time during prepaint
        self.frame_timer.borrow_mut().record_frame();
    }

    fn paint(
        &mut self,
        _global_id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout_state: &mut Self::RequestLayoutState,
        _prepaint_state: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        // Only render if render stats tracking is enabled
        if !is_render_stats_enabled() {
            return;
        }

        // Render the render stats overlay
        let overlay = RenderStatsOverlay::new(self.frame_timer.clone());
        overlay.paint(window, cx);
    }
}
