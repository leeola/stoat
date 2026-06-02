use async_trait::async_trait;
use lsp_types::{
    ApplyWorkspaceEditParams, CallHierarchyIncomingCall, CallHierarchyIncomingCallsParams,
    CallHierarchyItem, CallHierarchyOutgoingCall, CallHierarchyOutgoingCallsParams,
    CallHierarchyPrepareParams, CodeAction, CodeActionOrCommand, CodeActionParams,
    CodeActionProviderCapability, CodeLens, CodeLensParams, ColorInformation, ColorPresentation,
    ColorPresentationParams, ColorProviderCapability, CompletionItem, CompletionParams,
    CompletionResponse, ConfigurationParams, DeclarationCapability, Diagnostic,
    DidChangeConfigurationParams, DidChangeTextDocumentParams, DidChangeWatchedFilesParams,
    DidChangeWorkspaceFoldersParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
    DidSaveTextDocumentParams, DocumentColorParams, DocumentDiagnosticParams,
    DocumentDiagnosticReportResult, DocumentFormattingParams, DocumentHighlight,
    DocumentHighlightParams, DocumentLink, DocumentLinkParams, DocumentRangeFormattingParams,
    DocumentSymbolParams, DocumentSymbolResponse, ExecuteCommandParams, FoldingRange,
    FoldingRangeParams, GotoDefinitionParams, GotoDefinitionResponse, Hover, HoverParams,
    HoverProviderCapability, ImplementationProviderCapability, InitializeResult, InlayHint,
    InlayHintParams, InlayHintServerCapabilities, Location, MessageType, NumberOrString, OneOf,
    PrepareRenameResponse, ProgressToken, ReferenceParams, RegistrationParams, RenameFilesParams,
    RenameParams, SelectionRange, SelectionRangeParams, SemanticTokensParams,
    SemanticTokensRangeParams, SemanticTokensRangeResult, SemanticTokensResult, ServerCapabilities,
    ShowMessageRequestParams, SignatureHelp, SignatureHelpParams, TextDocumentPositionParams,
    TextEdit, TypeDefinitionProviderCapability, TypeHierarchyItem, TypeHierarchyPrepareParams,
    TypeHierarchySubtypesParams, TypeHierarchySupertypesParams, UnregistrationParams, Uri,
    WorkDoneProgress, WorkDoneProgressCreateParams, WorkspaceEdit, WorkspaceSymbolParams,
    WorkspaceSymbolResponse,
};
use serde_json::Value;
use std::{
    io,
    path::Path,
    sync::{Arc, LazyLock},
};
use stoat_language::Language;

static NOOP_CAPABILITIES: LazyLock<Arc<ServerCapabilities>> =
    LazyLock::new(|| Arc::new(ServerCapabilities::default()));

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

/// Server-to-client request that the editor must answer via
/// [`LspServer::reply`]. Distinct from [`LspNotification`] (which is
/// fire-and-forget) because the JSON-RPC envelope carries an `id`
/// the server uses to correlate the eventual response. Each variant
/// pairs the `id` with the typed params struct from `lsp_types`;
/// production transports decode the wire JSON eagerly so the editor
/// can pattern-match on method instead of branching on a string.
/// [`Self::Unknown`] is the fallback for methods this host has not
/// been taught about.
#[derive(Debug, Clone)]
pub enum IncomingRequest {
    /// `window/showMessageRequest` -- server asks the user a
    /// picker question; reply selects one of
    /// `params.actions` or replies `null` for dismissal.
    ShowMessageRequest {
        id: NumberOrString,
        params: ShowMessageRequestParams,
    },
    /// `window/workDoneProgress/create` -- server reserves a
    /// progress token before emitting [`LspNotification::Progress`].
    /// Reply `null` to acknowledge or with an error to refuse.
    WorkDoneProgressCreate {
        id: NumberOrString,
        params: WorkDoneProgressCreateParams,
    },
    /// `client/registerCapability` -- server dynamically registers
    /// capabilities (file watchers, command sets, etc.). Reply
    /// `null` to acknowledge.
    RegisterCapability {
        id: NumberOrString,
        params: RegistrationParams,
    },
    /// `client/unregisterCapability` -- inverse of
    /// [`Self::RegisterCapability`]. Reply `null` to acknowledge.
    UnregisterCapability {
        id: NumberOrString,
        params: UnregistrationParams,
    },
    /// `workspace/configuration` -- server pulls editor settings.
    /// Reply with `Vec<Value>` matching `params.items` order;
    /// missing settings are sent back as `Value::Null`.
    WorkspaceConfiguration {
        id: NumberOrString,
        params: ConfigurationParams,
    },
    /// `workspace/applyEdit` -- server requests a workspace edit
    /// (rename refactor, code action). Reply with
    /// `ApplyWorkspaceEditResponse` carrying `applied` plus an
    /// optional failure reason.
    WorkspaceApplyEdit {
        id: NumberOrString,
        params: ApplyWorkspaceEditParams,
    },
    /// Fallback for methods this host has not yet been taught
    /// about. `params` carries the raw wire JSON; the editor can
    /// surface the request without forcing a host update for every
    /// new LSP method.
    Unknown {
        id: NumberOrString,
        method: String,
        params: Value,
    },
}

