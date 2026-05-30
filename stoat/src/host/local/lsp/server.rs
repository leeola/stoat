use crate::host::lsp::{
    IncomingRequest, LspNotification, LspResponseError, LspServer, OffsetEncoding,
};
use async_trait::async_trait;
use lsp_types::{
    notification::Notification, request::Request, ApplyWorkspaceEditParams,
    CallHierarchyIncomingCall, CallHierarchyIncomingCallsParams, CallHierarchyItem,
    CallHierarchyOutgoingCall, CallHierarchyOutgoingCallsParams, CallHierarchyPrepareParams,
    ClientCapabilities, ClientInfo, CodeAction, CodeActionOrCommand, CodeActionParams,
    ColorInformation, ColorPresentation, ColorPresentationParams, CompletionItem, CompletionParams,
    CompletionResponse, ConfigurationParams, DidChangeConfigurationParams,
    DidChangeTextDocumentParams, DidChangeWatchedFilesParams, DidChangeWorkspaceFoldersParams,
    DidCloseTextDocumentParams, DidOpenTextDocumentParams, DidSaveTextDocumentParams,
    DocumentColorParams, DocumentDiagnosticParams, DocumentDiagnosticReportResult,
    DocumentFormattingParams, DocumentHighlight, DocumentHighlightParams, DocumentLink,
    DocumentLinkParams, DocumentRangeFormattingParams, DocumentSymbolParams,
    DocumentSymbolResponse, ExecuteCommandParams, FoldingRange, FoldingRangeParams,
    GotoDefinitionParams, GotoDefinitionResponse, Hover, HoverParams, InitializeParams,
    InitializeResult, InitializedParams, InlayHint, InlayHintParams, Location, LogMessageParams,
    LogTraceParams, NumberOrString, ProgressParams, ProgressParamsValue, PublishDiagnosticsParams,
    ReferenceParams, RegistrationParams, RenameFilesParams, RenameParams, SelectionRange,
    SelectionRangeParams, SemanticTokensParams, SemanticTokensRangeParams,
    SemanticTokensRangeResult, SemanticTokensResult, ServerCapabilities, ShowMessageParams,
    ShowMessageRequestParams, SignatureHelp, SignatureHelpParams, TextDocumentPositionParams,
    TextEdit, TypeHierarchyItem, TypeHierarchyPrepareParams, TypeHierarchySubtypesParams,
    TypeHierarchySupertypesParams, UnregistrationParams, Uri, WorkDoneProgressCreateParams,
    WorkspaceEdit, WorkspaceSymbolParams, WorkspaceSymbolResponse,
};
use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    io,
    process::Stdio,
    sync::{
        atomic::{AtomicI64, Ordering},
        Arc, LazyLock, Mutex,
    },
};
use stoat_config::LanguageServerCommand;
use stoat_scheduler::{Executor, Task};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader},
    process::{Child, ChildStderr, ChildStdin, ChildStdout, Command},
    sync::{
        mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender},
        oneshot, Mutex as TokioMutex,
    },
};
use tracing::{trace, warn};

static EMPTY_CAPABILITIES: LazyLock<Arc<ServerCapabilities>> =
    LazyLock::new(|| Arc::new(ServerCapabilities::default()));

/// In-flight request map: integer JSON-RPC id -> oneshot sender the
/// caller is parked on. Wrapped in [`Arc`] so the reader task and the
/// client requester share ownership; wrapped in [`Mutex`] because
/// insert (requester) and remove (reader) race.
type PendingMap = Arc<Mutex<HashMap<i64, oneshot::Sender<Result<Value, LspResponseError>>>>>;

/// Production [`LspServer`] driving an external language server over
/// stdin/stdout JSON-RPC. Spawns the child process at construction
/// time, runs a background reader task that demultiplexes responses,
/// notifications, and server-initiated requests, and serializes
/// writes through a single stdin lock so frames cannot interleave.
///
/// The child process is launched with `kill_on_drop(true)`; dropping
/// a `LocalLsp` value terminates the server and cancels the reader
/// and stderr tasks. The `initialize` handshake stores the server's
/// reported [`ServerCapabilities`] for later [`Self::capabilities`]
/// lookups and follows it with the `initialized` notification.
pub struct LocalLsp {
    stdin: TokioMutex<ChildStdin>,
    pending: PendingMap,
    next_id: AtomicI64,
    capabilities: Mutex<Arc<ServerCapabilities>>,
    notif_rx: TokioMutex<UnboundedReceiver<LspNotification>>,
    req_rx: TokioMutex<UnboundedReceiver<IncomingRequest>>,
    _child: TokioMutex<Child>,
    _reader_task: Task<()>,
    _stderr_task: Task<()>,
}

