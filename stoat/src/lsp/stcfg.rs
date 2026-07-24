//! In-process language server for `config.stcfg` buffers.
//!
//! [`StcfgLsp`] implements [`LspHost`] directly rather than spawning a
//! subprocess, so it rides the same client pipeline as a real server -- the
//! completion popup, diagnostics rendering, and hover -- with no process, no
//! PATH resolution, and candidates that cannot drift from the running binary's
//! settings table.
//!
//! Every recognized setting comes from [`stoat_config::settings_schema`], the
//! single source of truth that mirrors `Settings::apply`. The server advertises
//! UTF-8 position encoding so line/column offsets are plain byte counts.

use crate::host::lsp::{IncomingRequest, LspHost, LspNotification, LspResponseError};
use async_trait::async_trait;
use lsp_types::{
    CallHierarchyIncomingCall, CallHierarchyIncomingCallsParams, CallHierarchyItem,
    CallHierarchyOutgoingCall, CallHierarchyOutgoingCallsParams, CallHierarchyPrepareParams,
    CodeAction, CodeActionOrCommand, CodeActionParams, ColorInformation, ColorPresentation,
    ColorPresentationParams, CompletionItem, CompletionItemKind, CompletionOptions,
    CompletionParams, CompletionResponse, Diagnostic, DiagnosticOptions,
    DiagnosticServerCapabilities, DiagnosticSeverity, DidChangeConfigurationParams,
    DidChangeTextDocumentParams, DidChangeWatchedFilesParams, DidChangeWorkspaceFoldersParams,
    DidCloseTextDocumentParams, DidOpenTextDocumentParams, DidSaveTextDocumentParams,
    DocumentColorParams, DocumentDiagnosticParams, DocumentDiagnosticReport,
    DocumentDiagnosticReportResult, DocumentFormattingParams, DocumentHighlight,
    DocumentHighlightParams, DocumentLink, DocumentLinkParams, DocumentRangeFormattingParams,
    DocumentSymbolParams, DocumentSymbolResponse, ExecuteCommandParams, FoldingRange,
    FoldingRangeParams, FullDocumentDiagnosticReport, GotoDefinitionParams, GotoDefinitionResponse,
    Hover, HoverContents, HoverParams, HoverProviderCapability, InitializeResult, InlayHint,
    InlayHintParams, Location, MarkupContent, MarkupKind, NumberOrString, Position,
    PositionEncodingKind, PrepareRenameResponse, Range, ReferenceParams,
    RelatedFullDocumentDiagnosticReport, RenameFilesParams, RenameParams, SelectionRange,
    SelectionRangeParams, SemanticToken, SemanticTokenType, SemanticTokens,
    SemanticTokensFullOptions, SemanticTokensLegend, SemanticTokensOptions, SemanticTokensParams,
    SemanticTokensRangeParams, SemanticTokensRangeResult, SemanticTokensResult,
    SemanticTokensServerCapabilities, ServerCapabilities, SignatureHelp, SignatureHelpParams,
    TextDocumentPositionParams, TextDocumentSyncCapability, TextDocumentSyncKind, TextEdit,
    TypeHierarchyItem, TypeHierarchyPrepareParams, TypeHierarchySubtypesParams,
    TypeHierarchySupertypesParams, Uri, WorkspaceEdit, WorkspaceSymbolParams,
    WorkspaceSymbolResponse,
};
use serde_json::Value as JsonValue;
use std::{
    collections::{HashMap, HashSet},
    io,
    sync::{Arc, LazyLock, Mutex},
};
use stoat_config::{
    ActionExpr, Arg, Binding, EventBlock, EventType, Expr, PathSeg, Predicate, Setting, SettingDef,
    Span, Spanned, Statement, ThemeBlock, Value, ValueShape,
};

/// In-process settings language server backing `.stcfg` buffers.
///
/// Holds the latest text of each open document so pull requests
/// (completion, diagnostics, hover) can be answered against it without a
/// round trip. Documents arrive via `did_open` / `did_change` and are dropped
/// on `did_close`.
pub struct StcfgLsp {
    docs: Mutex<HashMap<Uri, String>>,
}

impl StcfgLsp {
    pub fn new() -> Self {
        Self {
            docs: Mutex::new(HashMap::new()),
        }
    }
}

impl Default for StcfgLsp {
    fn default() -> Self {
        Self::new()
    }
}

