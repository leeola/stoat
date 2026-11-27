mod clipboard;

pub use self::clipboard::FakeClipboard;
pub use stoat_host::{FakeEnv, FakeFs, FakeFsOp, FakeFsWatcher};
pub use stoat_host_git::{FakeGit, FakeGitRepo, FakeRepoBuilder};
pub use stoat_host_lsp::{
    change_params, completion_params, definition_params, document_highlight_params, hover_params,
    inlay_hint_params, open_params, reference_params, workspace_symbol_params, FakeLsp,
};