impl LocalLsp {
    /// Spawn the language server described by `cmd` and start the
    /// reader / stderr tasks. The returned server is not yet
    /// initialized; the caller drives the `initialize` / `initialized`
    /// handshake.
    pub async fn spawn(executor: Executor, cmd: &LanguageServerCommand) -> io::Result<Self> {
        let mut child = Command::new(&cmd.command)
            .args(&cmd.args)
            .envs(&cmd.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("spawned LSP child has no stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("spawned LSP child has no stdout"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| io::Error::other("spawned LSP child has no stderr"))?;

        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let (notif_tx, notif_rx) = unbounded_channel();
        let (req_tx, req_rx) = unbounded_channel();

        let reader_task =
            executor.spawn(reader_loop(stdout, Arc::clone(&pending), notif_tx, req_tx));
        let stderr_task = executor.spawn(stderr_loop(stderr));

        Ok(Self {
            stdin: TokioMutex::new(stdin),
            pending,
            next_id: AtomicI64::new(1),
            capabilities: Mutex::new(Arc::clone(&EMPTY_CAPABILITIES)),
            notif_rx: TokioMutex::new(notif_rx),
            req_rx: TokioMutex::new(req_rx),
            _child: TokioMutex::new(child),
            _reader_task: reader_task,
            _stderr_task: stderr_task,
        })
    }

    /// Send a typed JSON-RPC request and await the matching response,
    /// decoding it into `R::Result`. Returns an error when the server
    /// closed before responding or when the response carries a
    /// JSON-RPC error envelope (the envelope's message is propagated
    /// as [`io::ErrorKind::Other`]).
    async fn request<R: Request>(&self, params: R::Params) -> io::Result<R::Result> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);

        let envelope = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": R::METHOD,
            "params": params,
        });
        if let Err(err) = self.write_value(&envelope).await {
            self.pending.lock().unwrap().remove(&id);
            return Err(err);
        }

        match rx.await {
            Ok(Ok(value)) => serde_json::from_value(value)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e)),
            Ok(Err(err)) => Err(io::Error::other(err.message)),
            Err(_) => Err(io::Error::other("language server closed before responding")),
        }
    }

    /// Send a typed JSON-RPC notification. Fire-and-forget; the
    /// server does not acknowledge.
    async fn notify<N: Notification>(&self, params: N::Params) -> io::Result<()> {
        let envelope = json!({
            "jsonrpc": "2.0",
            "method": N::METHOD,
            "params": params,
        });
        self.write_value(&envelope).await
    }

    async fn write_value(&self, value: &Value) -> io::Result<()> {
        let body =
            serde_json::to_vec(value).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let mut stdin = self.stdin.lock().await;
        write_frame(&mut *stdin, &body).await
    }
}

#[async_trait]
impl LspServer for LocalLsp {
    fn capabilities(&self) -> Arc<ServerCapabilities> {
        Arc::clone(&self.capabilities.lock().unwrap())
    }

    async fn initialize(&self, root_uri: Option<Uri>) -> io::Result<InitializeResult> {
        #[allow(deprecated)]
        let params = InitializeParams {
            process_id: Some(std::process::id()),
            root_path: None,
            root_uri,
            initialization_options: None,
            capabilities: ClientCapabilities::default(),
            trace: None,
            workspace_folders: None,
            client_info: Some(ClientInfo {
                name: "stoat".into(),
                version: Some(env!("CARGO_PKG_VERSION").into()),
            }),
            locale: None,
            work_done_progress_params: Default::default(),
        };
        let result: InitializeResult = self
            .request::<lsp_types::request::Initialize>(params)
            .await?;
        *self.capabilities.lock().unwrap() = Arc::new(result.capabilities.clone());
        self.notify::<lsp_types::notification::Initialized>(InitializedParams {})
            .await?;
        Ok(result)
    }

    async fn shutdown(&self) -> io::Result<()> {
        self.request::<lsp_types::request::Shutdown>(()).await?;
        self.notify::<lsp_types::notification::Exit>(()).await
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) -> io::Result<()> {
        self.notify::<lsp_types::notification::DidOpenTextDocument>(params)
            .await
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) -> io::Result<()> {
        self.notify::<lsp_types::notification::DidChangeTextDocument>(params)
            .await
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) -> io::Result<()> {
        self.notify::<lsp_types::notification::DidSaveTextDocument>(params)
            .await
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) -> io::Result<()> {
        self.notify::<lsp_types::notification::DidCloseTextDocument>(params)
            .await
    }

