//! Content view abstraction for supporting multiple view types.
//!
//! Defines the [`ContentView`] trait that all pane content must implement,
//! enabling [`PaneGroupView`](crate::pane_group::PaneGroupView) to manage
//! different types of content (text editors, images, tables, etc.) uniformly.
//!
//! # Architecture
//!
//! The content view system enables extensibility without tight coupling:
//!
//! - **Views register handlers**: Each view type implements [`ContentView`] and registers its own
//!   action handlers via GPUI's `on_action()` mechanism
//! - **GPUI routes actions**: Focus-based routing automatically dispatches actions to the
//!   appropriate view without central dispatchers
//! - **Different behaviors**: The same action (e.g., `DeleteLeft`) can have different
//!   implementations in [`EditorView`](crate::editor::view::EditorView) vs future `ImageView` or
//!   `TableView`
//!
//! # Usage in PaneGroupView
//!
//! [`PaneGroupView`](crate::pane_group::PaneGroupView) stores pane contents using
//! the [`PaneContent`] enum, which wraps concrete view types while providing a
//! uniform interface via this trait.
//!
//! # Example
//!
//! ```rust,ignore
//! impl ContentView for EditorView {
//!     fn view_type(&self) -> ViewType {
//!         ViewType::Editor
//!     }
//!
//!     fn stoat(&self) -> Option<&Entity<Stoat>> {
//!         Some(&self.stoat)
//!     }
//! }
//! ```

use crate::Stoat;
use gpui::{Entity, Focusable, Render};

/// Trait implemented by all view types that can be displayed in panes.
///
/// This trait provides the core interface for pane content, enabling
/// [`PaneGroupView`](crate::pane_group::PaneGroupView) to manage heterogeneous
/// view types uniformly. All pane content must:
///
/// - Implement [`Render`] to draw itself
/// - Implement [`Focusable`] for GPUI's focus chain and action routing
/// - Provide a [`ViewType`] for type identification
/// - Optionally provide access to an underlying [`Stoat`] entity (for text-based views)
///
/// # Action Handling
///
/// Views register their own action handlers in their [`Render`] implementation
/// using GPUI's `on_action()` method. GPUI automatically routes actions to the
/// focused view, enabling the same action name to have different implementations
/// in different view types.
///
/// # Relationship to PaneContent
///
/// The [`PaneContent`] enum wraps concrete implementations of this trait, allowing
/// [`PaneGroupView`](crate::pane_group::PaneGroupView) to store different view
/// types in a single collection.
pub trait ContentView: Render + Focusable {
    /// Returns the type of this view.
    ///
    /// Used by [`PaneGroupView`](crate::pane_group::PaneGroupView) to identify
    /// view types when needed (e.g., for view-specific behavior or debugging).
    fn view_type(&self) -> ViewType;

    /// Returns the underlying [`Stoat`] entity if this view is backed by one.
    ///
    /// Text-based views like [`EditorView`](crate::editor::view::EditorView) are
    /// backed by a [`Stoat`] entity which manages the text buffer, cursor, and
    /// editing state. Other view types (images, tables, etc.) may not need a
    /// Stoat and return `None`.
    ///
    /// # Usage
    ///
    /// This is primarily used by [`PaneGroupView`](crate::pane_group::PaneGroupView)
    /// when opening finders, modals, or other features that need access to the
    /// text editing state.
    fn stoat(&self) -> Option<&Entity<Stoat>> {
        None
    }
}

/// Enumeration of all supported view types.
///
/// Each variant corresponds to a concrete view implementation that implements
/// [`ContentView`]. Adding a new view type requires:
///
/// 1. Creating the view struct (e.g., `ImageView`, `TableView`)
/// 2. Implementing [`ContentView`], [`Render`], and [`Focusable`] for it
/// 3. Adding a variant to this enum
/// 4. Adding a variant to [`PaneContent`]
/// 5. Registering action handlers in the view's [`Render`] implementation
///
/// # Example Flow
///
/// ```text
/// User presses 'j' in focused EditorView
///   -> GPUI routes MoveDown action to EditorView
///   -> EditorView::handle_move_down executes
///
/// User presses 'j' in focused ImageView (future)
///   -> GPUI routes MoveDown action to ImageView
///   -> ImageView::handle_scroll_down executes (different behavior)
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ViewType {
    Editor,
    Static,
    Claude,
}

