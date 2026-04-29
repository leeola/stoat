use async_trait::async_trait;
use lsp_types::{
    CodeAction, CodeActionOrCommand, CodeActionParams, CompletionItem, CompletionParams,
    CompletionResponse, Diagnostic, DidChangeTextDocumentParams, DidCloseTextDocumentParams,
    DidOpenTextDocumentParams, DidSaveTextDocumentParams, DocumentFormattingParams,
    DocumentHighlight, DocumentHighlightParams, DocumentSymbolParams, DocumentSymbolResponse,
    GotoDefinitionParams, GotoDefinitionResponse, Hover, HoverParams, InitializeResult, InlayHint,
    InlayHintParams, Location, ProgressToken, ReferenceParams, RenameParams, ServerCapabilities,
    SignatureHelp, SignatureHelpParams, TextEdit, Uri, WorkDoneProgress, WorkspaceEdit,
    WorkspaceSymbolParams, WorkspaceSymbolResponse,
};
use std::io;

#[derive(Debug, Clone)]
pub enum LspNotification {
    Diagnostics {
        uri: Uri,
        diagnostics: Vec<Diagnostic>,
        version: Option<i32>,
    },
    Progress {
        token: ProgressToken,
        value: WorkDoneProgress,
    },
}

#[async_trait]
pub trait LspHost: Send + Sync {
    // Lifecycle
    async fn initialize(&self, root_uri: Option<Uri>) -> io::Result<InitializeResult>;
    async fn shutdown(&self) -> io::Result<()>;

    // Document sync
    async fn did_open(&self, params: DidOpenTextDocumentParams) -> io::Result<()>;
    async fn did_change(&self, params: DidChangeTextDocumentParams) -> io::Result<()>;
    async fn did_save(&self, params: DidSaveTextDocumentParams) -> io::Result<()>;
    async fn did_close(&self, params: DidCloseTextDocumentParams) -> io::Result<()>;

    // Navigation
    async fn hover(&self, params: HoverParams) -> io::Result<Option<Hover>>;
    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> io::Result<Option<GotoDefinitionResponse>>;
    async fn goto_declaration(
        &self,
        params: GotoDefinitionParams,
    ) -> io::Result<Option<GotoDefinitionResponse>>;
    async fn goto_type_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> io::Result<Option<GotoDefinitionResponse>>;
    async fn goto_implementation(
        &self,
        params: GotoDefinitionParams,
    ) -> io::Result<Option<GotoDefinitionResponse>>;
    async fn references(&self, params: ReferenceParams) -> io::Result<Option<Vec<Location>>>;
    async fn document_highlight(
        &self,
        params: DocumentHighlightParams,
    ) -> io::Result<Option<Vec<DocumentHighlight>>>;

    // Completion
    async fn completion(&self, params: CompletionParams) -> io::Result<Option<CompletionResponse>>;
    async fn completion_resolve(&self, item: CompletionItem) -> io::Result<CompletionItem>;

    // Code intelligence
    async fn code_action(
        &self,
        params: CodeActionParams,
    ) -> io::Result<Option<Vec<CodeActionOrCommand>>>;
    async fn code_action_resolve(&self, action: CodeAction) -> io::Result<CodeAction>;
    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> io::Result<Option<DocumentSymbolResponse>>;
    async fn workspace_symbol(
        &self,
        params: WorkspaceSymbolParams,
    ) -> io::Result<Option<WorkspaceSymbolResponse>>;
    async fn signature_help(
        &self,
        params: SignatureHelpParams,
    ) -> io::Result<Option<SignatureHelp>>;
    async fn inlay_hint(&self, params: InlayHintParams) -> io::Result<Option<Vec<InlayHint>>>;
    async fn inlay_hint_resolve(&self, hint: InlayHint) -> io::Result<InlayHint>;

    // Editing
    async fn rename(&self, params: RenameParams) -> io::Result<Option<WorkspaceEdit>>;
    async fn formatting(
        &self,
        params: DocumentFormattingParams,
    ) -> io::Result<Option<Vec<TextEdit>>>;

    // Server-pushed notifications
    async fn recv_notification(&self) -> Option<LspNotification>;
}

