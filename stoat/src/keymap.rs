pub mod compiled;
pub mod dispatch;
pub mod hint;
pub mod query;

use crate::stoat::{KeyContext, KeyContextMeta, Mode};
pub use hint::KeybindingHint;
use std::collections::HashMap;

pub fn default_modes() -> HashMap<String, Mode> {
    let mut modes = HashMap::new();

    modes.insert("normal".into(), Mode::new("normal", "NORMAL", false));
    modes.insert("insert".into(), Mode::new("insert", "INSERT", false));
    modes.insert("visual".into(), Mode::new("visual", "VISUAL", true));
    modes.insert("pane".into(), Mode::new("pane", "PANE", false));
    modes.insert(
        "file_finder".into(),
        Mode::with_previous("file_finder", "FILE FINDER", "normal", false),
    );
    modes.insert(
        "buffer_finder".into(),
        Mode::with_previous("buffer_finder", "BUFFER FINDER", "normal", false),
    );
    modes.insert("space".into(), Mode::new("space", "SPACE", false));
    modes.insert(
        "goto".into(),
        Mode::with_previous("goto", "GOTO", "normal", false),
    );
    modes.insert(
        "buffer".into(),
        Mode::with_previous("buffer", "BUFFER", "normal", false),
    );
    modes.insert(
        "command_palette".into(),
        Mode::with_previous("command_palette", "COMMAND", "normal", false),
    );
    modes.insert(
        "git_status".into(),
        Mode::with_previous("git_status", "GIT STATUS", "normal", false),
    );
    modes.insert(
        "git_filter".into(),
        Mode::with_previous("git_filter", "GIT FILTER", "git_status", false),
    );
    modes.insert(
        "diff_review".into(),
        Mode::with_previous("diff_review", "DIFF REVIEW", "normal", false),
    );
    modes.insert(
        "help_modal".into(),
        Mode::with_previous("help_modal", "HELP", "normal", false),
    );
    modes.insert(
        "about_modal".into(),
        Mode::with_previous("about_modal", "ABOUT", "normal", false),
    );

    modes
}

pub fn default_contexts() -> HashMap<KeyContext, KeyContextMeta> {
    let mut contexts = HashMap::new();

    contexts.insert(KeyContext::TextEditor, KeyContextMeta::new("normal".into()));
    contexts.insert(KeyContext::Git, KeyContextMeta::new("git_status".into()));
    contexts.insert(
        KeyContext::FileFinder,
        KeyContextMeta::new("file_finder".into()),
    );
    contexts.insert(
        KeyContext::CommandPalette,
        KeyContextMeta::new("command_palette".into()),
    );
    contexts.insert(
        KeyContext::BufferFinder,
        KeyContextMeta::new("buffer_finder".into()),
    );
    contexts.insert(
        KeyContext::DiffReview,
        KeyContextMeta::new("diff_review".into()),
    );
    contexts.insert(
        KeyContext::HelpModal,
        KeyContextMeta::new("help_modal".into()),
    );
    contexts.insert(
        KeyContext::AboutModal,
        KeyContextMeta::new("about_modal".into()),
    );

    contexts
}