static STCFG_CAPABILITIES: LazyLock<Arc<ServerCapabilities>> = LazyLock::new(|| {
    Arc::new(ServerCapabilities {
        position_encoding: Some(PositionEncodingKind::UTF8),
        text_document_sync: Some(TextDocumentSyncCapability::Kind(TextDocumentSyncKind::FULL)),
        completion_provider: Some(CompletionOptions {
            trigger_characters: Some(vec![".".to_string(), "=".to_string(), " ".to_string()]),
            ..CompletionOptions::default()
        }),
        hover_provider: Some(HoverProviderCapability::Simple(true)),
        diagnostic_provider: Some(DiagnosticServerCapabilities::Options(
            DiagnosticOptions::default(),
        )),
        semantic_tokens_provider: Some(SemanticTokensServerCapabilities::SemanticTokensOptions(
            SemanticTokensOptions {
                legend: SemanticTokensLegend {
                    token_types: vec![
                        SemanticTokenType::KEYWORD,
                        SemanticTokenType::PROPERTY,
                        SemanticTokenType::VARIABLE,
                        SemanticTokenType::FUNCTION,
                        SemanticTokenType::PARAMETER,
                        SemanticTokenType::ENUM_MEMBER,
                        SemanticTokenType::TYPE,
                        SemanticTokenType::STRING,
                        SemanticTokenType::NUMBER,
                        SemanticTokenType::COMMENT,
                    ],
                    token_modifiers: vec![],
                },
                full: Some(SemanticTokensFullOptions::Bool(true)),
                range: None,
                work_done_progress_options: Default::default(),
            },
        )),
        ..ServerCapabilities::default()
    })
});

#[async_trait]
impl LspHost for StcfgLsp {
    fn capabilities(&self) -> Arc<ServerCapabilities> {
        STCFG_CAPABILITIES.clone()
    }

    async fn initialize(&self, _root_uri: Option<Uri>) -> io::Result<InitializeResult> {
        Ok(InitializeResult {
            capabilities: (**STCFG_CAPABILITIES).clone(),
            server_info: None,
        })
    }

    async fn shutdown(&self) -> io::Result<()> {
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) -> io::Result<()> {
        let doc = params.text_document;
        self.docs
            .lock()
            .expect("stcfg docs poisoned")
            .insert(doc.uri, doc.text);
        Ok(())
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) -> io::Result<()> {
        let uri = params.text_document.uri;
        if let Some(change) = params.content_changes.into_iter().next_back() {
            self.docs
                .lock()
                .expect("stcfg docs poisoned")
                .insert(uri, change.text);
        }
        Ok(())
    }

    async fn did_save(&self, _params: DidSaveTextDocumentParams) -> io::Result<()> {
        Ok(())
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) -> io::Result<()> {
        self.docs
            .lock()
            .expect("stcfg docs poisoned")
            .remove(&params.text_document.uri);
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

    async fn hover(&self, params: HoverParams) -> io::Result<Option<Hover>> {
        let TextDocumentPositionParams {
            text_document,
            position,
        } = params.text_document_position_params;
        let result = {
            let docs = self.docs.lock().expect("stcfg docs poisoned");
            let Some(text) = docs.get(&text_document.uri) else {
                return Ok(None);
            };
            hover(text, position_to_offset(text, position))
        };
        Ok(result)
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

    async fn completion(&self, params: CompletionParams) -> io::Result<Option<CompletionResponse>> {
        let TextDocumentPositionParams {
            text_document,
            position,
        } = params.text_document_position;
        let items = {
            let docs = self.docs.lock().expect("stcfg docs poisoned");
            let Some(text) = docs.get(&text_document.uri) else {
                return Ok(None);
            };
            complete(text, position_to_offset(text, position))
        };
        Ok(Some(CompletionResponse::Array(items)))
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
        params: SemanticTokensParams,
    ) -> io::Result<Option<SemanticTokensResult>> {
        let data = {
            let docs = self.docs.lock().expect("stcfg docs poisoned");
            let Some(text) = docs.get(&params.text_document.uri) else {
                return Ok(None);
            };
            semantic_tokens(text)
        };
        Ok(data.map(|data| {
            SemanticTokensResult::Tokens(SemanticTokens {
                result_id: None,
                data,
            })
        }))
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
        params: DocumentDiagnosticParams,
    ) -> io::Result<Option<DocumentDiagnosticReportResult>> {
        let items = {
            let docs = self.docs.lock().expect("stcfg docs poisoned");
            let Some(text) = docs.get(&params.text_document.uri) else {
                return Ok(None);
            };
            diagnose(text)
        };
        Ok(Some(DocumentDiagnosticReportResult::Report(
            DocumentDiagnosticReport::Full(RelatedFullDocumentDiagnosticReport {
                related_documents: None,
                full_document_diagnostic_report: FullDocumentDiagnosticReport {
                    result_id: None,
                    items,
                },
            }),
        )))
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

    async fn execute_command(
        &self,
        _params: ExecuteCommandParams,
    ) -> io::Result<Option<JsonValue>> {
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
        _result: Result<JsonValue, LspResponseError>,
    ) -> io::Result<()> {
        Ok(())
    }
}

/// Offer setting completions for the cursor at `offset`.
///
/// Empty outside an `on init` block. Before an `=` the dotted prefix drives
/// setting-path completion. After an `=` the resolved setting's value shape
/// drives value completion.
fn complete(text: &str, offset: usize) -> Vec<CompletionItem> {
    let offset = offset.min(text.len());
    if !enclosing_block_is_init(text, offset) {
        return Vec::new();
    }

    let (start, _) = statement_bounds(text, offset);
    let fragment = &text[start..offset];

    match fragment.split_once('=') {
        Some((path_text, value_prefix)) => {
            value_completions(path_text.trim(), value_prefix.trim_start())
        },
        None => path_completions(fragment.trim()),
    }
}

/// Diagnose parse errors and unrecognized settings across the whole document.
///
/// Every [`stoat_config::ParseError`] becomes an error diagnostic. Each `on
/// init` setting whose path matches no schema entry becomes an "unknown
/// setting" warning spanning its path.
fn diagnose(text: &str) -> Vec<Diagnostic> {
    let (config, errors) = stoat_config::parse(text);

    let mut diagnostics: Vec<Diagnostic> = errors
        .iter()
        .map(|error| {
            diagnostic(
                range_from_span(text, error.span.clone()),
                DiagnosticSeverity::ERROR,
                error.message.clone(),
            )
        })
        .collect();

    let Some(config) = config else {
        return diagnostics;
    };

    for block in &config.blocks {
        if block.node.event != EventType::Init {
            continue;
        }
        for statement in &block.node.statements {
            let Statement::Setting(setting) = &statement.node else {
                continue;
            };
            let segments: Vec<&str> = setting.path.iter().map(|seg| seg.node.as_str()).collect();
            if def_for_path(&segments).is_none() {
                diagnostics.push(diagnostic(
                    range_from_span(text, path_span(&setting.path)),
                    DiagnosticSeverity::WARNING,
                    format!("unknown setting `{}`", segments.join(".")),
                ));
            }
        }
    }

    diagnostics
}

/// Documentation and default for the setting under the cursor at `offset`.
///
/// Returns `None` outside an `on init` block or when the statement's path
/// matches no schema entry.
fn hover(text: &str, offset: usize) -> Option<Hover> {
    let offset = offset.min(text.len());
    if !enclosing_block_is_init(text, offset) {
        return None;
    }

    let (start, end) = statement_bounds(text, offset);
    let statement = &text[start..end];
    let path_text = statement
        .split_once('=')
        .map_or(statement, |(path, _)| path)
        .trim();
    if path_text.is_empty() {
        return None;
    }

    let segments: Vec<&str> = path_text.split('.').collect();
    let def = def_for_path(&segments)?;
    let value = format!("`{path_text}`\n\n{}\n\nDefault: `{}`", def.doc, def.default);

    Some(Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value,
        }),
        range: None,
    })
}

