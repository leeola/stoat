//! Item trait system for pane contents.
//!
//! This module defines the trait-based architecture for items that can be displayed
//! in editor panes. Inspired by Zed's workspace item system, it uses trait objects
//! to allow heterogeneous collections of different item types (buffers, terminals, etc.)
//! in a single pane.
//!
//! # Architecture
//!
//! The system uses two key traits:
//!
//! - [`Item`] - Implemented by concrete item types (e.g., [`super::buffer_item::BufferItem`])
//! - [`ItemHandle`] - Type-erased wrapper trait for storing items in collections
//!
//! The bridge between these is `impl ItemHandle for Entity<T> where T: Item`, which
//! allows `Entity<BufferItem>` to be used as `Box<dyn ItemHandle>`.
//!
//! # Usage
//!
//! ```ignore
//! // Create a concrete item
//! let buffer_item = cx.new(|cx| BufferItem::new(buffer, cx));
//!
//! // Store as type-erased handle
//! let items: Vec<Box<dyn ItemHandle>> = vec![Box::new(buffer_item)];
//!
//! // Access through trait methods
//! if items[0].is_dirty(cx) {
//!     items[0].save(cx);
//! }
//! ```
//!
//! # Related
//!
//! See also:
//! - [`super::buffer_item::BufferItem`] - Concrete item implementation for text buffers
//! - [`crate::Stoat`] - Main editor state that manages items

use gpui::{
    AnyElement, AnyView, App, Context, Entity, EntityId, EventEmitter, IntoElement, Render,
    SharedString, Window,
};
use std::any::Any;

/// Core trait for pane items.
///
/// Implemented by concrete types that can be displayed in an editor pane.
/// Provides methods for rendering tabs, saving, and lifecycle management.
///
/// # Type Parameters
///
/// - `Event` - Event type emitted by this item (e.g., for dirty state changes)
///
/// # Essential Methods
///
/// Items must implement:
/// - [`tab_content_text`](Self::tab_content_text) - Text shown in tab
/// - [`is_dirty`](Self::is_dirty) - Whether has unsaved changes
/// - [`can_save`](Self::can_save) - Whether saving is supported
///
/// # Example
///
/// ```ignore
/// struct BufferItem {
///     buffer: Entity<Buffer>,
/// }
///
/// impl Item for BufferItem {
///     type Event = BufferEvent;
///
///     fn tab_content_text(&self, cx: &App) -> SharedString {
///         "untitled".into()
///     }
///
///     fn is_dirty(&self, cx: &App) -> bool {
///         self.buffer.read(cx).is_dirty()
///     }
/// }
/// ```
pub trait Item: EventEmitter<Self::Event> + Render + Sized {
    /// Event type emitted by this item
    type Event;

    /// Text displayed in the tab for this item.
    ///
    /// Typically the filename for file-backed items, or a description for other types.
    ///
    /// # Arguments
    ///
    /// * `cx` - App context for reading state
    ///
    /// # Returns
    ///
    /// Text to display in tab, e.g., "main.rs" or "untitled"
    fn tab_content_text(&self, cx: &App) -> SharedString;

    /// Whether this item has unsaved changes.
    ///
    /// Controls visual dirty indicator in tab (typically a dot or asterisk).
    /// Used to prompt for save before closing.
    ///
    /// # Arguments
    ///
    /// * `cx` - App context for reading buffer state
    ///
    /// # Returns
    ///
    /// `true` if item has unsaved changes, `false` otherwise
    fn is_dirty(&self, cx: &App) -> bool;

    /// Whether this item can be saved.
    ///
    /// Returns `false` for read-only items or items without a backing store.
    ///
    /// # Arguments
    ///
    /// * `cx` - App context
    ///
    /// # Returns
    ///
    /// `true` if save operation is supported, `false` otherwise
    fn can_save(&self, cx: &App) -> bool;