/// Default [`LspHost`] used when no language server is configured.
/// Every method returns the empty / no-op success response so action
/// handlers can call into the host unconditionally without branching
/// on "is a real server installed". Replaced by `LocalLsp` once the
/// production stdio transport lands; the test harness installs
/// [`crate::host::FakeLsp`] in its place.
pub struct NoopLsp;

#[async_trait]
impl LspHost for NoopLsp {
    async fn initialize(&self, _root_uri: Option<Uri>) -> io::Result<InitializeResult> {
        Ok(InitializeResult {
            capabilities: ServerCapabilities::default(),
            server_info: None,
        })
    }

    async fn shutdown(&self) -> io::Result<()> {
        Ok(())
    }

    async fn did_open(&self, _params: DidOpenTextDocumentParams) -> io::Result<()> {
        Ok(())
    }

    async fn did_change(&self, _params: DidChangeTextDocumentParams) -> io::Result<()> {
        Ok(())
    }

    async fn did_save(&self, _params: DidSaveTextDocumentParams) -> io::Result<()> {
        Ok(())
    }

    async fn did_close(&self, _params: DidCloseTextDocumentParams) -> io::Result<()> {
        Ok(())
    }

    async fn hover(&self, _params: HoverParams) -> io::Result<Option<Hover>> {
        Ok(None)
    }

    async fn goto_definition(
        &self,
        _params: GotoDefinitionParams,
    ) -> io::Result<Option<GotoDefinitionResponse>> {
        Ok(None)
    }

    async fn goto_declaration(
        &self,
        _params: GotoDefinitionParams,
    ) -> io::Result<Option<GotoDefinitionResponse>> {
        Ok(None)
    }

    async fn goto_type_definition(
        &self,
        _params: GotoDefinitionParams,
    ) -> io::Result<Option<GotoDefinitionResponse>> {
        Ok(None)
    }

    async fn goto_implementation(
        &self,
        _params: GotoDefinitionParams,
    ) -> io::Result<Option<GotoDefinitionResponse>> {
        Ok(None)
    }

    async fn references(&self, _params: ReferenceParams) -> io::Result<Option<Vec<Location>>> {
        Ok(None)
    }

    async fn document_highlight(
        &self,
        _params: DocumentHighlightParams,
    ) -> io::Result<Option<Vec<DocumentHighlight>>> {
        Ok(None)
    }

    async fn completion(
        &self,
        _params: CompletionParams,
    ) -> io::Result<Option<CompletionResponse>> {
        Ok(None)
    }

    async fn completion_resolve(&self, item: CompletionItem) -> io::Result<CompletionItem> {
        Ok(item)
    }

    async fn code_action(
        &self,
        _params: CodeActionParams,
    ) -> io::Result<Option<Vec<CodeActionOrCommand>>> {
        Ok(None)
    }

    async fn code_action_resolve(&self, action: CodeAction) -> io::Result<CodeAction> {
        Ok(action)
    }

    async fn document_symbol(
        &self,
        _params: DocumentSymbolParams,
    ) -> io::Result<Option<DocumentSymbolResponse>> {
        Ok(None)
    }

    async fn workspace_symbol(
        &self,
        _params: WorkspaceSymbolParams,
    ) -> io::Result<Option<WorkspaceSymbolResponse>> {
        Ok(None)
    }

    async fn signature_help(
        &self,
        _params: SignatureHelpParams,
    ) -> io::Result<Option<SignatureHelp>> {
        Ok(None)
    }

    async fn inlay_hint(&self, _params: InlayHintParams) -> io::Result<Option<Vec<InlayHint>>> {
        Ok(None)
    }

    async fn inlay_hint_resolve(&self, hint: InlayHint) -> io::Result<InlayHint> {
        Ok(hint)
    }

    async fn rename(&self, _params: RenameParams) -> io::Result<Option<WorkspaceEdit>> {
        Ok(None)
    }

    async fn formatting(
        &self,
        _params: DocumentFormattingParams,
    ) -> io::Result<Option<Vec<TextEdit>>> {
        Ok(None)
    }

    async fn recv_notification(&self) -> Option<LspNotification> {
        None
    }
}
