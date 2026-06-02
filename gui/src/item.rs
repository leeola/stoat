use gpui::{AnyView, App, Context, Entity, EntityId, Render, SharedString, Task};
use serde::{Deserialize, Serialize};
use snafu::Snafu;

/// Discriminator carried alongside each pane item's serialized
/// blob so the workspace-persistence layer can dispatch to the
/// right materialization helper on restore. Concrete
/// [`ItemView`] impls override [`ItemView::item_kind`] to declare
/// their variant; the default [`ItemKind::Unknown`] is reserved
/// for transient items that don't need to round-trip.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ItemKind {
    Editor,
    Run,
    Claude,
    Conflict,
    Rebase,
    Review,
    CommitList,
    ProjectTree,
    OutlinePanel,
    DiagnosticsPanel,
    MarkdownPreview,
    Unknown,
}

/// Contract every pane-hosted item satisfies. Concrete impls
/// (`Editor`, `Run`, `ClaudeChat`, `CommitList`, `Rebase`, `Review`)
/// expose enough state for a `Pane` to paint its tab strip, route
/// save / persistence calls, and round-trip the item across a
/// workspace reload.
///
/// `tab_label` and `deserialize` are required; the remaining four
/// methods have read-only defaults so an item that has no icon, no
/// dirty state, no save side-effects, and no persistence only
/// overrides the two required methods.
///
/// The trait is intentionally not object-safe -- `deserialize`
/// returns `Self`. The future `ItemHandle` wrapper hides this
/// behind an object-safe trait (a `Box<dyn ItemHandle>` that omits
/// the `deserialize -> Self` method) so a `Vec<Box<dyn ItemHandle>>`
/// tab list still works.
pub trait ItemView: Render + 'static {
    /// Tab label rendered in the pane's tab strip. Concrete impls
    /// derive this from their underlying buffer / session.
    fn tab_label(&self, cx: &App) -> SharedString;

    /// Optional icon name resolved by the theme. `None` means the
    /// tab has no icon. Defaults to `None`.
    fn tab_icon(&self, _cx: &App) -> Option<SharedString> {
        None
    }

    /// Whether the item has unsaved changes. The tab bar paints a
    /// dirty marker when this returns `true`. Defaults to `false`
    /// for read-only items.
    fn is_dirty(&self, _cx: &App) -> bool {
        false
    }

    /// Persist the item's changes. Default no-op for read-only
    /// items. Items that perform IO return a task that resolves on
    /// completion; the pane awaits the task to decide whether to
    /// clear the dirty marker.
    fn save(&mut self, _cx: &mut Context<'_, Self>) -> Task<Result<(), ItemError>> {
        Task::ready(Ok(()))
    }

    /// Serialize the item's state for workspace persistence.
    /// Defaults to `Value::Null` so transient items don't reserve
    /// space in the on-disk workspace blob.
    fn serialize(&self, _cx: &App) -> serde_json::Value {
        serde_json::Value::Null
    }

    /// Reconstruct the item from previously-serialized state.
    /// Returns an `ItemError::Deserialize` when the value's shape
    /// does not match this item's schema; concrete impls call this
    /// directly with the persisted JSON.
    fn deserialize(value: serde_json::Value, cx: &mut Context<'_, Self>) -> Result<Self, ItemError>
    where
        Self: Sized;

    /// Discriminator used by workspace persistence to dispatch on
    /// item type at restore time. Default returns
    /// [`ItemKind::Unknown`] so transient or test-only items do
    /// not need to opt into the persistence schema.
    fn item_kind(&self) -> ItemKind {
        ItemKind::Unknown
    }
}

/// Object-safe wrapper over an `Entity<T: ItemView>` so a pane's
/// tab list can hold heterogeneous items as `Box<dyn ItemHandle>`.
/// Exposes only the call-the-inner-method surface; `deserialize`
/// stays on [`ItemView`] because it returns `Self` and is not
/// object-safe. A blanket impl below makes any `Entity<T: ItemView>`
/// usable as a handle.
pub trait ItemHandle: Send + 'static {
    fn item_id(&self) -> EntityId;
    fn to_any_view(&self) -> AnyView;
    /// Clone the underlying entity handle into a fresh boxed
    /// trait object. Lets callers pass an owned `Box<dyn
    /// ItemHandle>` (and therefore decouple from any read borrow
    /// the source was extracted from) without making the trait
    /// non-object-safe via a `Clone` bound.
    fn boxed_clone(&self) -> Box<dyn ItemHandle>;
    fn tab_label(&self, cx: &App) -> SharedString;
    fn tab_icon(&self, cx: &App) -> Option<SharedString>;
    fn is_dirty(&self, cx: &App) -> bool;
    fn serialize(&self, cx: &App) -> serde_json::Value;
    fn item_kind(&self, cx: &App) -> ItemKind;
    fn save(&self, cx: &mut App) -> Task<Result<(), ItemError>>;
}

impl<T: ItemView> ItemHandle for Entity<T> {
    fn item_id(&self) -> EntityId {
        self.entity_id()
    }

    fn to_any_view(&self) -> AnyView {
        AnyView::from(self.clone())
    }

    fn boxed_clone(&self) -> Box<dyn ItemHandle> {
        Box::new(self.clone())
    }