    async fn did_rename(&self, params: RenameFilesParams) -> io::Result<()> {
        self.notify::<lsp_types::notification::DidRenameFiles>(params)
            .await
    }

    async fn did_change_watched_files(
        &self,
        params: DidChangeWatchedFilesParams,
    ) -> io::Result<()> {
        self.notify::<lsp_types::notification::DidChangeWatchedFiles>(params)
            .await
    }

    async fn did_change_configuration(
        &self,
        params: DidChangeConfigurationParams,
    ) -> io::Result<()> {
        self.notify::<lsp_types::notification::DidChangeConfiguration>(params)
            .await
    }

    async fn did_change_workspace_folders(
        &self,
        params: DidChangeWorkspaceFoldersParams,
    ) -> io::Result<()> {
        self.notify::<lsp_types::notification::DidChangeWorkspaceFolders>(params)
            .await
    }

    async fn hover(&self, params: HoverParams) -> io::Result<Option<Hover>> {
        self.request::<lsp_types::request::HoverRequest>(params)
            .await
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> io::Result<Option<GotoDefinitionResponse>> {
        self.request::<lsp_types::request::GotoDefinition>(params)
            .await
    }

    async fn goto_declaration(
        &self,
        params: GotoDefinitionParams,
    ) -> io::Result<Option<GotoDefinitionResponse>> {
        self.request::<lsp_types::request::GotoDeclaration>(params)
            .await
    }

    async fn goto_type_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> io::Result<Option<GotoDefinitionResponse>> {
        self.request::<lsp_types::request::GotoTypeDefinition>(params)
            .await
    }

    async fn goto_implementation(
        &self,
        params: GotoDefinitionParams,
    ) -> io::Result<Option<GotoDefinitionResponse>> {
        self.request::<lsp_types::request::GotoImplementation>(params)
            .await
    }

    async fn references(&self, params: ReferenceParams) -> io::Result<Option<Vec<Location>>> {
        self.request::<lsp_types::request::References>(params).await
    }

    async fn document_highlight(
        &self,
        params: DocumentHighlightParams,
    ) -> io::Result<Option<Vec<DocumentHighlight>>> {
        self.request::<lsp_types::request::DocumentHighlightRequest>(params)
            .await
    }

    async fn completion(&self, params: CompletionParams) -> io::Result<Option<CompletionResponse>> {
        self.request::<lsp_types::request::Completion>(params).await
    }

    async fn completion_resolve(&self, item: CompletionItem) -> io::Result<CompletionItem> {
        self.request::<lsp_types::request::ResolveCompletionItem>(item)
            .await
    }

    async fn code_action(
        &self,
        params: CodeActionParams,
    ) -> io::Result<Option<Vec<CodeActionOrCommand>>> {
        self.request::<lsp_types::request::CodeActionRequest>(params)
            .await
    }

    async fn code_action_resolve(&self, action: CodeAction) -> io::Result<CodeAction> {
        self.request::<lsp_types::request::CodeActionResolveRequest>(action)
            .await
    }

    async fn document_link(
        &self,
        params: DocumentLinkParams,
    ) -> io::Result<Option<Vec<DocumentLink>>> {
        self.request::<lsp_types::request::DocumentLinkRequest>(params)
            .await
    }

    async fn document_link_resolve(&self, link: DocumentLink) -> io::Result<DocumentLink> {
        self.request::<lsp_types::request::DocumentLinkResolve>(link)
            .await
    }

    async fn document_color(
        &self,
        params: DocumentColorParams,
    ) -> io::Result<Option<Vec<ColorInformation>>> {
        self.request::<lsp_types::request::DocumentColor>(params)
            .await
            .map(Some)
    }

    async fn color_presentation(
        &self,
        params: ColorPresentationParams,
    ) -> io::Result<Option<Vec<ColorPresentation>>> {
        self.request::<lsp_types::request::ColorPresentationRequest>(params)
            .await
            .map(Some)
    }

    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> io::Result<Option<SemanticTokensResult>> {
        self.request::<lsp_types::request::SemanticTokensFullRequest>(params)
            .await
    }

    async fn semantic_tokens_range(
        &self,
        params: SemanticTokensRangeParams,
    ) -> io::Result<Option<SemanticTokensRangeResult>> {
        self.request::<lsp_types::request::SemanticTokensRangeRequest>(params)
            .await
    }

    async fn prepare_call_hierarchy(
        &self,
        params: CallHierarchyPrepareParams,
    ) -> io::Result<Option<Vec<CallHierarchyItem>>> {
        self.request::<lsp_types::request::CallHierarchyPrepare>(params)
            .await
    }

    async fn call_hierarchy_incoming_calls(
        &self,
        params: CallHierarchyIncomingCallsParams,
    ) -> io::Result<Option<Vec<CallHierarchyIncomingCall>>> {
        self.request::<lsp_types::request::CallHierarchyIncomingCalls>(params)
            .await
    }

    async fn call_hierarchy_outgoing_calls(
        &self,
        params: CallHierarchyOutgoingCallsParams,
    ) -> io::Result<Option<Vec<CallHierarchyOutgoingCall>>> {
        self.request::<lsp_types::request::CallHierarchyOutgoingCalls>(params)
            .await
    }

    async fn prepare_type_hierarchy(
        &self,
        params: TypeHierarchyPrepareParams,
    ) -> io::Result<Option<Vec<TypeHierarchyItem>>> {
        self.request::<lsp_types::request::TypeHierarchyPrepare>(params)
            .await
    }

    async fn type_hierarchy_supertypes(
        &self,
        params: TypeHierarchySupertypesParams,
    ) -> io::Result<Option<Vec<TypeHierarchyItem>>> {
        self.request::<lsp_types::request::TypeHierarchySupertypes>(params)
            .await
    }

    async fn type_hierarchy_subtypes(
        &self,
        params: TypeHierarchySubtypesParams,
    ) -> io::Result<Option<Vec<TypeHierarchyItem>>> {
        self.request::<lsp_types::request::TypeHierarchySubtypes>(params)
            .await
    }

    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> io::Result<Option<DocumentSymbolResponse>> {
        self.request::<lsp_types::request::DocumentSymbolRequest>(params)
            .await
    }

    async fn document_diagnostic(
        &self,
        params: DocumentDiagnosticParams,
    ) -> io::Result<Option<DocumentDiagnosticReportResult>> {
        self.request::<lsp_types::request::DocumentDiagnosticRequest>(params)
            .await
            .map(Some)
    }

    async fn folding_range(
        &self,
        params: FoldingRangeParams,
    ) -> io::Result<Option<Vec<FoldingRange>>> {
        self.request::<lsp_types::request::FoldingRangeRequest>(params)
            .await
    }

    async fn selection_range(
        &self,
        params: SelectionRangeParams,
    ) -> io::Result<Option<Vec<SelectionRange>>> {
        self.request::<lsp_types::request::SelectionRangeRequest>(params)
            .await
    }

    async fn workspace_symbol(
        &self,
        params: WorkspaceSymbolParams,
    ) -> io::Result<Option<WorkspaceSymbolResponse>> {
        self.request::<lsp_types::request::WorkspaceSymbolRequest>(params)
            .await
    }

    async fn signature_help(
        &self,
        params: SignatureHelpParams,
    ) -> io::Result<Option<SignatureHelp>> {
        self.request::<lsp_types::request::SignatureHelpRequest>(params)
            .await
    }

    async fn inlay_hint(&self, params: InlayHintParams) -> io::Result<Option<Vec<InlayHint>>> {
        self.request::<lsp_types::request::InlayHintRequest>(params)
            .await
    }

    async fn inlay_hint_resolve(&self, hint: InlayHint) -> io::Result<InlayHint> {
        self.request::<lsp_types::request::InlayHintResolveRequest>(hint)
            .await
    }

    async fn range_inlay_hint(
        &self,
        params: InlayHintParams,
    ) -> io::Result<Option<Vec<InlayHint>>> {
        self.request::<lsp_types::request::InlayHintRequest>(params)
            .await
    }

    async fn prepare_rename(
        &self,
        params: TextDocumentPositionParams,
    ) -> io::Result<Option<lsp_types::PrepareRenameResponse>> {
        self.request::<lsp_types::request::PrepareRenameRequest>(params)
            .await
    }

    async fn rename(&self, params: RenameParams) -> io::Result<Option<WorkspaceEdit>> {
        self.request::<lsp_types::request::Rename>(params).await
    }

    async fn formatting(
        &self,
        params: DocumentFormattingParams,
    ) -> io::Result<Option<Vec<TextEdit>>> {
        self.request::<lsp_types::request::Formatting>(params).await
    }

    async fn range_formatting(
        &self,
        params: DocumentRangeFormattingParams,
    ) -> io::Result<Option<Vec<TextEdit>>> {
        self.request::<lsp_types::request::RangeFormatting>(params)
            .await
    }

    async fn will_rename(&self, params: RenameFilesParams) -> io::Result<Option<WorkspaceEdit>> {
        self.request::<lsp_types::request::WillRenameFiles>(params)
            .await
    }

    async fn execute_command(&self, params: ExecuteCommandParams) -> io::Result<Option<Value>> {
        self.request::<lsp_types::request::ExecuteCommand>(params)
            .await
    }

    async fn recv_notification(&self) -> Option<LspNotification> {
        self.notif_rx.lock().await.recv().await
    }

    async fn try_recv_notification(&self) -> Option<LspNotification> {
        self.notif_rx.lock().await.try_recv().ok()
    }

    async fn recv_incoming_request(&self) -> Option<IncomingRequest> {
        self.req_rx.lock().await.recv().await
    }

    async fn try_recv_incoming_request(&self) -> Option<IncomingRequest> {
        self.req_rx.lock().await.try_recv().ok()
    }

    async fn reply(
        &self,
        id: NumberOrString,
        result: Result<Value, LspResponseError>,
    ) -> io::Result<()> {
        let envelope = match result {
            Ok(value) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": value,
            }),
            Err(err) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {
                    "code": err.code,
                    "message": err.message,
                    "data": err.data,
                },
            }),
        };
        self.write_value(&envelope).await
    }
}

