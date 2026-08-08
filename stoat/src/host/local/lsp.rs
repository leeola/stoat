//! Production stdio [`LspHost`]: spawns a language server as a child process
//! and speaks JSON-RPC over its stdin/stdout.
//!
//! The transport mirrors [`crate::host::local::terminal::PtyTerminalSession`]: a
//! dedicated OS thread does blocking reads off the child's stdout, frames the
//! JSON-RPC stream, and pushes decoded messages onto tokio channels the async
//! trait methods drain. Responses to our own requests are matched back to the
//! awaiting caller through a `pending` id-to-oneshot map. Server-pushed
//! notifications and server-to-client requests fan out to their own channels.

use crate::host::lsp::{IncomingRequest, LspHost, LspNotification, LspResponseError};
use async_trait::async_trait;
use lsp_types::{
    CallHierarchyIncomingCall, CallHierarchyIncomingCallsParams, CallHierarchyItem,
    CallHierarchyOutgoingCall, CallHierarchyOutgoingCallsParams, CallHierarchyPrepareParams,
    ClientCapabilities, CodeAction, CodeActionCapabilityResolveSupport,
    CodeActionClientCapabilities, CodeActionKindLiteralSupport, CodeActionLiteralSupport,
    CodeActionOrCommand, CodeActionParams, ColorInformation, ColorPresentation,
    ColorPresentationParams, CompletionClientCapabilities, CompletionItem,
    CompletionItemCapability, CompletionItemCapabilityResolveSupport, CompletionParams,
    CompletionResponse, DiagnosticTag, DidChangeConfigurationParams, DidChangeTextDocumentParams,
    DidChangeWatchedFilesClientCapabilities, DidChangeWatchedFilesParams,
    DidChangeWorkspaceFoldersParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
    DidSaveTextDocumentParams, DocumentColorParams, DocumentDiagnosticParams,
    DocumentDiagnosticReportResult, DocumentFormattingParams, DocumentHighlight,
    DocumentHighlightParams, DocumentLink, DocumentLinkParams, DocumentRangeFormattingParams,
    DocumentSymbolClientCapabilities, DocumentSymbolParams, DocumentSymbolResponse,
    DynamicRegistrationClientCapabilities, ExecuteCommandParams, FoldingRange, FoldingRangeParams,
    GeneralClientCapabilities, GotoCapability, GotoDefinitionParams, GotoDefinitionResponse, Hover,
    HoverClientCapabilities, HoverParams, InitializeParams, InitializeResult, InitializedParams,
    InlayHint, InlayHintClientCapabilities, InlayHintParams, Location, LogMessageParams,
    MarkupKind, NumberOrString, PositionEncodingKind, PrepareRenameResponse, ProgressParams,
    ProgressParamsValue, ProgressToken, PublishDiagnosticsClientCapabilities,
    PublishDiagnosticsParams, ReferenceClientCapabilities, ReferenceParams,
    RenameClientCapabilities, RenameFilesParams, RenameParams, SelectionRange,
    SelectionRangeParams, SemanticTokensParams, SemanticTokensRangeParams,
    SemanticTokensRangeResult, SemanticTokensResult, ServerCapabilities, ShowMessageParams,
    ShowMessageRequestClientCapabilities, SignatureHelp, SignatureHelpParams, TagSupport,
    TextDocumentClientCapabilities, TextDocumentPositionParams, TextEdit, TypeHierarchyItem,
    TypeHierarchyPrepareParams, TypeHierarchySubtypesParams, TypeHierarchySupertypesParams, Uri,
    WindowClientCapabilities, WorkDoneProgress, WorkspaceClientCapabilities, WorkspaceEdit,
    WorkspaceSymbolClientCapabilities, WorkspaceSymbolParams, WorkspaceSymbolResponse,
};
use serde::{de::DeserializeOwned, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    io::{self, BufRead, BufReader, Read, Write},
    path::Path,
    process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio},
    sync::{
        atomic::{AtomicI64, Ordering},
        mpsc::{self, Receiver, SyncSender},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use stoat_log::TextProtoLog;
use tokio::sync::{
    mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender},
    oneshot, Mutex as TokioMutex, Notify,
};

/// How long a server has to answer `shutdown` on quit before the editor sends
/// `exit` regardless.
const SHUTDOWN_REQUEST_TIMEOUT: Duration = Duration::from_millis(250);

/// How long a server has to exit on its own after `exit` before it is killed.
const REAP_TIMEOUT: Duration = Duration::from_millis(250);

/// How often the reap looks, so a server that goes promptly is noticed rather
/// than waited out.
const REAP_POLL: Duration = Duration::from_millis(10);

/// How many outgoing bodies may wait on the writer thread before senders block.
///
/// Deep enough that a burst of syncs and requests never waits on a server that
/// is keeping up, and bounded so one that has stopped reading its stdin cannot
/// grow the queue without limit.
const WRITE_QUEUE_DEPTH: usize = 256;

type PendingMap = Arc<Mutex<HashMap<i64, oneshot::Sender<Result<Value, LspResponseError>>>>>;

/// Byte-faithful JSONL transcripts of the two protocol directions, enabled
/// by the `text_proto_log` setting.
///
/// `tx` records every frame stoat writes to the server's stdin. `rx` records
/// every frame read from the server's stdout, including frames that fail to
/// parse, so the transcript is a faithful record of the raw stream rather
/// than only the messages that decoded.
pub struct LspTranscript {
    pub tx: TextProtoLog,
    pub rx: TextProtoLog,
}

impl LspTranscript {
    /// Create the paired protocol transcripts for `text_proto_log`, named to
    /// sort beside the editor's own log.
    ///
    /// Writes `<stem>.tx.jsonl` (frames sent to the server) and `<stem>.rx.jsonl`
    /// (frames received) under the shared log directory, creating it if it does
    /// not exist. The stem is `<process log stem>-lsp-<server slug>` when a log
    /// identity is installed, so it shares the editor log's filename prefix, and
    /// `lsp-<pid>-<server slug>` otherwise.
    pub fn create(server: &str) -> io::Result<Self> {
        let dir = stoat_log::log_dir()?;
        std::fs::create_dir_all(&dir)?;
        let slug = transcript_slug(server);
        let process_stem = stoat_log::ident::get().map(|i| i.file_stem.as_str());
        let stem = transcript_stem(process_stem, std::process::id(), &slug);
        let tx = TextProtoLog::create_at(&dir.join(format!("{stem}.tx.jsonl")))?;
        let rx = TextProtoLog::create_at(&dir.join(format!("{stem}.rx.jsonl")))?;
        Ok(LspTranscript { tx, rx })
    }
}

