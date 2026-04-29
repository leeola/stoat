use crate::host::lsp::{LspHost, LspNotification, OffsetEncoding};
use async_trait::async_trait;
use lsp_types::{
    CodeAction, CodeActionOrCommand, CodeActionParams, CompletionItem, CompletionList,
    CompletionParams, CompletionResponse, Diagnostic, DiagnosticSeverity,
    DidChangeTextDocumentParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
    DidSaveTextDocumentParams, DocumentFormattingParams, DocumentHighlight, DocumentHighlightKind,
    DocumentHighlightParams, DocumentRangeFormattingParams, DocumentSymbolParams,
    DocumentSymbolResponse, FoldingRange, FoldingRangeParams, FormattingOptions,
    GotoDefinitionParams, GotoDefinitionResponse, Hover, HoverContents, HoverParams,
    InitializeResult, InlayHint, InlayHintKind, InlayHintLabel, InlayHintParams, Location,
    MarkupContent, MarkupKind, MessageType, NumberOrString, PartialResultParams, Position,
    PositionEncodingKind, PrepareRenameResponse, Range, ReferenceContext, ReferenceParams,
    RenameParams, SelectionRange, SelectionRangeParams, ServerCapabilities, SignatureHelp,
    SignatureHelpParams, SymbolInformation, SymbolKind, TextDocumentContentChangeEvent,
    TextDocumentIdentifier, TextDocumentItem, TextDocumentPositionParams, TextEdit, Uri,
    VersionedTextDocumentIdentifier, WorkDoneProgress, WorkDoneProgressBegin,
    WorkDoneProgressParams, WorkspaceEdit, WorkspaceSymbolParams, WorkspaceSymbolResponse,
};
use std::{
    collections::BTreeMap,
    io,
    str::FromStr,
    sync::{Arc, Mutex},
};
use tokio::sync::{
    mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender},
    Mutex as TokioMutex,
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

pub struct FakeLsp {
    state: Mutex<FakeLspState>,
    notif_tx: UnboundedSender<LspNotification>,
    notif_rx: TokioMutex<UnboundedReceiver<LspNotification>>,
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
    workspace_symbols: BTreeMap<String, Vec<SymbolInformation>>,
    folding_ranges: BTreeMap<Uri, Vec<FoldingRange>>,
    selection_ranges: BTreeMap<LspKey, SelectionRange>,
    range_formatting: BTreeMap<Uri, Vec<TextEdit>>,
    prepare_renames: BTreeMap<LspKey, PrepareRenameResponse>,
    open_documents: BTreeMap<Uri, String>,
    request_failures_oneshot: BTreeMap<String, io::ErrorKind>,
    request_failures_persistent: BTreeMap<String, io::ErrorKind>,
    initialized: bool,
    shut_down: bool,
}

impl Default for FakeLsp {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeLsp {
    pub fn new() -> Self {
        let (notif_tx, notif_rx) = unbounded_channel();
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
                workspace_symbols: BTreeMap::new(),
                folding_ranges: BTreeMap::new(),
                selection_ranges: BTreeMap::new(),
                range_formatting: BTreeMap::new(),
                prepare_renames: BTreeMap::new(),
                open_documents: BTreeMap::new(),
                request_failures_oneshot: BTreeMap::new(),
                request_failures_persistent: BTreeMap::new(),
                initialized: false,
                shut_down: false,
            }),
            notif_tx,
            notif_rx: TokioMutex::new(notif_rx),
        }
    }

    /// Replace the server capabilities returned by
    /// [`LspHost::capabilities`] (and consulted by
    /// [`LspHost::supports_feature`]). Tests call this before
    /// driving capability-dependent code paths so the host advertises
    /// the right feature set.
    pub fn set_capabilities(&self, capabilities: ServerCapabilities) {
        self.state.lock().unwrap().capabilities = Arc::new(capabilities);
    }

    /// Convenience setter that swaps just the
    /// `position_encoding` field on the stored capabilities so
    /// [`LspHost::offset_encoding`] reflects `encoding`. Other
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
    /// [`LspHost::selection_range`] looks up every position in the
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
    /// [`LspHost::recv_notification`]. Lets tests inject server-push
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
impl LspHost for FakeLsp {
    fn capabilities(&self) -> Arc<ServerCapabilities> {
        self.state.lock().unwrap().capabilities.clone()
    }

