//! In-process language server for the `stcfg` config language, gated behind
//! the `builtin-lsp` feature.
//!
//! [`StcfgLsp`] implements [`LspServer`] entirely in-process -- no subprocess,
//! no stdio transport. It advertises completion and hover so the editor routes
//! those requests to it (diagnostics travel the push path, which every server
//! supports). The request handlers are no-ops for now; completion, diagnostics,
//! and hover are filled in by the follow-on work that reads the
//! [`stoat_config::setting_catalog`]. [`crate::host::local::LocalLspHost`]
//! returns one of these for `stcfg`-language launches when the feature is on.

use crate::host::lsp::{IncomingRequest, LspNotification, LspResponseError, LspServer};
use async_trait::async_trait;
use lsp_types::{
    CallHierarchyIncomingCall, CallHierarchyIncomingCallsParams, CallHierarchyItem,
    CallHierarchyOutgoingCall, CallHierarchyOutgoingCallsParams, CallHierarchyPrepareParams,
    CodeAction, CodeActionOrCommand, CodeActionParams, CodeLens, CodeLensParams, ColorInformation,
    ColorPresentation, ColorPresentationParams, CompletionItem, CompletionOptions,
    CompletionParams, CompletionResponse, DidChangeConfigurationParams,
    DidChangeTextDocumentParams, DidChangeWatchedFilesParams, DidChangeWorkspaceFoldersParams,
    DidCloseTextDocumentParams, DidOpenTextDocumentParams, DidSaveTextDocumentParams,
    DocumentColorParams, DocumentDiagnosticParams, DocumentDiagnosticReportResult,
    DocumentFormattingParams, DocumentHighlight, DocumentHighlightParams, DocumentLink,
    DocumentLinkParams, DocumentRangeFormattingParams, DocumentSymbolParams,
    DocumentSymbolResponse, ExecuteCommandParams, FoldingRange, FoldingRangeParams,
    GotoDefinitionParams, GotoDefinitionResponse, Hover, HoverParams, HoverProviderCapability,
    InitializeResult, InlayHint, InlayHintParams, Location, NumberOrString, PrepareRenameResponse,
    ReferenceParams, RenameFilesParams, RenameParams, SelectionRange, SelectionRangeParams,
    SemanticTokensParams, SemanticTokensRangeParams, SemanticTokensRangeResult,
    SemanticTokensResult, ServerCapabilities, ServerInfo, SignatureHelp, SignatureHelpParams,
    TextDocumentPositionParams, TextEdit, TypeHierarchyItem, TypeHierarchyPrepareParams,
    TypeHierarchySubtypesParams, TypeHierarchySupertypesParams, Uri, WorkspaceEdit,
    WorkspaceSymbolParams, WorkspaceSymbolResponse,
};
use serde_json::Value;
use std::{
    io,
    sync::{Arc, LazyLock},
};

static STCFG_CAPABILITIES: LazyLock<Arc<ServerCapabilities>> = LazyLock::new(|| {
    Arc::new(ServerCapabilities {
        completion_provider: Some(CompletionOptions::default()),
        hover_provider: Some(HoverProviderCapability::Simple(true)),
        ..ServerCapabilities::default()
    })
});

/// Builtin in-process LSP server for `.stcfg` buffers.
///
/// Advertises completion and hover; every request handler is currently a
/// no-op success, so the server is inert until the completion/diagnostics/hover
/// handlers land. Construct via [`StcfgLsp::new`]; the host hands it out for
/// `stcfg`-language launches.
pub struct StcfgLsp;

impl StcfgLsp {
    pub fn new() -> Self {
        StcfgLsp
    }
}

impl Default for StcfgLsp {
    fn default() -> Self {
        StcfgLsp::new()
    }
}

#[async_trait]
impl LspServer for StcfgLsp {
    fn capabilities(&self) -> Arc<ServerCapabilities> {
        STCFG_CAPABILITIES.clone()
    }