    fn tab_label(&self, cx: &App) -> SharedString {
        self.read(cx).tab_label(cx)
    }

    fn tab_icon(&self, cx: &App) -> Option<SharedString> {
        self.read(cx).tab_icon(cx)
    }

    fn is_dirty(&self, cx: &App) -> bool {
        self.read(cx).is_dirty(cx)
    }

    fn serialize(&self, cx: &App) -> serde_json::Value {
        self.read(cx).serialize(cx)
    }

    fn item_kind(&self, cx: &App) -> ItemKind {
        self.read(cx).item_kind()
    }

    fn save(&self, cx: &mut App) -> Task<Result<(), ItemError>> {
        self.update(cx, |item, ctx| item.save(ctx))
    }
}

#[derive(Debug, Snafu)]
#[snafu(visibility(pub))]
pub enum ItemError {
    #[snafu(display("save failed: {reason}"))]
    Save {
        reason: String,
        #[snafu(implicit)]
        location: snafu::Location,
    },
    #[snafu(display("deserialize failed: {reason}"))]
    Deserialize {
        reason: String,
        #[snafu(implicit)]
        location: snafu::Location,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{div, AppContext, IntoElement, Styled, TestAppContext, Window};
    use serde_json::Value;

    struct ReadOnlyItem {
        label: SharedString,
    }

    impl Render for ReadOnlyItem {
        fn render(
            &mut self,
            _window: &mut Window,
            _cx: &mut Context<'_, Self>,
        ) -> impl IntoElement {
            div().size_full()
        }
    }

    impl ItemView for ReadOnlyItem {
        fn tab_label(&self, _cx: &App) -> SharedString {
            self.label.clone()
        }

        fn deserialize(value: Value, _cx: &mut Context<'_, Self>) -> Result<Self, ItemError> {
            match value {
                Value::String(s) => Ok(Self { label: s.into() }),
                _ => DeserializeSnafu {
                    reason: "expected JSON string label",
                }
                .fail(),
            }
        }
    }

    struct DirtyItem {
        label: SharedString,
        body: String,
    }

    impl Render for DirtyItem {
        fn render(
            &mut self,
            _window: &mut Window,
            _cx: &mut Context<'_, Self>,
        ) -> impl IntoElement {
            div().size_full()
        }
    }

    impl ItemView for DirtyItem {
        fn tab_label(&self, _cx: &App) -> SharedString {
            self.label.clone()
        }

        fn is_dirty(&self, _cx: &App) -> bool {
            true
        }

        fn tab_icon(&self, _cx: &App) -> Option<SharedString> {
            Some("file".into())
        }

        fn serialize(&self, _cx: &App) -> Value {
            Value::String(self.body.clone())
        }

        fn deserialize(value: Value, _cx: &mut Context<'_, Self>) -> Result<Self, ItemError> {
            match value {
                Value::String(body) => Ok(Self {
                    label: "restored".into(),
                    body,
                }),
                _ => DeserializeSnafu {
                    reason: "expected JSON string body",
                }
                .fail(),
            }
        }
    }

    #[test]
    fn defaults_cover_read_only_items() {
        let cx = TestAppContext::single();
        let item = cx.update(|cx| {
            cx.new(|_| ReadOnlyItem {
                label: "readme".into(),
            })
        });

        item.read_with(&cx, |item, app| {
            assert_eq!(item.tab_label(app), SharedString::from("readme"));
            assert!(item.tab_icon(app).is_none());
            assert!(!item.is_dirty(app));
            assert!(item.serialize(app).is_null());
        });
    }

    #[test]
    fn overrides_dispatch_through_trait() {
        let cx = TestAppContext::single();
        let item = cx.update(|cx| {
            cx.new(|_| DirtyItem {
                label: "draft".into(),
                body: "hello".into(),
            })
        });

        item.read_with(&cx, |item, app| {
            assert_eq!(item.tab_label(app), SharedString::from("draft"));
            assert_eq!(item.tab_icon(app), Some(SharedString::from("file")));
            assert!(item.is_dirty(app));
            assert_eq!(item.serialize(app), Value::String("hello".into()));
        });
    }

    #[test]
    fn deserialize_round_trips_serialized_state() {
        let cx = TestAppContext::single();
        let restored = cx.update(|cx| {
            cx.new(|cx| {
                DirtyItem::deserialize(Value::String("from disk".into()), cx)
                    .expect("deserialize succeeds for a string value")
            })
        });

        restored.read_with(&cx, |item, _| {
            assert_eq!(item.body, "from disk");
            assert_eq!(item.label, SharedString::from("restored"));
        });
    }

    #[test]
    fn deserialize_rejects_wrong_shape() {
        let cx = TestAppContext::single();
        let result = cx.update(|cx| {
            let mut error: Option<ItemError> = None;
            let _ = cx.new(|cx| match DirtyItem::deserialize(Value::Null, cx) {
                Ok(item) => item,
                Err(e) => {
                    error = Some(e);
                    DirtyItem {
                        label: "unused".into(),
                        body: String::new(),
                    }
                },
            });
            error
        });

        let err = result.expect("deserialize fails for Value::Null");
        assert!(matches!(err, ItemError::Deserialize { .. }));
    }
}