    /// Navigate to a position within this item.
    ///
    /// Used by navigation history to restore cursor position when switching items.
    /// Default implementation does nothing.
    ///
    /// # Arguments
    ///
    /// * `data` - Navigation data (type-specific, typically contains position)
    /// * `window` - Window for focus management
    /// * `cx` - Context for this item
    ///
    /// # Returns
    ///
    /// `true` if navigation succeeded and changed position, `false` otherwise
    fn navigate(
        &mut self,
        _data: Box<dyn Any>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> bool {
        false
    }

    /// Called when this item is deactivated (another item becomes active).
    ///
    /// Used to save navigation state, hide popups, etc.
    /// Default implementation does nothing.
    ///
    /// # Arguments
    ///
    /// * `window` - Window for UI updates
    /// * `cx` - Context for this item
    fn deactivated(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {}
}

/// Type-erased handle to an item.
///
/// Allows storing heterogeneous item types in collections like `Vec<Box<dyn ItemHandle>>`.
/// Provides the same interface as [`Item`] but without generic type parameters.
///
/// # Implementation
///
/// Automatically implemented for `Entity<T> where T: Item` via blanket impl.
/// Users typically don't implement this directly.
///
/// # Usage
///
/// ```ignore
/// let items: Vec<Box<dyn ItemHandle>> = vec![
///     Box::new(buffer_item_entity),
///     Box::new(terminal_item_entity),
/// ];
///
/// for item in &items {
///     println!("Tab: {}", item.tab_content_text(cx));
/// }
/// ```
///
/// # Related
///
/// See also:
/// - [`Item`] - Concrete item trait
/// - [`crate::Stoat`] - Stores `Vec<Box<dyn ItemHandle>>` for multiple items
pub trait ItemHandle: 'static + Send {
    /// Get unique identifier for this item entity.
    ///
    /// Used to track items across the pane, detect duplicates, and manage focus.
    ///
    /// # Arguments
    ///
    /// * `cx` - App context
    ///
    /// # Returns
    ///
    /// Unique entity ID
    fn item_id(&self, cx: &App) -> EntityId;

    /// Text displayed in tab for this item.
    ///
    /// See [`Item::tab_content_text`] for details.
    fn tab_content_text(&self, cx: &App) -> SharedString;

    /// Whether this item has unsaved changes.
    ///
    /// See [`Item::is_dirty`] for details.
    fn is_dirty(&self, cx: &App) -> bool;

    /// Whether this item can be saved.
    ///
    /// See [`Item::can_save`] for details.
    fn can_save(&self, cx: &App) -> bool;

    /// Navigate to a position within this item.
    ///
    /// See [`Item::navigate`] for details.
    fn navigate(&self, data: Box<dyn Any>, window: &mut Window, cx: &mut App) -> bool;

    /// Called when this item is deactivated.
    ///
    /// See [`Item::deactivated`] for details.
    fn deactivated(&self, window: &mut Window, cx: &mut App);

    /// Clone this handle into a new boxed trait object.
    ///
    /// Required because `Box<dyn ItemHandle>` doesn't automatically support cloning.
    /// Each item type must provide its own clone logic.
    ///
    /// # Returns
    ///
    /// New boxed handle pointing to the same item entity
    fn boxed_clone(&self) -> Box<dyn ItemHandle>;

    /// Convert to type-erased view for downcasting.
    ///
    /// Used by [`dyn ItemHandle::downcast`] to convert back to concrete `Entity<T>` types.
    /// Returns an [`AnyView`] that can be downcast to the original entity type.
    ///
    /// # Returns
    ///
    /// Type-erased view that can be downcast back to `Entity<T>`
    fn to_any(&self) -> AnyView;

    /// Render this item's content.
    ///
    /// Called by the pane to render the active item's content area.
    ///
    /// # Arguments
    ///
    /// * `window` - Window for rendering
    /// * `cx` - App context
    ///
    /// # Returns
    ///
    /// Element tree to render
    fn render(&self, window: &mut Window, cx: &mut App) -> AnyElement;
}

/// Bridge implementation allowing `Entity<T>` to act as `ItemHandle`.
///
/// This blanket impl is the key to the type erasure system - it allows any
/// `Entity<T>` where `T: Item` to be boxed as `Box<dyn ItemHandle>`.
///
/// # Example
///
/// ```ignore
/// let buffer_item: Entity<BufferItem> = cx.new(|cx| BufferItem::new(buffer, cx));
///
/// // Can convert to trait object
/// let handle: Box<dyn ItemHandle> = Box::new(buffer_item);
/// ```
impl<T: Item> ItemHandle for Entity<T> {
    fn item_id(&self, cx: &App) -> EntityId {
        self.entity_id()
    }

    fn tab_content_text(&self, cx: &App) -> SharedString {
        self.read(cx).tab_content_text(cx)
    }

    fn is_dirty(&self, cx: &App) -> bool {
        self.read(cx).is_dirty(cx)
    }

    fn can_save(&self, cx: &App) -> bool {
        self.read(cx).can_save(cx)
    }

    fn navigate(&self, data: Box<dyn Any>, window: &mut Window, cx: &mut App) -> bool {
        self.update(cx, |item, cx| item.navigate(data, window, cx))
    }

    fn deactivated(&self, window: &mut Window, cx: &mut App) {
        self.update(cx, |item, cx| item.deactivated(window, cx))
    }

    fn boxed_clone(&self) -> Box<dyn ItemHandle> {
        Box::new(self.clone())
    }

    fn to_any(&self) -> AnyView {
        self.clone().into()
    }

    fn render(&self, window: &mut Window, cx: &mut App) -> AnyElement {
        self.update(cx, |item, cx| item.render(window, cx).into_any_element())
    }
}

/// Extension methods for `dyn ItemHandle` trait objects.
///
/// Provides downcasting utilities to convert from trait objects back to concrete types.
impl dyn ItemHandle {
    /// Downcast this trait object to a concrete `Entity<V>` type.
    ///
    /// Attempts to convert the type-erased handle back to its original entity type.
    /// Returns `None` if the handle is not of type `V`.
    ///
    /// # Type Parameters
    ///
    /// * `V` - The concrete item type to downcast to (must implement [`Item`])
    ///
    /// # Returns
    ///
    /// `Some(Entity<V>)` if downcast succeeds, `None` if types don't match
    ///
    /// # Example
    ///
    /// ```ignore
    /// let item: Box<dyn ItemHandle> = Box::new(buffer_item);
    ///
    /// if let Some(buffer) = item.downcast::<BufferItem>() {
    ///     // Access BufferItem-specific methods
    ///     let contents = buffer.read(cx).buffer_snapshot(cx).text();
    /// }
    /// ```
    pub fn downcast<V: 'static>(&self) -> Option<Entity<V>> {
        self.to_any().downcast().ok()
    }
}