impl LocalLsp {
    /// Negotiated [`OffsetEncoding`] for `Position.character` width.
    /// Matches the [`LspServer`] default but is exposed publicly so
    /// the launcher can read it without an `LspServer` import.
    pub fn current_offset_encoding(&self) -> OffsetEncoding {
        match self
            .capabilities
            .lock()
            .unwrap()
            .position_encoding
            .as_ref()
            .map(|e| e.as_str())
        {
            Some("utf-8") => OffsetEncoding::Utf8,
            Some("utf-32") => OffsetEncoding::Utf32,
            _ => OffsetEncoding::Utf16,
        }
    }
}

async fn reader_loop(
    stdout: ChildStdout,
    pending: PendingMap,
    notif_tx: UnboundedSender<LspNotification>,
    req_tx: UnboundedSender<IncomingRequest>,
) {
    let mut reader = BufReader::new(stdout);
    loop {
        match read_frame(&mut reader).await {
            Ok(Some(body)) => dispatch_frame(&body, &pending, &notif_tx, &req_tx),
            Ok(None) => break,
            Err(err) => {
                warn!("LSP reader closing on framing error: {}", err);
                break;
            },
        }
    }
}

async fn stderr_loop(stderr: ChildStderr) {
    let mut reader = BufReader::new(stderr);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) => break,
            Ok(_) => {
                let trimmed = line.trim_end_matches(['\r', '\n']);
                if !trimmed.is_empty() {
                    warn!("LSP stderr: {}", trimmed);
                }
            },
            Err(err) => {
                warn!("LSP stderr closed on error: {}", err);
                break;
            },
        }
    }
}

