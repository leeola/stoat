use async_trait::async_trait;
use lsp_types::{
    CodeAction, CodeActionOrCommand, CodeActionParams, CompletionItem, CompletionParams,
    CompletionResponse, Diagnostic, DidChangeTextDocumentParams, DidCloseTextDocumentParams,
    DidOpenTextDocumentParams, DidSaveTextDocumentParams, DocumentFormattingParams,
    DocumentHighlight, DocumentHighlightParams, DocumentSymbolParams, DocumentSymbolResponse,
    GotoDefinitionParams, GotoDefinitionResponse, Hover, HoverParams, InitializeResult, InlayHint,
    InlayHintParams, Location, ProgressToken, ReferenceParams, RenameParams, SignatureHelp,
    SignatureHelpParams, TextEdit, Uri, WorkDoneProgress, WorkspaceEdit, WorkspaceSymbolParams,
    WorkspaceSymbolResponse,
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