/// Next-segment path candidates matching the dotted `prefix`.
///
/// The prefix splits into already-typed segments and a partial trailing
/// segment. Candidates are the distinct next segments of schema paths that
/// extend the typed segments and start with the partial one. Wildcard segments
/// (free-form names like a language or scope) offer no candidate.
fn path_completions(prefix: &str) -> Vec<CompletionItem> {
    let (typed, partial): (Vec<&str>, &str) = match prefix.rsplit_once('.') {
        Some((head, last)) => (head.split('.').collect(), last),
        None => (Vec::new(), prefix),
    };

    let mut seen = HashSet::new();
    let mut items = Vec::new();
    for def in stoat_config::settings_schema() {
        if def.path.len() <= typed.len() || !prefix_segments_match(&typed, def) {
            continue;
        }
        let PathSeg::Lit(next) = def.path[typed.len()] else {
            continue;
        };
        if next.starts_with(partial) && seen.insert(next) {
            items.push(completion_item(next, CompletionItemKind::PROPERTY));
        }
    }
    items
}

/// Value candidates for the setting named by `path_text`, filtered by
/// `value_prefix`. Only [`ValueShape::Bool`] and [`ValueShape::Enum`] carry a
/// closed candidate set. Other shapes offer nothing.
fn value_completions(path_text: &str, value_prefix: &str) -> Vec<CompletionItem> {
    let segments: Vec<&str> = path_text.split('.').collect();
    let Some(def) = def_for_path(&segments) else {
        return Vec::new();
    };

    let candidates: Vec<&str> = match def.shape {
        ValueShape::Bool => vec!["true", "false"],
        ValueShape::Enum(variants) => variants.to_vec(),
        _ => Vec::new(),
    };

    candidates
        .into_iter()
        .filter(|candidate| candidate.starts_with(value_prefix))
        .map(|candidate| completion_item(candidate, CompletionItemKind::VALUE))
        .collect()
}

fn completion_item(label: &str, kind: CompletionItemKind) -> CompletionItem {
    CompletionItem {
        label: label.to_string(),
        kind: Some(kind),
        ..CompletionItem::default()
    }
}