/// A filesystem-safe slug for `server`, so several servers' transcripts under
/// one pid do not collide. Non-alphanumeric characters become `-`.
fn transcript_slug(server: &str) -> String {
    server
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// The transcript filename stem for a server, minus the direction and `.jsonl`
/// extension.
///
/// When the process installed a log identity, `process_stem` is its log file
/// stem (e.g. `stoatty-<sid>-stoat-<id>`), so the transcript sorts beside the
/// editor's own log as `<process_stem>-lsp-<slug>`. Without one (library
/// callers, tests), it falls back to `lsp-<pid>-<slug>`.
fn transcript_stem(process_stem: Option<&str>, pid: u32, slug: &str) -> String {
    match process_stem {
        Some(stem) => format!("{stem}-lsp-{slug}"),
        None => format!("lsp-{pid}-{slug}"),
    }
}

/// A language server running as a child process, addressed over stdio JSON-RPC.
///
/// Construct with [`Self::spawn`] to start the process and its reader threads,
/// then drive the handshake with [`LspHost::initialize`] before sending any
/// other traffic. The struct owns the child, so dropping it leaves the process
/// running until [`LspHost::shutdown`] reaps it. Production keeps the host in an
/// `Arc` for the app's lifetime.
pub struct LocalLsp {
    /// Bodies queued for the writer thread, which frames and writes each one.
    ///
    /// Writing to the child's stdin blocks once it stops draining its pipe, and
    /// a server busy indexing does exactly that, so the write happens on a
    /// thread of its own rather than wherever the sending task happened to be
    /// scheduled. The queue is bounded, so a server that never drains
    /// backpressures the senders instead of growing without limit.
    writer_tx: SyncSender<Vec<u8>>,
    child: Mutex<Child>,
    next_id: AtomicI64,
    pending: PendingMap,
    notif_rx: TokioMutex<UnboundedReceiver<LspNotification>>,
    incoming_rx: TokioMutex<UnboundedReceiver<IncomingRequest>>,
    capabilities: Mutex<Arc<ServerCapabilities>>,
    /// Outgoing-frame transcript, `Some` when `text_proto_log` is on. The
    /// paired incoming transcript lives in the reader thread.
    tx_transcript: Option<TextProtoLog>,
}

impl LocalLsp {
    /// Spawn `command` with `args` as a child process wired for stdio JSON-RPC.
    ///
    /// The child runs in `cwd` with `env` applied over the inherited
    /// environment. Each `Some` sets the variable, each `None` unsets it, which
    /// is how the workspace's project environment reaches the server.
    ///
    /// Starts the reader thread (stdout to the channels) and a stderr thread
    /// (server logs to the `stoat::lsp` tracing target). The reader signals
    /// `wake` on server-pushed traffic so the run loop drains it promptly. The
    /// returned host has not handshaked yet. Call [`LspHost::initialize`] next.
    /// Fails only if the process cannot be spawned.
    pub fn spawn(
        command: &str,
        args: &[String],
        env: &[(String, Option<String>)],
        cwd: &Path,
        transcript: Option<LspTranscript>,
        wake: Arc<Notify>,
    ) -> io::Result<Self> {
        let mut cmd = Command::new(command);
        cmd.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .current_dir(cwd);
        for (key, value) in env {
            match value {
                Some(value) => cmd.env(key, value),
                None => cmd.env_remove(key),
            };
        }
        let mut child = cmd.spawn()?;

        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");

        let (notif_tx, notif_rx) = unbounded_channel();
        let (incoming_tx, incoming_rx) = unbounded_channel();
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));

        let (tx_transcript, rx_transcript) = match transcript {
            Some(transcript) => (Some(transcript.tx), Some(transcript.rx)),
            None => (None, None),
        };

        std::thread::spawn({
            let pending = pending.clone();
            move || reader_loop(stdout, pending, notif_tx, incoming_tx, rx_transcript, wake)
        });
        std::thread::spawn(move || stderr_loop(stderr));

        let (writer_tx, writer_rx) = mpsc::sync_channel(WRITE_QUEUE_DEPTH);
        std::thread::spawn({
            let pending = pending.clone();
            move || writer_loop(stdin, writer_rx, pending)
        });

        Ok(Self {
            writer_tx,
            child: Mutex::new(child),
            next_id: AtomicI64::new(1),
            pending,
            notif_rx: TokioMutex::new(notif_rx),
            incoming_rx: TokioMutex::new(incoming_rx),
            capabilities: Mutex::new(Arc::new(ServerCapabilities::default())),
            tx_transcript,
        })
    }

    /// Send a JSON-RPC request and await the correlated response, deserializing
    /// its `result` into `R`. A server error envelope becomes an `io::Error`. A
    /// closed transport (server gone) becomes [`io::ErrorKind::BrokenPipe`].
    async fn request<P, R>(&self, method: &str, params: P) -> io::Result<R>
    where
        P: Serialize,
        R: DeserializeOwned,
    {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .expect("lsp pending map poisoned")
            .insert(id, tx);

        let body = serde_json::to_vec(&Envelope {
            jsonrpc: "2.0",
            id: Some(id),
            method,
            params,
        });
        let sent = body
            .map_err(io::Error::from)
            .and_then(|body| self.write_message(body));
        if let Err(err) = sent {
            self.pending
                .lock()
                .expect("lsp pending map poisoned")
                .remove(&id);
            return Err(err);
        }

        match rx.await {
            Ok(Ok(value)) => serde_json::from_value(value)
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err)),
            Ok(Err(err)) => Err(io::Error::other(format!(
                "lsp error {}: {}",
                err.code, err.message
            ))),
            Err(_) => Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "language server closed",
            )),
        }
    }

    /// Send a fire-and-forget JSON-RPC notification (no id, no response).
    fn notify<P: Serialize>(&self, method: &str, params: P) -> io::Result<()> {
        let body = serde_json::to_vec(&Envelope {
            jsonrpc: "2.0",
            id: None,
            method,
            params,
        })?;
        self.write_message(body)
    }

    /// Hand `body` to the writer thread, which frames and writes it.
    ///
    /// Returns once the body is queued, not once the server has it. A write that
    /// then fails takes the whole transport down, and every request waiting on
    /// one fails with the same closed-transport error a dead server gives.
    fn write_message(&self, body: Vec<u8>) -> io::Result<()> {
        if let Some(transcript) = &self.tx_transcript {
            transcript.record(&String::from_utf8_lossy(&body));
        }
        self.writer_tx
            .send(body)
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "language server closed"))
    }
}

