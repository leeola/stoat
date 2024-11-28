/// Raw output messages from the Stout runtime, expected to be rendered by a Stoat UI.
///
/// The resulting output from [`Input`](crate::input::Input).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Output {
    Close,
}
