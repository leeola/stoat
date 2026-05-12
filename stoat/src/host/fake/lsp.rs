use crate::host::lsp::{
    IncomingRequest, LspNotification, LspResponseError, LspServer, OffsetEncoding,
};
use async_trait::async_trait;
use lsp_types::{
    ApplyWorkspaceEditParams, CallHierarchyIncomingCall, CallHierarchyIncomingCallsParams,
    CallHierarchyItem, CallHierarchyOutgoingCall, CallHierarchyOutgoingCallsParams,
    CallHierarchyPrepareParams, CodeAction, CodeActionContext, CodeActionOrCommand,
    CodeActionParams, Color, ColorInformation, ColorPresentation, ColorPresentationParams,
    CompletionItem, CompletionList, CompletionParams, CompletionResponse, ConfigurationItem,
    ConfigurationParams, Diagnostic, DiagnosticSeverity, DidChangeConfigurationParams,
    DidChangeTextDocumentParams, DidChangeWatchedFilesParams, DidChangeWorkspaceFoldersParams,
    DidCloseTextDocumentParams, DidOpenTextDocumentParams, DidSaveTextDocumentParams,
    DocumentColorParams, DocumentDiagnosticParams, DocumentDiagnosticReportResult,
    DocumentFormattingParams, DocumentHighlight, DocumentHighlightKind, DocumentHighlightParams,
    DocumentLink, DocumentLinkParams, DocumentRangeFormattingParams, DocumentSymbolParams,
    DocumentSymbolResponse, ExecuteCommandParams, FileChangeType, FileEvent, FileRename,
    FoldingRange, FoldingRangeParams, FormattingOptions, GotoDefinitionParams,
    GotoDefinitionResponse, Hover, HoverContents, HoverParams, InitializeResult, InlayHint,
    InlayHintKind, InlayHintLabel, InlayHintParams, Location, MarkupContent, MarkupKind,
    MessageType, NumberOrString, PartialResultParams, Position, PositionEncodingKind,
    PrepareRenameResponse, Range, ReferenceContext, ReferenceParams, RenameFilesParams,
    RenameParams, SelectionRange, SelectionRangeParams, SemanticTokensParams,
    SemanticTokensRangeParams, SemanticTokensRangeResult, SemanticTokensResult, ServerCapabilities,
    SignatureHelp, SignatureHelpParams, SymbolInformation, SymbolKind,
    TextDocumentContentChangeEvent, TextDocumentIdentifier, TextDocumentItem,
    TextDocumentPositionParams, TextDocumentSyncCapability, TextDocumentSyncKind, TextEdit,
    TypeHierarchyItem, TypeHierarchyPrepareParams, TypeHierarchySubtypesParams,
    TypeHierarchySupertypesParams, Uri, VersionedTextDocumentIdentifier, WorkDoneProgress,
    WorkDoneProgressBegin, WorkDoneProgressParams, WorkspaceEdit, WorkspaceFolder,
    WorkspaceFoldersChangeEvent, WorkspaceSymbolParams, WorkspaceSymbolResponse,
};
use serde_json::Value;
use std::{
    any::Any,
    collections::{BTreeMap, VecDeque},
    io,
    str::FromStr,
    sync::{Arc, Mutex},
    time::Duration,
};
use stoat_scheduler::Executor;
use tokio::sync::{
    mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender},
    oneshot, Mutex as TokioMutex,
};

fn file_uri(path: &str) -> Uri {
    Uri::from_str(&format!("file://{path}")).expect("valid file URI")
}

fn text_doc_id(path: &str) -> TextDocumentIdentifier {
    TextDocumentIdentifier::new(file_uri(path))
}

fn text_doc_pos(path: &str, line: u32, col: u32) -> TextDocumentPositionParams {
    TextDocumentPositionParams {
        text_document: text_doc_id(path),
        position: Position::new(line, col),
    }
}

fn wdp() -> WorkDoneProgressParams {
    WorkDoneProgressParams {
        work_done_token: None,
    }
}

fn prp() -> PartialResultParams {
    PartialResultParams {
        partial_result_token: None,
    }
}

// --- Param builders ---

pub fn hover_params(path: &str, line: u32, col: u32) -> HoverParams {
    HoverParams {
        text_document_position_params: text_doc_pos(path, line, col),
        work_done_progress_params: wdp(),
    }
}

pub fn signature_help_params(path: &str, line: u32, col: u32) -> SignatureHelpParams {
    SignatureHelpParams {
        text_document_position_params: text_doc_pos(path, line, col),
        work_done_progress_params: wdp(),
        context: None,
    }
}

pub fn completion_params(path: &str, line: u32, col: u32) -> CompletionParams {
    CompletionParams {
        text_document_position: text_doc_pos(path, line, col),
        work_done_progress_params: wdp(),
        partial_result_params: prp(),
        context: None,
    }
}

pub fn definition_params(path: &str, line: u32, col: u32) -> GotoDefinitionParams {
    GotoDefinitionParams {
        text_document_position_params: text_doc_pos(path, line, col),
        work_done_progress_params: wdp(),
        partial_result_params: prp(),
    }
}

pub fn reference_params(path: &str, line: u32, col: u32) -> ReferenceParams {
    ReferenceParams {
        text_document_position: text_doc_pos(path, line, col),
        work_done_progress_params: wdp(),
        partial_result_params: prp(),
        context: ReferenceContext {
            include_declaration: true,
        },
    }
}

pub fn document_highlight_params(path: &str, line: u32, col: u32) -> DocumentHighlightParams {
    DocumentHighlightParams {
        text_document_position_params: text_doc_pos(path, line, col),
        work_done_progress_params: wdp(),
        partial_result_params: prp(),
    }
}

pub fn inlay_hint_params(path: &str, start_line: u32, end_line: u32) -> InlayHintParams {
    InlayHintParams {
        work_done_progress_params: wdp(),
        text_document: text_doc_id(path),
        range: Range::new(Position::new(start_line, 0), Position::new(end_line, 0)),
    }
}

pub fn range_inlay_hint_params(
    path: &str,
    start_line: u32,
    start_col: u32,
    end_line: u32,
    end_col: u32,
) -> InlayHintParams {
    InlayHintParams {
        work_done_progress_params: wdp(),
        text_document: text_doc_id(path),
        range: Range::new(
            Position::new(start_line, start_col),
            Position::new(end_line, end_col),
        ),
    }
}

pub fn folding_range_params(path: &str) -> FoldingRangeParams {
    FoldingRangeParams {
        text_document: text_doc_id(path),
        work_done_progress_params: wdp(),
        partial_result_params: prp(),
    }
}

pub fn selection_range_params(path: &str, positions: &[(u32, u32)]) -> SelectionRangeParams {
    SelectionRangeParams {
        text_document: text_doc_id(path),
        positions: positions
            .iter()
            .map(|(line, col)| Position::new(*line, *col))
            .collect(),
        work_done_progress_params: wdp(),
        partial_result_params: prp(),
    }
}

pub fn range_formatting_params(
    path: &str,
    start_line: u32,
    start_col: u32,
    end_line: u32,
    end_col: u32,
) -> DocumentRangeFormattingParams {
    DocumentRangeFormattingParams {
        text_document: text_doc_id(path),
        range: Range::new(
            Position::new(start_line, start_col),
            Position::new(end_line, end_col),
        ),
        options: FormattingOptions::default(),
        work_done_progress_params: wdp(),
    }
}

pub fn code_action_params(
    path: &str,
    start_line: u32,
    start_col: u32,
    end_line: u32,
    end_col: u32,
) -> CodeActionParams {
    CodeActionParams {
        text_document: text_doc_id(path),
        range: Range::new(
            Position::new(start_line, start_col),
            Position::new(end_line, end_col),
        ),
        context: CodeActionContext {
            diagnostics: Vec::new(),
            only: None,
            trigger_kind: None,
        },
        work_done_progress_params: wdp(),
        partial_result_params: prp(),
    }
}

pub fn document_diagnostic_params(path: &str) -> DocumentDiagnosticParams {
    DocumentDiagnosticParams {
        text_document: text_doc_id(path),
        identifier: None,
        previous_result_id: None,
        work_done_progress_params: wdp(),
        partial_result_params: prp(),
    }
}

pub fn document_link_params(path: &str) -> DocumentLinkParams {
    DocumentLinkParams {
        text_document: text_doc_id(path),
        work_done_progress_params: wdp(),
        partial_result_params: prp(),
    }
}

pub fn document_symbol_params(path: &str) -> DocumentSymbolParams {
    DocumentSymbolParams {
        text_document: text_doc_id(path),
        work_done_progress_params: wdp(),
        partial_result_params: prp(),
    }
}

pub fn document_color_params(path: &str) -> DocumentColorParams {
    DocumentColorParams {
        text_document: text_doc_id(path),
        work_done_progress_params: wdp(),
        partial_result_params: prp(),
    }
}

pub fn color_presentation_params(
    path: &str,
    color: Color,
    start_line: u32,
    start_col: u32,
    end_line: u32,
    end_col: u32,
) -> ColorPresentationParams {
    ColorPresentationParams {
        text_document: text_doc_id(path),
        color,
        range: Range::new(
            Position::new(start_line, start_col),
            Position::new(end_line, end_col),
        ),
        work_done_progress_params: wdp(),
        partial_result_params: prp(),
    }
}

pub fn semantic_tokens_params(path: &str) -> SemanticTokensParams {
    SemanticTokensParams {
        work_done_progress_params: wdp(),
        partial_result_params: prp(),
        text_document: text_doc_id(path),
    }
}

pub fn semantic_tokens_range_params(
    path: &str,
    start_line: u32,
    start_col: u32,
    end_line: u32,
    end_col: u32,
) -> SemanticTokensRangeParams {
    SemanticTokensRangeParams {
        work_done_progress_params: wdp(),
        partial_result_params: prp(),
        text_document: text_doc_id(path),
        range: Range::new(
            Position::new(start_line, start_col),
            Position::new(end_line, end_col),
        ),
    }
}

pub fn call_hierarchy_prepare_params(
    path: &str,
    line: u32,
    col: u32,
) -> CallHierarchyPrepareParams {
    CallHierarchyPrepareParams {
        text_document_position_params: text_doc_pos(path, line, col),
        work_done_progress_params: wdp(),
    }
}

/// Builds a minimal [`CallHierarchyItem`] anchored at
/// `(path, line, col)`. The item's `range` and `selection_range`
/// both span a single position so the fake's lookup
/// (`item.range.start`) matches `(line, col)`.
pub fn call_hierarchy_item(
    path: &str,
    name: &str,
    kind: SymbolKind,
    line: u32,
    col: u32,
) -> CallHierarchyItem {
    let pos = Position::new(line, col);
    let range = Range::new(pos, pos);
    CallHierarchyItem {
        name: name.to_string(),
        kind,
        tags: None,
        detail: None,
        uri: file_uri(path),
        range,
        selection_range: range,
        data: None,
    }
}

pub fn type_hierarchy_prepare_params(
    path: &str,
    line: u32,
    col: u32,
) -> TypeHierarchyPrepareParams {
    TypeHierarchyPrepareParams {
        text_document_position_params: text_doc_pos(path, line, col),
        work_done_progress_params: wdp(),
    }
}

/// Builds a minimal [`TypeHierarchyItem`] anchored at
/// `(path, line, col)`. The item's `range` and `selection_range`
/// both span a single position so the fake's supertypes/subtypes
/// lookup (`item.range.start`) matches `(line, col)`.
pub fn type_hierarchy_item(
    path: &str,
    name: &str,
    kind: SymbolKind,
    line: u32,
    col: u32,
) -> TypeHierarchyItem {
    let pos = Position::new(line, col);
    let range = Range::new(pos, pos);
    TypeHierarchyItem {
        name: name.to_string(),
        kind,
        tags: None,
        detail: None,
        uri: file_uri(path),
        range,
        selection_range: range,
        data: None,
    }
}

pub fn workspace_symbol_params(query: &str) -> WorkspaceSymbolParams {
    WorkspaceSymbolParams {
        partial_result_params: prp(),
        work_done_progress_params: wdp(),
        query: query.to_string(),
    }
}

pub fn open_params(path: &str, text: &str, language: &str) -> DidOpenTextDocumentParams {
    DidOpenTextDocumentParams {
        text_document: TextDocumentItem::new(
            file_uri(path),
            language.to_string(),
            0,
            text.to_string(),
        ),
    }
}

pub fn change_params(path: &str, version: i32, new_text: &str) -> DidChangeTextDocumentParams {
    DidChangeTextDocumentParams {
        text_document: VersionedTextDocumentIdentifier::new(file_uri(path), version),
        content_changes: vec![TextDocumentContentChangeEvent {
            range: None,
            range_length: None,
            text: new_text.to_string(),
        }],
    }
}

/// Builds [`RenameFilesParams`] from a slice of `(old_path,
/// new_path)` pairs. Each path is converted to a `file://` URI
/// via [`file_uri`] so test inputs match the URIs the fake's
/// `will_rename` lookup uses.
pub fn rename_files_params(renames: &[(&str, &str)]) -> RenameFilesParams {
    RenameFilesParams {
        files: renames
            .iter()
            .map(|(old, new)| FileRename {
                old_uri: file_uri(old).as_str().to_string(),
                new_uri: file_uri(new).as_str().to_string(),
            })
            .collect(),
    }
}

pub fn execute_command_params(command: &str, arguments: Vec<Value>) -> ExecuteCommandParams {
    ExecuteCommandParams {
        command: command.to_string(),
        arguments,
        work_done_progress_params: wdp(),
    }
}

/// Build an [`IncomingRequest::Unknown`] for a method this host has
/// not yet been taught about. Tests for typed variants
/// (`IncomingRequest::WorkspaceApplyEdit`, etc.) construct the
/// variant directly so the params type stays checked at compile
/// time; this helper exists for the fallback path.
pub fn incoming_request(method: &str, id: i32, params: Value) -> IncomingRequest {
    IncomingRequest::Unknown {
        id: NumberOrString::Number(id),
        method: method.to_string(),
        params,
    }
}

// --- FakeLsp ---

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct LspKey {
    uri: String,
    line: u32,
    character: u32,
}

impl LspKey {
    fn new(path: &str, line: u32, col: u32) -> Self {
        Self {
            uri: file_uri(path).as_str().to_string(),
            line,
            character: col,
        }
    }

    fn from_position(uri: &Uri, pos: &Position) -> Self {
        Self {
            uri: uri.as_str().to_string(),
            line: pos.line,
            character: pos.character,
        }
    }
}

/// Short-circuits an `LspServer` request method into the pending
/// queue when [`FakeLsp::set_pending_mode`] has flagged its
/// `R::METHOD` as enabled. Expansion enqueues the params plus a
/// `oneshot::Sender<R::Result>`, then awaits the receiver and
/// returns its value as the method's response. Place the
/// invocation after `apply_delay` and any failure-injection
/// short-circuit so those existing branches still win when
/// pending mode is off.
macro_rules! pending_check {
    ($self:ident, $req:ty, $params:ident) => {
        if $self.is_pending_mode_for(<$req as lsp_types::request::Request>::METHOD) {
            let rx = $self.enqueue_pending::<$req>($params);
            return rx
                .await
                .map_err(|_| io::Error::new(io::ErrorKind::Other, "pending sender dropped"));
        }
    };
}

/// Variant of [`pending_check`] for trait methods whose return
/// type wraps the spec-defined `R::Result` in `Option<...>` even
/// though the LSP spec produces it directly. Wraps the awaited
/// response in `Some` so the method's `io::Result<Option<...>>`
/// signature matches.
macro_rules! pending_check_some {
    ($self:ident, $req:ty, $params:ident) => {
        if $self.is_pending_mode_for(<$req as lsp_types::request::Request>::METHOD) {
            let rx = $self.enqueue_pending::<$req>($params);
            return rx
                .await
                .map(Some)
                .map_err(|_| io::Error::new(io::ErrorKind::Other, "pending sender dropped"));
        }
    };
}

pub struct FakeLsp {
    state: Mutex<FakeLspState>,
    notif_tx: UnboundedSender<LspNotification>,
    notif_rx: TokioMutex<UnboundedReceiver<LspNotification>>,
    req_tx: UnboundedSender<IncomingRequest>,
    req_rx: TokioMutex<UnboundedReceiver<IncomingRequest>>,
    executor: Mutex<Option<Executor>>,
}

/// RAII helper that records a request as cancelled when its
/// future is dropped before completion. Constructed inside
/// [`FakeLsp::apply_delay`] just before the timer await; if the
/// future drops while the timer is parked, the guard's `armed`
/// flag is still set and [`Drop`] pushes the method into
/// `cancelled_requests`. Calling [`Self::disarm`] after the timer
/// elapses normally clears the flag so completed requests are
/// not recorded as cancellations.
struct CancellationGuard<'a> {
    state: &'a Mutex<FakeLspState>,
    method: String,
    armed: bool,
}

impl<'a> CancellationGuard<'a> {
    fn armed(state: &'a Mutex<FakeLspState>, method: String) -> Self {
        Self {
            state,
            method,
            armed: true,
        }
    }

    fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for CancellationGuard<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.state
            .lock()
            .unwrap()
            .cancelled_requests
            .push(std::mem::take(&mut self.method));
    }
}