impl IncomingRequest {
    /// JSON-RPC request id, regardless of variant. The editor
    /// echoes this back to [`LspServer::reply`] so the server can
    /// correlate the response with the originating request.
    pub fn id(&self) -> &NumberOrString {
        match self {
            Self::ShowMessageRequest { id, .. }
            | Self::WorkDoneProgressCreate { id, .. }
            | Self::RegisterCapability { id, .. }
            | Self::UnregisterCapability { id, .. }
            | Self::WorkspaceConfiguration { id, .. }
            | Self::WorkspaceApplyEdit { id, .. }
            | Self::Unknown { id, .. } => id,
        }
    }
}

/// JSON-RPC error envelope returned in place of a successful result
/// from [`LspServer::reply`]. Mirrors the spec shape (`code`, `message`,
/// optional `data`); `lsp_types` deliberately omits the JSON-RPC
/// envelope so we model it locally. Code values follow the JSON-RPC
/// 2.0 + LSP error-code conventions (e.g. `-32601` for
/// `MethodNotFound`, `-32603` for `InternalError`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LspResponseError {
    pub code: i64,
    pub message: String,
    pub data: Option<Value>,
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
/// stoat dispatches; `LspServer::supports_feature` decodes each
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
    SemanticTokens,
}

#[async_trait]
pub trait LspServer: Send + Sync {
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
            LanguageServerFeature::SemanticTokens => caps.semantic_tokens_provider.is_some(),
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
    async fn did_rename(&self, params: RenameFilesParams) -> io::Result<()>;
    async fn did_change_watched_files(&self, params: DidChangeWatchedFilesParams)
        -> io::Result<()>;
    async fn did_change_configuration(
        &self,
        params: DidChangeConfigurationParams,
    ) -> io::Result<()>;
    async fn did_change_workspace_folders(
        &self,
        params: DidChangeWorkspaceFoldersParams,
    ) -> io::Result<()>;

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
    async fn code_lens(&self, params: CodeLensParams) -> io::Result<Option<Vec<CodeLens>>>;
    async fn code_lens_resolve(&self, lens: CodeLens) -> io::Result<CodeLens>;
    async fn document_link(
        &self,
        params: DocumentLinkParams,
    ) -> io::Result<Option<Vec<DocumentLink>>>;
    async fn document_link_resolve(&self, link: DocumentLink) -> io::Result<DocumentLink>;
    async fn document_color(
        &self,
        params: DocumentColorParams,
    ) -> io::Result<Option<Vec<ColorInformation>>>;
    async fn color_presentation(
        &self,
        params: ColorPresentationParams,
    ) -> io::Result<Option<Vec<ColorPresentation>>>;
    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> io::Result<Option<SemanticTokensResult>>;
    async fn semantic_tokens_range(
        &self,
        params: SemanticTokensRangeParams,
    ) -> io::Result<Option<SemanticTokensRangeResult>>;
    async fn prepare_call_hierarchy(
        &self,
        params: CallHierarchyPrepareParams,
    ) -> io::Result<Option<Vec<CallHierarchyItem>>>;
    async fn call_hierarchy_incoming_calls(
        &self,
        params: CallHierarchyIncomingCallsParams,
    ) -> io::Result<Option<Vec<CallHierarchyIncomingCall>>>;
    async fn call_hierarchy_outgoing_calls(
        &self,
        params: CallHierarchyOutgoingCallsParams,
    ) -> io::Result<Option<Vec<CallHierarchyOutgoingCall>>>;
    async fn prepare_type_hierarchy(
        &self,
        params: TypeHierarchyPrepareParams,
    ) -> io::Result<Option<Vec<TypeHierarchyItem>>>;
    async fn type_hierarchy_supertypes(
        &self,
        params: TypeHierarchySupertypesParams,
    ) -> io::Result<Option<Vec<TypeHierarchyItem>>>;
    async fn type_hierarchy_subtypes(
        &self,
        params: TypeHierarchySubtypesParams,
    ) -> io::Result<Option<Vec<TypeHierarchyItem>>>;
    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> io::Result<Option<DocumentSymbolResponse>>;
    async fn document_diagnostic(
        &self,
        params: DocumentDiagnosticParams,
    ) -> io::Result<Option<DocumentDiagnosticReportResult>>;
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
    /// `textDocument/inlayHint` request for an arbitrary range
    /// (typically the full document). Use [`Self::range_inlay_hint`]
    /// when the range is the editor's current viewport, so the server
    /// load is bounded to visible rows; the LSP wire method is the
    /// same in both cases.
    async fn inlay_hint(&self, params: InlayHintParams) -> io::Result<Option<Vec<InlayHint>>>;
    async fn inlay_hint_resolve(&self, hint: InlayHint) -> io::Result<InlayHint>;

