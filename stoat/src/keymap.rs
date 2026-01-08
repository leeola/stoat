mod key;

use crate::actions::Action;
pub use key::Key;

pub struct KeymapContext {
    // Empty for now, will add mode, has_buffer, etc. later
}

pub(crate) struct Binding {
    pub key: Key,
    pub action: Action,
    pub predicate: Box<dyn Fn(&KeymapContext) -> bool + Send + Sync>,
}