struct FakeLspState {
    capabilities: Arc<ServerCapabilities>,
    diagnostics: BTreeMap<Uri, Vec<Diagnostic>>,
    hovers: BTreeMap<LspKey, Hover>,
    completions: BTreeMap<LspKey, Vec<CompletionItem>>,
    definitions: BTreeMap<LspKey, GotoDefinitionResponse>,
    declarations: BTreeMap<LspKey, GotoDefinitionResponse>,
    type_definitions: BTreeMap<LspKey, GotoDefinitionResponse>,
    implementations: BTreeMap<LspKey, GotoDefinitionResponse>,
    references: BTreeMap<LspKey, Vec<Location>>,
    highlights: BTreeMap<LspKey, Vec<DocumentHighlight>>,
    inlay_hints: BTreeMap<Uri, Vec<InlayHint>>,
    range_inlay_hints: BTreeMap<Uri, Vec<InlayHint>>,
    workspace_symbols: BTreeMap<String, Vec<SymbolInformation>>,
    workspace_symbol_responses: BTreeMap<String, WorkspaceSymbolResponse>,
    folding_ranges: BTreeMap<Uri, Vec<FoldingRange>>,
    selection_ranges: BTreeMap<LspKey, SelectionRange>,
    range_formatting: BTreeMap<Uri, Vec<TextEdit>>,
    document_diagnostics: BTreeMap<Uri, DocumentDiagnosticReportResult>,
    document_links: BTreeMap<Uri, Vec<DocumentLink>>,
    document_colors: BTreeMap<Uri, Vec<ColorInformation>>,
    color_presentations: BTreeMap<Uri, Vec<ColorPresentation>>,
    semantic_tokens_full: BTreeMap<Uri, SemanticTokensResult>,
    semantic_tokens_range: BTreeMap<Uri, SemanticTokensRangeResult>,
    document_symbols: BTreeMap<Uri, DocumentSymbolResponse>,
    signature_helps: BTreeMap<LspKey, SignatureHelp>,
    code_actions: BTreeMap<Uri, Vec<CodeActionOrCommand>>,
    call_hierarchy_prepare: BTreeMap<LspKey, Vec<CallHierarchyItem>>,
    call_hierarchy_incoming: BTreeMap<LspKey, Vec<CallHierarchyIncomingCall>>,
    call_hierarchy_outgoing: BTreeMap<LspKey, Vec<CallHierarchyOutgoingCall>>,
    type_hierarchy_prepare: BTreeMap<LspKey, Vec<TypeHierarchyItem>>,
    type_hierarchy_supertypes: BTreeMap<LspKey, Vec<TypeHierarchyItem>>,
    type_hierarchy_subtypes: BTreeMap<LspKey, Vec<TypeHierarchyItem>>,
    will_renames: BTreeMap<(String, String), WorkspaceEdit>,
    observed_renames: Vec<RenameFilesParams>,
    executed_commands: BTreeMap<String, Value>,
    observed_executed_commands: Vec<ExecuteCommandParams>,
    observed_watched_file_changes: Vec<DidChangeWatchedFilesParams>,
    observed_configuration_changes: Vec<DidChangeConfigurationParams>,
    observed_workspace_folder_changes: Vec<DidChangeWorkspaceFoldersParams>,
    observed_replies: Vec<(NumberOrString, Result<Value, LspResponseError>)>,
    observed_opens: Vec<DidOpenTextDocumentParams>,
    observed_changes: Vec<DidChangeTextDocumentParams>,
    prepare_renames: BTreeMap<LspKey, PrepareRenameResponse>,
    renames: BTreeMap<LspKey, WorkspaceEdit>,
    open_documents: BTreeMap<Uri, String>,
    request_failures_oneshot: BTreeMap<String, io::ErrorKind>,
    request_failures_persistent: BTreeMap<String, io::ErrorKind>,
    request_delays: BTreeMap<String, Duration>,
    cancelled_requests: Vec<String>,
    pending_modes: BTreeMap<&'static str, bool>,
    pending_queue: BTreeMap<&'static str, VecDeque<PendingEntry>>,
    initialized: bool,
    shut_down: bool,
}

/// Type-erased `(Params, oneshot::Sender<Result>)` tuple stored
/// in [`FakeLspState::pending_queue`]. Downcast back to the
/// concrete request type at [`FakeLsp::take_pending`] time using
/// `R::Params` and `R::Result` from the
/// [`lsp_types::request::Request`] impl.
struct PendingEntry {
    inner: Box<dyn Any + Send>,
}

impl Default for FakeLsp {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeLsp {
    pub fn new() -> Self {
        let (notif_tx, notif_rx) = unbounded_channel();
        let (req_tx, req_rx) = unbounded_channel();
        Self {
            state: Mutex::new(FakeLspState {
                capabilities: Arc::new(ServerCapabilities::default()),
                diagnostics: BTreeMap::new(),
                hovers: BTreeMap::new(),
                completions: BTreeMap::new(),
                definitions: BTreeMap::new(),
                declarations: BTreeMap::new(),
                type_definitions: BTreeMap::new(),
                implementations: BTreeMap::new(),
                references: BTreeMap::new(),
                highlights: BTreeMap::new(),
                inlay_hints: BTreeMap::new(),
                range_inlay_hints: BTreeMap::new(),
                workspace_symbols: BTreeMap::new(),
                workspace_symbol_responses: BTreeMap::new(),
                folding_ranges: BTreeMap::new(),
                selection_ranges: BTreeMap::new(),
                range_formatting: BTreeMap::new(),
                document_diagnostics: BTreeMap::new(),
                document_links: BTreeMap::new(),
                document_colors: BTreeMap::new(),
                color_presentations: BTreeMap::new(),
                semantic_tokens_full: BTreeMap::new(),
                semantic_tokens_range: BTreeMap::new(),
                document_symbols: BTreeMap::new(),
                signature_helps: BTreeMap::new(),
                code_actions: BTreeMap::new(),
                call_hierarchy_prepare: BTreeMap::new(),
                call_hierarchy_incoming: BTreeMap::new(),
                call_hierarchy_outgoing: BTreeMap::new(),
                type_hierarchy_prepare: BTreeMap::new(),
                type_hierarchy_supertypes: BTreeMap::new(),
                type_hierarchy_subtypes: BTreeMap::new(),
                will_renames: BTreeMap::new(),
                observed_renames: Vec::new(),
                executed_commands: BTreeMap::new(),
                observed_executed_commands: Vec::new(),
                observed_watched_file_changes: Vec::new(),
                observed_configuration_changes: Vec::new(),
                observed_workspace_folder_changes: Vec::new(),
                observed_replies: Vec::new(),
                observed_opens: Vec::new(),
                observed_changes: Vec::new(),
                prepare_renames: BTreeMap::new(),
                renames: BTreeMap::new(),
                open_documents: BTreeMap::new(),
                request_failures_oneshot: BTreeMap::new(),
                request_failures_persistent: BTreeMap::new(),
                request_delays: BTreeMap::new(),
                cancelled_requests: Vec::new(),
                pending_modes: BTreeMap::new(),
                pending_queue: BTreeMap::new(),
                initialized: false,
                shut_down: false,
            }),
            notif_tx,
            notif_rx: TokioMutex::new(notif_rx),
            req_tx,
            req_rx: TokioMutex::new(req_rx),
            executor: Mutex::new(None),
        }
    }

    /// Replace the server capabilities returned by
    /// [`LspServer::capabilities`] (and consulted by
    /// [`LspServer::supports_feature`]). Tests call this before
    /// driving capability-dependent code paths so the host advertises
    /// the right feature set.
    pub fn set_capabilities(&self, capabilities: ServerCapabilities) {
        self.state.lock().unwrap().capabilities = Arc::new(capabilities);
    }

    /// Convenience setter that swaps just the
    /// `position_encoding` field on the stored capabilities so
    /// [`LspServer::offset_encoding`] reflects `encoding`. Other
    /// capability fields are preserved. Use this when a test only
    /// needs to control the negotiated offset encoding without
    /// rebuilding a full [`ServerCapabilities`].
    pub fn set_offset_encoding(&self, encoding: OffsetEncoding) {
        let kind = match encoding {
            OffsetEncoding::Utf8 => PositionEncodingKind::UTF8,
            OffsetEncoding::Utf16 => PositionEncodingKind::UTF16,
            OffsetEncoding::Utf32 => PositionEncodingKind::UTF32,
        };
        let mut state = self.state.lock().unwrap();
        let mut caps = (*state.capabilities).clone();
        caps.position_encoding = Some(kind);
        state.capabilities = Arc::new(caps);
    }

    /// Convenience setter that swaps just the
    /// `text_document_sync` field on the stored capabilities so
    /// the editor's [`LspServer::did_change`] dispatch can find the
    /// configured `TextDocumentSyncKind`. Other capability fields
    /// are preserved.
    pub fn set_text_document_sync(&self, kind: TextDocumentSyncKind) {
        let mut state = self.state.lock().unwrap();
        let mut caps = (*state.capabilities).clone();
        caps.text_document_sync = Some(TextDocumentSyncCapability::Kind(kind));
        state.capabilities = Arc::new(caps);
    }

    /// Programs the folding ranges returned for a
    /// `textDocument/foldingRange` call against `path`. Replaces any
    /// previously seeded ranges for the same document.
    pub fn set_folding_ranges(&self, path: &str, ranges: Vec<FoldingRange>) {
        self.state
            .lock()
            .unwrap()
            .folding_ranges
            .insert(file_uri(path), ranges);
    }

    /// Programs the [`SelectionRange`] chain returned for the
    /// `textDocument/selectionRange` call whose request position
    /// matches `(path, line, col)`. Each call to
    /// [`LspServer::selection_range`] looks up every position in the
    /// request; if any position is unprogrammed the host returns
    /// `None`. Replaces any previously seeded chain for the same
    /// position.
    pub fn set_selection_range(&self, path: &str, line: u32, col: u32, range: SelectionRange) {
        self.state
            .lock()
            .unwrap()
            .selection_ranges
            .insert(LspKey::new(path, line, col), range);
    }

    /// Programs the [`TextEdit`]s returned for a
    /// `textDocument/rangeFormatting` call against `path`. The
    /// fake ignores the request's range and returns whatever was
    /// programmed for the document; tests arrange edits and the
    /// requested range to match. Replaces any previously seeded
    /// edits for the same document.
    pub fn set_range_formatting(&self, path: &str, edits: Vec<TextEdit>) {
        self.state
            .lock()
            .unwrap()
            .range_formatting
            .insert(file_uri(path), edits);
    }

    /// Programs the [`CodeActionOrCommand`]s returned for a
    /// `textDocument/codeAction` call against `path`. The fake
    /// ignores the request's range and context and returns whatever
    /// was programmed for the document; tests arrange URI and
    /// range to match. Replaces any previously seeded actions for
    /// the same document.
    pub fn set_code_actions(&self, path: &str, actions: Vec<CodeActionOrCommand>) {
        self.state
            .lock()
            .unwrap()
            .code_actions
            .insert(file_uri(path), actions);
    }

    /// Programs the pull-style diagnostic report returned for a
    /// `textDocument/diagnostic` call against `path`. Distinct from
    /// the push-style diagnostics seeded via [`Self::add_error`] /
    /// [`Self::add_warning`], which the fake delivers through
    /// [`LspNotification::Diagnostics`] on `did_open` / `did_change`.
    /// Replaces any previously seeded report for the same document.
    pub fn set_document_diagnostic(&self, path: &str, report: DocumentDiagnosticReportResult) {
        self.state
            .lock()
            .unwrap()
            .document_diagnostics
            .insert(file_uri(path), report);
    }

    /// Programs the [`DocumentLink`]s returned for a
    /// `textDocument/documentLink` call against `path`. The
    /// resolve step (`documentLink/resolve`) is a passthrough on
    /// the fake -- tests should program fully-resolved links up
    /// front. Replaces any previously seeded links for the same
    /// document.
    pub fn set_document_links(&self, path: &str, links: Vec<DocumentLink>) {
        self.state
            .lock()
            .unwrap()
            .document_links
            .insert(file_uri(path), links);
    }

    /// Programs the [`ColorInformation`] entries returned for a
    /// `textDocument/documentColor` call against `path`. Replaces
    /// any previously seeded colors for the same document.
    pub fn set_document_colors(&self, path: &str, colors: Vec<ColorInformation>) {
        self.state
            .lock()
            .unwrap()
            .document_colors
            .insert(file_uri(path), colors);
    }

    /// Programs the [`ColorPresentation`] entries returned for a
    /// `textDocument/colorPresentation` call against `path`. The
    /// fake ignores the request's color and range fields and
    /// returns whatever was programmed for the document; tests
    /// arrange the URI and presentations to match. Replaces any
    /// previously seeded presentations for the same document.
    pub fn set_color_presentations(&self, path: &str, presentations: Vec<ColorPresentation>) {
        self.state
            .lock()
            .unwrap()
            .color_presentations
            .insert(file_uri(path), presentations);
    }

    /// Programs the [`SemanticTokensResult`] returned for a
    /// `textDocument/semanticTokens/full` call against `path`.
    /// Replaces any previously seeded result for the same document.
    pub fn set_semantic_tokens_full(&self, path: &str, result: SemanticTokensResult) {
        self.state
            .lock()
            .unwrap()
            .semantic_tokens_full
            .insert(file_uri(path), result);
    }

    /// Programs the [`SemanticTokensRangeResult`] returned for a
    /// `textDocument/semanticTokens/range` call against `path`.
    /// The fake ignores the request's range and returns whatever
    /// was programmed for the document; tests arrange URI and
    /// range to match. Replaces any previously seeded result for
    /// the same document.
    pub fn set_semantic_tokens_range(&self, path: &str, result: SemanticTokensRangeResult) {
        self.state
            .lock()
            .unwrap()
            .semantic_tokens_range
            .insert(file_uri(path), result);
    }

    /// Programs the [`DocumentSymbolResponse`] returned for a
    /// `textDocument/documentSymbol` call against `path`. Both
    /// [`DocumentSymbolResponse::Flat`] and
    /// [`DocumentSymbolResponse::Nested`] variants round-trip; tests
    /// pick the shape they need. Replaces any previously seeded
    /// response for the same document. Distinct from
    /// [`Self::add_workspace_symbol`], which seeds the
    /// `workspace/symbol` query lookup.
    pub fn set_document_symbols(&self, path: &str, response: DocumentSymbolResponse) {
        self.state
            .lock()
            .unwrap()
            .document_symbols
            .insert(file_uri(path), response);
    }

    /// Programs the [`CallHierarchyItem`]s returned for a
    /// `textDocument/prepareCallHierarchy` call whose request
    /// position matches `(path, line, col)`. Replaces any
    /// previously seeded items for the same position.
    pub fn set_prepare_call_hierarchy(
        &self,
        path: &str,
        line: u32,
        col: u32,
        items: Vec<CallHierarchyItem>,
    ) {
        self.state
            .lock()
            .unwrap()
            .call_hierarchy_prepare
            .insert(LspKey::new(path, line, col), items);
    }

    /// Programs the [`CallHierarchyIncomingCall`]s returned when
    /// `callHierarchy/incomingCalls` is sent with an item anchored
    /// at `(path, line, col)`. The fake matches on the requested
    /// item's `range.start`, so tests should construct items via
    /// [`call_hierarchy_item`] (or set `range.start` to match).
    pub fn set_call_hierarchy_incoming_calls(
        &self,
        path: &str,
        line: u32,
        col: u32,
        calls: Vec<CallHierarchyIncomingCall>,
    ) {
        self.state
            .lock()
            .unwrap()
            .call_hierarchy_incoming
            .insert(LspKey::new(path, line, col), calls);
    }

    /// Programs the [`CallHierarchyOutgoingCall`]s returned when
    /// `callHierarchy/outgoingCalls` is sent with an item anchored
    /// at `(path, line, col)`. Lookup matches on the requested
    /// item's `range.start` -- see
    /// [`Self::set_call_hierarchy_incoming_calls`].
    pub fn set_call_hierarchy_outgoing_calls(
        &self,
        path: &str,
        line: u32,
        col: u32,
        calls: Vec<CallHierarchyOutgoingCall>,
    ) {
        self.state
            .lock()
            .unwrap()
            .call_hierarchy_outgoing
            .insert(LspKey::new(path, line, col), calls);
    }

    /// Programs the [`TypeHierarchyItem`]s returned for a
    /// `textDocument/prepareTypeHierarchy` call whose request
    /// position matches `(path, line, col)`. Replaces any
    /// previously seeded items for the same position.
    pub fn set_prepare_type_hierarchy(
        &self,
        path: &str,
        line: u32,
        col: u32,
        items: Vec<TypeHierarchyItem>,
    ) {
        self.state
            .lock()
            .unwrap()
            .type_hierarchy_prepare
            .insert(LspKey::new(path, line, col), items);
    }

    /// Programs the supertype [`TypeHierarchyItem`]s returned when
    /// `typeHierarchy/supertypes` is sent with an item anchored at
    /// `(path, line, col)`. The fake matches on the requested
    /// item's `range.start`, so tests should construct items via
    /// [`type_hierarchy_item`] (or set `range.start` to match).
    pub fn set_type_hierarchy_supertypes(
        &self,
        path: &str,
        line: u32,
        col: u32,
        items: Vec<TypeHierarchyItem>,
    ) {
        self.state
            .lock()
            .unwrap()
            .type_hierarchy_supertypes
            .insert(LspKey::new(path, line, col), items);
    }

    /// Programs the subtype [`TypeHierarchyItem`]s returned when
    /// `typeHierarchy/subtypes` is sent with an item anchored at
    /// `(path, line, col)`. Lookup matches on the requested item's
    /// `range.start` -- see [`Self::set_type_hierarchy_supertypes`].
    pub fn set_type_hierarchy_subtypes(
        &self,
        path: &str,
        line: u32,
        col: u32,
        items: Vec<TypeHierarchyItem>,
    ) {
        self.state
            .lock()
            .unwrap()
            .type_hierarchy_subtypes
            .insert(LspKey::new(path, line, col), items);
    }