    /// Viewport-bounded variant of [`Self::inlay_hint`]. The LSP
    /// wire method (`textDocument/inlayHint`) is identical, but the
    /// range carried in `params` is the editor's currently visible
    /// region. Splitting the trait surface lets callers signal load-
    /// bounding intent and lets test fakes program separate
    /// responses for full-document and viewport requests.
    async fn range_inlay_hint(&self, params: InlayHintParams)
        -> io::Result<Option<Vec<InlayHint>>>;

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
    async fn will_rename(&self, params: RenameFilesParams) -> io::Result<Option<WorkspaceEdit>>;
    async fn execute_command(&self, params: ExecuteCommandParams) -> io::Result<Option<Value>>;

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

    // Server-initiated requests

    /// Wait for the next server-initiated [`IncomingRequest`]. Each
    /// returned request *must* be answered with [`Self::reply`]
    /// quoting the same id; the server blocks until a response
    /// arrives. Distinct channel from [`Self::recv_notification`]
    /// because notifications and requests are different LSP message
    /// types. `None` means the channel is closed (server gone, no
    /// further requests possible).
    async fn recv_incoming_request(&self) -> Option<IncomingRequest>;

    /// Non-blocking variant of [`Self::recv_incoming_request`].
    /// Returns `None` immediately when no request is queued or the
    /// channel is closed.
    async fn try_recv_incoming_request(&self) -> Option<IncomingRequest>;

    /// Send the response for an [`IncomingRequest`]. `id` echoes the
    /// id from the request so the server can correlate; `result` is
    /// `Ok(value)` for success or `Err(error)` for an error envelope.
    /// The caller is responsible for serializing typed responses
    /// (`serde_json::to_value`); the trait stays method-agnostic so
    /// new request methods do not require a new trait method per
    /// type.
    async fn reply(
        &self,
        id: NumberOrString,
        result: Result<Value, LspResponseError>,
    ) -> io::Result<()>;
}

/// Factory that launches new [`LspServer`] sessions for a given
/// language at a given workspace root. Production wires
/// [`crate::host::local::LocalLspHost`]; tests wire a factory that
/// hands out a pre-configured [`crate::host::FakeLsp`].
///
/// Mirrors the factory shape of [`crate::host::ClaudeCodeHost`] and
/// [`crate::host::TerminalHost`] so all hosts register uniformly.
#[async_trait]
pub trait LspHost: Send + Sync {
    /// Launch a server session for `language` rooted at `root`. The
    /// returned [`LspServer`] has not yet been initialized; the caller
    /// is responsible for the `initialize` handshake.
    async fn launch(&self, language: &Language, root: &Path) -> io::Result<Box<dyn LspServer>>;
}

/// Default [`LspServer`] returned by [`NoopLspHost::launch`] when no
/// language server is configured for a given language. Every method
/// returns the empty / no-op success response so action handlers can
/// call into the server unconditionally without branching on "is a
/// real server installed". The test harness installs
/// [`crate::host::FakeLsp`] in its place.
pub struct NoopLspServer;

#[async_trait]
impl LspServer for NoopLspServer {
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

/// [`LspHost`] that always launches a [`NoopLspServer`]. Used as the
/// default factory when no production stdio transport is wired, so
/// editor flows that consult an [`LspServer`] never observe an
/// "absent host" branch.
pub struct NoopLspHost;

#[async_trait]
impl LspHost for NoopLspHost {
    async fn launch(&self, _language: &Language, _root: &Path) -> io::Result<Box<dyn LspServer>> {
        Ok(Box::new(NoopLspServer))
    }
}