    async fn initialize(&self, _root_uri: Option<Uri>) -> io::Result<InitializeResult> {
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
                    .insert(uri.clone(), params.text_document.text);
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

    async fn hover(&self, params: HoverParams) -> io::Result<Option<Hover>> {
        if let Some(err) = self.take_request_failure("textDocument/hover") {
            return Err(err);
        }
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
        if let Some(err) = self.take_request_failure("textDocument/definition") {
            return Err(err);
        }
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
        if let Some(err) = self.take_request_failure("textDocument/declaration") {
            return Err(err);
        }
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
        if let Some(err) = self.take_request_failure("textDocument/typeDefinition") {
            return Err(err);
        }
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
        if let Some(err) = self.take_request_failure("textDocument/implementation") {
            return Err(err);
        }
        let state = self.state.lock().unwrap();
        Ok(lookup_goto(
            &state.implementations,
            &params.text_document_position_params.text_document.uri,
            &params.text_document_position_params.position,
        ))
    }

    async fn references(&self, params: ReferenceParams) -> io::Result<Option<Vec<Location>>> {
        if let Some(err) = self.take_request_failure("textDocument/references") {
            return Err(err);
        }
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
        if let Some(err) = self.take_request_failure("textDocument/documentHighlight") {
            return Err(err);
        }
        let state = self.state.lock().unwrap();
        let key = LspKey::from_position(
            &params.text_document_position_params.text_document.uri,
            &params.text_document_position_params.position,
        );
        Ok(state.highlights.get(&key).cloned())
    }

    async fn completion(&self, params: CompletionParams) -> io::Result<Option<CompletionResponse>> {
        if let Some(err) = self.take_request_failure("textDocument/completion") {
            return Err(err);
        }
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
        if let Some(err) = self.take_request_failure("completionItem/resolve") {
            return Err(err);
        }
        Ok(item)
    }

    async fn code_action(
        &self,
        _params: CodeActionParams,
    ) -> io::Result<Option<Vec<CodeActionOrCommand>>> {
        if let Some(err) = self.take_request_failure("textDocument/codeAction") {
            return Err(err);
        }
        Ok(None)
    }

    async fn code_action_resolve(&self, action: CodeAction) -> io::Result<CodeAction> {
        if let Some(err) = self.take_request_failure("codeAction/resolve") {
            return Err(err);
        }
        Ok(action)
    }

    async fn document_symbol(
        &self,
        _params: DocumentSymbolParams,
    ) -> io::Result<Option<DocumentSymbolResponse>> {
        if let Some(err) = self.take_request_failure("textDocument/documentSymbol") {
            return Err(err);
        }
        Ok(None)
    }

    async fn folding_range(
        &self,
        params: FoldingRangeParams,
    ) -> io::Result<Option<Vec<FoldingRange>>> {
        if let Some(err) = self.take_request_failure("textDocument/foldingRange") {
            return Err(err);
        }
        let state = self.state.lock().unwrap();
        Ok(state.folding_ranges.get(&params.text_document.uri).cloned())
    }

    async fn selection_range(
        &self,
        params: SelectionRangeParams,
    ) -> io::Result<Option<Vec<SelectionRange>>> {
        if let Some(err) = self.take_request_failure("textDocument/selectionRange") {
            return Err(err);
        }
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
        if let Some(err) = self.take_request_failure("workspace/symbol") {
            return Err(err);
        }
        let state = self.state.lock().unwrap();
        Ok(state
            .workspace_symbols
            .get(&params.query)
            .map(|symbols| WorkspaceSymbolResponse::Flat(symbols.clone())))
    }

    async fn signature_help(
        &self,
        _params: SignatureHelpParams,
    ) -> io::Result<Option<SignatureHelp>> {
        if let Some(err) = self.take_request_failure("textDocument/signatureHelp") {
            return Err(err);
        }
        Ok(None)
    }

    async fn inlay_hint(&self, params: InlayHintParams) -> io::Result<Option<Vec<InlayHint>>> {
        if let Some(err) = self.take_request_failure("textDocument/inlayHint") {
            return Err(err);
        }
        let state = self.state.lock().unwrap();
        Ok(state.inlay_hints.get(&params.text_document.uri).cloned())
    }

    async fn inlay_hint_resolve(&self, hint: InlayHint) -> io::Result<InlayHint> {
        if let Some(err) = self.take_request_failure("inlayHint/resolve") {
            return Err(err);
        }
        Ok(hint)
    }

    async fn prepare_rename(
        &self,
        params: TextDocumentPositionParams,
    ) -> io::Result<Option<PrepareRenameResponse>> {
        if let Some(err) = self.take_request_failure("textDocument/prepareRename") {
            return Err(err);
        }
        let state = self.state.lock().unwrap();
        let key = LspKey::from_position(&params.text_document.uri, &params.position);
        Ok(state.prepare_renames.get(&key).cloned())
    }

    async fn rename(&self, _params: RenameParams) -> io::Result<Option<WorkspaceEdit>> {
        if let Some(err) = self.take_request_failure("textDocument/rename") {
            return Err(err);
        }
        Ok(None)
    }

    async fn formatting(
        &self,
        _params: DocumentFormattingParams,
    ) -> io::Result<Option<Vec<TextEdit>>> {
        if let Some(err) = self.take_request_failure("textDocument/formatting") {
            return Err(err);
        }
        Ok(None)
    }

    async fn range_formatting(
        &self,
        params: DocumentRangeFormattingParams,
    ) -> io::Result<Option<Vec<TextEdit>>> {
        if let Some(err) = self.take_request_failure("textDocument/rangeFormatting") {
            return Err(err);
        }
        let state = self.state.lock().unwrap();
        Ok(state
            .range_formatting
            .get(&params.text_document.uri)
            .cloned())
    }

    async fn recv_notification(&self) -> Option<LspNotification> {
        self.notif_rx.lock().await.recv().await
    }

    async fn try_recv_notification(&self) -> Option<LspNotification> {
        self.notif_rx.try_lock().ok()?.try_recv().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
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
        rt().block_on(async {
            let lsp = Arc::new(FakeLsp::new());
            let recv_lsp = lsp.clone();
            let handle = tokio::spawn(async move { recv_lsp.recv_notification().await });

            tokio::task::yield_now().await;

            lsp.push_notification(LspNotification::Diagnostics {
                uri: file_uri("/late.rs"),
                diagnostics: Vec::new(),
                version: None,
            });

            let notif = handle
                .await
                .expect("recv task panicked")
                .expect("recv returned None");
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