/// A JSON-RPC message on its way out, serialized straight from the typed params.
///
/// Building a [`Value`] tree first materializes the whole payload as a map of
/// boxed nodes and then serializes that, so params large enough to matter --
/// a full document sync, a semantic-token request -- are built twice over
/// before a byte reaches the pipe.
///
/// Declaration order is wire order, and this order is the one the map these
/// replaced serialized its keys in, so the bytes a server reads are unchanged.
#[derive(Serialize)]
struct Envelope<'a, P> {
    /// Absent on a notification, which is what distinguishes one from a request.
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<i64>,
    jsonrpc: &'static str,
    method: &'a str,
    params: P,
}

#[async_trait]
impl LspHost for LocalLsp {
    fn capabilities(&self) -> Arc<ServerCapabilities> {
        self.capabilities
            .lock()
            .expect("lsp capabilities poisoned")
            .clone()
    }

    async fn initialize(&self, root_uri: Option<Uri>) -> io::Result<InitializeResult> {
        // root_uri is deprecated upstream in favor of workspace_folders, but it
        // is the field this trait's contract hands us and every server still
        // accepts it.
        #[allow(deprecated)]
        let params = InitializeParams {
            process_id: Some(std::process::id()),
            root_uri,
            capabilities: client_capabilities(),
            ..Default::default()
        };
        let result: InitializeResult = self.request("initialize", params).await?;
        *self.capabilities.lock().expect("lsp capabilities poisoned") =
            Arc::new(result.capabilities.clone());
        self.notify("initialized", InitializedParams {})?;
        Ok(result)
    }

    async fn shutdown(&self) -> io::Result<()> {
        let _ = tokio::time::timeout(
            SHUTDOWN_REQUEST_TIMEOUT,
            self.request::<_, Value>("shutdown", Value::Null),
        )
        .await;
        let _ = self.notify("exit", Value::Null);
        reap_child(&self.child, REAP_TIMEOUT, REAP_POLL).await;
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) -> io::Result<()> {
        self.notify("textDocument/didOpen", params)
    }
    async fn did_change(&self, params: DidChangeTextDocumentParams) -> io::Result<()> {
        self.notify("textDocument/didChange", params)
    }
    async fn did_save(&self, params: DidSaveTextDocumentParams) -> io::Result<()> {
        self.notify("textDocument/didSave", params)
    }
    async fn did_close(&self, params: DidCloseTextDocumentParams) -> io::Result<()> {
        self.notify("textDocument/didClose", params)
    }
    async fn did_rename(&self, params: RenameFilesParams) -> io::Result<()> {
        self.notify("workspace/didRenameFiles", params)
    }
    async fn did_change_watched_files(
        &self,
        params: DidChangeWatchedFilesParams,
    ) -> io::Result<()> {
        self.notify("workspace/didChangeWatchedFiles", params)
    }
    async fn did_change_configuration(
        &self,
        params: DidChangeConfigurationParams,
    ) -> io::Result<()> {
        self.notify("workspace/didChangeConfiguration", params)
    }
    async fn did_change_workspace_folders(
        &self,
        params: DidChangeWorkspaceFoldersParams,
    ) -> io::Result<()> {
        self.notify("workspace/didChangeWorkspaceFolders", params)
    }

    async fn hover(&self, params: HoverParams) -> io::Result<Option<Hover>> {
        self.request("textDocument/hover", params).await
    }
    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> io::Result<Option<GotoDefinitionResponse>> {
        self.request("textDocument/definition", params).await
    }
    async fn goto_declaration(
        &self,
        params: GotoDefinitionParams,
    ) -> io::Result<Option<GotoDefinitionResponse>> {
        self.request("textDocument/declaration", params).await
    }
    async fn goto_type_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> io::Result<Option<GotoDefinitionResponse>> {
        self.request("textDocument/typeDefinition", params).await
    }
    async fn goto_implementation(
        &self,
        params: GotoDefinitionParams,
    ) -> io::Result<Option<GotoDefinitionResponse>> {
        self.request("textDocument/implementation", params).await
    }
    async fn references(&self, params: ReferenceParams) -> io::Result<Option<Vec<Location>>> {
        self.request("textDocument/references", params).await
    }
    async fn document_highlight(
        &self,
        params: DocumentHighlightParams,
    ) -> io::Result<Option<Vec<DocumentHighlight>>> {
        self.request("textDocument/documentHighlight", params).await
    }

    async fn completion(&self, params: CompletionParams) -> io::Result<Option<CompletionResponse>> {
        self.request("textDocument/completion", params).await
    }
    async fn completion_resolve(&self, item: CompletionItem) -> io::Result<CompletionItem> {
        self.request("completionItem/resolve", item).await
    }