fn dispatch_frame(
    body: &[u8],
    pending: &Mutex<HashMap<i64, oneshot::Sender<Result<Value, LspResponseError>>>>,
    notif_tx: &UnboundedSender<LspNotification>,
    req_tx: &UnboundedSender<IncomingRequest>,
) {
    let value: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(err) => {
            warn!("LSP frame ignored, not valid JSON: {}", err);
            return;
        },
    };
    let map = match value.as_object() {
        Some(m) => m,
        None => {
            warn!("LSP frame ignored, not a JSON object");
            return;
        },
    };

    match (map.get("id"), map.get("method")) {
        (Some(id_value), Some(method_value)) => {
            let Some(method) = method_value.as_str() else {
                warn!("LSP server request method is not a string");
                return;
            };
            let id = match parse_id(id_value) {
                Some(id) => id,
                None => {
                    warn!("LSP server request id is neither integer nor string");
                    return;
                },
            };
            let params = map.get("params").cloned().unwrap_or(Value::Null);
            let request = build_incoming_request(id, method, params);
            let _ = req_tx.send(request);
        },
        (Some(id_value), None) => {
            let Some(id) = id_value.as_i64() else {
                warn!("LSP response id is not an integer");
                return;
            };
            let sender = pending.lock().unwrap().remove(&id);
            let Some(sender) = sender else {
                warn!("LSP response for unknown id {}", id);
                return;
            };
            let response = if let Some(err) = map.get("error") {
                Err(decode_error(err))
            } else {
                Ok(map.get("result").cloned().unwrap_or(Value::Null))
            };
            let _ = sender.send(response);
        },
        (None, Some(method_value)) => {
            let Some(method) = method_value.as_str() else {
                warn!("LSP notification method is not a string");
                return;
            };
            let params = map.get("params").cloned().unwrap_or(Value::Null);
            if let Some(notif) = build_notification(method, params) {
                let _ = notif_tx.send(notif);
            }
        },
        (None, None) => {
            warn!("LSP frame ignored, neither response nor request nor notification");
        },
    }
}

