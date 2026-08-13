mod action;
pub mod defs;
mod kind;
mod param;
pub mod registry;

pub use action::{Action, ActionDef, ActionPriority};
// Every action type is re-exported at the crate root, so a consumer writes
// `stoat_action::Quit` rather than the module it happens to be defined in.
//
// Glob rather than a list per module, because a hand-written list falls behind
// the moment an action is added, and a consumer that fails to find a name at the
// root deep-paths around the gap instead of noticing it.
pub use defs::{
    agent::*, app::*, commits::*, conflict::*, dump::*, editor::*, file::*, file_finder::*,
    help::*, lsp::*, palette::*, pane::*, picker::*, prompt::*, rebase::*, review::*, run::*,
    set_theme::*, tab::*, terminal::*, walkthrough::*, workspace::*,
};
pub use kind::ActionKind;
pub use param::{ParamDef, ParamError, ParamKind, ParamValue, ValueSource};