fn diagnostic(range: Range, severity: DiagnosticSeverity, message: String) -> Diagnostic {
    Diagnostic {
        range,
        severity: Some(severity),
        message,
        ..Diagnostic::default()
    }
}

/// The schema entry whose path matches `segments` exactly, wildcards included.
fn def_for_path(segments: &[&str]) -> Option<&'static SettingDef> {
    stoat_config::settings_schema()
        .iter()
        .find(|def| segments.len() == def.path.len() && prefix_segments_match(segments, def))
}

/// Whether `def`'s path begins with `segments`, matching wildcard positions
/// against any segment. `segments` must not be longer than `def`'s path.
fn prefix_segments_match(segments: &[&str], def: &SettingDef) -> bool {
    segments
        .iter()
        .enumerate()
        .all(|(i, segment)| match def.path[i] {
            PathSeg::Lit(lit) => lit == *segment,
            PathSeg::Wildcard(_) => true,
        })
}

/// Byte span covering a setting's whole dotted path.
fn path_span(path: &[Spanned<String>]) -> Span {
    let start = path.first().map(|seg| seg.span.start).unwrap_or(0);
    let end = path.last().map(|seg| seg.span.end).unwrap_or(0);
    start..end
}

/// Whether the innermost block enclosing `offset` is an `on init` block.
///
/// This is a best-effort lexical scan. It tracks brace nesting and the header
/// text preceding each `{`, so it works on the partial, mid-edit text that pull
/// requests see before the document parses cleanly. Value maps like `ui.cursor
/// = { ... }` push a non-init frame and pop correctly.
fn enclosing_block_is_init(text: &str, offset: usize) -> bool {
    let mut stack: Vec<bool> = Vec::new();
    let mut header_start = 0;
    for (i, ch) in text[..offset].char_indices() {
        match ch {
            '{' => {
                let header: Vec<&str> = text[header_start..i].split_whitespace().collect();
                stack.push(header == ["on", "init"]);
                header_start = i + 1;
            },
            '}' => {
                stack.pop();
                header_start = i + 1;
            },
            ';' => header_start = i + 1,
            _ => {},
        }
    }
    stack.last().copied().unwrap_or(false)
}

/// Byte range of the statement containing `offset`, bounded by the surrounding
/// `;`, `{`, `}`, or newline separators.
fn statement_bounds(text: &str, offset: usize) -> (usize, usize) {
    let is_boundary = |ch: char| matches!(ch, ';' | '{' | '}' | '\n');
    let start = text[..offset]
        .rfind(is_boundary)
        .map(|i| i + 1)
        .unwrap_or(0);
    let end = text[offset..]
        .find(is_boundary)
        .map(|i| offset + i)
        .unwrap_or(text.len());
    (start, end)
}

fn range_from_span(text: &str, span: Span) -> Range {
    Range::new(
        offset_to_position(text, span.start),
        offset_to_position(text, span.end),
    )
}

/// Byte offset to a UTF-8 [`Position`] (line and byte column, both 0-based).
fn offset_to_position(text: &str, offset: usize) -> Position {
    let offset = offset.min(text.len());
    let line = text[..offset].bytes().filter(|&b| b == b'\n').count() as u32;
    let line_start = text[..offset].rfind('\n').map(|i| i + 1).unwrap_or(0);
    Position::new(line, (offset - line_start) as u32)
}

/// UTF-8 [`Position`] (line and byte column) to a byte offset, clamping a
/// column past the line's end to the line's end and a line past the document
/// to its end.
fn position_to_offset(text: &str, position: Position) -> usize {
    let mut offset = 0;
    for (n, line) in text.split_inclusive('\n').enumerate() {
        if n as u32 == position.line {
            let max_col = line.strip_suffix('\n').unwrap_or(line).len();
            return offset + (position.character as usize).min(max_col);
        }
        offset += line.len();
    }
    text.len()
}

const TK_KEYWORD: u32 = 0;
const TK_PROPERTY: u32 = 1;
const TK_VARIABLE: u32 = 2;
const TK_FUNCTION: u32 = 3;
const TK_PARAMETER: u32 = 4;
const TK_ENUM_MEMBER: u32 = 5;
const TK_TYPE: u32 = 6;
const TK_STRING: u32 = 7;
const TK_NUMBER: u32 = 8;
const TK_COMMENT: u32 = 9;

/// Encode `text` as the semantic-token stream for `.stcfg` highlighting.
///
/// Returns [`None`] when the text does not parse to a config, so the client
/// keeps its last-good colors during a broken mid-edit state rather than
/// clearing them. Token type indices match the legend order declared in
/// [`STCFG_CAPABILITIES`].
fn semantic_tokens(text: &str) -> Option<Vec<SemanticToken>> {
    let (config, _errors) = stoat_config::parse(text);
    let config = config?;

    let mut spans: Vec<(Span, u32)> = Vec::new();
    for block in &config.blocks {
        walk_event_block(text, block, &mut spans);
    }
    for theme in &config.themes {
        walk_theme_block(text, theme, &mut spans);
    }
    collect_comment_spans(text, &mut spans);

    spans.sort_by_key(|(span, _)| span.start);
    Some(encode_tokens(text, &spans))
}

