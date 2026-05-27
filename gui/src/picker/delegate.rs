use crate::{editor::Editor, picker::Picker};
use gpui::{AnyElement, Context, Entity, Task, Window};
use stoat_action::Action;

/// Confirmation modifier carried by [`PickerDelegate::confirm`].
///
/// `None` is the primary confirm (typically `Enter`); the variants
/// land with the modifier-routed confirmations follow-up that wires
/// the matching keystrokes through the picker's action dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerSecondary {
    OpenInRight,
    OpenInDown,
}

/// Per-picker behavior driven by the [`Picker`] container.
///
/// The container owns the query editor, the result list scroll, and
/// the action dispatch; the delegate owns the items, the active
/// selection cursor, and the per-item rendering. Implementors hand
/// out the selection cursor through [`selected_index`] /
/// [`set_selected_index`] so the container can wire keystroke-driven
/// navigation without reaching into the delegate's storage.
///
/// [`update_matches`] returns a [`Task`] so a delegate that walks
/// the filesystem or queries an LSP server can run on the background
/// executor; the container drops the prior task on every fresh edit
/// so an in-flight walk is cancelled when the query changes.
pub trait PickerDelegate: Sized + 'static {
    fn match_count(&self) -> usize;

    fn selected_index(&self) -> usize;

    fn set_selected_index(&mut self, ix: usize, cx: &mut Context<'_, Picker<Self>>);

    fn update_matches(&mut self, query: String, cx: &mut Context<'_, Picker<Self>>) -> Task<()>;

    fn confirm(
        &mut self,
        secondary: Option<PickerSecondary>,
        window: &mut Window,
        cx: &mut Context<'_, Picker<Self>>,
    );

    fn dismissed(&mut self, cx: &mut Context<'_, Picker<Self>>);

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        cx: &mut Context<'_, Picker<Self>>,
    ) -> AnyElement;

    /// Invoked once during [`Picker::new`] after the query editor is
    /// constructed. Lets delegates that need to mutate the query
    /// editor (e.g. multi-step argument collection) capture a weak
    /// handle without reaching into the picker through its own
    /// context, which is being mutated while the delegate runs.
    fn on_attach(&mut self, _query_editor: &Entity<Editor>) {}

    /// Claim a delegate-specific action before the picker's generic
    /// dispatch path inspects it. Returns `true` when the delegate
    /// consumed the action so the picker skips its own arms. The
    /// default no-op returns `false` so generic delegates inherit
    /// the picker's standard handling untouched.
    fn handle_action(
        &mut self,
        _action: &dyn Action,
        _window: &mut Window,
        _cx: &mut Context<'_, Picker<Self>>,
    ) -> bool {
        false
    }

    /// Render an optional preview pane next to the picker's result
    /// list. Returning `Some(element)` switches the picker layout
    /// from a single vertical column to a horizontal split with the
    /// query+list on the left and the preview on the right. Default
    /// is `None`, which keeps the original column layout.
    fn render_preview(&self, _cx: &mut Context<'_, Picker<Self>>) -> Option<AnyElement> {
        None
    }

    /// Invoked after the active row changes (via arrow-key
    /// navigation or `Picker::set_selected_index`). Default is a
    /// no-op; preview-bearing delegates override to refresh the
    /// preview content lazily as selection moves.
    fn selection_changed(&mut self, _cx: &mut Context<'_, Picker<Self>>) {}
}