    /// Programs the [`WorkspaceEdit`] returned for a
    /// `workspace/willRenameFiles` request whose first
    /// [`FileRename`] entry matches `(old_path, new_path)`. The
    /// fake keys on the first entry only -- multi-file rename
    /// requests look up only the head pair, mirroring the typical
    /// editor flow of renaming one file at a time. Replaces any
    /// previously seeded edit for the same pair.
    pub fn set_will_rename(&self, old_path: &str, new_path: &str, edit: WorkspaceEdit) {
        let key = (
            file_uri(old_path).as_str().to_string(),
            file_uri(new_path).as_str().to_string(),
        );
        self.state.lock().unwrap().will_renames.insert(key, edit);
    }

    /// Snapshot of every [`RenameFilesParams`] received via
    /// [`LspServer::did_rename`] in call order. Tests use this to
    /// assert the editor notified the server about a completed
    /// rename.
    pub fn observed_renames(&self) -> Vec<RenameFilesParams> {
        self.state.lock().unwrap().observed_renames.clone()
    }

    /// Snapshot of every [`DidOpenTextDocumentParams`] received
    /// via [`LspServer::did_open`] in call order. Tests use this to
    /// assert that the editor notified the server when a buffer
    /// was opened, and that re-opens dedupe at the call site.
    pub fn observed_opens(&self) -> Vec<DidOpenTextDocumentParams> {
        self.state.lock().unwrap().observed_opens.clone()
    }

    /// Snapshot of every [`DidChangeTextDocumentParams`] received
    /// via [`LspServer::did_change`] in call order. Tests use this to
    /// assert that the editor's debouncer fired exactly once per
    /// quiet window with the latest text and a monotonic version.
    pub fn observed_changes(&self) -> Vec<DidChangeTextDocumentParams> {
        self.state.lock().unwrap().observed_changes.clone()
    }

    /// Snapshot of every [`DidChangeWatchedFilesParams`] received
    /// via [`LspServer::did_change_watched_files`] in call order.
    pub fn observed_watched_file_changes(&self) -> Vec<DidChangeWatchedFilesParams> {
        self.state
            .lock()
            .unwrap()
            .observed_watched_file_changes
            .clone()
    }

    /// Snapshot of every [`DidChangeConfigurationParams`] received
    /// via [`LspServer::did_change_configuration`] in call order.
    pub fn observed_configuration_changes(&self) -> Vec<DidChangeConfigurationParams> {
        self.state
            .lock()
            .unwrap()
            .observed_configuration_changes
            .clone()
    }

    /// Snapshot of every [`DidChangeWorkspaceFoldersParams`] received
    /// via [`LspServer::did_change_workspace_folders`] in call order.
    pub fn observed_workspace_folder_changes(&self) -> Vec<DidChangeWorkspaceFoldersParams> {
        self.state
            .lock()
            .unwrap()
            .observed_workspace_folder_changes
            .clone()
    }

    /// Inject a synthetic server-initiated request onto the fake's
    /// incoming-request channel. The next
    /// [`LspServer::recv_incoming_request`] call returns this request,
    /// after which the editor is expected to answer via
    /// [`LspServer::reply`]. Quiet-fails if the channel is closed
    /// (matches the production "server gone" case).
    pub fn push_incoming_request(&self, req: IncomingRequest) {
        let _ = self.req_tx.send(req);
    }

    /// Snapshot of every reply the host has received via
    /// [`LspServer::reply`] in call order. Tests assert the editor
    /// answered an [`IncomingRequest`] with the right id and result.
    pub fn observed_replies(&self) -> Vec<(NumberOrString, Result<Value, LspResponseError>)> {
        self.state.lock().unwrap().observed_replies.clone()
    }

    /// Programs the response value returned for a
    /// `workspace/executeCommand` request whose `params.command`
    /// matches `command`. Replaces any previously seeded
    /// response for the same command name. Tests assert the
    /// fake returns the seeded `Value`; unprogrammed commands
    /// return `None`.
    pub fn set_execute_command(&self, command: &str, response: Value) {
        self.state
            .lock()
            .unwrap()
            .executed_commands
            .insert(command.to_string(), response);
    }

    /// Snapshot of every `workspace/executeCommand` request the fake
    /// has received, in dispatch order. Tests assert that command
    /// dispatch fired with the expected `(command, arguments)` pair.
    pub fn observed_executed_commands(&self) -> Vec<ExecuteCommandParams> {
        self.state
            .lock()
            .unwrap()
            .observed_executed_commands
            .clone()
    }

    /// Programs a [`PrepareRenameResponse`] returned for the
    /// `textDocument/prepareRename` call whose position matches
    /// `(path, line, col)`. Tests use this to drive the rename
    /// pre-flight UI flow without a real server.
    pub fn set_prepare_rename(
        &self,
        path: &str,
        line: u32,
        col: u32,
        response: PrepareRenameResponse,
    ) {
        self.state
            .lock()
            .unwrap()
            .prepare_renames
            .insert(LspKey::new(path, line, col), response);
    }

    /// Programs the [`WorkspaceEdit`] returned for a
    /// `textDocument/rename` call whose request position matches
    /// `(path, line, col)`. The fake matches on the request position
    /// component of [`RenameParams`] only; the new name supplied at
    /// rename time is ignored, so tests construct an edit that
    /// references the symbol's range and the new text directly.
    pub fn set_rename(&self, path: &str, line: u32, col: u32, edit: WorkspaceEdit) {
        self.state
            .lock()
            .unwrap()
            .renames
            .insert(LspKey::new(path, line, col), edit);
    }

    // --- Hover ---

    pub fn set_hover(&self, path: &str, line: u32, col: u32, markdown: &str) {
        let hover = Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: markdown.to_string(),
            }),
            range: None,
        };
        self.state
            .lock()
            .unwrap()
            .hovers
            .insert(LspKey::new(path, line, col), hover);
    }

    // --- Signature help ---

    /// Programs the [`SignatureHelp`] returned for a
    /// `textDocument/signatureHelp` call whose request position
    /// matches `(path, line, col)`. Replaces any previously seeded
    /// help for the same position.
    pub fn set_signature_help(&self, path: &str, line: u32, col: u32, help: SignatureHelp) {
        self.state
            .lock()
            .unwrap()
            .signature_helps
            .insert(LspKey::new(path, line, col), help);
    }

    // --- Diagnostics ---

    pub fn add_error(&self, path: &str, line: u32, start_col: u32, end_col: u32, message: &str) {
        self.add_diagnostic(
            path,
            line,
            start_col,
            end_col,
            DiagnosticSeverity::ERROR,
            message,
        );
    }

    pub fn add_warning(&self, path: &str, line: u32, start_col: u32, end_col: u32, message: &str) {
        self.add_diagnostic(
            path,
            line,
            start_col,
            end_col,
            DiagnosticSeverity::WARNING,
            message,
        );
    }

    pub fn add_diagnostic(
        &self,
        path: &str,
        line: u32,
        start_col: u32,
        end_col: u32,
        severity: DiagnosticSeverity,
        message: &str,
    ) {
        let diag = Diagnostic::new(
            Range::new(Position::new(line, start_col), Position::new(line, end_col)),
            Some(severity),
            None,
            None,
            message.to_string(),
            None,
            None,
        );
        self.state
            .lock()
            .unwrap()
            .diagnostics
            .entry(file_uri(path))
            .or_default()
            .push(diag);
    }

    /// Append a notification to the queue exposed via
    /// [`LspServer::recv_notification`]. Lets tests inject server-push
    /// flows that bypass the existing `did_open` / `did_change` path:
    /// diagnostics published asynchronously after analysis,
    /// [`LspNotification::Progress`] frames, etc.
    pub fn push_notification(&self, notification: LspNotification) {
        let _ = self.notif_tx.send(notification);
    }

    /// Arm a one-shot failure for the next call that targets `method`
    /// (e.g. `"textDocument/hover"`). The next matching request
    /// returns `io::Error::new(kind, "<method>: injected request
    /// failure")` and clears the arm; subsequent calls behave
    /// normally. Overwrites any previously armed one-shot for the
    /// same method.
    pub fn fail_next_request(&self, method: &str, kind: io::ErrorKind) {
        self.state
            .lock()
            .unwrap()
            .request_failures_oneshot
            .insert(method.to_string(), kind);
    }

    /// Arm a sticky failure for every call that targets `method`.
    /// Each matching request returns `io::Error::new(kind, ...)`
    /// until [`Self::clear_method_error`] is called for the same
    /// method.
    pub fn set_method_error(&self, method: &str, kind: io::ErrorKind) {
        self.state
            .lock()
            .unwrap()
            .request_failures_persistent
            .insert(method.to_string(), kind);
    }

    /// Clear a sticky failure previously armed via
    /// [`Self::set_method_error`]. No-op if none was armed.
    pub fn clear_method_error(&self, method: &str) {
        self.state
            .lock()
            .unwrap()
            .request_failures_persistent
            .remove(method);
    }

    fn take_request_failure(&self, method: &str) -> Option<io::Error> {
        let kind = {
            let mut state = self.state.lock().unwrap();
            if let Some(kind) = state.request_failures_oneshot.remove(method) {
                kind
            } else if let Some(&kind) = state.request_failures_persistent.get(method) {
                kind
            } else {
                return None;
            }
        };
        Some(io::Error::new(
            kind,
            format!("{method}: injected request failure"),
        ))
    }

    /// Install the [`Executor`] used by [`Self::set_request_delay`] to
    /// schedule per-request timer waits. Call once after construction;
    /// without an executor `set_request_delay` records the delay but
    /// requests resolve immediately.
    pub fn set_executor(&self, executor: Executor) {
        *self.executor.lock().unwrap() = Some(executor);
    }

    /// Arm a sticky delay for every request that targets `method`
    /// (e.g. `"textDocument/hover"`). Each matching call awaits the
    /// configured duration on the installed [`Executor`] timer before
    /// the response (success or failure) is produced. Overwrites any
    /// previously armed delay for the same method. Has no effect on
    /// notifications, which have no response.
    pub fn set_request_delay(&self, method: &str, duration: Duration) {
        self.state
            .lock()
            .unwrap()
            .request_delays
            .insert(method.to_string(), duration);
    }

    /// Clear a delay previously armed via [`Self::set_request_delay`].
    /// No-op if none was armed.
    pub fn clear_request_delay(&self, method: &str) {
        self.state.lock().unwrap().request_delays.remove(method);
    }

    async fn apply_delay(&self, method: &str) {
        let duration = self
            .state
            .lock()
            .unwrap()
            .request_delays
            .get(method)
            .copied();
        let Some(duration) = duration else { return };
        let executor = self.executor.lock().unwrap().clone();
        let Some(executor) = executor else { return };
        let guard = CancellationGuard::armed(&self.state, method.to_string());
        executor.timer(duration).await;
        guard.disarm();
    }

    /// Methods recorded as cancelled because their futures were
    /// dropped during the delay window armed by
    /// [`Self::set_request_delay`]. Callers spawning a request via
    /// [`Executor::spawn`] cancel by dropping the returned `Task`;
    /// the [`CancellationGuard`] inside [`Self::apply_delay`]
    /// observes the drop and pushes the method name here. Order
    /// preserves arrival.
    pub fn cancelled_requests(&self) -> Vec<String> {
        self.state.lock().unwrap().cancelled_requests.clone()
    }

    /// Clear the cancellation log. Tests use this between phases
    /// when they want the next assertion to start from an empty
    /// slate.
    pub fn clear_cancelled_requests(&self) {
        self.state.lock().unwrap().cancelled_requests.clear();
    }

    /// Toggle pending mode for the request method `R`. While
    /// enabled, calls to the matching [`LspServer`] method enqueue
    /// `(params, oneshot::Sender<R::Result>)` onto an internal
    /// queue and await the receiver instead of returning the
    /// programmed response. Tests drain the queue with
    /// [`Self::take_pending`] and drive the response on the
    /// bundled sender. Disabling restores the synchronous
    /// programmed-response path.
    pub fn set_pending_mode<R: lsp_types::request::Request>(&self, enabled: bool)
    where
        R::Params: Send + 'static,
        R::Result: Send + 'static,
    {
        self.state
            .lock()
            .unwrap()
            .pending_modes
            .insert(R::METHOD, enabled);
    }

    /// Number of in-flight requests waiting on a test response
    /// for `method`. Returns 0 when no entry has been queued
    /// (whether pending mode is disabled or no request fired).
    pub fn pending_count(&self, method: &str) -> usize {
        self.state
            .lock()
            .unwrap()
            .pending_queue
            .get(method)
            .map(|q| q.len())
            .unwrap_or(0)
    }

    /// Pop the oldest pending entry for request type `R`,
    /// returning the original params alongside the
    /// `oneshot::Sender` the awaiting future is parked on.
    /// Returns `None` if the queue is empty or the stored entry
    /// fails to downcast to `(R::Params, oneshot::Sender<R::Result>)`
    /// -- the latter indicates a programming error (queue
    /// poisoned by a different request type).
    pub fn take_pending<R: lsp_types::request::Request>(
        &self,
    ) -> Option<(R::Params, oneshot::Sender<R::Result>)>
    where
        R::Params: Send + 'static,
        R::Result: Send + 'static,
    {
        let entry = self
            .state
            .lock()
            .unwrap()
            .pending_queue
            .get_mut(R::METHOD)
            .and_then(|q| q.pop_front())?;
        entry
            .inner
            .downcast::<(R::Params, oneshot::Sender<R::Result>)>()
            .ok()
            .map(|b| *b)
    }

    fn is_pending_mode_for(&self, method: &str) -> bool {
        self.state
            .lock()
            .unwrap()
            .pending_modes
            .get(method)
            .copied()
            .unwrap_or(false)
    }

    fn enqueue_pending<R: lsp_types::request::Request>(
        &self,
        params: R::Params,
    ) -> oneshot::Receiver<R::Result>
    where
        R::Params: Send + 'static,
        R::Result: Send + 'static,
    {
        let (tx, rx) = oneshot::channel();
        let entry = PendingEntry {
            inner: Box::new((params, tx)),
        };
        self.state
            .lock()
            .unwrap()
            .pending_queue
            .entry(R::METHOD)
            .or_default()
            .push_back(entry);
        rx
    }

    // --- Completions ---

    pub fn set_completions(&self, path: &str, line: u32, col: u32, labels: &[&str]) {
        let items = labels
            .iter()
            .map(|label| CompletionItem {
                label: label.to_string(),
                ..CompletionItem::default()
            })
            .collect();
        self.state
            .lock()
            .unwrap()
            .completions
            .insert(LspKey::new(path, line, col), items);
    }

    /// Like [`Self::set_completions`] but accepts fully-formed
    /// [`CompletionItem`]s so callers can program `text_edit`,
    /// `kind`, `detail`, and other fields the label-only setter
    /// cannot reach. Used by completion-source tests that exercise
    /// non-default item shapes.
    pub fn set_completion_items(
        &self,
        path: &str,
        line: u32,
        col: u32,
        items: Vec<CompletionItem>,
    ) {
        self.state
            .lock()
            .unwrap()
            .completions
            .insert(LspKey::new(path, line, col), items);
    }

    // --- Navigation (definition, declaration, type definition, implementation) ---

    fn set_goto(
        map: &mut BTreeMap<LspKey, GotoDefinitionResponse>,
        path: &str,
        line: u32,
        col: u32,
        target_path: &str,
        target_line: u32,
        target_col: u32,
    ) {
        let location = Location::new(
            file_uri(target_path),
            Range::new(
                Position::new(target_line, target_col),
                Position::new(target_line, target_col),
            ),
        );
        map.insert(
            LspKey::new(path, line, col),
            GotoDefinitionResponse::Scalar(location),
        );
    }

    pub fn set_definition(
        &self,
        path: &str,
        line: u32,
        col: u32,
        target_path: &str,
        target_line: u32,
        target_col: u32,
    ) {
        Self::set_goto(
            &mut self.state.lock().unwrap().definitions,
            path,
            line,
            col,
            target_path,
            target_line,
            target_col,
        );
    }

    pub fn set_declaration(
        &self,
        path: &str,
        line: u32,
        col: u32,
        target_path: &str,
        target_line: u32,
        target_col: u32,
    ) {
        Self::set_goto(
            &mut self.state.lock().unwrap().declarations,
            path,
            line,
            col,
            target_path,
            target_line,
            target_col,
        );
    }

    pub fn set_type_definition(
        &self,
        path: &str,
        line: u32,
        col: u32,
        target_path: &str,
        target_line: u32,
        target_col: u32,
    ) {
        Self::set_goto(
            &mut self.state.lock().unwrap().type_definitions,
            path,
            line,
            col,
            target_path,
            target_line,
            target_col,
        );
    }

    pub fn set_implementation(
        &self,
        path: &str,
        line: u32,
        col: u32,
        target_path: &str,
        target_line: u32,
        target_col: u32,
    ) {
        Self::set_goto(
            &mut self.state.lock().unwrap().implementations,
            path,
            line,
            col,
            target_path,
            target_line,
            target_col,
        );
    }

    // --- References ---

    pub fn set_references(&self, path: &str, line: u32, col: u32, refs: &[(&str, u32, u32)]) {
        let locations = refs
            .iter()
            .map(|(p, l, c)| {
                Location::new(
                    file_uri(p),
                    Range::new(Position::new(*l, *c), Position::new(*l, *c)),
                )
            })
            .collect();
        self.state
            .lock()
            .unwrap()
            .references
            .insert(LspKey::new(path, line, col), locations);
    }

    // --- Document highlight ---

    pub fn set_highlights(&self, path: &str, line: u32, col: u32, ranges: &[(u32, u32, u32)]) {
        let highlights = ranges
            .iter()
            .map(|(l, start, end)| DocumentHighlight {
                range: Range::new(Position::new(*l, *start), Position::new(*l, *end)),
                kind: Some(DocumentHighlightKind::READ),
            })
            .collect();
        self.state
            .lock()
            .unwrap()
            .highlights
            .insert(LspKey::new(path, line, col), highlights);
    }

    // --- Inlay hints ---

    pub fn add_inlay_hint(
        &self,
        path: &str,
        line: u32,
        col: u32,
        label: &str,
        kind: InlayHintKind,
    ) {
        let hint = InlayHint {
            position: Position::new(line, col),
            label: InlayHintLabel::String(label.to_string()),
            kind: Some(kind),
            text_edits: None,
            tooltip: None,
            padding_left: None,
            padding_right: None,
            data: None,
        };
        self.state
            .lock()
            .unwrap()
            .inlay_hints
            .entry(file_uri(path))
            .or_default()
            .push(hint);
    }

    pub fn add_type_hint(&self, path: &str, line: u32, col: u32, type_label: &str) {
        self.add_inlay_hint(path, line, col, type_label, InlayHintKind::TYPE);
    }

    pub fn add_parameter_hint(&self, path: &str, line: u32, col: u32, param_label: &str) {
        self.add_inlay_hint(path, line, col, param_label, InlayHintKind::PARAMETER);
    }

    /// Programs the [`InlayHint`]s returned by
    /// [`LspServer::range_inlay_hint`] -- the viewport-bounded sibling of
    /// [`LspServer::inlay_hint`]. The fake ignores the request's range
    /// and returns whatever was programmed for the document; tests
    /// arrange URI and range to match. Replaces any previously seeded
    /// hints for the same document. Distinct from the
    /// [`Self::add_inlay_hint`] / [`Self::add_type_hint`] /
    /// [`Self::add_parameter_hint`] family, which seed responses for
    /// the full-document [`LspServer::inlay_hint`] call.
    pub fn set_range_inlay_hints(&self, path: &str, hints: Vec<InlayHint>) {
        self.state
            .lock()
            .unwrap()
            .range_inlay_hints
            .insert(file_uri(path), hints);
    }

    // --- Workspace symbols ---

    pub fn add_workspace_symbol(
        &self,
        query: &str,
        name: &str,
        kind: SymbolKind,
        path: &str,
        line: u32,
        col: u32,
    ) {
        #[allow(deprecated)]
        let info = SymbolInformation {
            name: name.to_string(),
            kind,
            tags: None,
            deprecated: None,
            location: Location::new(
                file_uri(path),
                Range::new(Position::new(line, col), Position::new(line, col)),
            ),
            container_name: None,
        };
        self.state
            .lock()
            .unwrap()
            .workspace_symbols
            .entry(query.to_string())
            .or_default()
            .push(info);
    }

    /// Programs a raw [`WorkspaceSymbolResponse`] for the given
    /// `workspace/symbol` query. Replaces any previously seeded
    /// response for the same query and shadows entries seeded via
    /// [`Self::add_workspace_symbol`]. Lets tests exercise the
    /// `Nested` shape, which `add_workspace_symbol` cannot produce.
    pub fn set_workspace_symbol_response(&self, query: &str, response: WorkspaceSymbolResponse) {
        self.state
            .lock()
            .unwrap()
            .workspace_symbol_responses
            .insert(query.to_string(), response);
    }

    // --- Assertions ---

    pub fn opened_documents(&self) -> Vec<Uri> {
        self.state
            .lock()
            .unwrap()
            .open_documents
            .keys()
            .cloned()
            .collect()
    }

    pub fn is_open(&self, path: &str) -> bool {
        self.state
            .lock()
            .unwrap()
            .open_documents
            .contains_key(&file_uri(path))
    }
}