    async fn code_action(
        &self,
        params: CodeActionParams,
    ) -> io::Result<Option<Vec<CodeActionOrCommand>>> {
        self.request("textDocument/codeAction", params).await
    }
    async fn code_action_resolve(&self, action: CodeAction) -> io::Result<CodeAction> {
        self.request("codeAction/resolve", action).await
    }
    async fn document_link(
        &self,
        params: DocumentLinkParams,
    ) -> io::Result<Option<Vec<DocumentLink>>> {
        self.request("textDocument/documentLink", params).await
    }
    async fn document_link_resolve(&self, link: DocumentLink) -> io::Result<DocumentLink> {
        self.request("documentLink/resolve", link).await
    }
    async fn document_color(
        &self,
        params: DocumentColorParams,
    ) -> io::Result<Option<Vec<ColorInformation>>> {
        self.request("textDocument/documentColor", params).await
    }
    async fn color_presentation(
        &self,
        params: ColorPresentationParams,
    ) -> io::Result<Option<Vec<ColorPresentation>>> {
        self.request("textDocument/colorPresentation", params).await
    }
    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> io::Result<Option<SemanticTokensResult>> {
        self.request("textDocument/semanticTokens/full", params)
            .await
    }
    async fn semantic_tokens_range(
        &self,
        params: SemanticTokensRangeParams,
    ) -> io::Result<Option<SemanticTokensRangeResult>> {
        self.request("textDocument/semanticTokens/range", params)
            .await
    }
    async fn prepare_call_hierarchy(
        &self,
        params: CallHierarchyPrepareParams,
    ) -> io::Result<Option<Vec<CallHierarchyItem>>> {
        self.request("textDocument/prepareCallHierarchy", params)
            .await
    }
    async fn call_hierarchy_incoming_calls(
        &self,
        params: CallHierarchyIncomingCallsParams,
    ) -> io::Result<Option<Vec<CallHierarchyIncomingCall>>> {
        self.request("callHierarchy/incomingCalls", params).await
    }
    async fn call_hierarchy_outgoing_calls(
        &self,
        params: CallHierarchyOutgoingCallsParams,
    ) -> io::Result<Option<Vec<CallHierarchyOutgoingCall>>> {
        self.request("callHierarchy/outgoingCalls", params).await
    }
    async fn prepare_type_hierarchy(
        &self,
        params: TypeHierarchyPrepareParams,
    ) -> io::Result<Option<Vec<TypeHierarchyItem>>> {
        self.request("textDocument/prepareTypeHierarchy", params)
            .await
    }
    async fn type_hierarchy_supertypes(
        &self,
        params: TypeHierarchySupertypesParams,
    ) -> io::Result<Option<Vec<TypeHierarchyItem>>> {
        self.request("typeHierarchy/supertypes", params).await
    }
    async fn type_hierarchy_subtypes(
        &self,
        params: TypeHierarchySubtypesParams,
    ) -> io::Result<Option<Vec<TypeHierarchyItem>>> {
        self.request("typeHierarchy/subtypes", params).await
    }
    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> io::Result<Option<DocumentSymbolResponse>> {
        self.request("textDocument/documentSymbol", params).await
    }
    async fn document_diagnostic(
        &self,
        params: DocumentDiagnosticParams,
    ) -> io::Result<Option<DocumentDiagnosticReportResult>> {
        self.request("textDocument/diagnostic", params).await
    }
    async fn folding_range(
        &self,
        params: FoldingRangeParams,
    ) -> io::Result<Option<Vec<FoldingRange>>> {
        self.request("textDocument/foldingRange", params).await
    }
    async fn selection_range(
        &self,
        params: SelectionRangeParams,
    ) -> io::Result<Option<Vec<SelectionRange>>> {
        self.request("textDocument/selectionRange", params).await
    }
    async fn workspace_symbol(
        &self,
        params: WorkspaceSymbolParams,
    ) -> io::Result<Option<WorkspaceSymbolResponse>> {
        self.request("workspace/symbol", params).await
    }
    async fn signature_help(
        &self,
        params: SignatureHelpParams,
    ) -> io::Result<Option<SignatureHelp>> {
        self.request("textDocument/signatureHelp", params).await
    }
    async fn inlay_hint(&self, params: InlayHintParams) -> io::Result<Option<Vec<InlayHint>>> {
        self.request("textDocument/inlayHint", params).await
    }
    async fn inlay_hint_resolve(&self, hint: InlayHint) -> io::Result<InlayHint> {
        self.request("inlayHint/resolve", hint).await
    }
    async fn range_inlay_hint(
        &self,
        params: InlayHintParams,
    ) -> io::Result<Option<Vec<InlayHint>>> {
        self.request("textDocument/inlayHint", params).await
    }

    async fn prepare_rename(
        &self,
        params: TextDocumentPositionParams,
    ) -> io::Result<Option<PrepareRenameResponse>> {
        self.request("textDocument/prepareRename", params).await
    }
    async fn rename(&self, params: RenameParams) -> io::Result<Option<WorkspaceEdit>> {
        self.request("textDocument/rename", params).await
    }
    async fn formatting(
        &self,
        params: DocumentFormattingParams,
    ) -> io::Result<Option<Vec<TextEdit>>> {
        self.request("textDocument/formatting", params).await
    }
    async fn range_formatting(
        &self,
        params: DocumentRangeFormattingParams,
    ) -> io::Result<Option<Vec<TextEdit>>> {
        self.request("textDocument/rangeFormatting", params).await
    }
    async fn will_rename(&self, params: RenameFilesParams) -> io::Result<Option<WorkspaceEdit>> {
        self.request("workspace/willRenameFiles", params).await
    }
    async fn execute_command(&self, params: ExecuteCommandParams) -> io::Result<Option<Value>> {
        self.request("workspace/executeCommand", params).await
    }

    async fn recv_notification(&self) -> Option<LspNotification> {
        self.notif_rx.lock().await.recv().await
    }
    async fn try_recv_notification(&self) -> Option<LspNotification> {
        self.notif_rx.try_lock().ok()?.try_recv().ok()
    }
    async fn recv_incoming_request(&self) -> Option<IncomingRequest> {
        self.incoming_rx.lock().await.recv().await
    }
    async fn try_recv_incoming_request(&self) -> Option<IncomingRequest> {
        self.incoming_rx.try_lock().ok()?.try_recv().ok()
    }
    async fn reply(
        &self,
        id: NumberOrString,
        result: Result<Value, LspResponseError>,
    ) -> io::Result<()> {
        // A reply carries no method and so does not fit the request envelope,
        // and the payloads a client answers with are small enough that going
        // through a `Value` costs nothing measurable.
        let message = match result {
            Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
            Err(err) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": err.code, "message": err.message, "data": err.data },
            }),
        };
        self.write_message(serde_json::to_vec(&message)?)
    }
}

/// Read framed JSON-RPC off the child's stdout until EOF, routing each message
/// to the pending map (responses) or the notification / incoming-request
/// channels. Runs on a dedicated OS thread because the read is blocking.
///
/// Server-pushed notifications and incoming requests signal `wake` so the run
/// loop, which drains those channels only when it wakes, applies them without
/// waiting for the next keystroke. `notify_one` collapses a burst into a single
/// permit, so heavy push traffic wakes the loop at most once per drained batch.
fn reader_loop(
    stdout: ChildStdout,
    pending: PendingMap,
    notif_tx: UnboundedSender<LspNotification>,
    incoming_tx: UnboundedSender<IncomingRequest>,
    rx_transcript: Option<TextProtoLog>,
    wake: Arc<Notify>,
) {
    let mut reader = BufReader::new(stdout);
    let mut decoder = FrameDecoder::new();
    let mut buf = [0u8; 4096];
    let mut active_progress = HashSet::new();
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        decoder.push(&buf[..n]);
        while let Some(body) = decoder.next_body() {
            if let Some(transcript) = &rx_transcript {
                transcript.record(&String::from_utf8_lossy(&body));
            }
            let Ok(message) = serde_json::from_slice::<Value>(&body) else {
                tracing::warn!(target: "stoat::lsp", "dropping unparseable lsp frame");
                continue;
            };
            let routed = classify(message);
            let wakes = wakes_for(&routed, &mut active_progress);
            match routed {
                Routed::Response { id, result } => {
                    if let Some(tx) = pending
                        .lock()
                        .expect("lsp pending map poisoned")
                        .remove(&id)
                    {
                        let _ = tx.send(result);
                    }
                },
                Routed::Notification(notification) => {
                    if notif_tx.send(notification).is_err() {
                        return;
                    }
                },
                Routed::Incoming(request) => {
                    if incoming_tx.send(request).is_err() {
                        return;
                    }
                },
                Routed::Ignore => {},
            }
            if wakes {
                wake.notify_one();
            }
        }
    }
}

