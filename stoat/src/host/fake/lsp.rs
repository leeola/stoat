use crate::host::lsp::{LspHost, LspNotification};
use async_trait::async_trait;
use lsp_types::{
    CodeAction, CodeActionOrCommand, CodeActionParams, CompletionItem, CompletionList,
    CompletionParams, CompletionResponse, Diagnostic, DiagnosticSeverity,
    DidChangeTextDocumentParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
    DidSaveTextDocumentParams, DocumentFormattingParams, DocumentHighlight, DocumentHighlightKind,
    DocumentHighlightParams, DocumentSymbolParams, DocumentSymbolResponse, GotoDefinitionParams,
    GotoDefinitionResponse, Hover, HoverContents, HoverParams, InitializeResult, InlayHint,
    InlayHintKind, InlayHintLabel, InlayHintParams, Location, MarkupContent, MarkupKind,
    PartialResultParams, Position, Range, ReferenceContext, ReferenceParams, RenameParams,
    ServerCapabilities, SignatureHelp, SignatureHelpParams, SymbolInformation, SymbolKind,
    TextDocumentContentChangeEvent, TextDocumentIdentifier, TextDocumentItem,
    TextDocumentPositionParams, TextEdit, Uri, VersionedTextDocumentIdentifier,
    WorkDoneProgressParams, WorkspaceEdit, WorkspaceSymbolParams, WorkspaceSymbolResponse,
};
use std::{
    collections::{BTreeMap, VecDeque},
    io,
    str::FromStr,
    sync::Mutex,
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
    uri: Uri,
    line: u32,
    character: u32,
}

impl LspKey {
    fn new(path: &str, line: u32, col: u32) -> Self {
        Self {
            uri: file_uri(path),
            line,
            character: col,
        }
    }

    fn from_position(uri: &Uri, pos: &Position) -> Self {
        Self {
            uri: uri.clone(),
            line: pos.line,
            character: pos.character,
        }
    }
}

pub struct FakeLsp {
    state: Mutex<FakeLspState>,
}

struct FakeLspState {
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
    notifications: VecDeque<LspNotification>,
    open_documents: BTreeMap<Uri, String>,
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
        Self {
            state: Mutex::new(FakeLspState {
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
                notifications: VecDeque::new(),
                open_documents: BTreeMap::new(),
                initialized: false,
                shut_down: false,
            }),
        }
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
    async fn initialize(&self, _root_uri: Option<Uri>) -> io::Result<InitializeResult> {
        let mut state = self.state.lock().unwrap();
        state.initialized = true;
        Ok(InitializeResult {
            capabilities: ServerCapabilities::default(),
            server_info: None,
        })
    }

    async fn shutdown(&self) -> io::Result<()> {
        self.state.lock().unwrap().shut_down = true;
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) -> io::Result<()> {
        let mut state = self.state.lock().unwrap();
        let uri = params.text_document.uri.clone();
        state
            .open_documents
            .insert(uri.clone(), params.text_document.text);
        if let Some(diagnostics) = state.diagnostics.get(&uri).cloned() {
            state.notifications.push_back(LspNotification::Diagnostics {
                uri,
                diagnostics,
                version: None,
            });
        }
        Ok(())
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) -> io::Result<()> {
        let mut state = self.state.lock().unwrap();
        let uri = params.text_document.uri.clone();
        if let Some(change) = params.content_changes.into_iter().last() {
            state.open_documents.insert(uri.clone(), change.text);
        }
        if let Some(diagnostics) = state.diagnostics.get(&uri).cloned() {
            state.notifications.push_back(LspNotification::Diagnostics {
                uri,
                diagnostics,
                version: None,
            });
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
        let state = self.state.lock().unwrap();
        Ok(lookup_goto(
            &state.implementations,
            &params.text_document_position_params.text_document.uri,
            &params.text_document_position_params.position,
        ))
    }

    async fn references(&self, params: ReferenceParams) -> io::Result<Option<Vec<Location>>> {
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
        let state = self.state.lock().unwrap();
        let key = LspKey::from_position(
            &params.text_document_position_params.text_document.uri,
            &params.text_document_position_params.position,
        );
        Ok(state.highlights.get(&key).cloned())
    }

    async fn completion(&self, params: CompletionParams) -> io::Result<Option<CompletionResponse>> {
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
        params: WorkspaceSymbolParams,
    ) -> io::Result<Option<WorkspaceSymbolResponse>> {
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
        Ok(None)
    }

    async fn inlay_hint(&self, params: InlayHintParams) -> io::Result<Option<Vec<InlayHint>>> {
        let state = self.state.lock().unwrap();
        Ok(state.inlay_hints.get(&params.text_document.uri).cloned())
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
        self.state.lock().unwrap().notifications.pop_front()
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
            assert!(lsp.recv_notification().await.is_none());
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
}
