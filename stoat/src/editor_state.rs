use slotmap::new_key_type;

new_key_type! {
    /// Workspace-scoped editor key. Referenced by [`crate::pane::View::Editor`]
    /// and [`crate::review_session`] to point at a specific editor without
    /// owning it.
    pub struct EditorId;
}