/// Frame and write each queued body to the child's stdin until the queue closes
/// or the pipe breaks. Runs on a dedicated OS thread because the write blocks.
///
/// A server stops draining its 64KB stdin pipe while it is busy, and the write
/// that fills it blocks until it resumes. On an executor thread that stalls
/// whatever else was scheduled there, so the block is confined to this thread.
///
/// Every waiting request is failed on the way out. A body that never reached the
/// server has no response coming, and the caller would otherwise wait on a
/// oneshot nothing will ever send.
fn writer_loop(mut stdin: ChildStdin, bodies: Receiver<Vec<u8>>, pending: PendingMap) {
    while let Ok(body) = bodies.recv() {
        if write_framed(&mut stdin, &body)
            .and_then(|()| stdin.flush())
            .is_err()
        {
            break;
        }
    }
    pending.lock().expect("lsp pending map poisoned").clear();
}

/// Forward the child's stderr lines to the `stoat::lsp` tracing target. Language
/// servers log to stderr, so this keeps that stream visible without mixing it
/// into the protocol channel.
fn stderr_loop(stderr: ChildStderr) {
    for line in BufReader::new(stderr).lines() {
        match line {
            Ok(line) => tracing::debug!(target: "stoat::lsp", "{line}"),
            Err(_) => break,
        }
    }
}

/// Give `child` up to `timeout` to exit on its own, then kill it. Reports
/// whether it went by itself.
///
/// Polled every `poll` rather than waited out, so a server that honors `exit`
/// costs the editor's quit only as long as it actually takes. The kill runs on
/// this side of the bound, which a sleep ahead of it cannot promise: whatever
/// bounds the caller cuts the sleep short and takes the kill with it.
///
/// The kill is followed by a wait, since a signal alone leaves the process a
/// zombie for as long as the editor outlives it. That wait returns at once,
/// the child having just been killed.
async fn reap_child(child: &Mutex<Child>, timeout: Duration, poll: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        let exited = {
            let mut child = child.lock().expect("lsp child poisoned");
            matches!(child.try_wait(), Ok(Some(_)))
        };
        if exited {
            return true;
        }
        if Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(poll).await;
    }

    let mut child = child.lock().expect("lsp child poisoned");
    let _ = child.kill();
    let _ = child.wait();
    false
}

/// Write `body` framed with the `Content-Length` header the LSP base protocol
/// requires.
///
/// The header and body go as separate writes rather than through one joined
/// buffer, so a payload of any size is not copied again just to carry a
/// twenty-odd byte prefix.
fn write_framed(out: &mut impl Write, body: &[u8]) -> io::Result<()> {
    write!(out, "Content-Length: {}\r\n\r\n", body.len())?;
    out.write_all(body)
}

/// Reassembles JSON-RPC message bodies from a byte stream that may split a
/// header across reads or pack several messages into one read.
struct FrameDecoder {
    buf: Vec<u8>,
}

impl FrameDecoder {
    fn new() -> Self {
        Self { buf: Vec::new() }
    }

