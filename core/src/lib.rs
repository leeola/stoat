//! The lower layer of the editor, which the app layer in `stoat` builds on.
//!
//! It holds the hosts over the file system, git, language servers, and
//! terminals. Nothing here reaches up into the app layer, and it is its own
//! crate so that an edit to the app layer leaves it compiled. `stoat` names
//! each of its modules through an alias of the same name, so `stoat` code
//! reaches a module here at `crate::<module>`.

pub mod host;