/// Type-erased wrapper for different view types that can be stored in panes.
///
/// This enum enables [`PaneGroupView`](crate::pane_group::PaneGroupView) to
/// store different view types in a single `HashMap<PaneId, PaneContent>`. Each
/// variant wraps a GPUI [`Entity`] of a concrete view type.
///
/// # Relationship to ContentView
///
/// Each variant's inner type implements [`ContentView`]. This enum provides
/// type erasure at the storage level while preserving type safety when accessing
/// the underlying views.
///
/// # Usage Pattern
///
/// ```rust,ignore
/// // Creating pane content
/// let editor_entity = cx.new(|cx| EditorView::new(stoat, cx));
/// let content = PaneContent::Editor(editor_entity);
///
/// // Accessing the view
/// if let Some(editor) = content.as_editor() {
///     editor.update(cx, |view, cx| {
///         // Work with EditorView
///     });
/// }
/// ```
///
/// # Adding New View Types
///
/// To add a new view type:
///
/// 1. Add variant: `NewViewType(Entity<NewView>)`
/// 2. Implement accessor: `pub fn as_new_view(&self) -> Option<&Entity<NewView>>`
/// 3. Update `view_type()` to handle the new variant
/// 4. Update [`PaneGroupView`](crate::pane_group::PaneGroupView) methods that pattern match on
///    [`PaneContent`]
///
/// This enum approach scales well (2-20 view types) without requiring refactors
/// to all actions when adding new types.
#[derive(Clone)]
pub enum PaneContent {
    Editor(Entity<crate::editor::view::EditorView>),
    Static(Entity<crate::static_view::StaticView>),
    Claude(Entity<crate::claude::view::ClaudeView>),
}

impl PaneContent {
    /// Returns the type of view contained in this pane.
    ///
    /// Used for type identification without extracting the concrete entity.
    pub fn view_type(&self) -> ViewType {
        match self {
            Self::Editor(_) => ViewType::Editor,
            Self::Static(_) => ViewType::Static,
            Self::Claude(_) => ViewType::Claude,
        }
    }

    /// Returns a reference to the contained [`EditorView`](crate::editor::view::EditorView)
    /// entity if this is an editor pane.
    ///
    /// # Usage
    ///
    /// This is the primary way [`PaneGroupView`](crate::pane_group::PaneGroupView)
    /// accesses editor-specific functionality. Most pane operations currently assume
    /// the pane contains an editor.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// if let Some(editor) = pane_content.as_editor() {
    ///     editor.update(cx, |view, cx| {
    ///         view.stoat.update(cx, |stoat, cx| {
    ///             stoat.open_file_finder(cx);
    ///         });
    ///     });
    /// }
    /// ```
    pub fn as_editor(&self) -> Option<&Entity<crate::editor::view::EditorView>> {
        match self {
            Self::Editor(entity) => Some(entity),
            _ => None,
        }
    }

    /// Returns a reference to the contained [`StaticView`](crate::static_view::StaticView)
    /// entity if this is a static pane.
    ///
    /// # Usage
    ///
    /// This allows [`PaneGroupView`](crate::pane_group::PaneGroupView) to access
    /// static view functionality when needed.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// if let Some(static_view) = pane_content.as_static() {
    ///     static_view.read(cx).title()
    /// }
    /// ```
    pub fn as_static(&self) -> Option<&Entity<crate::static_view::StaticView>> {
        match self {
            Self::Static(entity) => Some(entity),
            _ => None,
        }
    }

    pub fn as_claude(&self) -> Option<&Entity<crate::claude::view::ClaudeView>> {
        match self {
            Self::Claude(entity) => Some(entity),
            _ => None,
        }
    }
}
