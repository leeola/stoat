//! Frame timing tracker for performance monitoring.
//!
//! Tracks frame times with minimal overhead. When disabled via
//! environment variable, has near-zero cost (~1ns per frame for cached bool check).

use std::{
    collections::VecDeque,
    env,
    sync::OnceLock,
    time::{Duration, Instant},
};

const HISTORY_SIZE: usize = 60;

/// Tracks frame timing with minimal overhead.
///
/// Maintains a rolling window of the last 60 frame times for frame time calculation
/// and graph visualization. Cost when enabled: ~50ns per frame.
pub struct FrameTimer {
    frame_times: VecDeque<Duration>,
    last_frame: Instant,
}

impl FrameTimer {
    /// Creates a new frame timer.
    pub fn new() -> Self {
        Self {
            frame_times: VecDeque::with_capacity(HISTORY_SIZE),
            last_frame: Instant::now(),
        }
    }

    /// Records the current frame time.
    ///
    /// Should be called once per frame at the start of rendering. If render stats
    /// are disabled via `STOAT_RENDER_STATS` env var, returns immediately with ~1ns cost.
    pub fn record_frame(&mut self) {
        if !is_render_stats_enabled() {
            return;
        }

        let now = Instant::now();
        let delta = now.duration_since(self.last_frame);
        self.last_frame = now;

        self.frame_times.push_back(delta);
        if self.frame_times.len() > HISTORY_SIZE {
            self.frame_times.pop_front();
        }
    }

    /// Returns current FPS based on average frame time.
    ///
    /// Returns 0.0 if no frames have been recorded yet.
    pub fn fps(&self) -> f64 {
        if self.frame_times.is_empty() {
            return 0.0;
        }

        let total: Duration = self.frame_times.iter().sum();
        let avg = total / self.frame_times.len() as u32;
        let secs = avg.as_secs_f64();

        if secs > 0.0 { 1.0 / secs } else { 0.0 }
    }

    /// Returns average frame time in milliseconds.
    pub fn avg_frame_time_ms(&self) -> f64 {
        if self.frame_times.is_empty() {
            return 0.0;
        }

        let total: Duration = self.frame_times.iter().sum();
        let avg = total / self.frame_times.len() as u32;
        avg.as_secs_f64() * 1000.0
    }

    /// Returns frame times for graph visualization.
    ///
    /// Returns the last N frame times where N <= 60.
    pub fn frame_times(&self) -> &VecDeque<Duration> {
        &self.frame_times
    }
}

impl Default for FrameTimer {
    fn default() -> Self {
        Self::new()
    }
}

/// Checks if render stats are enabled via environment variable.
///
/// Reads `STOAT_RENDER_STATS` env var once and caches the result. Cost: ~1ns after first call.
/// Returns `true` if `STOAT_RENDER_STATS=1` or `STOAT_RENDER_STATS=true`.
pub fn is_render_stats_enabled() -> bool {
    static RENDER_STATS_ENABLED: OnceLock<bool> = OnceLock::new();
    *RENDER_STATS_ENABLED.get_or_init(|| {
        env::var("STOAT_RENDER_STATS")
            .map(|val| val == "1" || val == "true")
            .unwrap_or(false)
    })
}