fn lookup_goto(
    map: &BTreeMap<LspKey, GotoDefinitionResponse>,
    uri: &Uri,
    pos: &Position,
) -> Option<GotoDefinitionResponse> {
    map.get(&LspKey::from_position(uri, pos)).cloned()
}

#[async_trait]
impl LspServer for FakeLsp {
    fn capabilities(&self) -> Arc<ServerCapabilities> {
        self.state.lock().unwrap().capabilities.clone()
    }

    async fn initialize(&self, _root_uri: Option<Uri>) -> io::Result<InitializeResult> {
        self.apply_delay("initialize").await;
        if let Some(err) = self.take_request_failure("initialize") {
            return Err(err);
        }
        let mut state = self.state.lock().unwrap();
        state.initialized = true;
        Ok(InitializeResult {
            capabilities: (*state.capabilities).clone(),
            server_info: None,
        })
    }

    async fn shutdown(&self) -> io::Result<()> {
        self.apply_delay("shutdown").await;
        if let Some(err) = self.take_request_failure("shutdown") {
            return Err(err);
        }
        self.state.lock().unwrap().shut_down = true;
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) -> io::Result<()> {
        let pending =
            {
                let mut state = self.state.lock().unwrap();
                let uri = params.text_document.uri.clone();
                state
                    .open_documents
                    .insert(uri.clone(), params.text_document.text.clone());
                state.observed_opens.push(params.clone());
                state.diagnostics.get(&uri).cloned().map(|diagnostics| {
                    LspNotification::Diagnostics {
                        uri,
                        diagnostics,
                        version: None,
                    }
                })
            };
        if let Some(notif) = pending {
            let _ = self.notif_tx.send(notif);
        }
        Ok(())
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) -> io::Result<()> {
        let pending =
            {
                let mut state = self.state.lock().unwrap();
                let uri = params.text_document.uri.clone();
                state.observed_changes.push(params.clone());
                if let Some(change) = params.content_changes.into_iter().last() {
                    state.open_documents.insert(uri.clone(), change.text);
                }
                state.diagnostics.get(&uri).cloned().map(|diagnostics| {
                    LspNotification::Diagnostics {
                        uri,
                        diagnostics,
                        version: None,
                    }
                })
            };
        if let Some(notif) = pending {
            let _ = self.notif_tx.send(notif);
        }
        Ok(())
    }

    async fn did_save(&self, _params: DidSaveTextDocumentParams) -> io::Result<()> {
        Ok(())
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) -> io::Result<()> {
        self.state
            .lock()
            .unwrap()
            .open_documents
            .remove(&params.text_document.uri);
        Ok(())
    }

    async fn did_rename(&self, params: RenameFilesParams) -> io::Result<()> {
        self.state.lock().unwrap().observed_renames.push(params);
        Ok(())
    }

    async fn did_change_watched_files(
        &self,
        params: DidChangeWatchedFilesParams,
    ) -> io::Result<()> {
        self.state
            .lock()
            .unwrap()
            .observed_watched_file_changes
            .push(params);
        Ok(())
    }

    async fn did_change_configuration(
        &self,
        params: DidChangeConfigurationParams,
    ) -> io::Result<()> {
        self.state
            .lock()
            .unwrap()
            .observed_configuration_changes
            .push(params);
        Ok(())
    }

    async fn did_change_workspace_folders(
        &self,
        params: DidChangeWorkspaceFoldersParams,
    ) -> io::Result<()> {
        self.state
            .lock()
            .unwrap()
            .observed_workspace_folder_changes
            .push(params);
        Ok(())
    }

