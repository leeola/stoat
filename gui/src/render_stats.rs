//! Optional frame-time overlay for performance debugging, gated by the
//! `STOAT_RENDER_STATS` environment variable. [`FrameTimer`] records a
//! rolling window of input-to-screen durations; [`RenderStatsOverlay`]
//! paints the latest average plus a per-frame bar graph in the top-left
//! corner.
//!
//! The timer is two-phase: [`FrameTimer::start_frame`] is called when a
//! keystroke enters dispatch and [`FrameTimer::end_frame`] when the paint
//! phase runs. This measures user-perceived latency rather than
//! inter-frame gaps, which in an event-driven UI are dominated by idle
//! time. A paint with no preceding `start_frame` records nothing, so one
//! keystroke yields exactly one sample.

use crate::globals::EnvHostGlobal;
use gpui::{
    canvas, point, px, size as gpui_size, transparent_black, App, BorderStyle, Bounds, Font,
    FontStyle, FontWeight, Hsla, IntoElement, PaintQuad, Pixels, SharedString, Styled, TextRun,
    Window,
};
use std::{
    cell::RefCell,
    collections::VecDeque,
    rc::Rc,
    sync::OnceLock,
    time::{Duration, Instant},
};

const HISTORY_SIZE: usize = 60;

const OVERLAY_PADDING: Pixels = px(8.0);
const GRAPH_BAR_WIDTH: Pixels = px(3.0);
const GRAPH_BAR_SPACING: Pixels = px(1.0);
const GRAPH_HEIGHT: Pixels = px(40.0);

/// Frame time at 60 FPS; bars at or under this render green.
const TARGET_FRAME_TIME: Duration = Duration::from_micros(16667);
/// Fixed Y-axis maximum for the bar graph; longer frames clamp to full
/// height.
const GRAPH_CEILING: Duration = Duration::from_millis(100);

const TEXT_COLOR: Hsla = Hsla {
    h: 0.0,
    s: 0.0,
    l: 0.9,
    a: 1.0,
};
const BACKGROUND_COLOR: Hsla = Hsla {
    h: 0.0,
    s: 0.0,
    l: 0.1,
    a: 0.8,
};
const BORDER_COLOR: Hsla = Hsla {
    h: 0.0,
    s: 0.0,
    l: 0.3,
    a: 0.8,
};
const BAR_GOOD: Hsla = Hsla {
    h: 120.0,
    s: 0.8,
    l: 0.5,
    a: 0.9,
};
const BAR_WARN: Hsla = Hsla {
    h: 60.0,
    s: 0.8,
    l: 0.5,
    a: 0.9,
};
const BAR_BAD: Hsla = Hsla {
    h: 0.0,
    s: 0.8,
    l: 0.5,
    a: 0.9,
};

/// Rolling window of the last [`HISTORY_SIZE`] input-to-screen frame
/// durations. [`start_frame`](Self::start_frame) stamps the start of a
/// frame and [`end_frame`](Self::end_frame) records the elapsed time;
/// `end_frame` without a preceding `start_frame` is a no-op.
pub struct FrameTimer {
    frame_times: VecDeque<Duration>,
    pending_start: Option<Instant>,
}

impl FrameTimer {
    pub fn new() -> Self {
        Self {
            frame_times: VecDeque::with_capacity(HISTORY_SIZE),
            pending_start: None,
        }
    }

    /// Stamp the start of a frame. `now` is supplied by the caller so the
    /// timer holds no clock of its own and stays deterministic in tests.
    pub fn start_frame(&mut self, now: Instant) {
        self.pending_start = Some(now);
    }

    /// Record the elapsed time since the matching
    /// [`start_frame`](Self::start_frame), evicting the oldest sample once
    /// the window is full. No-op when no frame is in progress.
    pub fn end_frame(&mut self, now: Instant) {
        let Some(start) = self.pending_start.take() else {
            return;
        };
        self.frame_times
            .push_back(now.saturating_duration_since(start));
        if self.frame_times.len() > HISTORY_SIZE {
            self.frame_times.pop_front();
        }
    }

    /// Mean of the recorded frame durations in milliseconds, or `0.0`
    /// when none have been recorded.
    pub fn avg_frame_time_ms(&self) -> f64 {
        if self.frame_times.is_empty() {
            return 0.0;
        }
        let total: Duration = self.frame_times.iter().sum();
        (total / self.frame_times.len() as u32).as_secs_f64() * 1000.0
    }

    pub fn frame_times(&self) -> &VecDeque<Duration> {
        &self.frame_times
    }
}

impl Default for FrameTimer {
    fn default() -> Self {
        Self::new()
    }
}

/// Whether the render-stats overlay is enabled, from the
/// `STOAT_RENDER_STATS` environment variable read through the installed
/// [`EnvHost`]. The value is read once and cached, so call-site gating
/// costs a single atomic load after the first call. Returns `false` when
/// no [`EnvHostGlobal`] is installed (headless runs, most tests).
pub fn render_stats_enabled(cx: &App) -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        cx.try_global::<EnvHostGlobal>()
            .map(|env| env.0.var("STOAT_RENDER_STATS").is_some())
            .unwrap_or(false)
    })
}

/// Top-left overlay painting the frame-time average and per-frame bar
/// graph from a shared [`FrameTimer`]. Wrap [`Self::element`] in
/// [`gpui::deferred`] so it layers above the workspace content.
pub struct RenderStatsOverlay {
    frame_timer: Rc<RefCell<FrameTimer>>,
}

impl RenderStatsOverlay {
    pub fn new(frame_timer: Rc<RefCell<FrameTimer>>) -> Self {
        Self { frame_timer }
    }

