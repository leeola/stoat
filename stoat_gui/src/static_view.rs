//! Static read-only text view.
//!
//! Demonstrates the multi-view architecture by providing a view type that
//! handles actions differently from [`EditorView`](crate::editor_view::EditorView).
//! This view displays fixed content without text editing capabilities, proving
//! that GPUI's focus-based action routing correctly dispatches to different
//! view implementations.
//!
//! # Architecture Demo
//!
//! The same action can have different behaviors in different views:
//!
//! - `DeleteLeft` in [`EditorView`](crate::editor_view::EditorView): Deletes a character
//! - `DeleteLeft` in [`StaticView`]: No-op (not registered)
//! - `Quit` in both views: Closes the pane
//!
//! This demonstrates GPUI's action routing working correctly - the focused
//! view determines which handler executes.
//!
//! # Usage
//!
//! StaticView is used for displaying fixed content like help text, licenses,
//! or other read-only information that should appear in panes alongside
//! editable content.

use crate::content_view::{ContentView, ViewType};
use gpui::{
    div, App, Context, FocusHandle, Focusable, InteractiveElement, IntoElement, ParentElement,
    Render, Styled, Window,
};

/// A read-only view for displaying static text content.
///
/// This view demonstrates the multi-view architecture by showing that different
/// view types can exist in the same pane system. Unlike
/// [`EditorView`](crate::editor_view::EditorView), this view:
///
/// - Does not register text editing action handlers (no `DeleteLeft`, `InsertText`, etc.)
/// - Has its own rendering style
/// - Still participates in the focus chain and pane management
///
/// # Relationship to PaneGroupView
///
/// [`PaneGroupView`](crate::pane_group::PaneGroupView) stores StaticView instances
/// as `PaneContent::Static(Entity<StaticView>)`, demonstrating type-erased storage
/// working correctly. The pane system treats it identically to other view types
/// for focus, splits, and close operations.
///
/// # Action Handling
///
/// StaticView intentionally does NOT register handlers for text editing actions,
/// demonstrating that:
///
/// 1. Views selectively register only relevant actions
/// 2. GPUI's action routing respects view-specific handlers
/// 3. The same action name can have different meanings (or no meaning) in different views
pub struct StaticView {
    /// The static text content to display
    content: String,
    /// Focus handle for GPUI's focus chain
    focus_handle: FocusHandle,
    /// Optional title for the view
    title: Option<String>,
}

impl StaticView {
    /// Creates a new static view with the given content.
    ///
    /// # Arguments
    ///
    /// - `content`: The text to display (will be rendered as-is)
    /// - `cx`: The GPUI context for creating the focus handle
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let help_view = cx.new(|cx| {
    ///     StaticView::new(
    ///         "Help text goes here\nLine 2\nLine 3".to_string(),
    ///         cx
    ///     )
    /// });
    /// ```
    pub fn new(content: String, cx: &mut Context<'_, Self>) -> Self {
        Self {
            content,
            focus_handle: cx.focus_handle(),
            title: None,
        }
    }

    /// Creates a new static view with a title.
    ///
    /// The title can be used by [`PaneGroupView`](crate::pane_group::PaneGroupView)
    /// for display purposes (future enhancement).
    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    /// Returns the title of this view, if set.
    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }
}

impl ContentView for StaticView {
    fn view_type(&self) -> ViewType {
        ViewType::Static
    }

    fn stoat(&self) -> Option<&gpui::Entity<stoat::Stoat>> {
        // Static views are not backed by Stoat
        None
    }
}

impl Focusable for StaticView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for StaticView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<'_, Self>) -> impl IntoElement {
        div()
            .id("static-view")
            .track_focus(&self.focus_handle)
            // Note: We intentionally do NOT register text editing actions here
            // This demonstrates selective action handling - only relevant actions
            // are registered. Text editing actions like DeleteLeft, InsertText, etc.
            // are not applicable to read-only content.
            .size_full()
            .p_4()
            .bg(gpui::rgb(0x1e1e1e))
            .text_color(gpui::rgb(0xcccccc))
            .child(
                div()
                    .font_family("Menlo")
                    .text_size(gpui::px(14.0))
                    .line_height(gpui::relative(1.5))
                    .child(self.content.clone()),
            )
    }
}
