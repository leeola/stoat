//! Scroll operations
//!
//! This module provides commands for handling viewport scrolling. Scroll operations
//! update the visible portion of the buffer in response to user input from mouse wheels,
//! trackpads, and keyboard commands.
//!
//! # Scroll Commands
//!
//! - [`handle_scroll`] - processes scroll events from input devices
//!
//! # Scroll Modes
//!
//! The scroll system supports two modes:
//! - **Normal scrolling**: Standard scroll speed (1.0x sensitivity)
//! - **Fast scrolling**: Accelerated scroll (3.0x sensitivity) when modifier key held
//!
//! # Input Sources
//!
//! Handles scroll input from multiple sources:
//! - **Mouse wheel**: Discrete line-based scrolling
//! - **Trackpad**: Smooth pixel-based scrolling with momentum
//! - **Keyboard**: Animated page scrolling (see [`crate::actions::movement`])
//!
//! # Scroll State
//!
//! Scroll state is managed by [`crate::scroll::ScrollPosition`], which tracks:
//! - Current viewport position
//! - Animation state for smooth scrolling
//! - Target position for animated scrolls
//!
//! # Bounds Management
//!
//! All scroll operations enforce bounds checking to prevent:
//! - Negative scroll positions
//! - Scrolling past the end of the buffer
//! - Invalid viewport states
//!
//! # Related
//!
//! Animated scrolling is handled by movement commands:
//! - [`crate::actions::movement::page_up`] - animated upward page scroll
//! - [`crate::actions::movement::page_down`] - animated downward page scroll
//!
//! # Integration
//!
//! This module is part of the [`crate::actions`] system and integrates with:
//! - [`crate::Stoat`] - the main editor state that manages scroll position
//! - [`crate::scroll::ScrollPosition`] - scroll state and animation management
//! - [`crate::scroll::ScrollDelta`] - scroll input representation
//! - GPUI action system - for keyboard bindings and command dispatch
//! - [`crate::actions::HandleScroll`] - the action struct for scroll events

mod handle_scroll;
