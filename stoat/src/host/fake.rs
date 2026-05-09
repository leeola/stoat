mod claude_code;
mod clipboard;
mod git;
mod lsp;
pub mod terminal;

pub use self::{
    claude_code::{FakeClaudeCode, FakeClaudeCodeHost},
    clipboard::FakeClipboard,
    git::{FakeGit, FakeGitRepo, FakeRepoBuilder},
    lsp::{
        change_params, completion_params, definition_params, document_highlight_params,
        hover_params, inlay_hint_params, open_params, reference_params, workspace_symbol_params,
        FakeLsp,
    },
};
pub use stoat_host::{FakeEnv, FakeFs, FakeFsOp, FakeFsWatcher};
