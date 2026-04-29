use async_trait::async_trait;
use lsp_types::{
    CodeAction, CodeActionOrCommand, CodeActionParams, CodeActionProviderCapability,
    ColorProviderCapability, CompletionItem, CompletionParams, CompletionResponse,
    DeclarationCapability, Diagnostic, DidChangeTextDocumentParams, DidCloseTextDocumentParams,
    DidOpenTextDocumentParams, DidSaveTextDocumentParams, DocumentFormattingParams,
    DocumentHighlight, DocumentHighlightParams, DocumentRangeFormattingParams,
    DocumentSymbolParams, DocumentSymbolResponse, FoldingRange, FoldingRangeParams,
    GotoDefinitionParams, GotoDefinitionResponse, Hover, HoverParams, HoverProviderCapability,
    ImplementationProviderCapability, InitializeResult, InlayHint, InlayHintParams,
    InlayHintServerCapabilities, Location, MessageType, OneOf, PrepareRenameResponse,
    ProgressToken, ReferenceParams, RenameParams, SelectionRange, SelectionRangeParams,
    ServerCapabilities, SignatureHelp, SignatureHelpParams, TextDocumentPositionParams, TextEdit,
    TypeDefinitionProviderCapability, Uri, WorkDoneProgress, WorkspaceEdit, WorkspaceSymbolParams,
    WorkspaceSymbolResponse,
};
use std::{
    io,
    sync::{Arc, LazyLock},
};

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
    /// `window/logMessage` -- server-emitted log entry. Severity
    /// carried by `typ` (`MessageType::ERROR` / `WARNING` / `INFO`
    /// / `LOG`). Editor typically routes these into a tracing-style
    /// log rather than user-visible UI.
    LogMessage { typ: MessageType, message: String },
    /// `window/showMessage` -- server-initiated message intended
    /// for user display (toast / status entry).
    ShowMessage { typ: MessageType, message: String },
    /// `$/logTrace` -- protocol-level trace; emitted only when the
    /// client requested `trace=verbose` during initialization.
    /// `verbose` carries an optional secondary string (extra
    /// detail; spec calls it `verbose`).
    LogTrace {
        message: String,
        verbose: Option<String>,
    },
}

/// Width of the `Position.character` offset negotiated with the
/// server during initialization. LSP defaults to UTF-16 code units,
/// but stoat's rope works in UTF-8 byte offsets, so every
/// position-conversion helper has to know which encoding the server
/// is using to translate without off-by-one errors on multi-byte
/// chars.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum OffsetEncoding {
    /// Offsets count UTF-8 code units (bytes).
    Utf8,
    /// Offsets count UTF-16 code units. The LSP default; every
    /// server is required to support it.
    #[default]
    Utf16,
    /// Offsets count UTF-32 code units (Unicode code points).
    Utf32,
}

/// Coarse capability category used to ask "does this server support
/// feature X" without re-walking the raw [`ServerCapabilities`] at
/// every callsite. The variant set mirrors the user-facing actions
/// stoat dispatches; `LspHost::supports_feature` decodes each
/// against the relevant `ServerCapabilities` field.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LanguageServerFeature {
    Format,
    GotoDeclaration,
    GotoDefinition,
    GotoTypeDefinition,
    GotoReference,
    GotoImplementation,
    SignatureHelp,
    Hover,
    DocumentHighlight,
    Completion,
    CodeAction,
    WorkspaceCommand,
    DocumentSymbols,
    WorkspaceSymbols,
    /// Push-style diagnostics arrive unsolicited; every server is
    /// considered to support this since opting out is signalled by
    /// the absence of `publishDiagnostics` traffic, not a capability.
    Diagnostics,
    PullDiagnostics,
    RenameSymbol,
    InlayHints,
    DocumentColors,
}

#[async_trait]
pub trait LspHost: Send + Sync {
    /// Capabilities the server reported in its `InitializeResult`.
    /// Returned by [`Arc`] clone so impls with interior-mutable
    /// storage (e.g. test fakes that swap capabilities mid-test)
    /// stay lock-free for readers. Hosts that have not yet
    /// completed initialization return the empty defaults.
    fn capabilities(&self) -> Arc<ServerCapabilities>;

    /// Negotiated [`OffsetEncoding`] for `Position.character` width.
    /// Default impl reads from
    /// `capabilities().position_encoding`; absent or unrecognized
    /// values fall back to UTF-16 per the LSP spec.
    fn offset_encoding(&self) -> OffsetEncoding {
        match self
            .capabilities()
            .position_encoding
            .as_ref()
            .map(|e| e.as_str())
        {
            Some("utf-8") => OffsetEncoding::Utf8,
            Some("utf-32") => OffsetEncoding::Utf32,
            _ => OffsetEncoding::Utf16,
        }
    }