    async fn initialize(&self, _root_uri: Option<Uri>) -> io::Result<InitializeResult> {
        Ok(InitializeResult {
            capabilities: (*self.capabilities()).clone(),
            server_info: Some(ServerInfo {
                name: "stcfg-builtin".to_string(),
                version: None,
            }),
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

    async fn did_rename(&self, _params: RenameFilesParams) -> io::Result<()> {
        Ok(())
    }

    async fn did_change_watched_files(
        &self,
        _params: DidChangeWatchedFilesParams,
    ) -> io::Result<()> {
        Ok(())
    }

    async fn did_change_configuration(
        &self,
        _params: DidChangeConfigurationParams,
    ) -> io::Result<()> {
        Ok(())
    }

    async fn did_change_workspace_folders(
        &self,
        _params: DidChangeWorkspaceFoldersParams,
    ) -> io::Result<()> {
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

    async fn code_lens(&self, _params: CodeLensParams) -> io::Result<Option<Vec<CodeLens>>> {
        Ok(None)
    }

    async fn code_lens_resolve(&self, lens: CodeLens) -> io::Result<CodeLens> {
        Ok(lens)
    }

    async fn document_link(
        &self,
        _params: DocumentLinkParams,
    ) -> io::Result<Option<Vec<DocumentLink>>> {
        Ok(None)
    }

    async fn document_link_resolve(&self, link: DocumentLink) -> io::Result<DocumentLink> {
        Ok(link)
    }

    async fn document_color(
        &self,
        _params: DocumentColorParams,
    ) -> io::Result<Option<Vec<ColorInformation>>> {
        Ok(None)
    }

    async fn color_presentation(
        &self,
        _params: ColorPresentationParams,
    ) -> io::Result<Option<Vec<ColorPresentation>>> {
        Ok(None)
    }

    async fn semantic_tokens_full(
        &self,
        _params: SemanticTokensParams,
    ) -> io::Result<Option<SemanticTokensResult>> {
        Ok(None)
    }

    async fn semantic_tokens_range(
        &self,
        _params: SemanticTokensRangeParams,
    ) -> io::Result<Option<SemanticTokensRangeResult>> {
        Ok(None)
    }

    async fn prepare_call_hierarchy(
        &self,
        _params: CallHierarchyPrepareParams,
    ) -> io::Result<Option<Vec<CallHierarchyItem>>> {
        Ok(None)
    }

    async fn call_hierarchy_incoming_calls(
        &self,
        _params: CallHierarchyIncomingCallsParams,
    ) -> io::Result<Option<Vec<CallHierarchyIncomingCall>>> {
        Ok(None)
    }

    async fn call_hierarchy_outgoing_calls(
        &self,
        _params: CallHierarchyOutgoingCallsParams,
    ) -> io::Result<Option<Vec<CallHierarchyOutgoingCall>>> {
        Ok(None)
    }

    async fn prepare_type_hierarchy(
        &self,
        _params: TypeHierarchyPrepareParams,
    ) -> io::Result<Option<Vec<TypeHierarchyItem>>> {
        Ok(None)
    }

    async fn type_hierarchy_supertypes(
        &self,
        _params: TypeHierarchySupertypesParams,
    ) -> io::Result<Option<Vec<TypeHierarchyItem>>> {
        Ok(None)
    }

    async fn type_hierarchy_subtypes(
        &self,
        _params: TypeHierarchySubtypesParams,
    ) -> io::Result<Option<Vec<TypeHierarchyItem>>> {
        Ok(None)
    }

    async fn document_symbol(
        &self,
        _params: DocumentSymbolParams,
    ) -> io::Result<Option<DocumentSymbolResponse>> {
        Ok(None)
    }

    async fn document_diagnostic(
        &self,
        _params: DocumentDiagnosticParams,
    ) -> io::Result<Option<DocumentDiagnosticReportResult>> {
        Ok(None)
    }

    async fn folding_range(
        &self,
        _params: FoldingRangeParams,
    ) -> io::Result<Option<Vec<FoldingRange>>> {
        Ok(None)
    }

    async fn selection_range(
        &self,
        _params: SelectionRangeParams,
    ) -> io::Result<Option<Vec<SelectionRange>>> {
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

    async fn range_inlay_hint(
        &self,
        _params: InlayHintParams,
    ) -> io::Result<Option<Vec<InlayHint>>> {
        Ok(None)
    }

    async fn prepare_rename(
        &self,
        _params: TextDocumentPositionParams,
    ) -> io::Result<Option<PrepareRenameResponse>> {
        Ok(None)
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

    async fn range_formatting(
        &self,
        _params: DocumentRangeFormattingParams,
    ) -> io::Result<Option<Vec<TextEdit>>> {
        Ok(None)
    }

    async fn will_rename(&self, _params: RenameFilesParams) -> io::Result<Option<WorkspaceEdit>> {
        Ok(None)
    }

    async fn execute_command(&self, _params: ExecuteCommandParams) -> io::Result<Option<Value>> {
        Ok(None)
    }

    async fn recv_notification(&self) -> Option<LspNotification> {
        None
    }

    async fn try_recv_notification(&self) -> Option<LspNotification> {
        None
    }

    async fn recv_incoming_request(&self) -> Option<IncomingRequest> {
        None
    }

    async fn try_recv_incoming_request(&self) -> Option<IncomingRequest> {
        None
    }

    async fn reply(
        &self,
        _id: NumberOrString,
        _result: Result<Value, LspResponseError>,
    ) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::lsp::LanguageServerFeature;

    #[test]
    fn advertises_completion_and_hover() {
        let server = StcfgLsp::new();
        assert!(server.supports_feature(LanguageServerFeature::Completion));
        assert!(server.supports_feature(LanguageServerFeature::Hover));
        // Push diagnostics need no capability and are always supported.
        assert!(server.supports_feature(LanguageServerFeature::Diagnostics));
        // Features it does not implement stay unadvertised.
        assert!(!server.supports_feature(LanguageServerFeature::Format));
        assert!(!server.supports_feature(LanguageServerFeature::RenameSymbol));
    }
}
