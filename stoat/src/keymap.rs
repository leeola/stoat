pub mod compiled;
pub mod dispatch;
pub mod infobox;
pub mod query;
pub mod usage;

use crate::stoat::{KeyContext, KeyContextMeta, Mode};
use std::collections::HashMap;

pub fn default_modes() -> HashMap<String, Mode> {
    let mut modes = HashMap::new();

    modes.insert("normal".into(), Mode::new("normal", "NORMAL", false, false));
    modes.insert("insert".into(), Mode::new("insert", "INSERT", false, false));
    modes.insert("visual".into(), Mode::new("visual", "VISUAL", true, true));
    modes.insert("pane".into(), Mode::new("pane", "PANE", false, true));
    modes.insert(
        "file_finder".into(),
        Mode::with_previous("file_finder", "FILE FINDER", "normal", false, true),
    );
    modes.insert(
        "buffer_finder".into(),
        Mode::with_previous("buffer_finder", "BUFFER FINDER", "normal", false, true),
    );
    modes.insert("space".into(), Mode::new("space", "SPACE", false, true));
    modes.insert(
        "goto".into(),
        Mode::with_previous("goto", "GOTO", "normal", false, true),
    );
    modes.insert(
        "buffer".into(),
        Mode::with_previous("buffer", "BUFFER", "normal", false, true),
    );
    modes.insert(
        "command_palette".into(),
        Mode::with_previous("command_palette", "COMMAND", "normal", false, true),
    );
    modes.insert(
        "git_status".into(),
        Mode::with_previous("git_status", "GIT STATUS", "normal", false, true),
    );
    modes.insert(
        "git_filter".into(),
        Mode::with_previous("git_filter", "GIT FILTER", "git_status", false, true),
    );
    modes.insert(
        "diff_review".into(),
        Mode::with_previous("diff_review", "DIFF REVIEW", "normal", false, true),
    );
    modes.insert(
        "conflict_review".into(),
        Mode::with_previous("conflict_review", "CONFLICT", "normal", false, true),
    );
    modes.insert(
        "help_modal".into(),
        Mode::with_previous("help_modal", "HELP", "normal", false, true),
    );
    modes.insert(
        "about_modal".into(),
        Mode::with_previous("about_modal", "ABOUT", "normal", false, true),
    );
    modes.insert(
        "lsp".into(),
        Mode::with_previous("lsp", "LSP", "normal", false, true),
    );
    modes.insert(
        "view".into(),
        Mode::with_previous("view", "VIEW", "normal", false, true),
    );
    modes.insert(
        "git".into(),
        Mode::with_previous("git", "GIT", "normal", false, true),
    );
    modes.insert(
        "blame_review".into(),
        Mode::with_previous("blame_review", "BLAME", "normal", false, true),
    );
    modes.insert(
        "blame_commit_diff".into(),
        Mode::with_previous(
            "blame_commit_diff",
            "COMMIT DIFF",
            "blame_review",
            false,
            true,
        ),
    );
    modes.insert(
        "symbol_picker".into(),
        Mode::with_previous("symbol_picker", "SYMBOL PICKER", "normal", false, true),
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
        KeyContext::ConflictReview,
        KeyContextMeta::new("conflict_review".into()),
    );
    contexts.insert(
        KeyContext::HelpModal,
        KeyContextMeta::new("help_modal".into()),
    );
    contexts.insert(
        KeyContext::AboutModal,
        KeyContextMeta::new("about_modal".into()),
    );
    contexts.insert(KeyContext::Claude, KeyContextMeta::new("normal".into()));
    contexts.insert(
        KeyContext::SymbolPicker,
        KeyContextMeta::new("symbol_picker".into()),
    );
    contexts.insert(
        KeyContext::BlameReview,
        KeyContextMeta::new("blame_review".into()),
    );
    contexts.insert(
        KeyContext::BlameCommitDiff,
        KeyContextMeta::new("blame_commit_diff".into()),
    );

    contexts
}
