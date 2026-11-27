//! The lower layer of the editor, which the app layer in `stoat` builds on.
//!
//! It holds the hosts over the file system, git, language servers, and
//! terminals, the keymap, the selections, and the syntax parse. Nothing here
//! reaches up into the app layer, and it is its own crate so that an edit to
//! the app layer leaves it compiled. `stoat` names each of its modules through
//! an alias of the same name, so `stoat` code reaches a module here at
//! `crate::<module>`.

use stoat_buffer::{buffer, display_map, multi_buffer};
#[cfg(test)]
use stoat_theme as theme;

pub mod host;
pub mod keymap;
pub mod selection;
pub mod syntax_parse;