    /// Whether the connected server advertises support for
    /// `feature`. Default impl decodes against the cached
    /// [`Self::capabilities`]; impls override only when they
    /// have a cheaper path.
    fn supports_feature(&self, feature: LanguageServerFeature) -> bool {
        let caps = self.capabilities();
        match feature {
            LanguageServerFeature::Format => matches!(
                caps.document_formatting_provider,
                Some(OneOf::Left(true) | OneOf::Right(_))
            ),
            LanguageServerFeature::GotoDeclaration => matches!(
                caps.declaration_provider,
                Some(
                    DeclarationCapability::Simple(true)
                        | DeclarationCapability::RegistrationOptions(_)
                        | DeclarationCapability::Options(_),
                )
            ),
            LanguageServerFeature::GotoDefinition => matches!(
                caps.definition_provider,
                Some(OneOf::Left(true) | OneOf::Right(_))
            ),
            LanguageServerFeature::GotoTypeDefinition => matches!(
                caps.type_definition_provider,
                Some(
                    TypeDefinitionProviderCapability::Simple(true)
                        | TypeDefinitionProviderCapability::Options(_),
                )
            ),
            LanguageServerFeature::GotoReference => matches!(
                caps.references_provider,
                Some(OneOf::Left(true) | OneOf::Right(_))
            ),
            LanguageServerFeature::GotoImplementation => matches!(
                caps.implementation_provider,
                Some(
                    ImplementationProviderCapability::Simple(true)
                        | ImplementationProviderCapability::Options(_),
                )
            ),
            LanguageServerFeature::SignatureHelp => caps.signature_help_provider.is_some(),
            LanguageServerFeature::Hover => matches!(
                caps.hover_provider,
                Some(HoverProviderCapability::Simple(true) | HoverProviderCapability::Options(_))
            ),
            LanguageServerFeature::DocumentHighlight => matches!(
                caps.document_highlight_provider,
                Some(OneOf::Left(true) | OneOf::Right(_))
            ),
            LanguageServerFeature::Completion => caps.completion_provider.is_some(),
            LanguageServerFeature::CodeAction => matches!(
                caps.code_action_provider,
                Some(
                    CodeActionProviderCapability::Simple(true)
                        | CodeActionProviderCapability::Options(_),
                )
            ),
            LanguageServerFeature::WorkspaceCommand => caps.execute_command_provider.is_some(),
            LanguageServerFeature::DocumentSymbols => matches!(
                caps.document_symbol_provider,
                Some(OneOf::Left(true) | OneOf::Right(_))
            ),
            LanguageServerFeature::WorkspaceSymbols => matches!(
                caps.workspace_symbol_provider,
                Some(OneOf::Left(true) | OneOf::Right(_))
            ),
            LanguageServerFeature::Diagnostics => true,
            LanguageServerFeature::PullDiagnostics => caps.diagnostic_provider.is_some(),
            LanguageServerFeature::RenameSymbol => matches!(
                caps.rename_provider,
                Some(OneOf::Left(true) | OneOf::Right(_))
            ),
            LanguageServerFeature::InlayHints => matches!(
                caps.inlay_hint_provider,
                Some(OneOf::Left(true) | OneOf::Right(InlayHintServerCapabilities::Options(_)))
            ),
            LanguageServerFeature::DocumentColors => matches!(
                caps.color_provider,
                Some(
                    ColorProviderCapability::Simple(true)
                        | ColorProviderCapability::ColorProvider(_)
                        | ColorProviderCapability::Options(_),
                )
            ),
        }
    }

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
    async fn folding_range(
        &self,
        params: FoldingRangeParams,
    ) -> io::Result<Option<Vec<FoldingRange>>>;
    async fn selection_range(
        &self,
        params: SelectionRangeParams,
    ) -> io::Result<Option<Vec<SelectionRange>>>;
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
    async fn prepare_rename(
        &self,
        params: TextDocumentPositionParams,
    ) -> io::Result<Option<PrepareRenameResponse>>;
    async fn rename(&self, params: RenameParams) -> io::Result<Option<WorkspaceEdit>>;
    async fn formatting(
        &self,
        params: DocumentFormattingParams,
    ) -> io::Result<Option<Vec<TextEdit>>>;
    async fn range_formatting(
        &self,
        params: DocumentRangeFormattingParams,
    ) -> io::Result<Option<Vec<TextEdit>>>;

    // Server-pushed notifications

    /// Wait for the next server-pushed notification. Returns `None`
    /// when the underlying channel is closed (server gone, no further
    /// notifications possible). Use [`Self::try_recv_notification`]
    /// for a non-blocking peek.
    async fn recv_notification(&self) -> Option<LspNotification>;

    /// Non-blocking variant of [`Self::recv_notification`]. Returns
    /// `None` immediately when no notification is queued or the
    /// channel is closed.
    async fn try_recv_notification(&self) -> Option<LspNotification>;
}

/// Default [`LspHost`] used when no language server is configured.
/// Every method returns the empty / no-op success response so action
/// handlers can call into the host unconditionally without branching
/// on "is a real server installed". Replaced by `LocalLsp` once the
/// production stdio transport lands; the test harness installs
/// [`crate::host::FakeLsp`] in its place.
pub struct NoopLsp;

static NOOP_CAPABILITIES: LazyLock<Arc<ServerCapabilities>> =
    LazyLock::new(|| Arc::new(ServerCapabilities::default()));

#[async_trait]
impl LspHost for NoopLsp {
    fn capabilities(&self) -> Arc<ServerCapabilities> {
        NOOP_CAPABILITIES.clone()
    }

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

    async fn recv_notification(&self) -> Option<LspNotification> {
        None
    }

    async fn try_recv_notification(&self) -> Option<LspNotification> {
        None
    }
}