fn parse_id(value: &Value) -> Option<NumberOrString> {
    if let Some(n) = value.as_i64() {
        if let Ok(i) = i32::try_from(n) {
            return Some(NumberOrString::Number(i));
        }
        return None;
    }
    value
        .as_str()
        .map(|s| NumberOrString::String(s.to_string()))
}

fn decode_error(value: &Value) -> LspResponseError {
    let code = value.get("code").and_then(Value::as_i64).unwrap_or(-32603);
    let message = value
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("unspecified error")
        .to_string();
    let data = value.get("data").cloned();
    LspResponseError {
        code,
        message,
        data,
    }
}

fn build_notification(method: &str, params: Value) -> Option<LspNotification> {
    match method {
        <lsp_types::notification::PublishDiagnostics as Notification>::METHOD => {
            let p: PublishDiagnosticsParams = parse_or_warn(method, params)?;
            Some(LspNotification::Diagnostics {
                uri: p.uri,
                diagnostics: p.diagnostics,
                version: p.version,
            })
        },
        <lsp_types::notification::Progress as Notification>::METHOD => {
            let p: ProgressParams = parse_or_warn(method, params)?;
            let ProgressParamsValue::WorkDone(value) = p.value;
            Some(LspNotification::Progress {
                token: p.token,
                value,
            })
        },
        <lsp_types::notification::LogMessage as Notification>::METHOD => {
            let p: LogMessageParams = parse_or_warn(method, params)?;
            Some(LspNotification::LogMessage {
                typ: p.typ,
                message: p.message,
            })
        },
        <lsp_types::notification::ShowMessage as Notification>::METHOD => {
            let p: ShowMessageParams = parse_or_warn(method, params)?;
            Some(LspNotification::ShowMessage {
                typ: p.typ,
                message: p.message,
            })
        },
        <lsp_types::notification::LogTrace as Notification>::METHOD => {
            let p: LogTraceParams = parse_or_warn(method, params)?;
            Some(LspNotification::LogTrace {
                message: p.message,
                verbose: p.verbose,
            })
        },
        _ => {
            trace!(
                "LSP notification {} dropped: not surfaced as LspNotification",
                method
            );
            None
        },
    }
}

fn build_incoming_request(id: NumberOrString, method: &str, params: Value) -> IncomingRequest {
    match method {
        "window/showMessageRequest" => parse_known::<ShowMessageRequestParams>(method, &params)
            .map(|p| IncomingRequest::ShowMessageRequest {
                id: id.clone(),
                params: p,
            })
            .unwrap_or_else(|| IncomingRequest::Unknown {
                id: id.clone(),
                method: method.to_string(),
                params: params.clone(),
            }),
        "window/workDoneProgress/create" => {
            parse_known::<WorkDoneProgressCreateParams>(method, &params)
                .map(|p| IncomingRequest::WorkDoneProgressCreate {
                    id: id.clone(),
                    params: p,
                })
                .unwrap_or_else(|| IncomingRequest::Unknown {
                    id: id.clone(),
                    method: method.to_string(),
                    params: params.clone(),
                })
        },
        "client/registerCapability" => parse_known::<RegistrationParams>(method, &params)
            .map(|p| IncomingRequest::RegisterCapability {
                id: id.clone(),
                params: p,
            })
            .unwrap_or_else(|| IncomingRequest::Unknown {
                id: id.clone(),
                method: method.to_string(),
                params: params.clone(),
            }),
        "client/unregisterCapability" => parse_known::<UnregistrationParams>(method, &params)
            .map(|p| IncomingRequest::UnregisterCapability {
                id: id.clone(),
                params: p,
            })
            .unwrap_or_else(|| IncomingRequest::Unknown {
                id: id.clone(),
                method: method.to_string(),
                params: params.clone(),
            }),
        "workspace/configuration" => parse_known::<ConfigurationParams>(method, &params)
            .map(|p| IncomingRequest::WorkspaceConfiguration {
                id: id.clone(),
                params: p,
            })
            .unwrap_or_else(|| IncomingRequest::Unknown {
                id: id.clone(),
                method: method.to_string(),
                params: params.clone(),
            }),
        "workspace/applyEdit" => parse_known::<ApplyWorkspaceEditParams>(method, &params)
            .map(|p| IncomingRequest::WorkspaceApplyEdit {
                id: id.clone(),
                params: p,
            })
            .unwrap_or_else(|| IncomingRequest::Unknown {
                id: id.clone(),
                method: method.to_string(),
                params: params.clone(),
            }),
        _ => IncomingRequest::Unknown {
            id,
            method: method.to_string(),
            params,
        },
    }
}

