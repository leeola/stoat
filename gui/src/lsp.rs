use crate::workspace::Workspace;
use gpui::{AsyncApp, WeakEntity};
use std::sync::Arc;
use stoat::host::LspServer;
use stoat_language::Language;

pub mod code_action;
pub mod code_lens;
pub mod completion;
pub mod edit_apply;
pub mod goto;
pub mod hover;
pub mod inlay_hints;
pub mod popup;
pub mod references;
pub mod rename;
pub mod semantic_tokens;
pub mod signature_help;

pub use code_action::CodeActionPickerDelegate;
pub use code_lens::CodeLensManager;
pub use completion::CompletionPopup;
pub use hover::HoverPopup;
pub use inlay_hints::InlayHintsManager;
pub use references::ReferencesPickerDelegate;
pub use semantic_tokens::SemanticTokensManager;
pub use signature_help::SignatureHelpManager;

/// Resolve the persistent, document-synced server for `language` from the
/// workspace's [`crate::LspManager`] cache, for use inside a request handler's
/// async task. `None` when there is no workspace (the editor is unattached), it
/// has been dropped, or no server is available.
pub(crate) async fn cached_server(
    workspace: &Option<WeakEntity<Workspace>>,
    language: Arc<Language>,
    cx: &mut AsyncApp,
) -> Option<Arc<dyn LspServer>> {
    let workspace = workspace.as_ref()?;
    let task = workspace
        .update(cx, |workspace, cx| workspace.lsp_server(language, cx))
        .ok()?;
    task.await
}