fn event_keyword(event: EventType) -> &'static str {
    match event {
        EventType::Init => "init",
        EventType::Buffer => "buffer",
        EventType::Key => "key",
    }
}

fn walk_event_block(text: &str, block: &Spanned<EventBlock>, out: &mut Vec<(Span, u32)>) {
    let on_start = block.span.start;
    out.push((on_start..on_start + "on".len(), TK_KEYWORD));

    let keyword = event_keyword(block.node.event);
    if let Some(rel) = text[block.span.clone()].find(keyword) {
        let start = block.span.start + rel;
        out.push((start..start + keyword.len(), TK_KEYWORD));
    }

    for statement in &block.node.statements {
        walk_statement(text, statement, out);
    }
}

fn walk_theme_block(text: &str, theme: &Spanned<ThemeBlock>, out: &mut Vec<(Span, u32)>) {
    let start = theme.span.start;
    out.push((start..start + "theme".len(), TK_KEYWORD));

    let name = &theme.node.name;
    out.push((name.span.clone(), TK_TYPE));

    if let Some(parent) = &theme.node.parent {
        if let Some(rel) = text[name.span.end..parent.span.start].find("inherits") {
            let start = name.span.end + rel;
            out.push((start..start + "inherits".len(), TK_KEYWORD));
        }
        out.push((parent.span.clone(), TK_TYPE));
    }

    for statement in &theme.node.statements {
        walk_statement(text, statement, out);
    }
}

fn walk_statement(text: &str, statement: &Spanned<Statement>, out: &mut Vec<(Span, u32)>) {
    match &statement.node {
        Statement::Setting(setting) => walk_setting(setting, out),
        Statement::Binding(binding) => walk_binding(binding, out),
        Statement::Let(binding) => {
            let start = statement.span.start;
            out.push((start..start + "let".len(), TK_KEYWORD));
            out.push((binding.name.span.clone(), TK_VARIABLE));
            walk_expr(text, &binding.value, out);
        },
        Statement::FnDecl(decl) => {
            let start = statement.span.start;
            out.push((start..start + "fn".len(), TK_KEYWORD));
            out.push((decl.name.span.clone(), TK_FUNCTION));
            for inner in &decl.body {
                walk_statement(text, inner, out);
            }
        },
        Statement::FnCall(name) => out.push((name.span.clone(), TK_FUNCTION)),
        Statement::PredicateBlock(block) => {
            walk_predicate(&block.predicate, out);
            for inner in &block.body {
                walk_statement(text, inner, out);
            }
        },
    }
}

fn walk_setting(setting: &Setting, out: &mut Vec<(Span, u32)>) {
    for segment in &setting.path {
        out.push((segment.span.clone(), TK_PROPERTY));
    }
    walk_value(&setting.value, out);
}

fn walk_binding(binding: &Binding, out: &mut Vec<(Span, u32)>) {
    out.push((binding.key.span.clone(), TK_ENUM_MEMBER));
    match &binding.action.node {
        ActionExpr::Single(action) => {
            let start = binding.action.span.start;
            out.push((start..start + action.name.len(), TK_FUNCTION));
            for arg in &action.args {
                walk_arg(arg, out);
            }
        },
        ActionExpr::Sequence(actions) => {
            for action in actions {
                let start = action.span.start;
                out.push((start..start + action.node.name.len(), TK_FUNCTION));
                for arg in &action.node.args {
                    walk_arg(arg, out);
                }
            }
        },
    }
}

fn walk_arg(arg: &Spanned<Arg>, out: &mut Vec<(Span, u32)>) {
    match &arg.node {
        Arg::Positional(value) => walk_value(value, out),
        Arg::Named { name, value } => {
            out.push((name.span.clone(), TK_PARAMETER));
            walk_value(value, out);
        },
    }
}

fn walk_expr(text: &str, expr: &Spanned<Expr>, out: &mut Vec<(Span, u32)>) {
    match &expr.node {
        Expr::Value(value) => {
            if let Some(kind) = scalar_token(value) {
                out.push((expr.span.clone(), kind));
            }
        },
        Expr::Variable(_) => out.push((expr.span.clone(), TK_VARIABLE)),
        Expr::If {
            condition,
            then_expr,
            else_expr,
        } => {
            let start = expr.span.start;
            out.push((start..start + "if".len(), TK_KEYWORD));
            walk_predicate(condition, out);
            walk_expr(text, then_expr, out);
            if let Some(rel) = text[then_expr.span.end..else_expr.span.start].find("else") {
                let start = then_expr.span.end + rel;
                out.push((start..start + "else".len(), TK_KEYWORD));
            }
            walk_expr(text, else_expr, out);
        },
    }
}