fn parse_or_warn<T: DeserializeOwned>(method: &str, value: Value) -> Option<T> {
    match serde_json::from_value(value) {
        Ok(v) => Some(v),
        Err(err) => {
            warn!(
                "LSP notification {} dropped: failed to decode params: {}",
                method, err
            );
            None
        },
    }
}

fn parse_known<T: DeserializeOwned>(method: &str, value: &Value) -> Option<T> {
    match serde_json::from_value(value.clone()) {
        Ok(v) => Some(v),
        Err(err) => {
            warn!(
                "LSP server request {} params failed to decode: {}",
                method, err
            );
            None
        },
    }
}

async fn read_frame<R: AsyncBufReadExt + Unpin>(reader: &mut R) -> io::Result<Option<Vec<u8>>> {
    let mut content_length: Option<usize> = None;
    let mut header_buf = String::new();
    loop {
        header_buf.clear();
        let read = reader.read_line(&mut header_buf).await?;
        if read == 0 {
            return Ok(None);
        }
        let trimmed = header_buf.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        let Some((name, value)) = trimmed.split_once(':') else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("malformed LSP header: {trimmed}"),
            ));
        };
        if name.eq_ignore_ascii_case("Content-Length") {
            let parsed = value.trim().parse::<usize>().map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("bad Content-Length: {e}"),
                )
            })?;
            content_length = Some(parsed);
        }
    }
    let len = content_length.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "LSP frame missing Content-Length",
        )
    })?;
    let mut body = vec![0u8; len];
    AsyncReadExt::read_exact(reader, &mut body).await?;
    Ok(Some(body))
}