    async fn hover(&self, params: HoverParams) -> io::Result<Option<Hover>> {
        self.apply_delay("textDocument/hover").await;
        if let Some(err) = self.take_request_failure("textDocument/hover") {
            return Err(err);
        }
        pending_check!(self, lsp_types::request::HoverRequest, params);
        let state = self.state.lock().unwrap();
        let key = LspKey::from_position(
            &params.text_document_position_params.text_document.uri,
            &params.text_document_position_params.position,
        );
        Ok(state.hovers.get(&key).cloned())
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> io::Result<Option<GotoDefinitionResponse>> {
        self.apply_delay("textDocument/definition").await;
        if let Some(err) = self.take_request_failure("textDocument/definition") {
            return Err(err);
        }
        pending_check!(self, lsp_types::request::GotoDefinition, params);
        let state = self.state.lock().unwrap();
        Ok(lookup_goto(
            &state.definitions,
            &params.text_document_position_params.text_document.uri,
            &params.text_document_position_params.position,
        ))
    }

    async fn goto_declaration(
        &self,
        params: GotoDefinitionParams,
    ) -> io::Result<Option<GotoDefinitionResponse>> {
        self.apply_delay("textDocument/declaration").await;
        if let Some(err) = self.take_request_failure("textDocument/declaration") {
            return Err(err);
        }
        pending_check!(self, lsp_types::request::GotoDeclaration, params);
        let state = self.state.lock().unwrap();
        Ok(lookup_goto(
            &state.declarations,
            &params.text_document_position_params.text_document.uri,
            &params.text_document_position_params.position,
        ))
    }

    async fn goto_type_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> io::Result<Option<GotoDefinitionResponse>> {
        self.apply_delay("textDocument/typeDefinition").await;
        if let Some(err) = self.take_request_failure("textDocument/typeDefinition") {
            return Err(err);
        }
        pending_check!(self, lsp_types::request::GotoTypeDefinition, params);
        let state = self.state.lock().unwrap();
        Ok(lookup_goto(
            &state.type_definitions,
            &params.text_document_position_params.text_document.uri,
            &params.text_document_position_params.position,
        ))
    }

    async fn goto_implementation(
        &self,
        params: GotoDefinitionParams,
    ) -> io::Result<Option<GotoDefinitionResponse>> {
        self.apply_delay("textDocument/implementation").await;
        if let Some(err) = self.take_request_failure("textDocument/implementation") {
            return Err(err);
        }
        pending_check!(self, lsp_types::request::GotoImplementation, params);
        let state = self.state.lock().unwrap();
        Ok(lookup_goto(
            &state.implementations,
            &params.text_document_position_params.text_document.uri,
            &params.text_document_position_params.position,
        ))
    }

    async fn references(&self, params: ReferenceParams) -> io::Result<Option<Vec<Location>>> {
        self.apply_delay("textDocument/references").await;
        if let Some(err) = self.take_request_failure("textDocument/references") {
            return Err(err);
        }
        pending_check!(self, lsp_types::request::References, params);
        let state = self.state.lock().unwrap();
        let key = LspKey::from_position(
            &params.text_document_position.text_document.uri,
            &params.text_document_position.position,
        );
        Ok(state.references.get(&key).cloned())
    }

    async fn document_highlight(
        &self,
        params: DocumentHighlightParams,
    ) -> io::Result<Option<Vec<DocumentHighlight>>> {
        self.apply_delay("textDocument/documentHighlight").await;
        if let Some(err) = self.take_request_failure("textDocument/documentHighlight") {
            return Err(err);
        }
        pending_check!(self, lsp_types::request::DocumentHighlightRequest, params);
        let state = self.state.lock().unwrap();
        let key = LspKey::from_position(
            &params.text_document_position_params.text_document.uri,
            &params.text_document_position_params.position,
        );
        Ok(state.highlights.get(&key).cloned())
    }

    async fn completion(&self, params: CompletionParams) -> io::Result<Option<CompletionResponse>> {
        self.apply_delay("textDocument/completion").await;
        if let Some(err) = self.take_request_failure("textDocument/completion") {
            return Err(err);
        }
        pending_check!(self, lsp_types::request::Completion, params);
        let state = self.state.lock().unwrap();
        let key = LspKey::from_position(
            &params.text_document_position.text_document.uri,
            &params.text_document_position.position,
        );
        Ok(state.completions.get(&key).map(|items| {
            CompletionResponse::List(CompletionList {
                is_incomplete: false,
                items: items.clone(),
            })
        }))
    }

    async fn completion_resolve(&self, item: CompletionItem) -> io::Result<CompletionItem> {
        self.apply_delay("completionItem/resolve").await;
        if let Some(err) = self.take_request_failure("completionItem/resolve") {
            return Err(err);
        }
        pending_check!(self, lsp_types::request::ResolveCompletionItem, item);
        Ok(item)
    }

    async fn code_action(
        &self,
        params: CodeActionParams,
    ) -> io::Result<Option<Vec<CodeActionOrCommand>>> {
        self.apply_delay("textDocument/codeAction").await;
        if let Some(err) = self.take_request_failure("textDocument/codeAction") {
            return Err(err);
        }
        pending_check!(self, lsp_types::request::CodeActionRequest, params);
        let state = self.state.lock().unwrap();
        Ok(state.code_actions.get(&params.text_document.uri).cloned())
    }

    async fn code_action_resolve(&self, action: CodeAction) -> io::Result<CodeAction> {
        self.apply_delay("codeAction/resolve").await;
        if let Some(err) = self.take_request_failure("codeAction/resolve") {
            return Err(err);
        }
        pending_check!(self, lsp_types::request::CodeActionResolveRequest, action);
        Ok(action)
    }

    async fn document_link(
        &self,
        params: DocumentLinkParams,
    ) -> io::Result<Option<Vec<DocumentLink>>> {
        self.apply_delay("textDocument/documentLink").await;
        if let Some(err) = self.take_request_failure("textDocument/documentLink") {
            return Err(err);
        }
        pending_check!(self, lsp_types::request::DocumentLinkRequest, params);
        let state = self.state.lock().unwrap();
        Ok(state.document_links.get(&params.text_document.uri).cloned())
    }

    async fn document_link_resolve(&self, link: DocumentLink) -> io::Result<DocumentLink> {
        self.apply_delay("documentLink/resolve").await;
        if let Some(err) = self.take_request_failure("documentLink/resolve") {
            return Err(err);
        }
        pending_check!(self, lsp_types::request::DocumentLinkResolve, link);
        Ok(link)
    }

    async fn document_color(
        &self,
        params: DocumentColorParams,
    ) -> io::Result<Option<Vec<ColorInformation>>> {
        self.apply_delay("textDocument/documentColor").await;
        if let Some(err) = self.take_request_failure("textDocument/documentColor") {
            return Err(err);
        }
        pending_check_some!(self, lsp_types::request::DocumentColor, params);
        let state = self.state.lock().unwrap();
        Ok(state
            .document_colors
            .get(&params.text_document.uri)
            .cloned())
    }

    async fn color_presentation(
        &self,
        params: ColorPresentationParams,
    ) -> io::Result<Option<Vec<ColorPresentation>>> {
        self.apply_delay("textDocument/colorPresentation").await;
        if let Some(err) = self.take_request_failure("textDocument/colorPresentation") {
            return Err(err);
        }
        pending_check_some!(self, lsp_types::request::ColorPresentationRequest, params);
        let state = self.state.lock().unwrap();
        Ok(state
            .color_presentations
            .get(&params.text_document.uri)
            .cloned())
    }

    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> io::Result<Option<SemanticTokensResult>> {
        self.apply_delay("textDocument/semanticTokens/full").await;
        if let Some(err) = self.take_request_failure("textDocument/semanticTokens/full") {
            return Err(err);
        }
        pending_check!(self, lsp_types::request::SemanticTokensFullRequest, params);
        let state = self.state.lock().unwrap();
        Ok(state
            .semantic_tokens_full
            .get(&params.text_document.uri)
            .cloned())
    }

    async fn semantic_tokens_range(
        &self,
        params: SemanticTokensRangeParams,
    ) -> io::Result<Option<SemanticTokensRangeResult>> {
        self.apply_delay("textDocument/semanticTokens/range").await;
        if let Some(err) = self.take_request_failure("textDocument/semanticTokens/range") {
            return Err(err);
        }
        pending_check!(self, lsp_types::request::SemanticTokensRangeRequest, params);
        let state = self.state.lock().unwrap();
        Ok(state
            .semantic_tokens_range
            .get(&params.text_document.uri)
            .cloned())
    }

    async fn prepare_call_hierarchy(
        &self,
        params: CallHierarchyPrepareParams,
    ) -> io::Result<Option<Vec<CallHierarchyItem>>> {
        self.apply_delay("textDocument/prepareCallHierarchy").await;
        if let Some(err) = self.take_request_failure("textDocument/prepareCallHierarchy") {
            return Err(err);
        }
        pending_check!(self, lsp_types::request::CallHierarchyPrepare, params);
        let state = self.state.lock().unwrap();
        let key = LspKey::from_position(
            &params.text_document_position_params.text_document.uri,
            &params.text_document_position_params.position,
        );
        Ok(state.call_hierarchy_prepare.get(&key).cloned())
    }

    async fn call_hierarchy_incoming_calls(
        &self,
        params: CallHierarchyIncomingCallsParams,
    ) -> io::Result<Option<Vec<CallHierarchyIncomingCall>>> {
        self.apply_delay("callHierarchy/incomingCalls").await;
        if let Some(err) = self.take_request_failure("callHierarchy/incomingCalls") {
            return Err(err);
        }
        pending_check!(self, lsp_types::request::CallHierarchyIncomingCalls, params);
        let state = self.state.lock().unwrap();
        let key = LspKey::from_position(&params.item.uri, &params.item.range.start);
        Ok(state.call_hierarchy_incoming.get(&key).cloned())
    }

    async fn call_hierarchy_outgoing_calls(
        &self,
        params: CallHierarchyOutgoingCallsParams,
    ) -> io::Result<Option<Vec<CallHierarchyOutgoingCall>>> {
        self.apply_delay("callHierarchy/outgoingCalls").await;
        if let Some(err) = self.take_request_failure("callHierarchy/outgoingCalls") {
            return Err(err);
        }
        pending_check!(self, lsp_types::request::CallHierarchyOutgoingCalls, params);
        let state = self.state.lock().unwrap();
        let key = LspKey::from_position(&params.item.uri, &params.item.range.start);
        Ok(state.call_hierarchy_outgoing.get(&key).cloned())
    }

    async fn prepare_type_hierarchy(
        &self,
        params: TypeHierarchyPrepareParams,
    ) -> io::Result<Option<Vec<TypeHierarchyItem>>> {
        self.apply_delay("textDocument/prepareTypeHierarchy").await;
        if let Some(err) = self.take_request_failure("textDocument/prepareTypeHierarchy") {
            return Err(err);
        }
        pending_check!(self, lsp_types::request::TypeHierarchyPrepare, params);
        let state = self.state.lock().unwrap();
        let key = LspKey::from_position(
            &params.text_document_position_params.text_document.uri,
            &params.text_document_position_params.position,
        );
        Ok(state.type_hierarchy_prepare.get(&key).cloned())
    }

    async fn type_hierarchy_supertypes(
        &self,
        params: TypeHierarchySupertypesParams,
    ) -> io::Result<Option<Vec<TypeHierarchyItem>>> {
        self.apply_delay("typeHierarchy/supertypes").await;
        if let Some(err) = self.take_request_failure("typeHierarchy/supertypes") {
            return Err(err);
        }
        pending_check!(self, lsp_types::request::TypeHierarchySupertypes, params);
        let state = self.state.lock().unwrap();
        let key = LspKey::from_position(&params.item.uri, &params.item.range.start);
        Ok(state.type_hierarchy_supertypes.get(&key).cloned())
    }

    async fn type_hierarchy_subtypes(
        &self,
        params: TypeHierarchySubtypesParams,
    ) -> io::Result<Option<Vec<TypeHierarchyItem>>> {
        self.apply_delay("typeHierarchy/subtypes").await;
        if let Some(err) = self.take_request_failure("typeHierarchy/subtypes") {
            return Err(err);
        }
        pending_check!(self, lsp_types::request::TypeHierarchySubtypes, params);
        let state = self.state.lock().unwrap();
        let key = LspKey::from_position(&params.item.uri, &params.item.range.start);
        Ok(state.type_hierarchy_subtypes.get(&key).cloned())
    }

    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> io::Result<Option<DocumentSymbolResponse>> {
        self.apply_delay("textDocument/documentSymbol").await;
        if let Some(err) = self.take_request_failure("textDocument/documentSymbol") {
            return Err(err);
        }
        pending_check!(self, lsp_types::request::DocumentSymbolRequest, params);
        let state = self.state.lock().unwrap();
        Ok(state
            .document_symbols
            .get(&params.text_document.uri)
            .cloned())
    }

    async fn document_diagnostic(
        &self,
        params: DocumentDiagnosticParams,
    ) -> io::Result<Option<DocumentDiagnosticReportResult>> {
        self.apply_delay("textDocument/diagnostic").await;
        if let Some(err) = self.take_request_failure("textDocument/diagnostic") {
            return Err(err);
        }
        pending_check_some!(self, lsp_types::request::DocumentDiagnosticRequest, params);
        let state = self.state.lock().unwrap();
        Ok(state
            .document_diagnostics
            .get(&params.text_document.uri)
            .cloned())
    }

    async fn folding_range(
        &self,
        params: FoldingRangeParams,
    ) -> io::Result<Option<Vec<FoldingRange>>> {
        self.apply_delay("textDocument/foldingRange").await;
        if let Some(err) = self.take_request_failure("textDocument/foldingRange") {
            return Err(err);
        }
        pending_check!(self, lsp_types::request::FoldingRangeRequest, params);
        let state = self.state.lock().unwrap();
        Ok(state.folding_ranges.get(&params.text_document.uri).cloned())
    }

    async fn selection_range(
        &self,
        params: SelectionRangeParams,
    ) -> io::Result<Option<Vec<SelectionRange>>> {
        self.apply_delay("textDocument/selectionRange").await;
        if let Some(err) = self.take_request_failure("textDocument/selectionRange") {
            return Err(err);
        }
        pending_check!(self, lsp_types::request::SelectionRangeRequest, params);
        let state = self.state.lock().unwrap();
        let uri = &params.text_document.uri;
        let mut chains = Vec::with_capacity(params.positions.len());
        for pos in &params.positions {
            let Some(chain) = state
                .selection_ranges
                .get(&LspKey::from_position(uri, pos))
                .cloned()
            else {
                return Ok(None);
            };
            chains.push(chain);
        }
        Ok(Some(chains))
    }

    async fn workspace_symbol(
        &self,
        params: WorkspaceSymbolParams,
    ) -> io::Result<Option<WorkspaceSymbolResponse>> {
        self.apply_delay("workspace/symbol").await;
        if let Some(err) = self.take_request_failure("workspace/symbol") {
            return Err(err);
        }
        pending_check!(self, lsp_types::request::WorkspaceSymbolRequest, params);
        let state = self.state.lock().unwrap();
        if let Some(response) = state.workspace_symbol_responses.get(&params.query) {
            return Ok(Some(response.clone()));
        }
        Ok(state
            .workspace_symbols
            .get(&params.query)
            .map(|symbols| WorkspaceSymbolResponse::Flat(symbols.clone())))
    }

    async fn signature_help(
        &self,
        params: SignatureHelpParams,
    ) -> io::Result<Option<SignatureHelp>> {
        self.apply_delay("textDocument/signatureHelp").await;
        if let Some(err) = self.take_request_failure("textDocument/signatureHelp") {
            return Err(err);
        }
        pending_check!(self, lsp_types::request::SignatureHelpRequest, params);
        let state = self.state.lock().unwrap();
        let key = LspKey::from_position(
            &params.text_document_position_params.text_document.uri,
            &params.text_document_position_params.position,
        );
        Ok(state.signature_helps.get(&key).cloned())
    }

    async fn inlay_hint(&self, params: InlayHintParams) -> io::Result<Option<Vec<InlayHint>>> {
        self.apply_delay("textDocument/inlayHint").await;
        if let Some(err) = self.take_request_failure("textDocument/inlayHint") {
            return Err(err);
        }
        pending_check!(self, lsp_types::request::InlayHintRequest, params);
        let state = self.state.lock().unwrap();
        Ok(state.inlay_hints.get(&params.text_document.uri).cloned())
    }

    async fn inlay_hint_resolve(&self, hint: InlayHint) -> io::Result<InlayHint> {
        self.apply_delay("inlayHint/resolve").await;
        if let Some(err) = self.take_request_failure("inlayHint/resolve") {
            return Err(err);
        }
        pending_check!(self, lsp_types::request::InlayHintResolveRequest, hint);
        Ok(hint)
    }

    async fn range_inlay_hint(
        &self,
        params: InlayHintParams,
    ) -> io::Result<Option<Vec<InlayHint>>> {
        self.apply_delay("textDocument/inlayHint").await;
        if let Some(err) = self.take_request_failure("textDocument/inlayHint") {
            return Err(err);
        }
        pending_check!(self, lsp_types::request::InlayHintRequest, params);
        let state = self.state.lock().unwrap();
        Ok(state
            .range_inlay_hints
            .get(&params.text_document.uri)
            .cloned())
    }

    async fn prepare_rename(
        &self,
        params: TextDocumentPositionParams,
    ) -> io::Result<Option<PrepareRenameResponse>> {
        self.apply_delay("textDocument/prepareRename").await;
        if let Some(err) = self.take_request_failure("textDocument/prepareRename") {
            return Err(err);
        }
        pending_check!(self, lsp_types::request::PrepareRenameRequest, params);
        let state = self.state.lock().unwrap();
        let key = LspKey::from_position(&params.text_document.uri, &params.position);
        Ok(state.prepare_renames.get(&key).cloned())
    }

    async fn rename(&self, params: RenameParams) -> io::Result<Option<WorkspaceEdit>> {
        self.apply_delay("textDocument/rename").await;
        if let Some(err) = self.take_request_failure("textDocument/rename") {
            return Err(err);
        }
        pending_check!(self, lsp_types::request::Rename, params);
        let state = self.state.lock().unwrap();
        let key = LspKey::from_position(
            &params.text_document_position.text_document.uri,
            &params.text_document_position.position,
        );
        Ok(state.renames.get(&key).cloned())
    }

    async fn formatting(
        &self,
        params: DocumentFormattingParams,
    ) -> io::Result<Option<Vec<TextEdit>>> {
        self.apply_delay("textDocument/formatting").await;
        if let Some(err) = self.take_request_failure("textDocument/formatting") {
            return Err(err);
        }
        pending_check!(self, lsp_types::request::Formatting, params);
        Ok(None)
    }

    async fn range_formatting(
        &self,
        params: DocumentRangeFormattingParams,
    ) -> io::Result<Option<Vec<TextEdit>>> {
        self.apply_delay("textDocument/rangeFormatting").await;
        if let Some(err) = self.take_request_failure("textDocument/rangeFormatting") {
            return Err(err);
        }
        pending_check!(self, lsp_types::request::RangeFormatting, params);
        let state = self.state.lock().unwrap();
        Ok(state
            .range_formatting
            .get(&params.text_document.uri)
            .cloned())
    }

    async fn will_rename(&self, params: RenameFilesParams) -> io::Result<Option<WorkspaceEdit>> {
        self.apply_delay("workspace/willRenameFiles").await;
        if let Some(err) = self.take_request_failure("workspace/willRenameFiles") {
            return Err(err);
        }
        pending_check!(self, lsp_types::request::WillRenameFiles, params);
        let Some(first) = params.files.first() else {
            return Ok(None);
        };
        let key = (first.old_uri.clone(), first.new_uri.clone());
        let state = self.state.lock().unwrap();
        Ok(state.will_renames.get(&key).cloned())
    }

    async fn execute_command(&self, params: ExecuteCommandParams) -> io::Result<Option<Value>> {
        self.apply_delay("workspace/executeCommand").await;
        if let Some(err) = self.take_request_failure("workspace/executeCommand") {
            return Err(err);
        }
        pending_check!(self, lsp_types::request::ExecuteCommand, params);
        let mut state = self.state.lock().unwrap();
        state.observed_executed_commands.push(params.clone());
        Ok(state.executed_commands.get(&params.command).cloned())
    }

    async fn recv_notification(&self) -> Option<LspNotification> {
        self.notif_rx.lock().await.recv().await
    }

    async fn try_recv_notification(&self) -> Option<LspNotification> {
        self.notif_rx.try_lock().ok()?.try_recv().ok()
    }

    async fn recv_incoming_request(&self) -> Option<IncomingRequest> {
        self.req_rx.lock().await.recv().await
    }

    async fn try_recv_incoming_request(&self) -> Option<IncomingRequest> {
        self.req_rx.try_lock().ok()?.try_recv().ok()
    }

    async fn reply(
        &self,
        id: NumberOrString,
        result: Result<Value, LspResponseError>,
    ) -> io::Result<()> {
        self.state
            .lock()
            .unwrap()
            .observed_replies
            .push((id, result));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stoat_scheduler::{Clock, TestScheduler};

    fn rt() -> TestScheduler {
        TestScheduler::new()
    }

    #[test]
    fn prepare_rename_programmed_response() {
        rt().block_on(async {
            let lsp = FakeLsp::new();
            lsp.set_prepare_rename(
                "/src/main.rs",
                4,
                7,
                PrepareRenameResponse::RangeWithPlaceholder {
                    range: Range::new(Position::new(4, 4), Position::new(4, 11)),
                    placeholder: "do_thing".to_string(),
                },
            );

            let response = lsp
                .prepare_rename(text_doc_pos("/src/main.rs", 4, 7))
                .await
                .unwrap()
                .expect("programmed response");
            match response {
                PrepareRenameResponse::RangeWithPlaceholder { range, placeholder } => {
                    assert_eq!(range.start, Position::new(4, 4));
                    assert_eq!(range.end, Position::new(4, 11));
                    assert_eq!(placeholder, "do_thing");
                },
                other => panic!("expected RangeWithPlaceholder, got {other:?}"),
            }

            let miss = lsp
                .prepare_rename(text_doc_pos("/src/main.rs", 99, 99))
                .await
                .unwrap();
            assert!(miss.is_none(), "unprogrammed positions return None");
        });
    }

    #[test]
    fn hover_programmed_response() {
        rt().block_on(async {
            let lsp = FakeLsp::new();
            lsp.set_hover("/src/main.rs", 10, 5, "```rust\nfn foo()\n```");

            let result = lsp
                .hover(hover_params("/src/main.rs", 10, 5))
                .await
                .unwrap();
            let hover = result.expect("should have hover");
            match hover.contents {
                HoverContents::Markup(m) => {
                    assert_eq!(m.kind, MarkupKind::Markdown);
                    assert!(m.value.contains("fn foo()"));
                },
                _ => panic!("expected markup"),
            }
        });
    }

    #[test]
    fn hover_no_response() {
        rt().block_on(async {
            let lsp = FakeLsp::new();
            let result = lsp.hover(hover_params("/src/main.rs", 0, 0)).await.unwrap();
            assert!(result.is_none());
        });
    }

    #[test]
    fn diagnostics_on_open() {
        rt().block_on(async {
            let lsp = FakeLsp::new();
            lsp.add_error("/src/main.rs", 3, 0, 10, "expected `;`");
            lsp.add_warning("/src/main.rs", 5, 4, 7, "unused variable");

            lsp.did_open(open_params("/src/main.rs", "let x = 1\n", "rust"))
                .await
                .unwrap();

            let notif = lsp
                .recv_notification()
                .await
                .expect("should have notification");
            match notif {
                LspNotification::Diagnostics { diagnostics, .. } => {
                    assert_eq!(diagnostics.len(), 2);
                    assert_eq!(diagnostics[0].severity, Some(DiagnosticSeverity::ERROR));
                    assert_eq!(diagnostics[0].message, "expected `;`");
                    assert_eq!(diagnostics[1].severity, Some(DiagnosticSeverity::WARNING));
                },
                _ => panic!("expected diagnostics notification"),
            }
        });
    }

    #[test]
    fn completions_programmed() {
        rt().block_on(async {
            let lsp = FakeLsp::new();
            lsp.set_completions("/src/lib.rs", 2, 8, &["println!", "print!", "panic!"]);

            let result = lsp
                .completion(completion_params("/src/lib.rs", 2, 8))
                .await
                .unwrap();
            match result.expect("should have completions") {
                CompletionResponse::List(list) => {
                    let labels: Vec<&str> = list.items.iter().map(|i| i.label.as_str()).collect();
                    assert_eq!(labels, ["println!", "print!", "panic!"]);
                },
                _ => panic!("expected list"),
            }
        });
    }

    #[test]
    fn goto_definition_programmed() {
        rt().block_on(async {
            let lsp = FakeLsp::new();
            lsp.set_definition("/src/main.rs", 10, 5, "/src/lib.rs", 20, 0);

            let result = lsp
                .goto_definition(definition_params("/src/main.rs", 10, 5))
                .await
                .unwrap();
            match result.expect("should have definition") {
                GotoDefinitionResponse::Scalar(loc) => {
                    assert_eq!(loc.uri, file_uri("/src/lib.rs"));
                    assert_eq!(loc.range.start.line, 20);
                },
                _ => panic!("expected scalar"),
            }
        });
    }

    #[test]
    fn goto_declaration_programmed() {
        rt().block_on(async {
            let lsp = FakeLsp::new();
            lsp.set_declaration("/src/main.rs", 5, 4, "/include/foo.h", 10, 0);

            let result = lsp
                .goto_declaration(definition_params("/src/main.rs", 5, 4))
                .await
                .unwrap();
            match result.expect("should have declaration") {
                GotoDefinitionResponse::Scalar(loc) => {
                    assert_eq!(loc.uri, file_uri("/include/foo.h"));
                    assert_eq!(loc.range.start.line, 10);
                },
                _ => panic!("expected scalar"),
            }
        });
    }

    #[test]
    fn goto_type_definition_programmed() {
        rt().block_on(async {
            let lsp = FakeLsp::new();
            lsp.set_type_definition("/src/main.rs", 8, 12, "/src/types.rs", 3, 0);

            let result = lsp
                .goto_type_definition(definition_params("/src/main.rs", 8, 12))
                .await
                .unwrap();
            assert!(result.is_some());
        });
    }

    #[test]
    fn goto_implementation_programmed() {
        rt().block_on(async {
            let lsp = FakeLsp::new();
            lsp.set_implementation("/src/trait.rs", 2, 10, "/src/impl.rs", 15, 0);

            let result = lsp
                .goto_implementation(definition_params("/src/trait.rs", 2, 10))
                .await
                .unwrap();
            assert!(result.is_some());
        });
    }

    #[test]
    fn references_programmed() {
        rt().block_on(async {
            let lsp = FakeLsp::new();
            lsp.set_references(
                "/src/lib.rs",
                5,
                4,
                &[("/src/main.rs", 10, 4), ("/src/lib.rs", 20, 8)],
            );

            let result = lsp
                .references(reference_params("/src/lib.rs", 5, 4))
                .await
                .unwrap();
            let refs = result.expect("should have references");
            assert_eq!(refs.len(), 2);
            assert_eq!(refs[0].range.start.line, 10);
            assert_eq!(refs[1].range.start.line, 20);
        });
    }

    #[test]
    fn document_highlight_programmed() {
        rt().block_on(async {
            let lsp = FakeLsp::new();
            lsp.set_highlights("/src/main.rs", 5, 4, &[(5, 4, 7), (10, 4, 7), (15, 8, 11)]);

            let result = lsp
                .document_highlight(document_highlight_params("/src/main.rs", 5, 4))
                .await
                .unwrap();
            let highlights = result.expect("should have highlights");
            assert_eq!(highlights.len(), 3);
            assert_eq!(highlights[0].range.start, Position::new(5, 4));
            assert_eq!(highlights[1].range.start, Position::new(10, 4));
            assert_eq!(highlights[2].range.start, Position::new(15, 8));
            assert_eq!(highlights[0].kind, Some(DocumentHighlightKind::READ));
        });
    }

    #[test]
    fn inlay_hints_programmed() {
        rt().block_on(async {
            let lsp = FakeLsp::new();
            lsp.add_type_hint("/src/main.rs", 3, 8, ": i32");
            lsp.add_type_hint("/src/main.rs", 5, 12, ": String");
            lsp.add_parameter_hint("/src/main.rs", 10, 15, "count:");

            let result = lsp
                .inlay_hint(inlay_hint_params("/src/main.rs", 0, 20))
                .await
                .unwrap();
            let hints = result.expect("should have hints");
            assert_eq!(hints.len(), 3);
            assert_eq!(hints[0].kind, Some(InlayHintKind::TYPE));
            assert_eq!(hints[2].kind, Some(InlayHintKind::PARAMETER));
            match &hints[0].label {
                InlayHintLabel::String(s) => assert_eq!(s, ": i32"),
                _ => panic!("expected string label"),
            }
        });
    }

    #[test]
    fn range_inlay_hint_programmed_response() {
        rt().block_on(async {
            let lsp = FakeLsp::new();
            let viewport_hint = InlayHint {
                position: Position::new(7, 12),
                label: InlayHintLabel::String(": u32".to_string()),
                kind: Some(InlayHintKind::TYPE),
                text_edits: None,
                tooltip: None,
                padding_left: None,
                padding_right: None,
                data: None,
            };
            lsp.set_range_inlay_hints("/src/main.rs", vec![viewport_hint.clone()]);
            lsp.add_type_hint("/src/main.rs", 3, 8, ": i32");

            let viewport = lsp
                .range_inlay_hint(range_inlay_hint_params("/src/main.rs", 5, 0, 10, 0))
                .await
                .unwrap();
            let hints = viewport.expect("viewport hints programmed");
            assert_eq!(hints.len(), 1);
            assert_eq!(hints[0].position, Position::new(7, 12));
            match &hints[0].label {
                InlayHintLabel::String(s) => assert_eq!(s, ": u32"),
                _ => panic!("expected string label"),
            }

            let unmapped = lsp
                .range_inlay_hint(range_inlay_hint_params("/src/other.rs", 0, 0, 1, 0))
                .await
                .unwrap();
            assert!(unmapped.is_none());

            let full = lsp
                .inlay_hint(inlay_hint_params("/src/main.rs", 0, 20))
                .await
                .unwrap()
                .expect("full-doc hints programmed");
            assert_eq!(full.len(), 1);
            assert_eq!(full[0].position, Position::new(3, 8));
        });
    }

    #[test]
    fn workspace_symbol_programmed() {
        rt().block_on(async {
            let lsp = FakeLsp::new();
            lsp.add_workspace_symbol("Foo", "FooStruct", SymbolKind::STRUCT, "/src/foo.rs", 5, 0);
            lsp.add_workspace_symbol(
                "Foo",
                "FooTrait",
                SymbolKind::INTERFACE,
                "/src/foo.rs",
                20,
                0,
            );

            let result = lsp
                .workspace_symbol(workspace_symbol_params("Foo"))
                .await
                .unwrap();
            match result.expect("should have symbols") {
                WorkspaceSymbolResponse::Flat(symbols) => {
                    assert_eq!(symbols.len(), 2);
                    assert_eq!(symbols[0].name, "FooStruct");
                    assert_eq!(symbols[0].kind, SymbolKind::STRUCT);
                    assert_eq!(symbols[1].name, "FooTrait");
                },
                _ => panic!("expected flat"),
            }
        });
    }

    #[test]
    fn folding_range_programmed_response() {
        rt().block_on(async {
            let lsp = FakeLsp::new();
            let ranges = vec![
                FoldingRange {
                    start_line: 0,
                    start_character: None,
                    end_line: 4,
                    end_character: None,
                    kind: None,
                    collapsed_text: None,
                },
                FoldingRange {
                    start_line: 6,
                    start_character: None,
                    end_line: 12,
                    end_character: None,
                    kind: None,
                    collapsed_text: None,
                },
            ];
            lsp.set_folding_ranges("/src/main.rs", ranges.clone());

            let result = lsp
                .folding_range(folding_range_params("/src/main.rs"))
                .await
                .unwrap()
                .expect("should have ranges");
            assert_eq!(result, ranges);

            let miss = lsp
                .folding_range(folding_range_params("/src/other.rs"))
                .await
                .unwrap();
            assert!(miss.is_none(), "unprogrammed documents return None");
        });
    }

    #[test]
    fn selection_range_programmed_response() {
        rt().block_on(async {
            let lsp = FakeLsp::new();
            let inner = SelectionRange {
                range: Range::new(Position::new(4, 8), Position::new(4, 11)),
                parent: None,
            };
            let outer = SelectionRange {
                range: Range::new(Position::new(4, 4), Position::new(4, 18)),
                parent: Some(Box::new(inner.clone())),
            };
            let other = SelectionRange {
                range: Range::new(Position::new(7, 0), Position::new(9, 1)),
                parent: None,
            };
            lsp.set_selection_range("/src/main.rs", 4, 9, outer.clone());
            lsp.set_selection_range("/src/main.rs", 7, 4, other.clone());

            let result = lsp
                .selection_range(selection_range_params("/src/main.rs", &[(4, 9), (7, 4)]))
                .await
                .unwrap()
                .expect("should have ranges");
            assert_eq!(result, vec![outer, other]);

            let partial = lsp
                .selection_range(selection_range_params("/src/main.rs", &[(4, 9), (99, 99)]))
                .await
                .unwrap();
            assert!(
                partial.is_none(),
                "any unprogrammed position collapses the response"
            );

            let miss = lsp
                .selection_range(selection_range_params("/src/other.rs", &[(0, 0)]))
                .await
                .unwrap();
            assert!(miss.is_none(), "unprogrammed documents return None");
        });
    }

    #[test]
    fn range_formatting_programmed_response() {
        rt().block_on(async {
            let lsp = FakeLsp::new();
            let edits = vec![
                TextEdit {
                    range: Range::new(Position::new(2, 0), Position::new(2, 4)),
                    new_text: "    ".to_string(),
                },
                TextEdit {
                    range: Range::new(Position::new(3, 8), Position::new(3, 8)),
                    new_text: ";".to_string(),
                },
            ];
            lsp.set_range_formatting("/src/main.rs", edits.clone());

            let result = lsp
                .range_formatting(range_formatting_params("/src/main.rs", 2, 0, 4, 0))
                .await
                .unwrap()
                .expect("should have edits");
            assert_eq!(result, edits);

            let miss = lsp
                .range_formatting(range_formatting_params("/src/other.rs", 0, 0, 1, 0))
                .await
                .unwrap();
            assert!(miss.is_none(), "unprogrammed documents return None");
        });
    }

    #[test]
    fn document_diagnostic_programmed_response() {
        use lsp_types::{
            DocumentDiagnosticReport, FullDocumentDiagnosticReport,
            RelatedFullDocumentDiagnosticReport,
        };

        rt().block_on(async {
            let lsp = FakeLsp::new();
            let diag = Diagnostic::new_simple(
                Range::new(Position::new(2, 4), Position::new(2, 8)),
                "unused variable".to_string(),
            );
            let report = DocumentDiagnosticReportResult::Report(DocumentDiagnosticReport::Full(
                RelatedFullDocumentDiagnosticReport {
                    related_documents: None,
                    full_document_diagnostic_report: FullDocumentDiagnosticReport {
                        result_id: Some("rev-1".to_string()),
                        items: vec![diag.clone()],
                    },
                },
            ));
            lsp.set_document_diagnostic("/src/main.rs", report.clone());

            let result = lsp
                .document_diagnostic(document_diagnostic_params("/src/main.rs"))
                .await
                .unwrap()
                .expect("should have report");
            assert_eq!(result, report);

            let miss = lsp
                .document_diagnostic(document_diagnostic_params("/src/other.rs"))
                .await
                .unwrap();
            assert!(miss.is_none(), "unprogrammed documents return None");
        });
    }

    #[test]
    fn document_link_programmed_response() {
        rt().block_on(async {
            let lsp = FakeLsp::new();
            let links = vec![
                DocumentLink {
                    range: Range::new(Position::new(1, 4), Position::new(1, 28)),
                    target: Some(file_uri("/src/lib.rs")),
                    tooltip: Some("Open lib.rs".to_string()),
                    data: None,
                },
                DocumentLink {
                    range: Range::new(Position::new(5, 10), Position::new(5, 32)),
                    target: None,
                    tooltip: None,
                    data: None,
                },
            ];
            lsp.set_document_links("/src/main.rs", links.clone());

            let result = lsp
                .document_link(document_link_params("/src/main.rs"))
                .await
                .unwrap()
                .expect("should have links");
            assert_eq!(result, links);

            let resolved = lsp.document_link_resolve(links[1].clone()).await.unwrap();
            assert_eq!(resolved, links[1], "fake resolve is a passthrough");

            let miss = lsp
                .document_link(document_link_params("/src/other.rs"))
                .await
                .unwrap();
            assert!(miss.is_none(), "unprogrammed documents return None");
        });
    }

    #[test]
    fn document_color_programmed_response() {
        rt().block_on(async {
            let lsp = FakeLsp::new();
            let red = Color {
                red: 1.0,
                green: 0.0,
                blue: 0.0,
                alpha: 1.0,
            };
            let colors = vec![ColorInformation {
                range: Range::new(Position::new(3, 12), Position::new(3, 19)),
                color: red,
            }];
            let presentations = vec![
                ColorPresentation {
                    label: "#ff0000".to_string(),
                    text_edit: None,
                    additional_text_edits: None,
                },
                ColorPresentation {
                    label: "rgb(255, 0, 0)".to_string(),
                    text_edit: None,
                    additional_text_edits: None,
                },
            ];
            lsp.set_document_colors("/src/main.css", colors.clone());
            lsp.set_color_presentations("/src/main.css", presentations.clone());

            let color_result = lsp
                .document_color(document_color_params("/src/main.css"))
                .await
                .unwrap()
                .expect("should have colors");
            assert_eq!(color_result, colors);

            let pres_result = lsp
                .color_presentation(color_presentation_params(
                    "/src/main.css",
                    red,
                    3,
                    12,
                    3,
                    19,
                ))
                .await
                .unwrap()
                .expect("should have presentations");
            assert_eq!(pres_result, presentations);

            let color_miss = lsp
                .document_color(document_color_params("/src/other.css"))
                .await
                .unwrap();
            assert!(color_miss.is_none(), "unprogrammed documents return None");

            let pres_miss = lsp
                .color_presentation(color_presentation_params("/src/other.css", red, 0, 0, 0, 0))
                .await
                .unwrap();
            assert!(pres_miss.is_none(), "unprogrammed documents return None");
        });
    }

    #[test]
    fn semantic_tokens_programmed_response() {
        use lsp_types::{SemanticToken, SemanticTokens};

        rt().block_on(async {
            let lsp = FakeLsp::new();
            let tokens = SemanticTokens {
                result_id: Some("rev-1".to_string()),
                data: vec![
                    SemanticToken {
                        delta_line: 0,
                        delta_start: 0,
                        length: 3,
                        token_type: 1,
                        token_modifiers_bitset: 0,
                    },
                    SemanticToken {
                        delta_line: 0,
                        delta_start: 4,
                        length: 5,
                        token_type: 2,
                        token_modifiers_bitset: 0,
                    },
                ],
            };
            let full = SemanticTokensResult::Tokens(tokens.clone());
            let range = SemanticTokensRangeResult::Tokens(tokens.clone());
            lsp.set_semantic_tokens_full("/src/main.rs", full.clone());
            lsp.set_semantic_tokens_range("/src/main.rs", range.clone());

            let full_result = lsp
                .semantic_tokens_full(semantic_tokens_params("/src/main.rs"))
                .await
                .unwrap()
                .expect("should have tokens");
            assert_eq!(full_result, full);

            let range_result = lsp
                .semantic_tokens_range(semantic_tokens_range_params("/src/main.rs", 0, 0, 10, 0))
                .await
                .unwrap()
                .expect("should have range tokens");
            assert_eq!(range_result, range);

            let full_miss = lsp
                .semantic_tokens_full(semantic_tokens_params("/src/other.rs"))
                .await
                .unwrap();
            assert!(full_miss.is_none(), "unprogrammed documents return None");

            let range_miss = lsp
                .semantic_tokens_range(semantic_tokens_range_params("/src/other.rs", 0, 0, 0, 0))
                .await
                .unwrap();
            assert!(range_miss.is_none(), "unprogrammed documents return None");
        });
    }

    #[test]
    fn workspace_symbol_no_match() {
        rt().block_on(async {
            let lsp = FakeLsp::new();
            let result = lsp
                .workspace_symbol(workspace_symbol_params("Nonexistent"))
                .await
                .unwrap();
            assert!(result.is_none());
        });
    }

    #[test]
    fn document_symbol_programmed_response() {
        use lsp_types::DocumentSymbol;

        rt().block_on(async {
            let lsp = FakeLsp::new();
            #[allow(deprecated)]
            let inner = DocumentSymbol {
                name: "helper".to_string(),
                detail: Some("fn() -> ()".to_string()),
                kind: SymbolKind::FUNCTION,
                tags: None,
                deprecated: None,
                range: Range::new(Position::new(2, 4), Position::new(4, 5)),
                selection_range: Range::new(Position::new(2, 7), Position::new(2, 13)),
                children: None,
            };
            #[allow(deprecated)]
            let outer = DocumentSymbol {
                name: "Foo".to_string(),
                detail: None,
                kind: SymbolKind::STRUCT,
                tags: None,
                deprecated: None,
                range: Range::new(Position::new(0, 0), Position::new(10, 1)),
                selection_range: Range::new(Position::new(0, 7), Position::new(0, 10)),
                children: Some(vec![inner.clone()]),
            };
            let nested = DocumentSymbolResponse::Nested(vec![outer.clone()]);
            lsp.set_document_symbols("/src/lib.rs", nested.clone());

            let nested_result = lsp
                .document_symbol(document_symbol_params("/src/lib.rs"))
                .await
                .unwrap()
                .expect("should have nested symbols");
            assert_eq!(nested_result, nested);

            #[allow(deprecated)]
            let flat = DocumentSymbolResponse::Flat(vec![SymbolInformation {
                name: "do_thing".to_string(),
                kind: SymbolKind::FUNCTION,
                tags: None,
                deprecated: None,
                location: Location::new(
                    file_uri("/src/main.rs"),
                    Range::new(Position::new(3, 0), Position::new(5, 1)),
                ),
                container_name: None,
            }]);
            lsp.set_document_symbols("/src/main.rs", flat.clone());

            let flat_result = lsp
                .document_symbol(document_symbol_params("/src/main.rs"))
                .await
                .unwrap()
                .expect("should have flat symbols");
            assert_eq!(flat_result, flat);

            let miss = lsp
                .document_symbol(document_symbol_params("/src/other.rs"))
                .await
                .unwrap();
            assert!(miss.is_none(), "unprogrammed documents return None");
        });
    }

    #[test]
    fn signature_help_programmed_response() {
        use lsp_types::{ParameterInformation, ParameterLabel, SignatureInformation};

        rt().block_on(async {
            let lsp = FakeLsp::new();
            let help = SignatureHelp {
                signatures: vec![SignatureInformation {
                    label: "fn add(x: i32, y: i32) -> i32".to_string(),
                    documentation: None,
                    parameters: Some(vec![
                        ParameterInformation {
                            label: ParameterLabel::Simple("x: i32".to_string()),
                            documentation: None,
                        },
                        ParameterInformation {
                            label: ParameterLabel::Simple("y: i32".to_string()),
                            documentation: None,
                        },
                    ]),
                    active_parameter: Some(1),
                }],
                active_signature: Some(0),
                active_parameter: Some(1),
            };
            lsp.set_signature_help("/src/main.rs", 12, 18, help.clone());

            let hit = lsp
                .signature_help(signature_help_params("/src/main.rs", 12, 18))
                .await
                .unwrap()
                .expect("programmed response");
            assert_eq!(hit, help);

            let position_miss = lsp
                .signature_help(signature_help_params("/src/main.rs", 12, 0))
                .await
                .unwrap();
            assert!(
                position_miss.is_none(),
                "unprogrammed position returns None"
            );

            let document_miss = lsp
                .signature_help(signature_help_params("/src/other.rs", 12, 18))
                .await
                .unwrap();
            assert!(
                document_miss.is_none(),
                "unprogrammed documents return None"
            );
        });
    }

    #[test]
    fn code_action_programmed_response() {
        use lsp_types::{CodeActionKind, Command};

        rt().block_on(async {
            let lsp = FakeLsp::new();
            let action = CodeActionOrCommand::CodeAction(CodeAction {
                title: "Replace with Vec::new()".to_string(),
                kind: Some(CodeActionKind::QUICKFIX),
                diagnostics: None,
                edit: None,
                command: None,
                is_preferred: Some(true),
                disabled: None,
                data: None,
            });
            let command = CodeActionOrCommand::Command(Command {
                title: "Restart rust-analyzer".to_string(),
                command: "rust-analyzer.restart".to_string(),
                arguments: None,
            });
            let actions = vec![action, command];
            lsp.set_code_actions("/src/main.rs", actions.clone());

            let hit = lsp
                .code_action(code_action_params("/src/main.rs", 5, 0, 5, 12))
                .await
                .unwrap()
                .expect("programmed response");
            assert_eq!(hit, actions);

            let miss = lsp
                .code_action(code_action_params("/src/other.rs", 0, 0, 0, 0))
                .await
                .unwrap();
            assert!(miss.is_none(), "unprogrammed documents return None");
        });
    }

    #[test]
    fn call_hierarchy_programmed_response() {
        rt().block_on(async {
            let lsp = FakeLsp::new();
            let target = call_hierarchy_item("/src/lib.rs", "do_thing", SymbolKind::FUNCTION, 5, 4);
            let caller = call_hierarchy_item("/src/main.rs", "run", SymbolKind::FUNCTION, 12, 0);
            let callee =
                call_hierarchy_item("/src/util.rs", "log_event", SymbolKind::FUNCTION, 30, 0);

            lsp.set_prepare_call_hierarchy("/src/lib.rs", 5, 4, vec![target.clone()]);
            lsp.set_call_hierarchy_incoming_calls(
                "/src/lib.rs",
                5,
                4,
                vec![CallHierarchyIncomingCall {
                    from: caller.clone(),
                    from_ranges: vec![Range::new(Position::new(13, 4), Position::new(13, 12))],
                }],
            );
            lsp.set_call_hierarchy_outgoing_calls(
                "/src/lib.rs",
                5,
                4,
                vec![CallHierarchyOutgoingCall {
                    to: callee.clone(),
                    from_ranges: vec![Range::new(Position::new(7, 8), Position::new(7, 17))],
                }],
            );

            let prepared = lsp
                .prepare_call_hierarchy(call_hierarchy_prepare_params("/src/lib.rs", 5, 4))
                .await
                .unwrap()
                .expect("should have items");
            assert_eq!(prepared, vec![target.clone()]);

            let incoming = lsp
                .call_hierarchy_incoming_calls(CallHierarchyIncomingCallsParams {
                    item: target.clone(),
                    work_done_progress_params: WorkDoneProgressParams::default(),
                    partial_result_params: PartialResultParams::default(),
                })
                .await
                .unwrap()
                .expect("should have incoming calls");
            assert_eq!(incoming.len(), 1);
            assert_eq!(incoming[0].from, caller);

            let outgoing = lsp
                .call_hierarchy_outgoing_calls(CallHierarchyOutgoingCallsParams {
                    item: target.clone(),
                    work_done_progress_params: WorkDoneProgressParams::default(),
                    partial_result_params: PartialResultParams::default(),
                })
                .await
                .unwrap()
                .expect("should have outgoing calls");
            assert_eq!(outgoing.len(), 1);
            assert_eq!(outgoing[0].to, callee);

            let prep_miss = lsp
                .prepare_call_hierarchy(call_hierarchy_prepare_params("/src/lib.rs", 99, 99))
                .await
                .unwrap();
            assert!(prep_miss.is_none(), "unprogrammed positions return None");

            let other_item =
                call_hierarchy_item("/src/lib.rs", "other", SymbolKind::FUNCTION, 42, 0);
            let in_miss = lsp
                .call_hierarchy_incoming_calls(CallHierarchyIncomingCallsParams {
                    item: other_item.clone(),
                    work_done_progress_params: WorkDoneProgressParams::default(),
                    partial_result_params: PartialResultParams::default(),
                })
                .await
                .unwrap();
            assert!(in_miss.is_none(), "unprogrammed items return None");
        });
    }

    #[test]
    fn type_hierarchy_programmed_response() {
        rt().block_on(async {
            let lsp = FakeLsp::new();
            let target = type_hierarchy_item("/src/shape.rs", "Square", SymbolKind::CLASS, 10, 6);
            let parent = type_hierarchy_item("/src/shape.rs", "Polygon", SymbolKind::CLASS, 4, 6);
            let child =
                type_hierarchy_item("/src/shape.rs", "FilledSquare", SymbolKind::CLASS, 30, 6);

            lsp.set_prepare_type_hierarchy("/src/shape.rs", 10, 6, vec![target.clone()]);
            lsp.set_type_hierarchy_supertypes("/src/shape.rs", 10, 6, vec![parent.clone()]);
            lsp.set_type_hierarchy_subtypes("/src/shape.rs", 10, 6, vec![child.clone()]);

            let prepared = lsp
                .prepare_type_hierarchy(type_hierarchy_prepare_params("/src/shape.rs", 10, 6))
                .await
                .unwrap()
                .expect("should have items");
            assert_eq!(prepared, vec![target.clone()]);

            let supertypes = lsp
                .type_hierarchy_supertypes(TypeHierarchySupertypesParams {
                    item: target.clone(),
                    work_done_progress_params: WorkDoneProgressParams::default(),
                    partial_result_params: PartialResultParams::default(),
                })
                .await
                .unwrap()
                .expect("should have supertypes");
            assert_eq!(supertypes, vec![parent]);

            let subtypes = lsp
                .type_hierarchy_subtypes(TypeHierarchySubtypesParams {
                    item: target.clone(),
                    work_done_progress_params: WorkDoneProgressParams::default(),
                    partial_result_params: PartialResultParams::default(),
                })
                .await
                .unwrap()
                .expect("should have subtypes");
            assert_eq!(subtypes, vec![child]);

            let prep_miss = lsp
                .prepare_type_hierarchy(type_hierarchy_prepare_params("/src/shape.rs", 99, 99))
                .await
                .unwrap();
            assert!(prep_miss.is_none(), "unprogrammed positions return None");

            let other_item =
                type_hierarchy_item("/src/shape.rs", "Other", SymbolKind::CLASS, 42, 0);
            let super_miss = lsp
                .type_hierarchy_supertypes(TypeHierarchySupertypesParams {
                    item: other_item.clone(),
                    work_done_progress_params: WorkDoneProgressParams::default(),
                    partial_result_params: PartialResultParams::default(),
                })
                .await
                .unwrap();
            assert!(super_miss.is_none(), "unprogrammed items return None");
        });
    }

    #[test]
    fn rename_file_ops_programmed_response() {
        rt().block_on(async {
            let lsp = FakeLsp::new();
            let edit = WorkspaceEdit {
                changes: Some(
                    BTreeMap::from([(file_uri("/src/old.rs"), vec![])])
                        .into_iter()
                        .collect(),
                ),
                ..Default::default()
            };
            lsp.set_will_rename("/src/old.rs", "/src/new.rs", edit.clone());

            let returned = lsp
                .will_rename(rename_files_params(&[("/src/old.rs", "/src/new.rs")]))
                .await
                .unwrap()
                .expect("should have programmed edit");
            assert_eq!(returned, edit);

            let miss = lsp
                .will_rename(rename_files_params(&[("/src/other.rs", "/src/new.rs")]))
                .await
                .unwrap();
            assert!(miss.is_none(), "unprogrammed pair returns None");

            let empty = lsp
                .will_rename(RenameFilesParams { files: vec![] })
                .await
                .unwrap();
            assert!(empty.is_none(), "empty files vec returns None");

            assert!(lsp.observed_renames().is_empty());

            let first = rename_files_params(&[("/src/old.rs", "/src/new.rs")]);
            let second = rename_files_params(&[
                ("/src/foo.rs", "/src/foo2.rs"),
                ("/src/bar.rs", "/src/bar2.rs"),
            ]);
            lsp.did_rename(first.clone()).await.unwrap();
            lsp.did_rename(second.clone()).await.unwrap();

            assert_eq!(lsp.observed_renames(), vec![first, second]);
        });
    }

    #[test]
    fn execute_command_programmed_response() {
        rt().block_on(async {
            let lsp = FakeLsp::new();
            let response = serde_json::json!({"applied": true});
            lsp.set_execute_command("rust-analyzer.applyImport", response.clone());

            let returned = lsp
                .execute_command(execute_command_params(
                    "rust-analyzer.applyImport",
                    vec![serde_json::json!("foo::Bar")],
                ))
                .await
                .unwrap()
                .expect("should have programmed response");
            assert_eq!(returned, response);

            let miss = lsp
                .execute_command(execute_command_params("rust-analyzer.unknown", vec![]))
                .await
                .unwrap();
            assert!(miss.is_none(), "unprogrammed command returns None");

            lsp.fail_next_request("workspace/executeCommand", io::ErrorKind::Other);
            let err = lsp
                .execute_command(execute_command_params("rust-analyzer.applyImport", vec![]))
                .await
                .expect_err("primed failure should propagate");
            assert_eq!(err.kind(), io::ErrorKind::Other);

            let after = lsp
                .execute_command(execute_command_params("rust-analyzer.applyImport", vec![]))
                .await
                .unwrap()
                .expect("one-shot failure clears after first call");
            assert_eq!(after, response);
        });
    }

    #[test]
    fn workspace_state_notifications_recorded() {
        rt().block_on(async {
            let lsp = FakeLsp::new();

            let watched = DidChangeWatchedFilesParams {
                changes: vec![FileEvent::new(
                    file_uri("/src/main.rs"),
                    FileChangeType::CHANGED,
                )],
            };
            lsp.did_change_watched_files(watched.clone()).await.unwrap();

            let configuration = DidChangeConfigurationParams {
                settings: serde_json::json!({"rust-analyzer": {"checkOnSave": true}}),
            };
            lsp.did_change_configuration(configuration.clone())
                .await
                .unwrap();

            let folders = DidChangeWorkspaceFoldersParams {
                event: WorkspaceFoldersChangeEvent {
                    added: vec![WorkspaceFolder {
                        uri: file_uri("/workspace/added"),
                        name: "added".to_string(),
                    }],
                    removed: vec![WorkspaceFolder {
                        uri: file_uri("/workspace/removed"),
                        name: "removed".to_string(),
                    }],
                },
            };
            lsp.did_change_workspace_folders(folders.clone())
                .await
                .unwrap();

            assert_eq!(lsp.observed_watched_file_changes(), vec![watched]);
            assert_eq!(lsp.observed_configuration_changes(), vec![configuration]);
            assert_eq!(lsp.observed_workspace_folder_changes(), vec![folders]);
        });
    }

    #[test]
    fn incoming_request_round_trip() {
        rt().block_on(async {
            let lsp = FakeLsp::new();
            assert!(lsp.try_recv_incoming_request().await.is_none());

            let apply_edit = ApplyWorkspaceEditParams {
                label: Some("Rename foo".to_string()),
                edit: WorkspaceEdit::default(),
            };
            lsp.push_incoming_request(IncomingRequest::WorkspaceApplyEdit {
                id: NumberOrString::Number(7),
                params: apply_edit.clone(),
            });

            let configuration = ConfigurationParams {
                items: vec![ConfigurationItem {
                    scope_uri: None,
                    section: Some("rust-analyzer".to_string()),
                }],
            };
            lsp.push_incoming_request(IncomingRequest::WorkspaceConfiguration {
                id: NumberOrString::Number(8),
                params: configuration.clone(),
            });

            lsp.push_incoming_request(incoming_request(
                "experimental/customMethod",
                9,
                serde_json::json!({"hint": "future"}),
            ));

            match lsp.recv_incoming_request().await.expect("apply edit") {
                IncomingRequest::WorkspaceApplyEdit { id, params } => {
                    assert_eq!(id, NumberOrString::Number(7));
                    assert_eq!(params, apply_edit);
                },
                other => panic!("expected WorkspaceApplyEdit, got {other:?}"),
            }

            match lsp.recv_incoming_request().await.expect("configuration") {
                IncomingRequest::WorkspaceConfiguration { id, params } => {
                    assert_eq!(id, NumberOrString::Number(8));
                    assert_eq!(params, configuration);
                },
                other => panic!("expected WorkspaceConfiguration, got {other:?}"),
            }

            match lsp.recv_incoming_request().await.expect("unknown") {
                IncomingRequest::Unknown { id, method, params } => {
                    assert_eq!(id, NumberOrString::Number(9));
                    assert_eq!(method, "experimental/customMethod");
                    assert_eq!(params, serde_json::json!({"hint": "future"}));
                },
                other => panic!("expected Unknown, got {other:?}"),
            }

            lsp.reply(
                NumberOrString::Number(7),
                Ok(serde_json::json!({"applied": true})),
            )
            .await
            .unwrap();
            let server_error = LspResponseError {
                code: -32603,
                message: "internal error".to_string(),
                data: None,
            };
            lsp.reply(NumberOrString::Number(8), Err(server_error.clone()))
                .await
                .unwrap();
            lsp.reply(NumberOrString::Number(9), Ok(Value::Null))
                .await
                .unwrap();

            let replies = lsp.observed_replies();
            assert_eq!(replies.len(), 3);
            assert_eq!(
                replies[0],
                (
                    NumberOrString::Number(7),
                    Ok(serde_json::json!({"applied": true})),
                )
            );
            assert_eq!(replies[1], (NumberOrString::Number(8), Err(server_error)));
            assert_eq!(replies[2], (NumberOrString::Number(9), Ok(Value::Null)));
        });
    }

    #[test]
    fn incoming_request_id_accessor() {
        let apply_edit = IncomingRequest::WorkspaceApplyEdit {
            id: NumberOrString::Number(42),
            params: ApplyWorkspaceEditParams {
                label: None,
                edit: WorkspaceEdit::default(),
            },
        };
        assert_eq!(apply_edit.id(), &NumberOrString::Number(42));

        let unknown = incoming_request("custom/method", 11, Value::Null);
        assert_eq!(unknown.id(), &NumberOrString::Number(11));
    }

    #[test]
    fn open_close_tracking() {
        rt().block_on(async {
            let lsp = FakeLsp::new();
            assert!(!lsp.is_open("/src/main.rs"));

            lsp.did_open(open_params("/src/main.rs", "fn main() {}", "rust"))
                .await
                .unwrap();
            assert!(lsp.is_open("/src/main.rs"));
            assert_eq!(lsp.opened_documents().len(), 1);

            lsp.did_close(DidCloseTextDocumentParams {
                text_document: text_doc_id("/src/main.rs"),
            })
            .await
            .unwrap();
            assert!(!lsp.is_open("/src/main.rs"));
        });
    }

    #[test]
    fn diagnostics_on_change() {
        rt().block_on(async {
            let lsp = FakeLsp::new();
            lsp.add_error("/src/main.rs", 0, 0, 5, "type error");

            lsp.did_open(open_params("/src/main.rs", "", "rust"))
                .await
                .unwrap();
            let _ = lsp.recv_notification().await;

            lsp.did_change(change_params("/src/main.rs", 1, "let x = 1;"))
                .await
                .unwrap();
            let notif = lsp
                .recv_notification()
                .await
                .expect("should re-emit diagnostics");
            assert!(matches!(notif, LspNotification::Diagnostics { .. }));
        });
    }

    #[test]
    fn no_diagnostics_when_none_programmed() {
        rt().block_on(async {
            let lsp = FakeLsp::new();
            lsp.did_open(open_params("/src/main.rs", "", "rust"))
                .await
                .unwrap();
            assert!(lsp.try_recv_notification().await.is_none());
        });
    }

    #[test]
    fn initialize_and_shutdown() {
        rt().block_on(async {
            let lsp = FakeLsp::new();
            let result = lsp.initialize(None).await.unwrap();
            assert!(result.server_info.is_none());
            lsp.shutdown().await.unwrap();
        });
    }

    #[test]
    fn resolve_passthrough() {
        rt().block_on(async {
            let lsp = FakeLsp::new();

            let item = CompletionItem {
                label: "test".to_string(),
                ..CompletionItem::default()
            };
            let resolved = lsp.completion_resolve(item.clone()).await.unwrap();
            assert_eq!(resolved.label, "test");

            let hint = InlayHint {
                position: Position::new(0, 0),
                label: InlayHintLabel::String("hint".to_string()),
                kind: None,
                text_edits: None,
                tooltip: None,
                padding_left: None,
                padding_right: None,
                data: None,
            };
            let resolved = lsp.inlay_hint_resolve(hint).await.unwrap();
            match resolved.label {
                InlayHintLabel::String(s) => assert_eq!(s, "hint"),
                _ => panic!("expected string"),
            }
        });
    }

    #[test]
    fn push_notification_round_trips_diagnostics() {
        rt().block_on(async {
            let lsp = FakeLsp::new();
            let diag = Diagnostic::new_simple(
                Range::new(Position::new(0, 0), Position::new(0, 4)),
                "lint".to_string(),
            );
            lsp.push_notification(LspNotification::Diagnostics {
                uri: file_uri("/src/main.rs"),
                diagnostics: vec![diag.clone()],
                version: Some(7),
            });

            let notif = lsp
                .recv_notification()
                .await
                .expect("pushed notification should be receivable");
            match notif {
                LspNotification::Diagnostics {
                    uri,
                    diagnostics,
                    version,
                } => {
                    assert_eq!(uri, file_uri("/src/main.rs"));
                    assert_eq!(diagnostics, vec![diag]);
                    assert_eq!(version, Some(7));
                },
                _ => panic!("expected diagnostics notification"),
            }
            assert!(lsp.try_recv_notification().await.is_none());
        });
    }

    #[test]
    fn push_notification_round_trips_progress() {
        rt().block_on(async {
            let lsp = FakeLsp::new();
            let begin = WorkDoneProgress::Begin(WorkDoneProgressBegin {
                title: "Indexing".to_string(),
                cancellable: Some(false),
                message: Some("3/25 files".to_string()),
                percentage: Some(12),
            });
            lsp.push_notification(LspNotification::Progress {
                token: NumberOrString::String("idx-1".to_string()),
                value: begin,
            });

            let notif = lsp
                .recv_notification()
                .await
                .expect("pushed notification should be receivable");
            match notif {
                LspNotification::Progress { token, value } => {
                    assert_eq!(token, NumberOrString::String("idx-1".to_string()));
                    let WorkDoneProgress::Begin(begin) = value else {
                        panic!("expected Begin frame")
                    };
                    assert_eq!(begin.title, "Indexing");
                    assert_eq!(begin.percentage, Some(12));
                },
                _ => panic!("expected progress notification"),
            }
        });
    }

    #[test]
    fn push_notification_round_trips_log_message() {
        rt().block_on(async {
            let lsp = FakeLsp::new();
            lsp.push_notification(LspNotification::LogMessage {
                typ: MessageType::WARNING,
                message: "deprecated API in use".to_string(),
            });

            let notif = lsp
                .recv_notification()
                .await
                .expect("pushed notification should be receivable");
            match notif {
                LspNotification::LogMessage { typ, message } => {
                    assert_eq!(typ, MessageType::WARNING);
                    assert_eq!(message, "deprecated API in use");
                },
                other => panic!("expected LogMessage, got {other:?}"),
            }
        });
    }

    #[test]
    fn push_notification_round_trips_show_message() {
        rt().block_on(async {
            let lsp = FakeLsp::new();
            lsp.push_notification(LspNotification::ShowMessage {
                typ: MessageType::ERROR,
                message: "rust-analyzer crashed".to_string(),
            });

            let notif = lsp
                .recv_notification()
                .await
                .expect("pushed notification should be receivable");
            match notif {
                LspNotification::ShowMessage { typ, message } => {
                    assert_eq!(typ, MessageType::ERROR);
                    assert_eq!(message, "rust-analyzer crashed");
                },
                other => panic!("expected ShowMessage, got {other:?}"),
            }
        });
    }

    #[test]
    fn push_notification_round_trips_log_trace() {
        rt().block_on(async {
            let lsp = FakeLsp::new();
            lsp.push_notification(LspNotification::LogTrace {
                message: "Sending textDocument/completion".to_string(),
                verbose: Some("position: 12:4".to_string()),
            });

            let notif = lsp
                .recv_notification()
                .await
                .expect("pushed notification should be receivable");
            match notif {
                LspNotification::LogTrace { message, verbose } => {
                    assert_eq!(message, "Sending textDocument/completion");
                    assert_eq!(verbose, Some("position: 12:4".to_string()));
                },
                other => panic!("expected LogTrace, got {other:?}"),
            }
        });
    }

    #[test]
    fn push_notification_preserves_fifo_order() {
        rt().block_on(async {
            let lsp = FakeLsp::new();
            let make = |path: &str| LspNotification::Diagnostics {
                uri: file_uri(path),
                diagnostics: Vec::new(),
                version: None,
            };
            lsp.push_notification(make("/a.rs"));
            lsp.push_notification(make("/b.rs"));
            lsp.push_notification(make("/c.rs"));

            let mut received = Vec::new();
            while let Some(notif) = lsp.try_recv_notification().await {
                let LspNotification::Diagnostics { uri, .. } = notif else {
                    panic!("expected diagnostics")
                };
                received.push(uri);
            }
            assert_eq!(
                received,
                vec![file_uri("/a.rs"), file_uri("/b.rs"), file_uri("/c.rs"),]
            );
        });
    }

    #[test]
    fn recv_notification_wakes_on_push() {
        let scheduler = Arc::new(TestScheduler::new());
        let executor = scheduler.executor();
        scheduler.block_on(async {
            let lsp = Arc::new(FakeLsp::new());
            let recv_lsp = lsp.clone();
            let task = executor.spawn(async move { recv_lsp.recv_notification().await });

            lsp.push_notification(LspNotification::Diagnostics {
                uri: file_uri("/late.rs"),
                diagnostics: Vec::new(),
                version: None,
            });

            let notif = task.await.expect("recv returned None");
            let LspNotification::Diagnostics { uri, .. } = notif else {
                panic!("expected diagnostics notification")
            };
            assert_eq!(uri, file_uri("/late.rs"));
        });
    }

    #[test]
    fn fail_next_request_fires_once() {
        rt().block_on(async {
            let lsp = FakeLsp::new();
            lsp.fail_next_request("textDocument/hover", io::ErrorKind::Unsupported);

            let err = lsp
                .hover(hover_params("/src/main.rs", 0, 0))
                .await
                .expect_err("first call should fail");
            assert_eq!(err.kind(), io::ErrorKind::Unsupported);
            assert!(err.to_string().contains("textDocument/hover"));

            let ok = lsp
                .hover(hover_params("/src/main.rs", 0, 0))
                .await
                .expect("second call should succeed");
            assert!(ok.is_none());
        });
    }

    #[test]
    fn set_method_error_is_sticky_until_cleared() {
        rt().block_on(async {
            let lsp = FakeLsp::new();
            lsp.set_method_error("textDocument/completion", io::ErrorKind::Other);

            for _ in 0..3 {
                let err = lsp
                    .completion(completion_params("/src/main.rs", 0, 0))
                    .await
                    .expect_err("should fail while sticky error is armed");
                assert_eq!(err.kind(), io::ErrorKind::Other);
            }

            lsp.clear_method_error("textDocument/completion");
            let ok = lsp
                .completion(completion_params("/src/main.rs", 0, 0))
                .await
                .expect("should succeed after clear");
            assert!(ok.is_none());
        });
    }

    #[test]
    fn fail_next_request_isolates_methods() {
        rt().block_on(async {
            let lsp = FakeLsp::new();
            lsp.fail_next_request("textDocument/hover", io::ErrorKind::Unsupported);

            let goto = lsp
                .goto_definition(definition_params("/src/main.rs", 0, 0))
                .await
                .expect("non-armed method should succeed");
            assert!(goto.is_none());

            let err = lsp
                .hover(hover_params("/src/main.rs", 0, 0))
                .await
                .expect_err("armed method should still fail");
            assert_eq!(err.kind(), io::ErrorKind::Unsupported);
        });
    }

    #[test]
    fn clear_method_error_no_op_when_unset() {
        let lsp = FakeLsp::new();
        lsp.clear_method_error("textDocument/hover");
    }

    #[test]
    fn request_delay_blocks_until_clock_advances() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let scheduler = Arc::new(TestScheduler::new());
        let executor = scheduler.executor();
        let lsp = Arc::new(FakeLsp::new());
        lsp.set_executor(executor.clone());
        lsp.set_request_delay("textDocument/hover", Duration::from_millis(800));

        let done = Arc::new(AtomicBool::new(false));
        executor
            .spawn({
                let lsp = lsp.clone();
                let done = done.clone();
                async move {
                    let _ = lsp.hover(hover_params("/src/main.rs", 0, 0)).await;
                    done.store(true, Ordering::SeqCst);
                }
            })
            .detach();

        scheduler.advance_clock(Duration::from_millis(700));
        assert!(
            !done.load(Ordering::SeqCst),
            "request must not complete before the configured delay elapses"
        );

        scheduler.advance_clock(Duration::from_millis(200));
        assert!(
            done.load(Ordering::SeqCst),
            "request must complete once the configured delay elapses"
        );
    }

    #[test]
    fn request_delay_per_method_isolated() {
        let scheduler = Arc::new(TestScheduler::new());
        let executor = scheduler.executor();
        let lsp = FakeLsp::new();
        lsp.set_executor(executor);
        lsp.set_request_delay("textDocument/hover", Duration::from_millis(800));

        let start = scheduler.test_clock().now();
        scheduler.block_on(async {
            let _ = lsp
                .completion(completion_params("/src/main.rs", 0, 0))
                .await
                .unwrap();
        });
        assert_eq!(
            scheduler.test_clock().now() - start,
            Duration::ZERO,
            "completion has no delay armed and must resolve immediately"
        );
    }

    #[test]
    fn clear_request_delay_removes_delay() {
        let scheduler = Arc::new(TestScheduler::new());
        let executor = scheduler.executor();
        let lsp = FakeLsp::new();
        lsp.set_executor(executor);
        lsp.set_request_delay("textDocument/hover", Duration::from_millis(800));
        lsp.clear_request_delay("textDocument/hover");

        let start = scheduler.test_clock().now();
        scheduler.block_on(async {
            let _ = lsp.hover(hover_params("/src/main.rs", 0, 0)).await.unwrap();
        });
        assert_eq!(
            scheduler.test_clock().now() - start,
            Duration::ZERO,
            "cleared delay must not block the request"
        );
    }

    #[test]
    fn request_delay_without_executor_resolves_immediately() {
        let scheduler = Arc::new(TestScheduler::new());
        let lsp = FakeLsp::new();
        lsp.set_request_delay("textDocument/hover", Duration::from_millis(800));

        let start = scheduler.test_clock().now();
        scheduler.block_on(async {
            let _ = lsp.hover(hover_params("/src/main.rs", 0, 0)).await.unwrap();
        });
        assert_eq!(
            scheduler.test_clock().now() - start,
            Duration::ZERO,
            "without an installed executor the delay is recorded but not awaited"
        );
    }

    #[test]
    fn dropped_during_delay_records_cancellation() {
        let scheduler = Arc::new(TestScheduler::new());
        let executor = scheduler.executor();
        let lsp = Arc::new(FakeLsp::new());
        lsp.set_executor(executor.clone());
        lsp.set_request_delay("textDocument/hover", Duration::from_millis(500));

        let task = executor.spawn({
            let lsp = lsp.clone();
            async move {
                let _ = lsp.hover(hover_params("/src/main.rs", 0, 0)).await;
            }
        });
        scheduler.run_until_parked();
        assert!(
            lsp.cancelled_requests().is_empty(),
            "no cancellation before the future is dropped"
        );

        drop(task);
        scheduler.run_until_parked();

        assert_eq!(
            lsp.cancelled_requests(),
            vec!["textDocument/hover".to_string()],
            "dropping the spawned task during the delay window records cancellation"
        );
    }

    #[test]
    fn completed_delay_does_not_record_cancellation() {
        let scheduler = Arc::new(TestScheduler::new());
        let executor = scheduler.executor();
        let lsp = Arc::new(FakeLsp::new());
        lsp.set_executor(executor.clone());
        lsp.set_request_delay("textDocument/hover", Duration::from_millis(100));

        executor
            .spawn({
                let lsp = lsp.clone();
                async move {
                    let _ = lsp.hover(hover_params("/src/main.rs", 0, 0)).await;
                }
            })
            .detach();
        scheduler.advance_clock(Duration::from_millis(150));
        scheduler.run_until_parked();

        assert!(
            lsp.cancelled_requests().is_empty(),
            "completed requests must not register as cancelled"
        );
    }

    #[test]
    fn pending_mode_holds_request_until_test_drives_response() {
        use lsp_types::{request::HoverRequest, MarkedString};

        let scheduler = Arc::new(TestScheduler::new());
        let executor = scheduler.executor();
        let lsp = Arc::new(FakeLsp::new());
        lsp.set_executor(executor.clone());
        lsp.set_pending_mode::<HoverRequest>(true);

        let result = Arc::new(Mutex::new(None));
        let task = executor.spawn({
            let lsp = lsp.clone();
            let result = result.clone();
            async move {
                let r = lsp.hover(hover_params("/src/main.rs", 0, 0)).await;
                *result.lock().unwrap() = Some(r);
            }
        });
        scheduler.run_until_parked();
        assert_eq!(
            lsp.pending_count("textDocument/hover"),
            1,
            "pending mode must enqueue the request rather than resolve it"
        );
        assert!(
            result.lock().unwrap().is_none(),
            "spawned future must still be parked on the oneshot receiver"
        );

        let (_params, sender) = lsp.take_pending::<HoverRequest>().expect("queued entry");
        assert_eq!(lsp.pending_count("textDocument/hover"), 0);
        let response = Some(Hover {
            contents: HoverContents::Scalar(MarkedString::String("driven".into())),
            range: None,
        });
        sender.send(response.clone()).expect("receiver still alive");
        scheduler.run_until_parked();
        task.detach();

        let observed = result.lock().unwrap().take().expect("future completed");
        assert_eq!(observed.expect("hover ok"), response);
    }

    #[test]
    fn pending_mode_disabled_falls_through_to_programmed_response() {
        use lsp_types::request::HoverRequest;

        let scheduler = Arc::new(TestScheduler::new());
        let lsp = FakeLsp::new();
        lsp.set_hover("/src/main.rs", 0, 0, "programmed");
        lsp.set_pending_mode::<HoverRequest>(false);

        let observed = scheduler
            .block_on(async { lsp.hover(hover_params("/src/main.rs", 0, 0)).await })
            .expect("hover ok")
            .expect("programmed hover present");

        match observed.contents {
            HoverContents::Markup(content) => assert_eq!(content.value, "programmed"),
            other => panic!("unexpected hover contents: {other:?}"),
        }
        assert_eq!(lsp.pending_count("textDocument/hover"), 0);
    }

    #[test]
    fn capabilities_default_empty() {
        let lsp = FakeLsp::new();
        let caps = lsp.capabilities();
        assert!(caps.hover_provider.is_none());
        assert!(caps.completion_provider.is_none());
    }

    #[test]
    fn supports_feature_default_caps() {
        use crate::host::LanguageServerFeature;
        let lsp = FakeLsp::new();
        assert!(!lsp.supports_feature(LanguageServerFeature::Hover));
        assert!(!lsp.supports_feature(LanguageServerFeature::Completion));
        assert!(!lsp.supports_feature(LanguageServerFeature::GotoDefinition));
        assert!(!lsp.supports_feature(LanguageServerFeature::RenameSymbol));
        // Push diagnostics has no provider field; always considered supported.
        assert!(lsp.supports_feature(LanguageServerFeature::Diagnostics));
    }

    #[test]
    fn set_capabilities_drives_supports_feature() {
        use crate::host::LanguageServerFeature;
        use lsp_types::{CompletionOptions, HoverProviderCapability};

        let lsp = FakeLsp::new();
        let caps = ServerCapabilities {
            hover_provider: Some(HoverProviderCapability::Simple(true)),
            completion_provider: Some(CompletionOptions::default()),
            ..ServerCapabilities::default()
        };
        lsp.set_capabilities(caps);

        assert!(lsp.supports_feature(LanguageServerFeature::Hover));
        assert!(lsp.supports_feature(LanguageServerFeature::Completion));
        assert!(!lsp.supports_feature(LanguageServerFeature::GotoDefinition));
        assert!(!lsp.supports_feature(LanguageServerFeature::RenameSymbol));
    }

    #[test]
    fn supports_feature_goto_and_rename_one_of() {
        use crate::host::LanguageServerFeature;
        use lsp_types::OneOf;

        let lsp = FakeLsp::new();
        let caps = ServerCapabilities {
            definition_provider: Some(OneOf::Left(true)),
            references_provider: Some(OneOf::Left(true)),
            rename_provider: Some(OneOf::Left(true)),
            ..ServerCapabilities::default()
        };
        lsp.set_capabilities(caps);

        assert!(lsp.supports_feature(LanguageServerFeature::GotoDefinition));
        assert!(lsp.supports_feature(LanguageServerFeature::GotoReference));
        assert!(lsp.supports_feature(LanguageServerFeature::RenameSymbol));
        assert!(!lsp.supports_feature(LanguageServerFeature::GotoDeclaration));
    }

    #[test]
    fn supports_feature_left_false_is_unsupported() {
        use crate::host::LanguageServerFeature;
        use lsp_types::OneOf;

        let lsp = FakeLsp::new();
        let caps = ServerCapabilities {
            definition_provider: Some(OneOf::Left(false)),
            ..ServerCapabilities::default()
        };
        lsp.set_capabilities(caps);

        assert!(!lsp.supports_feature(LanguageServerFeature::GotoDefinition));
    }

    #[test]
    fn initialize_returns_set_capabilities() {
        rt().block_on(async {
            use lsp_types::HoverProviderCapability;

            let lsp = FakeLsp::new();
            let caps = ServerCapabilities {
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                ..ServerCapabilities::default()
            };
            lsp.set_capabilities(caps);

            let result = lsp.initialize(None).await.unwrap();
            assert!(matches!(
                result.capabilities.hover_provider,
                Some(HoverProviderCapability::Simple(true))
            ));
        });
    }

    #[test]
    fn offset_encoding_default_is_utf16() {
        let lsp = FakeLsp::new();
        assert_eq!(lsp.offset_encoding(), OffsetEncoding::Utf16);
    }

    #[test]
    fn set_offset_encoding_drives_accessor() {
        let lsp = FakeLsp::new();

        lsp.set_offset_encoding(OffsetEncoding::Utf8);
        assert_eq!(lsp.offset_encoding(), OffsetEncoding::Utf8);

        lsp.set_offset_encoding(OffsetEncoding::Utf32);
        assert_eq!(lsp.offset_encoding(), OffsetEncoding::Utf32);

        lsp.set_offset_encoding(OffsetEncoding::Utf16);
        assert_eq!(lsp.offset_encoding(), OffsetEncoding::Utf16);
    }

    #[test]
    fn unknown_position_encoding_falls_back_to_utf16() {
        let lsp = FakeLsp::new();
        let caps = ServerCapabilities {
            position_encoding: Some(PositionEncodingKind::new("utf-1234")),
            ..ServerCapabilities::default()
        };
        lsp.set_capabilities(caps);

        assert_eq!(lsp.offset_encoding(), OffsetEncoding::Utf16);
    }

    #[test]
    fn set_offset_encoding_preserves_other_capabilities() {
        use lsp_types::HoverProviderCapability;

        let lsp = FakeLsp::new();
        let caps = ServerCapabilities {
            hover_provider: Some(HoverProviderCapability::Simple(true)),
            ..ServerCapabilities::default()
        };
        lsp.set_capabilities(caps);

        lsp.set_offset_encoding(OffsetEncoding::Utf8);

        assert_eq!(lsp.offset_encoding(), OffsetEncoding::Utf8);
        assert!(matches!(
            lsp.capabilities().hover_provider,
            Some(HoverProviderCapability::Simple(true))
        ));
    }
}