fn walk_predicate(predicate: &Spanned<Predicate>, out: &mut Vec<(Span, u32)>) {
    match &predicate.node {
        Predicate::Eq(field, value)
        | Predicate::NotEq(field, value)
        | Predicate::Gt(field, value)
        | Predicate::Lt(field, value)
        | Predicate::Gte(field, value)
        | Predicate::Lte(field, value) => {
            out.push((field.span.clone(), TK_VARIABLE));
            walk_value(value, out);
        },
        Predicate::Matches(field, glob) => {
            out.push((field.span.clone(), TK_VARIABLE));
            out.push((glob.span.clone(), TK_STRING));
        },
        Predicate::Bool(field) => out.push((field.span.clone(), TK_VARIABLE)),
        Predicate::Not(inner) => walk_predicate(inner, out),
        Predicate::And(left, right) | Predicate::Or(left, right) => {
            walk_predicate(left, out);
            walk_predicate(right, out);
        },
    }
}

fn walk_value(value: &Spanned<Value>, out: &mut Vec<(Span, u32)>) {
    match &value.node {
        Value::Array(elements) => {
            for element in elements {
                walk_value(element, out);
            }
        },
        Value::Map(entries) => {
            for (key, entry) in entries {
                out.push((key.span.clone(), TK_PROPERTY));
                walk_value(entry, out);
            }
        },
        scalar => {
            if let Some(kind) = scalar_token(scalar) {
                out.push((value.span.clone(), kind));
            }
        },
    }
}

/// Token type for a scalar [`Value`], or [`None`] for the compound Array and Map
/// variants that carry their own spanned children.
fn scalar_token(value: &Value) -> Option<u32> {
    Some(match value {
        Value::String(_) => TK_STRING,
        Value::Number(_) => TK_NUMBER,
        Value::Bool(_) => TK_KEYWORD,
        Value::Ident(_) | Value::StateRef(_) => TK_VARIABLE,
        Value::Enum { .. } => TK_ENUM_MEMBER,
        Value::Array(_) | Value::Map(_) => return None,
    })
}

/// Append a comment token for every `#`-to-end-of-line run outside a string.
///
/// A raw scan rather than an AST walk because the parser's whitespace rule eats
/// comments before they reach the AST. String tracking honors backslash escapes
/// so a `#` inside a `"..."` literal never opens a comment.
fn collect_comment_spans(text: &str, out: &mut Vec<(Span, u32)>) {
    let bytes = text.as_bytes();
    let mut i = 0;
    let mut in_string = false;
    let mut escaped = false;
    while i < bytes.len() {
        let byte = bytes[i];
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
        } else if byte == b'"' {
            in_string = true;
        } else if byte == b'#' {
            let end = text[i..].find('\n').map_or(text.len(), |n| i + n);
            out.push((i..end, TK_COMMENT));
            i = end;
            continue;
        }
        i += 1;
    }
}

/// Encode `(byte span, token type)` pairs as the LSP relative token stream.
///
/// Spans crossing a newline split at line boundaries, since LSP tokens are
/// single-line. Pairs need not arrive sorted: the split pieces are ordered by
/// line then column before the relative deltas are computed.
fn encode_tokens(text: &str, spans: &[(Span, u32)]) -> Vec<SemanticToken> {
    let mut pieces: Vec<(u32, u32, u32, u32)> = Vec::new();
    for (span, token_type) in spans {
        let mut line_start = span.start;
        for (rel, byte) in text[span.clone()].bytes().enumerate() {
            if byte == b'\n' {
                push_piece(text, line_start, span.start + rel, *token_type, &mut pieces);
                line_start = span.start + rel + 1;
            }
        }
        push_piece(text, line_start, span.end, *token_type, &mut pieces);
    }
    pieces.sort_by_key(|&(line, column, ..)| (line, column));

    let mut tokens = Vec::with_capacity(pieces.len());
    let mut prev_line = 0;
    let mut prev_column = 0;
    for (line, column, length, token_type) in pieces {
        let delta_line = line - prev_line;
        let delta_start = if delta_line == 0 {
            column - prev_column
        } else {
            column
        };
        tokens.push(SemanticToken {
            delta_line,
            delta_start,
            length,
            token_type,
            token_modifiers_bitset: 0,
        });
        prev_line = line;
        prev_column = column;
    }
    tokens
}