async fn write_frame<W: AsyncWrite + Unpin>(writer: &mut W, body: &[u8]) -> io::Result<()> {
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    writer.write_all(header.as_bytes()).await?;
    writer.write_all(body).await?;
    writer.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range};
    use serde_json::json;
    use tokio::io::{duplex, AsyncWriteExt};

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    #[test]
    fn write_and_read_frame_roundtrip() {
        rt().block_on(async {
            let (mut a, b) = duplex(1024);
            let body = br#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#;
            write_frame(&mut a, body).await.unwrap();
            let mut reader = BufReader::new(b);
            let received = read_frame(&mut reader).await.unwrap().unwrap();
            assert_eq!(received, body);
        });
    }

    #[test]
    fn read_frame_returns_none_on_eof() {
        rt().block_on(async {
            let (a, b) = duplex(1024);
            drop(a);
            let mut reader = BufReader::new(b);
            let received = read_frame(&mut reader).await.unwrap();
            assert!(received.is_none());
        });
    }

    #[test]
    fn read_frame_rejects_missing_content_length() {
        rt().block_on(async {
            let (mut a, b) = duplex(1024);
            a.write_all(b"X-Other: 1\r\n\r\n").await.unwrap();
            drop(a);
            let mut reader = BufReader::new(b);
            let err = read_frame(&mut reader).await.unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        });
    }

    #[test]
    fn dispatch_routes_response_to_pending() {
        rt().block_on(async {
            let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
            let (tx, rx) = oneshot::channel();
            pending.lock().unwrap().insert(7, tx);
            let (notif_tx, _notif_rx) = unbounded_channel();
            let (req_tx, _req_rx) = unbounded_channel();

            let body = json!({"jsonrpc": "2.0", "id": 7, "result": {"ok": true}}).to_string();
            dispatch_frame(body.as_bytes(), &pending, &notif_tx, &req_tx);

            let value = rx.await.unwrap().unwrap();
            assert_eq!(value, json!({"ok": true}));
            assert!(pending.lock().unwrap().is_empty());
        });
    }

    #[test]
    fn dispatch_routes_error_to_pending() {
        rt().block_on(async {
            let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
            let (tx, rx) = oneshot::channel();
            pending.lock().unwrap().insert(9, tx);
            let (notif_tx, _notif_rx) = unbounded_channel();
            let (req_tx, _req_rx) = unbounded_channel();

            let body = json!({
                "jsonrpc": "2.0",
                "id": 9,
                "error": {"code": -32601, "message": "method not found"}
            })
            .to_string();
            dispatch_frame(body.as_bytes(), &pending, &notif_tx, &req_tx);

            let err = rx.await.unwrap().unwrap_err();
            assert_eq!(err.code, -32601);
            assert_eq!(err.message, "method not found");
        });
    }

    #[test]
    fn dispatch_routes_diagnostics_notification() {
        rt().block_on(async {
            let pending = Arc::new(Mutex::new(HashMap::new()));
            let (notif_tx, mut notif_rx) = unbounded_channel();
            let (req_tx, _req_rx) = unbounded_channel();

            let diag = Diagnostic {
                range: Range {
                    start: Position {
                        line: 0,
                        character: 0,
                    },
                    end: Position {
                        line: 0,
                        character: 1,
                    },
                },
                severity: Some(DiagnosticSeverity::ERROR),
                code: None,
                code_description: None,
                source: None,
                message: "oops".into(),
                related_information: None,
                tags: None,
                data: None,
            };
            let body = json!({
                "jsonrpc": "2.0",
                "method": "textDocument/publishDiagnostics",
                "params": {
                    "uri": "file:///tmp/main.rs",
                    "diagnostics": [diag],
                }
            })
            .to_string();
            dispatch_frame(body.as_bytes(), &pending, &notif_tx, &req_tx);

            let notif = notif_rx.recv().await.unwrap();
            match notif {
                LspNotification::Diagnostics {
                    uri,
                    diagnostics,
                    version,
                } => {
                    assert_eq!(uri.as_str(), "file:///tmp/main.rs");
                    assert_eq!(diagnostics.len(), 1);
                    assert_eq!(diagnostics[0].message, "oops");
                    assert!(version.is_none());
                },
                other => panic!("unexpected notification: {other:?}"),
            }
        });
    }

    #[test]
    fn dispatch_routes_known_incoming_request() {
        rt().block_on(async {
            let pending = Arc::new(Mutex::new(HashMap::new()));
            let (notif_tx, _notif_rx) = unbounded_channel();
            let (req_tx, mut req_rx) = unbounded_channel();

            let body = json!({
                "jsonrpc": "2.0",
                "id": 42,
                "method": "workspace/configuration",
                "params": {"items": []}
            })
            .to_string();
            dispatch_frame(body.as_bytes(), &pending, &notif_tx, &req_tx);

            let req = req_rx.recv().await.unwrap();
            match req {
                IncomingRequest::WorkspaceConfiguration { id, params } => {
                    assert_eq!(id, NumberOrString::Number(42));
                    assert!(params.items.is_empty());
                },
                other => panic!("unexpected request: {other:?}"),
            }
        });
    }

    #[test]
    fn dispatch_routes_unknown_incoming_request() {
        rt().block_on(async {
            let pending = Arc::new(Mutex::new(HashMap::new()));
            let (notif_tx, _notif_rx) = unbounded_channel();
            let (req_tx, mut req_rx) = unbounded_channel();

            let body = json!({
                "jsonrpc": "2.0",
                "id": "abc",
                "method": "experimental/custom",
                "params": {"extra": 1}
            })
            .to_string();
            dispatch_frame(body.as_bytes(), &pending, &notif_tx, &req_tx);

            let req = req_rx.recv().await.unwrap();
            match req {
                IncomingRequest::Unknown { id, method, params } => {
                    assert_eq!(id, NumberOrString::String("abc".into()));
                    assert_eq!(method, "experimental/custom");
                    assert_eq!(params, json!({"extra": 1}));
                },
                other => panic!("unexpected request: {other:?}"),
            }
        });
    }

    #[test]
    fn spawn_with_missing_binary_returns_io_error() {
        rt().block_on(async {
            let scheduler = Arc::new(stoat_scheduler::TokioScheduler::new(
                tokio::runtime::Handle::current(),
            ));
            let executor = scheduler.executor();
            let outcome = LocalLsp::spawn(
                executor,
                &LanguageServerCommand {
                    command: "/nonexistent/lsp-binary-for-test".into(),
                    args: vec![],
                    env: Default::default(),
                },
            )
            .await;
            match outcome {
                Ok(_) => panic!("missing binary should fail to spawn"),
                Err(err) => assert_eq!(err.kind(), io::ErrorKind::NotFound),
            }
        });
    }
}
