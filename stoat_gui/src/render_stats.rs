//! Render statistics tracking and visualization.
//!
//! This module provides frame timing tracking and overlay visualization for monitoring
//! render performance in the Stoat editor.
//!
//! ## Components
//!
//! - [`tracker`]: Frame timing measurement with [`FrameTimer`]
//! - [`overlay`]: Visual overlay displaying frame time metrics with [`RenderStatsOverlay`]
//!
//! ## Usage
//!
//! Enable render stats by setting the `STOAT_RENDER_STATS` environment variable:
//!
//! ```bash
//! STOAT_RENDER_STATS=1 cargo run
//! ```
//!
//! The overlay displays in the top-left corner showing:
//! - Frame render time in milliseconds
//! - Rolling graph of the last 60 frames
//!
//! ## Integration
//!
//! Typically used in [`crate::pane_group::view::PaneGroupView`] by:
//! 1. Creating a [`FrameTimer`] instance
//! 2. Adding a [`RenderStatsOverlayElement`] to the view hierarchy
//!
//! The element automatically records frame times during prepaint and renders
//! the overlay during paint when stats are enabled.

pub mod overlay;
pub mod tracker;

pub use overlay::{RenderStatsOverlay, RenderStatsOverlayElement};
pub use tracker::{is_render_stats_enabled, FrameTimer};