fn push_piece(
    text: &str,
    start: usize,
    end: usize,
    token_type: u32,
    pieces: &mut Vec<(u32, u32, u32, u32)>,
) {
    if end <= start {
        return;
    }
    let position = offset_to_position(text, start);
    pieces.push((
        position.line,
        position.character,
        (end - start) as u32,
        token_type,
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_types::{
        TextDocumentContentChangeEvent, TextDocumentIdentifier, TextDocumentItem,
        VersionedTextDocumentIdentifier,
    };
    use std::str::FromStr;
    use stoat_scheduler::TestScheduler;

    fn uri() -> Uri {
        Uri::from_str("file:///config.stcfg").expect("valid uri")
    }

    fn labels(items: &[CompletionItem]) -> Vec<&str> {
        items.iter().map(|item| item.label.as_str()).collect()
    }

    /// Byte offset of the first `|` marker in `marked`, with the marker
    /// removed from the returned text.
    fn at_cursor(marked: &str) -> (String, usize) {
        let offset = marked.find('|').expect("cursor marker");
        (marked.replace('|', ""), offset)
    }

    #[test]
    fn partial_path_completes_to_setting() {
        let (text, offset) = at_cursor("on init { form| }");
        assert_eq!(labels(&complete(&text, offset)), ["format_on_save"]);
    }

    #[test]
    fn empty_path_offers_all_top_level_segments() {
        let (text, offset) = at_cursor("on init { | }");
        let items = complete(&text, offset);
        assert!(labels(&items).contains(&"editor"));
        assert!(labels(&items).contains(&"format_on_save"));
        assert!(labels(&items).contains(&"lsp"));
    }

    #[test]
    fn dotted_prefix_offers_next_segment() {
        let (text, offset) = at_cursor("on init { editor.line| }");
        assert_eq!(labels(&complete(&text, offset)), ["line_numbers"]);
    }

    #[test]
    fn enum_value_completes_after_equals() {
        let (text, offset) = at_cursor("on init { editor.line_numbers = | }");
        assert_eq!(
            labels(&complete(&text, offset)),
            ["off", "absolute", "relative"],
        );
    }

    #[test]
    fn bool_value_completes_after_equals() {
        let (text, offset) = at_cursor("on init { format_on_save = | }");
        assert_eq!(labels(&complete(&text, offset)), ["true", "false"]);
    }

    #[test]
    fn value_prefix_filters_candidates() {
        let (text, offset) = at_cursor("on init { editor.line_numbers = rel| }");
        assert_eq!(labels(&complete(&text, offset)), ["relative"]);
    }

    #[test]
    fn no_completion_outside_init_block() {
        let (text, offset) = at_cursor("theme dark { form| }");
        assert_eq!(complete(&text, offset), Vec::new());
    }

    #[test]
    fn unknown_setting_warns_at_path_span() {
        let text = "on init { editor.scrollofff = 3; }";
        let diagnostics = diagnose(text);
        assert_eq!(diagnostics.len(), 1);

        let warning = &diagnostics[0];
        assert_eq!(warning.severity, Some(DiagnosticSeverity::WARNING));
        assert_eq!(warning.message, "unknown setting `editor.scrollofff`");

        let path_start = text.find("editor.scrollofff").expect("path present");
        let path_end = path_start + "editor.scrollofff".len();
        assert_eq!(warning.range.start, offset_to_position(text, path_start));
        assert_eq!(warning.range.end, offset_to_position(text, path_end));
    }

    #[test]
    fn known_settings_produce_no_diagnostics() {
        let text = "on init {\n  format_on_save = true;\n  editor.line_numbers = relative;\n}";
        assert_eq!(diagnose(text), Vec::new());
    }

    #[test]
    fn syntax_error_diagnoses_at_its_line() {
        let text = "on init {\n  format_on_save = ;\n}";
        let diagnostics = diagnose(text);

        let error = diagnostics
            .iter()
            .find(|d| d.severity == Some(DiagnosticSeverity::ERROR))
            .expect("a syntax error diagnostic");
        assert_eq!(error.range.start.line, 1);
    }

    #[test]
    fn hover_reports_doc_and_default() {
        let (text, offset) = at_cursor("on init { editor.line_num|bers = relative; }");
        let hover = hover(&text, offset).expect("hover over a known setting");

        let HoverContents::Markup(markup) = hover.contents else {
            panic!("expected markup hover");
        };
        assert!(markup.value.contains("editor.line_numbers"));
        assert!(markup.value.contains("Default: `relative`"));
    }

    #[test]
    fn hover_absent_for_unknown_setting() {
        let (text, offset) = at_cursor("on init { editor.bogus| = 1; }");
        assert_eq!(hover(&text, offset), None);
    }

    fn open_params(uri: Uri, text: &str) -> DidOpenTextDocumentParams {
        DidOpenTextDocumentParams {
            text_document: TextDocumentItem {
                uri,
                language_id: "stcfg".to_string(),
                version: 1,
                text: text.to_string(),
            },
        }
    }

    fn completion_at(uri: Uri, position: Position) -> CompletionParams {
        CompletionParams {
            text_document_position: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position,
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
            context: None,
        }
    }

    #[test]
    fn did_open_then_completion_reads_stored_text() {
        TestScheduler::new().block_on(async {
            let server = StcfgLsp::new();
            server
                .did_open(open_params(uri(), "on init { form }"))
                .await
                .expect("did_open");

            let response = server
                .completion(completion_at(uri(), Position::new(0, 14)))
                .await
                .expect("completion")
                .expect("some response");

            let CompletionResponse::Array(items) = response else {
                panic!("expected an array response");
            };
            assert_eq!(labels(&items), ["format_on_save"]);
        });
    }

    #[test]
    fn did_change_updates_stored_text() {
        TestScheduler::new().block_on(async {
            let server = StcfgLsp::new();
            server
                .did_open(open_params(uri(), "on init { }"))
                .await
                .expect("did_open");
            server
                .did_change(DidChangeTextDocumentParams {
                    text_document: VersionedTextDocumentIdentifier {
                        uri: uri(),
                        version: 2,
                    },
                    content_changes: vec![TextDocumentContentChangeEvent {
                        range: None,
                        range_length: None,
                        text: "on init { editor.line_numbers = | }".replace('|', ""),
                    }],
                })
                .await
                .expect("did_change");

            let response = server
                .completion(completion_at(uri(), Position::new(0, 32)))
                .await
                .expect("completion")
                .expect("some response");

            let CompletionResponse::Array(items) = response else {
                panic!("expected an array response");
            };
            assert_eq!(labels(&items), ["off", "absolute", "relative"]);
        });
    }

    #[test]
    fn completion_absent_for_unopened_document() {
        TestScheduler::new().block_on(async {
            let server = StcfgLsp::new();
            let response = server
                .completion(completion_at(uri(), Position::new(0, 0)))
                .await
                .expect("completion");
            assert_eq!(response, None);
        });
    }

    fn decode(tokens: &[SemanticToken]) -> Vec<(u32, u32, u32, u32)> {
        let mut out = Vec::with_capacity(tokens.len());
        let mut line = 0;
        let mut column = 0;
        for token in tokens {
            if token.delta_line == 0 {
                column += token.delta_start;
            } else {
                line += token.delta_line;
                column = token.delta_start;
            }
            out.push((line, column, token.length, token.token_type));
        }
        out
    }

    /// Expected token tuple for the first occurrence of `needle`, in the
    /// `(line, column, length, token type)` shape [`decode`] produces.
    fn tok(text: &str, needle: &str, token_type: u32) -> (u32, u32, u32, u32) {
        let at = text.find(needle).expect("needle present");
        let position = offset_to_position(text, at);
        (
            position.line,
            position.character,
            needle.len() as u32,
            token_type,
        )
    }

    fn semantic_params(uri: Uri) -> SemanticTokensParams {
        SemanticTokensParams {
            text_document: TextDocumentIdentifier { uri },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }
    }

    #[test]
    fn semantic_tokens_cover_a_theme_and_key_block() {
        let text = [
            "theme ocean {",
            "  let accent = \"#ff8800\";",
            "  editor.wrap = true;",
            "  # note",
            "}",
            "on key {",
            "  Ctrl-x -> Jump(count: 7);",
            "}",
        ]
        .join("\n");

        let tokens = semantic_tokens(&text).expect("a parseable config yields tokens");
        assert_eq!(
            decode(&tokens),
            vec![
                tok(&text, "theme", TK_KEYWORD),
                tok(&text, "ocean", TK_TYPE),
                tok(&text, "let", TK_KEYWORD),
                tok(&text, "accent", TK_VARIABLE),
                tok(&text, "\"#ff8800\"", TK_STRING),
                tok(&text, "editor", TK_PROPERTY),
                tok(&text, "wrap", TK_PROPERTY),
                tok(&text, "true", TK_KEYWORD),
                tok(&text, "# note", TK_COMMENT),
                tok(&text, "on", TK_KEYWORD),
                tok(&text, "key", TK_KEYWORD),
                tok(&text, "Ctrl-x", TK_ENUM_MEMBER),
                tok(&text, "Jump", TK_FUNCTION),
                tok(&text, "count", TK_PARAMETER),
                tok(&text, "7", TK_NUMBER),
            ],
        );
    }

    #[test]
    fn semantic_tokens_full_is_none_for_a_broken_document() {
        TestScheduler::new().block_on(async {
            let server = StcfgLsp::new();
            server
                .did_open(open_params(uri(), "on init {"))
                .await
                .expect("did_open");
            let response = server
                .semantic_tokens_full(semantic_params(uri()))
                .await
                .expect("semantic_tokens_full");
            assert_eq!(response, None);
        });
    }

    #[test]
    fn semantic_tokens_full_is_none_for_an_unopened_document() {
        TestScheduler::new().block_on(async {
            let server = StcfgLsp::new();
            let response = server
                .semantic_tokens_full(semantic_params(uri()))
                .await
                .expect("semantic_tokens_full");
            assert_eq!(response, None);
        });
    }
}