    fn push(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    /// Pull the next complete body out of the buffer, or `None` when more bytes
    /// are needed. A header with no parseable `Content-Length` is skipped past
    /// so a single bad frame cannot wedge the stream.
    fn next_body(&mut self) -> Option<Vec<u8>> {
        let sep = find_subslice(&self.buf, b"\r\n\r\n")?;
        let Some(len) = parse_content_length(&self.buf[..sep]) else {
            tracing::warn!(target: "stoat::lsp", "lsp header without Content-Length");
            self.buf.drain(..sep + 4);
            return self.next_body();
        };
        let start = sep + 4;
        if self.buf.len() < start + len {
            return None;
        }
        let body = self.buf[start..start + len].to_vec();
        self.buf.drain(..start + len);
        Some(body)
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn parse_content_length(header: &[u8]) -> Option<usize> {
    let header = std::str::from_utf8(header).ok()?;
    for line in header.split("\r\n") {
        let (name, value) = line.split_once(':')?;
        if name.trim().eq_ignore_ascii_case("content-length") {
            return value.trim().parse().ok();
        }
    }
    None
}

/// The destination a decoded JSON-RPC message routes to. A response returns to
/// its awaiting caller, a notification or incoming request goes out its own
/// channel, and an unroutable message is dropped.
enum Routed {
    Response {
        id: i64,
        result: Result<Value, LspResponseError>,
    },
    Notification(LspNotification),
    Incoming(IncomingRequest),
    Ignore,
}

impl Routed {
    /// Whether delivering this message has to wake the run loop.
    ///
    /// Any wake costs a whole-screen repaint, so a message nothing paints from
    /// is one the reader delivers quietly. A server logging its way through an
    /// index pass emits those without pause.
    ///
    /// The message is still sent either way. It waits in its channel and is
    /// drained on the next pass some other event drives, so a skipped wake
    /// delays the trace line rather than losing it.
    fn warrants_wake(&self) -> bool {
        match self {
            // The oneshot the response goes out on wakes its awaiting task
            // directly, and that task was spawned against the redraw notify.
            Routed::Response { .. } | Routed::Ignore => false,
            Routed::Notification(notification) => !matches!(
                notification,
                LspNotification::LogMessage { .. } | LspNotification::LogTrace { .. }
            ),
            Routed::Incoming(_) => true,
        }
    }
}

/// [`Routed::warrants_wake`], with the progress tokens already on screen taken
/// into account.
///
/// A report on a token that is showing is the one message a wake buys nothing
/// for. Showing any progress arms the frame timer, and each spinner phase flip
/// merges a redraw, so the report's own text reaches the screen on a frame that
/// was already coming. A cold index sends several per phase, and each one was
/// costing a whole-screen paint.
///
/// Begin and end change whether anything is shown at all, so they wake. So does
/// a report on a token no begin arrived for, because the progress handling
/// synthesizes an entry from it and that entry is what starts the spinner. Left
/// quiet, nothing would arm the timer for it to be drawn on.
///
/// `active` records the tokens showing on this connection, which is the right
/// scope. Progress entries are keyed by server as well as token.
fn wakes_for(routed: &Routed, active: &mut HashSet<ProgressToken>) -> bool {
    let Routed::Notification(LspNotification::Progress { token, value }) = routed else {
        return routed.warrants_wake();
    };

    match value {
        WorkDoneProgress::Begin(_) => {
            active.insert(token.clone());
            true
        },
        WorkDoneProgress::End(_) => {
            active.remove(token);
            true
        },
        // Inserting answers the question it asks. It reports whether the token
        // was new, which is exactly when the report has to wake, and records it
        // either way so the ones after it stay quiet.
        WorkDoneProgress::Report(_) => active.insert(token.clone()),
    }
}

/// Classify one JSON-RPC message by its shape. An `id` without a `method` is a
/// response to us. A `method` with an `id` is a server request. A `method`
/// without an `id` is a notification.
fn classify(message: Value) -> Routed {
    // Taken apart rather than read through, so a result tree or a diagnostics
    // payload moves into the routed message instead of being copied out of one
    // this function is about to drop.
    let Value::Object(mut map) = message else {
        return Routed::Ignore;
    };
    let method = map.remove("method");
    let method = method.as_ref().and_then(Value::as_str);
    let id = map.remove("id");

    match (method, id) {
        (None, Some(id)) => {
            let Some(id) = id.as_i64() else {
                return Routed::Ignore;
            };
            match map.remove("error") {
                Some(error) => Routed::Response {
                    id,
                    result: Err(parse_response_error(&error)),
                },
                None => Routed::Response {
                    id,
                    result: Ok(map.remove("result").unwrap_or(Value::Null)),
                },
            }
        },
        (Some(method), Some(id)) => {
            let Ok(id) = serde_json::from_value::<NumberOrString>(id) else {
                return Routed::Ignore;
            };
            let params = map.remove("params").unwrap_or(Value::Null);
            Routed::Incoming(classify_incoming(id, method, params))
        },
        (Some(method), None) => match classify_notification(method, map.remove("params")) {
            Some(notification) => Routed::Notification(notification),
            None => Routed::Ignore,
        },
        (None, None) => Routed::Ignore,
    }
}

fn parse_response_error(error: &Value) -> LspResponseError {
    LspResponseError {
        code: error.get("code").and_then(Value::as_i64).unwrap_or(0),
        message: error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        data: error.get("data").cloned(),
    }
}

fn classify_incoming(id: NumberOrString, method: &str, params: Value) -> IncomingRequest {
    match method {
        "window/showMessageRequest" => decode(id, method, params, |id, params| {
            IncomingRequest::ShowMessageRequest { id, params }
        }),
        "window/workDoneProgress/create" => decode(id, method, params, |id, params| {
            IncomingRequest::WorkDoneProgressCreate { id, params }
        }),
        "client/registerCapability" => decode(id, method, params, |id, params| {
            IncomingRequest::RegisterCapability { id, params }
        }),
        "client/unregisterCapability" => decode(id, method, params, |id, params| {
            IncomingRequest::UnregisterCapability { id, params }
        }),
        "workspace/configuration" => decode(id, method, params, |id, params| {
            IncomingRequest::WorkspaceConfiguration { id, params }
        }),
        "workspace/applyEdit" => decode(id, method, params, |id, params| {
            IncomingRequest::WorkspaceApplyEdit { id, params }
        }),
        _ => IncomingRequest::Unknown {
            id,
            method: method.to_owned(),
            params,
        },
    }
}

/// Deserialize `params` into the typed request `build` wants, falling back to
/// [`IncomingRequest::Unknown`] when they do not fit.
///
/// The fallback carries no params. Deserializing consumes them, and keeping
/// them for this path would mean cloning the payload of every request that does
/// parse. Nothing reads them: an `Unknown` is answered "method not found"
/// whatever it holds.
fn decode<T: DeserializeOwned>(
    id: NumberOrString,
    method: &str,
    params: Value,
    build: impl FnOnce(NumberOrString, T) -> IncomingRequest,
) -> IncomingRequest {
    match serde_json::from_value(params) {
        Ok(params) => build(id, params),
        Err(_) => IncomingRequest::Unknown {
            id,
            method: method.to_owned(),
            params: Value::Null,
        },
    }
}

fn classify_notification(method: &str, params: Option<Value>) -> Option<LspNotification> {
    let params = params?;
    match method {
        "textDocument/publishDiagnostics" => {
            let params: PublishDiagnosticsParams = serde_json::from_value(params).ok()?;
            Some(LspNotification::Diagnostics {
                uri: params.uri,
                diagnostics: params.diagnostics,
                version: params.version,
            })
        },
        "$/progress" => {
            let params: ProgressParams = serde_json::from_value(params).ok()?;
            let ProgressParamsValue::WorkDone(value) = params.value;
            Some(LspNotification::Progress {
                token: params.token,
                value,
            })
        },
        "window/logMessage" => {
            let params: LogMessageParams = serde_json::from_value(params).ok()?;
            Some(LspNotification::LogMessage {
                typ: params.typ,
                message: params.message,
            })
        },
        "window/showMessage" => {
            let params: ShowMessageParams = serde_json::from_value(params).ok()?;
            Some(LspNotification::ShowMessage {
                typ: params.typ,
                message: params.message,
            })
        },
        "$/logTrace" => {
            let message = params.get("message")?.as_str()?.to_owned();
            let verbose = params
                .get("verbose")
                .and_then(Value::as_str)
                .map(str::to_owned);
            Some(LspNotification::LogTrace { message, verbose })
        },
        _ => None,
    }
}

/// The client capabilities stoat advertises during initialization, matching the
/// features its action layer actually consumes.
fn client_capabilities() -> ClientCapabilities {
    ClientCapabilities {
        workspace: Some(WorkspaceClientCapabilities {
            configuration: Some(true),
            apply_edit: Some(true),
            workspace_folders: Some(true),
            execute_command: Some(DynamicRegistrationClientCapabilities {
                dynamic_registration: Some(false),
            }),
            symbol: Some(WorkspaceSymbolClientCapabilities {
                dynamic_registration: Some(false),
                ..Default::default()
            }),
            did_change_watched_files: Some(DidChangeWatchedFilesClientCapabilities {
                dynamic_registration: Some(true),
                relative_pattern_support: Some(true),
            }),
            ..Default::default()
        }),
        text_document: Some(TextDocumentClientCapabilities {
            completion: Some(CompletionClientCapabilities {
                completion_item: Some(CompletionItemCapability {
                    snippet_support: Some(true),
                    insert_replace_support: Some(true),
                    resolve_support: Some(CompletionItemCapabilityResolveSupport {
                        properties: vec![
                            "documentation".to_owned(),
                            "detail".to_owned(),
                            "additionalTextEdits".to_owned(),
                        ],
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            hover: Some(HoverClientCapabilities {
                content_format: Some(vec![MarkupKind::Markdown, MarkupKind::PlainText]),
                ..Default::default()
            }),
            definition: Some(GotoCapability {
                link_support: Some(true),
                ..Default::default()
            }),
            type_definition: Some(GotoCapability {
                link_support: Some(true),
                ..Default::default()
            }),
            implementation: Some(GotoCapability {
                link_support: Some(true),
                ..Default::default()
            }),
            references: Some(ReferenceClientCapabilities {
                dynamic_registration: Some(false),
            }),
            document_symbol: Some(DocumentSymbolClientCapabilities {
                hierarchical_document_symbol_support: Some(true),
                ..Default::default()
            }),
            code_action: Some(CodeActionClientCapabilities {
                code_action_literal_support: Some(CodeActionLiteralSupport {
                    code_action_kind: CodeActionKindLiteralSupport {
                        value_set: vec![
                            "".to_owned(),
                            "quickfix".to_owned(),
                            "refactor".to_owned(),
                            "refactor.extract".to_owned(),
                            "refactor.inline".to_owned(),
                            "refactor.rewrite".to_owned(),
                            "source".to_owned(),
                            "source.organizeImports".to_owned(),
                        ],
                    },
                }),
                data_support: Some(true),
                resolve_support: Some(CodeActionCapabilityResolveSupport {
                    properties: vec!["edit".to_owned()],
                }),
                ..Default::default()
            }),
            rename: Some(RenameClientCapabilities {
                prepare_support: Some(true),
                ..Default::default()
            }),
            range_formatting: Some(DynamicRegistrationClientCapabilities {
                dynamic_registration: Some(false),
            }),
            publish_diagnostics: Some(PublishDiagnosticsClientCapabilities {
                version_support: Some(true),
                tag_support: Some(TagSupport {
                    value_set: vec![DiagnosticTag::UNNECESSARY],
                }),
                ..Default::default()
            }),
            inlay_hint: Some(InlayHintClientCapabilities {
                dynamic_registration: Some(false),
                resolve_support: None,
            }),
            ..Default::default()
        }),
        window: Some(WindowClientCapabilities {
            work_done_progress: Some(true),
            show_message: Some(ShowMessageRequestClientCapabilities {
                message_action_item: None,
            }),
            ..Default::default()
        }),
        general: Some(GeneralClientCapabilities {
            position_encodings: Some(vec![
                PositionEncodingKind::UTF8,
                PositionEncodingKind::UTF16,
            ]),
            ..Default::default()
        }),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        classify, client_capabilities, reap_child, transcript_slug, transcript_stem, wakes_for,
        write_framed, Command, DiagnosticTag, Duration, Envelope, FrameDecoder, HashSet, Instant,
        Mutex, Routed,
    };
    use crate::host::lsp::{IncomingRequest, LspNotification};
    use serde_json::{json, Value};

    /// `body` framed the way the transport frames it on its way to the child.
    fn encode_message(body: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        write_framed(&mut out, body).expect("writing to a vec cannot fail");
        out
    }

    fn decode_one(bytes: &[u8]) -> Vec<u8> {
        let mut decoder = FrameDecoder::new();
        decoder.push(bytes);
        decoder.next_body().expect("one complete body")
    }

    #[tokio::test]
    async fn a_server_that_ignores_exit_is_killed_and_waited_on() {
        let child = Mutex::new(
            Command::new("sleep")
                .arg("30")
                .spawn()
                .expect("spawn a child that outlives the bound"),
        );

        let started = Instant::now();
        let exited = reap_child(&child, Duration::from_millis(50), Duration::from_millis(10)).await;

        assert!(!exited, "the child was still running when the bound passed");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the reap returns on its own bound, not the child's lifetime"
        );
        assert!(
            matches!(
                child.lock().expect("child poisoned").try_wait(),
                Ok(Some(_))
            ),
            "the killed child is waited on rather than left a zombie"
        );
    }

    #[tokio::test]
    async fn a_server_that_exits_is_not_waited_out() {
        let child = Mutex::new(
            Command::new("true")
                .spawn()
                .expect("spawn a child that exits at once"),
        );

        let exited = reap_child(&child, Duration::from_secs(5), Duration::from_millis(10)).await;

        assert!(exited, "the child went on its own inside the bound");
    }

    #[test]
    fn transcript_slug_keeps_alphanumerics_and_dashes_the_rest() {
        assert_eq!(transcript_slug("rustanalyzer"), "rustanalyzer");
        assert_eq!(transcript_slug("rust-analyzer"), "rust-analyzer");
        assert_eq!(transcript_slug("clangd 15.0"), "clangd-15-0");
        assert_eq!(transcript_slug("a_b.c/d"), "a-b-c-d");
    }

    #[test]
    fn transcript_stem_reuses_the_process_stem_or_falls_back_to_pid() {
        assert_eq!(
            transcript_stem(Some("stoatty-a-stoat-b"), 12345, "rust-analyzer"),
            "stoatty-a-stoat-b-lsp-rust-analyzer"
        );
        assert_eq!(
            transcript_stem(None, 12345, "rust-analyzer"),
            "lsp-12345-rust-analyzer"
        );
    }

    #[test]
    fn framing_round_trips_a_body() {
        let body = br#"{"jsonrpc":"2.0","id":1}"#;
        assert_eq!(decode_one(&encode_message(body)), body);
    }

    /// Serializing the typed envelope replaces a `json!` tree, and a server
    /// reads the bytes rather than the type, so the document those bytes spell
    /// has to be the one this transport has always sent, field order included.
    /// The map it replaced serialized its keys sorted, so the envelope declares
    /// them in that order rather than the order they read naturally in.
    #[test]
    fn the_envelope_serializes_the_document_the_value_tree_did() {
        let params = json!({ "textDocument": { "uri": "file:///a.rs" }, "n": 1 });

        let request = serde_json::to_vec(&Envelope {
            jsonrpc: "2.0",
            id: Some(7),
            method: "textDocument/hover",
            params: &params,
        })
        .expect("serializes");
        assert_eq!(
            request,
            serde_json::to_vec(&json!({
                "jsonrpc": "2.0",
                "id": 7,
                "method": "textDocument/hover",
                "params": params,
            }))
            .expect("serializes"),
        );

        let notification = serde_json::to_vec(&Envelope {
            jsonrpc: "2.0",
            id: None,
            method: "textDocument/didOpen",
            params: &params,
        })
        .expect("serializes");
        assert_eq!(
            notification,
            serde_json::to_vec(&json!({
                "jsonrpc": "2.0",
                "method": "textDocument/didOpen",
                "params": params,
            }))
            .expect("serializes"),
            "a notification carries no id at all rather than a null one",
        );
    }

    #[test]
    fn decoder_reassembles_a_header_split_across_pushes() {
        let framed = encode_message(br#"{"id":7}"#);
        let (head, tail) = framed.split_at(6);

        let mut decoder = FrameDecoder::new();
        decoder.push(head);
        assert!(decoder.next_body().is_none(), "header alone is incomplete");
        decoder.push(tail);
        assert_eq!(decoder.next_body().unwrap(), br#"{"id":7}"#);
    }

    #[test]
    fn decoder_yields_two_messages_from_one_push() {
        let mut stream = encode_message(br#"{"id":1}"#);
        stream.extend_from_slice(&encode_message(br#"{"id":2}"#));

        let mut decoder = FrameDecoder::new();
        decoder.push(&stream);
        assert_eq!(decoder.next_body().unwrap(), br#"{"id":1}"#);
        assert_eq!(decoder.next_body().unwrap(), br#"{"id":2}"#);
        assert!(decoder.next_body().is_none());
    }

    #[test]
    fn classify_routes_a_response_to_its_id() {
        let message = json!({"jsonrpc": "2.0", "id": 5, "result": {"ok": true}});
        match classify(message) {
            Routed::Response { id, result } => {
                assert_eq!(id, 5);
                assert_eq!(result.unwrap(), json!({"ok": true}));
            },
            _ => panic!("expected a response"),
        }
    }

    #[test]
    fn classify_routes_an_error_response() {
        let message = json!({
            "jsonrpc": "2.0",
            "id": 9,
            "error": {"code": -32601, "message": "method not found"},
        });
        match classify(message) {
            Routed::Response {
                id,
                result: Err(err),
            } => {
                assert_eq!(id, 9);
                assert_eq!(err.code, -32601);
                assert_eq!(err.message, "method not found");
            },
            _ => panic!("expected an error response"),
        }
    }

    #[test]
    fn classify_routes_publish_diagnostics_as_a_notification() {
        let message = json!({
            "jsonrpc": "2.0",
            "method": "textDocument/publishDiagnostics",
            "params": {"uri": "file:///a.rs", "diagnostics": []},
        });
        match classify(message) {
            Routed::Notification(LspNotification::Diagnostics { diagnostics, .. }) => {
                assert!(diagnostics.is_empty());
            },
            _ => panic!("expected a diagnostics notification"),
        }
    }

    #[test]
    fn only_the_notifications_the_drain_traces_skip_the_wake() {
        let notification = |method: &str, params: Value| {
            json!({
                "jsonrpc": "2.0",
                "method": method,
                "params": params,
            })
        };
        let frames = [
            notification(
                "window/logMessage",
                json!({"type": 3, "message": "indexing"}),
            ),
            notification("$/logTrace", json!({"message": "textDocument/hover"})),
            notification(
                "textDocument/publishDiagnostics",
                json!({"uri": "file:///a.rs", "diagnostics": []}),
            ),
            notification("window/showMessage", json!({"type": 1, "message": "boom"})),
            notification("$/progress", json!({"token": 1, "value": {"kind": "end"}})),
            json!({
                "jsonrpc": "2.0",
                "id": 7,
                "method": "workspace/configuration",
                "params": {"items": []},
            }),
        ];

        let wakes: Vec<bool> = frames
            .into_iter()
            .map(|frame| classify(frame).warrants_wake())
            .collect();

        assert_eq!(
            wakes,
            [false, false, true, true, true, true],
            "only the log and trace notifications the drain traces skip the wake",
        );
    }

    #[test]
    fn a_progress_report_wakes_only_while_its_token_is_new() {
        let progress = |token: i32, value: Value| {
            json!({
                "jsonrpc": "2.0",
                "method": "$/progress",
                "params": {"token": token, "value": value},
            })
        };
        let report = json!({"kind": "report", "message": "crate 3/9"});
        let frames = [
            progress(1, json!({"kind": "begin", "title": "indexing"})),
            progress(1, report.clone()),
            progress(1, report.clone()),
            // No begin arrived for token 2, so the report is what synthesizes
            // its entry and starts the spinner the later ones ride on.
            progress(2, report.clone()),
            progress(2, report.clone()),
            progress(1, json!({"kind": "end"})),
            progress(1, report),
        ];

        let mut active = HashSet::new();
        let wakes: Vec<bool> = frames
            .into_iter()
            .map(|frame| wakes_for(&classify(frame), &mut active))
            .collect();

        assert_eq!(
            wakes,
            [true, false, false, true, false, true, true],
            "a report wakes only when it is the first sight of its token",
        );
    }

    #[test]
    fn client_capabilities_advertise_the_unnecessary_diagnostic_tag() {
        let tags = client_capabilities()
            .text_document
            .expect("text_document capabilities")
            .publish_diagnostics
            .expect("publish_diagnostics capability")
            .tag_support
            .expect("tag_support")
            .value_set;
        assert_eq!(tags, vec![DiagnosticTag::UNNECESSARY]);
    }

    #[test]
    fn classify_routes_a_known_server_request() {
        let message = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "workspace/configuration",
            "params": {"items": []},
        });
        assert!(matches!(
            classify(message),
            Routed::Incoming(IncomingRequest::WorkspaceConfiguration { .. })
        ));
    }

    #[test]
    fn classify_falls_back_to_unknown_for_unrecognized_requests() {
        let message = json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "workspace/somethingNew",
            "params": {"x": 1},
        });
        match classify(message) {
            Routed::Incoming(IncomingRequest::Unknown { method, params, .. }) => {
                assert_eq!(method, "workspace/somethingNew");
                assert_eq!(params, json!({"x": 1}), "params reach the fallback intact");
            },
            _ => panic!("expected an unknown request"),
        }
    }

    #[test]
    fn a_known_method_whose_params_do_not_fit_falls_back_without_them() {
        let message = json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "workspace/configuration",
            "params": {"items": "not a list"},
        });
        match classify(message) {
            Routed::Incoming(IncomingRequest::Unknown { method, params, .. }) => {
                assert_eq!(method, "workspace/configuration");
                assert_eq!(
                    params,
                    Value::Null,
                    "params no reply path reads are dropped rather than carried"
                );
            },
            _ => panic!("expected the unknown fallback"),
        }
    }
}