    /// Build the overlay element. Its paint phase first closes the
    /// in-progress frame ([`FrameTimer::end_frame`]) so the bar graph
    /// includes the frame being drawn, then paints the box, text, and
    /// bars at a fixed top-left position regardless of the element's own
    /// bounds.
    pub fn element(self) -> impl IntoElement {
        let frame_timer = self.frame_timer;
        canvas(
            |_, _, _| {},
            move |_bounds, _, window, cx| {
                frame_timer.borrow_mut().end_frame(Instant::now());
                let timer = frame_timer.borrow();
                paint_overlay(&timer, window, cx);
            },
        )
        .size_full()
    }
}

fn paint_overlay(timer: &FrameTimer, window: &mut Window, cx: &mut App) {
    let text = frame_text(timer.avg_frame_time_ms());
    let font = Font {
        family: SharedString::from("Menlo"),
        features: Default::default(),
        weight: FontWeight::NORMAL,
        style: FontStyle::Normal,
        fallbacks: None,
    };
    let text_run = TextRun {
        len: text.len(),
        font,
        color: TEXT_COLOR,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let shaped =
        window
            .text_system()
            .shape_line(SharedString::from(text), px(12.0), &[text_run], None);

    let frame_times = timer.frame_times();
    let graph_width = if frame_times.is_empty() {
        px(0.0)
    } else {
        (GRAPH_BAR_WIDTH + GRAPH_BAR_SPACING) * frame_times.len() as f32 - GRAPH_BAR_SPACING
    };
    let content_width = shaped.width.max(graph_width) + OVERLAY_PADDING * 2.0;
    let content_height = px(16.0) + OVERLAY_PADDING * 3.0 + GRAPH_HEIGHT;
    let bounds = Bounds {
        origin: point(px(10.0), px(10.0)),
        size: gpui_size(content_width, content_height),
    };

    window.paint_quad(PaintQuad {
        bounds,
        corner_radii: px(4.0).into(),
        background: BACKGROUND_COLOR.into(),
        border_color: BORDER_COLOR,
        border_widths: px(1.0).into(),
        border_style: BorderStyle::default(),
    });

    let text_origin = point(
        bounds.origin.x + OVERLAY_PADDING,
        bounds.origin.y + OVERLAY_PADDING,
    );
    let _ = shaped.paint(text_origin, px(16.0), window, cx);

    if frame_times.is_empty() {
        return;
    }
    let graph_origin_y = bounds.origin.y + px(16.0) + OVERLAY_PADDING * 2.0;
    let mut bar_x = bounds.origin.x + OVERLAY_PADDING;
    for &frame_time in frame_times.iter() {
        let bar_height = GRAPH_HEIGHT * bar_height_ratio(frame_time) as f32;
        let bar_bounds = Bounds {
            origin: point(bar_x, graph_origin_y + (GRAPH_HEIGHT - bar_height)),
            size: gpui_size(GRAPH_BAR_WIDTH, bar_height),
        };
        window.paint_quad(PaintQuad {
            bounds: bar_bounds,
            corner_radii: px(1.0).into(),
            background: bar_color(frame_time).into(),
            border_color: transparent_black(),
            border_widths: px(0.0).into(),
            border_style: BorderStyle::default(),
        });
        bar_x += GRAPH_BAR_WIDTH + GRAPH_BAR_SPACING;
    }
}

fn frame_text(avg_ms: f64) -> String {
    format!("Frame: {avg_ms:.1}ms")
}

fn bar_color(frame_time: Duration) -> Hsla {
    if frame_time <= TARGET_FRAME_TIME {
        BAR_GOOD
    } else if frame_time <= TARGET_FRAME_TIME * 2 {
        BAR_WARN
    } else {
        BAR_BAD
    }
}

fn bar_height_ratio(frame_time: Duration) -> f64 {
    (frame_time.as_secs_f64() / GRAPH_CEILING.as_secs_f64()).min(1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn end_frame_records_elapsed_since_start() {
        let mut timer = FrameTimer::new();
        let t0 = Instant::now();
        timer.start_frame(t0);
        timer.end_frame(t0 + Duration::from_millis(16));

        assert_eq!(timer.frame_times().len(), 1);
        assert!((timer.avg_frame_time_ms() - 16.0).abs() < 0.001);
    }

    #[test]
    fn end_frame_without_start_records_nothing() {
        let mut timer = FrameTimer::new();
        timer.end_frame(Instant::now());

        assert!(timer.frame_times().is_empty());
        assert_eq!(timer.avg_frame_time_ms(), 0.0);
    }

    #[test]
    fn frame_times_cap_at_history_size() {
        let mut timer = FrameTimer::new();
        let t0 = Instant::now();
        for _ in 0..HISTORY_SIZE + 10 {
            timer.start_frame(t0);
            timer.end_frame(t0 + Duration::from_millis(1));
        }

        assert_eq!(timer.frame_times().len(), HISTORY_SIZE);
    }

    #[test]
    fn frame_text_formats_one_decimal() {
        assert_eq!(frame_text(16.66), "Frame: 16.7ms");
        assert_eq!(frame_text(0.0), "Frame: 0.0ms");
    }

    #[test]
    fn bar_color_buckets_by_target() {
        assert_eq!(bar_color(Duration::from_millis(10)), BAR_GOOD);
        assert_eq!(bar_color(Duration::from_millis(25)), BAR_WARN);
        assert_eq!(bar_color(Duration::from_millis(50)), BAR_BAD);
    }

    #[test]
    fn bar_height_ratio_scales_and_clamps() {
        assert!((bar_height_ratio(Duration::from_millis(50)) - 0.5).abs() < 1e-9);
        assert_eq!(bar_height_ratio(Duration::from_millis(200)), 1.0);
    }
}
