//! In-process language server offering emoji shortcodes in every buffer.
//!
//! Typing `:sm` anywhere offers `:smile:`. That has nothing to do with the
//! language the buffer holds, so [`EmojiLsp`] is registered as a global server
//! rather than a per-language one. It runs in process because its whole
//! candidate table is compiled in, which costs no subprocess, no PATH lookup,
//! and no handshake to wait out.
//!
//! The shortcodes are GitHub's gemoji set, by way of the `emojis` crate.
//!
//! What keeps it quiet inside code is the boundary rule. A completion answers
//! only when the opening `:` sits at a word boundary, so `std::` and `x: T`
//! offer nothing while a colon at the start of a line or after a space offers
//! everything. That rule lives in [`crate::emoji_expand`], shared with the
//! typed-colon swap so the two never disagree about what opens a shortcode.
//!
//! A bare colon after punctuation waits for a letter. `"key":` in JSON and `(:`
//! pass the boundary rule, and the whole table there costs each such keystroke
//! a popup nobody asked for.

use crate::{
    emoji_expand::{is_shortcode_char, opens_a_shortcode},
    host::{
        lsp::{IncomingRequest, LspHost, LspNotification, LspResponseError},
        OffsetEncoding,
    },
    lsp::util,
};
use async_trait::async_trait;
use lsp_types::{
    CallHierarchyIncomingCall, CallHierarchyIncomingCallsParams, CallHierarchyItem,
    CallHierarchyOutgoingCall, CallHierarchyOutgoingCallsParams, CallHierarchyPrepareParams,
    CodeAction, CodeActionOrCommand, CodeActionParams, ColorInformation, ColorPresentation,
    ColorPresentationParams, CompletionItem, CompletionItemKind, CompletionOptions,
    CompletionParams, CompletionResponse, DidChangeConfigurationParams,
    DidChangeTextDocumentParams, DidChangeWatchedFilesParams, DidChangeWorkspaceFoldersParams,
    DidCloseTextDocumentParams, DidOpenTextDocumentParams, DidSaveTextDocumentParams,
    DocumentColorParams, DocumentDiagnosticParams, DocumentDiagnosticReportResult,
    DocumentFormattingParams, DocumentHighlight, DocumentHighlightParams, DocumentLink,
    DocumentLinkParams, DocumentRangeFormattingParams, DocumentSymbolParams,
    DocumentSymbolResponse, ExecuteCommandParams, FoldingRange, FoldingRangeParams,
    GotoDefinitionParams, GotoDefinitionResponse, Hover, HoverParams, InitializeResult, InlayHint,
    InlayHintParams, Location, NumberOrString, Position, PositionEncodingKind,
    PrepareRenameResponse, ReferenceParams, RenameFilesParams, RenameParams, SelectionRange,
    SelectionRangeParams, SemanticTokensDeltaParams, SemanticTokensFullDeltaResult,
    SemanticTokensParams, SemanticTokensRangeParams, SemanticTokensRangeResult,
    SemanticTokensResult, ServerCapabilities, SignatureHelp, SignatureHelpParams,
    TextDocumentPositionParams, TextDocumentSyncCapability, TextDocumentSyncKind, TextEdit,
    TypeHierarchyItem, TypeHierarchyPrepareParams, TypeHierarchySubtypesParams,
    TypeHierarchySupertypesParams, Uri, WorkspaceEdit, WorkspaceSymbolParams,
    WorkspaceSymbolResponse,
};
use serde_json::Value as JsonValue;
use std::{
    collections::HashMap,
    io,
    sync::{Arc, LazyLock, Mutex},
};
use stoat_text::Rope;

/// In-process shortcode server backing every buffer.
///
/// Holds the latest text of each open document so a completion is answered
/// against it without a round trip. Documents arrive via `did_open` and are
/// dropped on `did_close`. A `did_change` carries only the edited ranges, and
/// each one splices into the kept rope. A settled edit then costs the edit
/// rather than a copy of the document.
pub struct EmojiLsp {
    docs: Mutex<HashMap<Uri, Rope>>,
}

impl EmojiLsp {
    pub fn new() -> Self {
        Self {
            docs: Mutex::new(HashMap::new()),
        }
    }
}

impl Default for EmojiLsp {
    fn default() -> Self {
        Self::new()
    }
}

static EMOJI_CAPABILITIES: LazyLock<Arc<ServerCapabilities>> = LazyLock::new(|| {
    Arc::new(ServerCapabilities {
        position_encoding: Some(PositionEncodingKind::UTF8),
        text_document_sync: Some(TextDocumentSyncCapability::Kind(
            TextDocumentSyncKind::INCREMENTAL,
        )),
        completion_provider: Some(CompletionOptions {
            trigger_characters: Some(vec![":".to_string()]),
            ..CompletionOptions::default()
        }),
        ..ServerCapabilities::default()
    })
});

#[async_trait]
impl LspHost for EmojiLsp {
    fn capabilities(&self) -> Arc<ServerCapabilities> {
        EMOJI_CAPABILITIES.clone()
    }

    async fn initialize(&self, _root_uri: Option<Uri>) -> io::Result<InitializeResult> {
        Ok(InitializeResult {
            capabilities: (**EMOJI_CAPABILITIES).clone(),
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
            .expect("emoji docs poisoned")
            .insert(doc.uri, Rope::from(doc.text.as_str()));
        Ok(())
    }

    /// Apply each change in order, each against the text the one before it
    /// left.
    ///
    /// The server drops a change for a document it never opened, since it
    /// holds no text for the range to name.
    async fn did_change(&self, params: DidChangeTextDocumentParams) -> io::Result<()> {
        let mut docs = self.docs.lock().expect("emoji docs poisoned");
        let Some(rope) = docs.get_mut(&params.text_document.uri) else {
            return Ok(());
        };
        for change in params.content_changes {
            match change.range {
                Some(range) => {
                    let bytes = util::lsp_range_to_byte_range(rope, range, OffsetEncoding::Utf8);
                    rope.replace(bytes, &change.text);
                },
                None => *rope = Rope::from(change.text.as_str()),
            }
        }
        Ok(())
    }

    async fn did_save(&self, _params: DidSaveTextDocumentParams) -> io::Result<()> {
        Ok(())
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) -> io::Result<()> {
        self.docs
            .lock()
            .expect("emoji docs poisoned")
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

    async fn hover(&self, _params: HoverParams) -> io::Result<Option<Hover>> {
        Ok(None)
    }

    async fn references(&self, _params: ReferenceParams) -> io::Result<Option<Vec<Location>>> {
        Ok(None)
    }

    async fn completion(&self, params: CompletionParams) -> io::Result<Option<CompletionResponse>> {
        let TextDocumentPositionParams {
            text_document,
            position,
        } = params.text_document_position;
        let items = {
            let docs = self.docs.lock().expect("emoji docs poisoned");
            let Some(rope) = docs.get(&text_document.uri) else {
                return Ok(None);
            };
            complete(rope, position)
        };
        Ok(Some(CompletionResponse::Array(items)))
    }

    async fn completion_resolve(&self, item: CompletionItem) -> io::Result<CompletionItem> {
        Ok(item)
    }

    async fn code_action_resolve(&self, action: CodeAction) -> io::Result<CodeAction> {
        Ok(action)
    }

    async fn document_link_resolve(&self, link: DocumentLink) -> io::Result<DocumentLink> {
        Ok(link)
    }

    async fn inlay_hint(&self, _params: InlayHintParams) -> io::Result<Option<Vec<InlayHint>>> {
        Ok(None)
    }

    async fn inlay_hint_resolve(&self, hint: InlayHint) -> io::Result<InlayHint> {
        Ok(hint)
    }

    async fn rename(&self, _params: RenameParams) -> io::Result<Option<WorkspaceEdit>> {
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

    // Every request below is one this server has nothing to say about.
    // It answers completion and nothing else, which is what its
    // capabilities already tell a client.

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

    async fn document_highlight(
        &self,
        _params: DocumentHighlightParams,
    ) -> io::Result<Option<Vec<DocumentHighlight>>> {
        Ok(None)
    }

    async fn code_action(
        &self,
        _params: CodeActionParams,
    ) -> io::Result<Option<Vec<CodeActionOrCommand>>> {
        Ok(None)
    }

    async fn document_link(
        &self,
        _params: DocumentLinkParams,
    ) -> io::Result<Option<Vec<DocumentLink>>> {
        Ok(None)
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

    async fn semantic_tokens_full_delta(
        &self,
        _params: SemanticTokensDeltaParams,
    ) -> io::Result<Option<SemanticTokensFullDeltaResult>> {
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

    async fn reply(
        &self,
        _id: NumberOrString,
        _result: Result<JsonValue, LspResponseError>,
    ) -> io::Result<()> {
        Ok(())
    }
}

/// The shortcode being typed at `position`, as the column the opening `:` sits
/// at, the character before that colon, and the letters typed since.
///
/// `None` unless the cursor sits inside text that still reads as a shortcode
/// in progress. The opening colon must follow a word boundary, which leaves
/// `std::` and a `x: T` annotation alone, and everything after it must be a
/// character a shortcode is spelled with. The character before the colon is
/// `None` at the start of the line.
///
/// It reads only the cursor's row out of `rope`.
fn typing_shortcode(rope: &Rope, position: Position) -> Option<(u32, Option<char>, String)> {
    let line: String = rope.chunks_in_line(position.line).collect();
    let cursor = (position.character as usize).min(line.len());
    if !line.is_char_boundary(cursor) {
        return None;
    }

    let before_cursor = &line[..cursor];
    let colon = before_cursor.rfind(':')?;
    let typed = &before_cursor[colon + 1..];
    if !typed.chars().all(is_shortcode_char) {
        return None;
    }
    let before = before_cursor[..colon].chars().next_back();
    opens_a_shortcode(before).then(|| (colon as u32, before, typed.to_owned()))
}

/// Every shortcode that starts with what has been typed at `position`.
///
/// Empty unless a shortcode is in progress. Each item replaces the colon and
/// the letters after it with the emoji itself, so accepting one leaves the
/// glyph rather than the name.
///
/// A colon with nothing typed after it offers the table only at the start of a
/// line or after whitespace. After a quote or a bracket it is a JSON key or a
/// call far more often, and the first letter typed asks again.
fn complete(rope: &Rope, position: Position) -> Vec<CompletionItem> {
    let Some((colon, before, typed)) = typing_shortcode(rope, position) else {
        return Vec::new();
    };
    if typed.is_empty() && !before.is_none_or(char::is_whitespace) {
        return Vec::new();
    }
    let replacing = lsp_types::Range {
        start: Position::new(position.line, colon),
        end: position,
    };

    emojis::iter()
        .flat_map(|emoji| {
            emoji
                .shortcodes()
                .filter(|shortcode| shortcode.starts_with(typed.as_str()))
                .map(move |shortcode| item(emoji.as_str(), shortcode, replacing))
        })
        .collect()
}

fn item(glyph: &str, shortcode: &str, replacing: lsp_types::Range) -> CompletionItem {
    CompletionItem {
        label: format!(":{shortcode}:"),
        // Matched on the bare name, since the colon the user typed is not part
        // of the word itself.
        filter_text: Some(shortcode.to_string()),
        detail: Some(glyph.to_string()),
        kind: Some(CompletionItemKind::TEXT),
        text_edit: Some(lsp_types::CompletionTextEdit::Edit(TextEdit {
            range: replacing,
            new_text: glyph.to_string(),
        })),
        ..CompletionItem::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_types::{
        TextDocumentContentChangeEvent, TextDocumentItem, VersionedTextDocumentIdentifier,
    };
    use std::str::FromStr;
    use stoat_scheduler::TestScheduler;

    fn at(text: &str, line: u32, character: u32) -> Option<(u32, String)> {
        typing_shortcode(&Rope::from(text), Position::new(line, character))
            .map(|(colon, _, typed)| (colon, typed))
    }

    fn offered(text: &str, line: u32, character: u32) -> Vec<CompletionItem> {
        complete(&Rope::from(text), Position::new(line, character))
    }

    #[test]
    fn a_colon_opens_a_shortcode_only_at_a_word_boundary() {
        assert_eq!(at(":sm", 0, 3), Some((0, "sm".to_string())), "line start");
        assert_eq!(
            at("hi :sm", 0, 6),
            Some((3, "sm".to_string())),
            "after space"
        );
        assert_eq!(
            at("(:sm", 0, 4),
            Some((1, "sm".to_string())),
            "after a paren"
        );

        assert_eq!(at("std:", 0, 4), None, "after a letter");
        assert_eq!(at("std::", 0, 5), None, "the second colon of a path");
        assert_eq!(at("x1:", 0, 3), None, "after a digit");
        assert_eq!(at("a_:", 0, 3), None, "after an underscore");
    }

    #[test]
    fn a_shortcode_stops_at_a_character_it_cannot_contain() {
        assert_eq!(at(": sm", 0, 4), None, "a space ends it");
        assert_eq!(at(":SM", 0, 3), None, "shortcodes are lowercase");
        assert_eq!(at(":a_1+", 0, 5), Some((0, "a_1+".to_string())));
    }

    #[test]
    fn a_bare_colon_after_a_space_is_the_whole_table() {
        assert_eq!(at("hi :", 0, 4), Some((3, String::new())));
        assert!(
            offered("hi :", 0, 4).len() > 1000,
            "every shortcode is offered, and the client narrows from there",
        );
    }

    #[test]
    fn a_bare_colon_after_punctuation_waits_for_a_letter() {
        assert!(offered("{\"key\":", 0, 7).is_empty(), "a JSON key");
        assert!(offered("f(:", 0, 3).is_empty(), "a colon after a paren");
        assert!(
            offered("f(:smil", 0, 7)
                .iter()
                .any(|item| item.label == ":smile:"),
            "a letter after the colon offers what it starts",
        );
    }

    #[test]
    fn the_typed_letters_narrow_what_is_offered() {
        let whole_table = offered(":", 0, 1).len();
        let narrowed = offered(":smil", 0, 5);

        assert!(
            narrowed.len() < whole_table,
            "{} of {whole_table} offered for a four-letter prefix",
            narrowed.len(),
        );
        assert!(
            narrowed.iter().all(|item| item.label.starts_with(":smil")),
            "every item answers what was typed: {:?}",
            narrowed.iter().map(|item| &item.label).collect::<Vec<_>>(),
        );
    }

    #[test]
    fn only_the_line_the_cursor_is_on_is_read() {
        assert_eq!(at("std:\n:sm", 1, 3), Some((0, "sm".to_string())));
        assert_eq!(at(":sm\nstd:", 1, 4), None, "the line above does not carry");
    }

    #[test]
    fn an_item_replaces_the_typed_shortcode_with_its_emoji() {
        let items = offered("hi :smil", 0, 8);
        let smile = items
            .iter()
            .find(|item| item.label == ":smile:")
            .expect("smile is offered");

        assert_eq!(smile.filter_text.as_deref(), Some("smile"));
        assert_eq!(smile.detail.as_deref(), Some("\u{1f604}"));
        let Some(lsp_types::CompletionTextEdit::Edit(edit)) = &smile.text_edit else {
            panic!("an emoji item carries its own edit");
        };
        assert_eq!(edit.new_text, "\u{1f604}");
        assert_eq!(
            (edit.range.start, edit.range.end),
            (Position::new(0, 3), Position::new(0, 8)),
            "the edit spans the colon through the cursor",
        );
    }

    #[test]
    fn code_that_only_looks_like_a_shortcode_is_left_alone() {
        assert!(offered("use std::", 0, 9).is_empty());
        assert!(offered("let x: T", 0, 6).is_empty());
    }

    /// The second range names text that only the first change creates. The `é`
    /// is two bytes but one UTF-16 unit. A change applied out of order, or
    /// measured in the wrong unit, leaves other text.
    #[test]
    fn a_ranged_change_edits_the_kept_rope() {
        let change =
            |range: Option<(u32, u32, u32, u32)>, text: &str| TextDocumentContentChangeEvent {
                range: range.map(|(line, column, end_line, end_column)| {
                    lsp_types::Range::new(
                        Position::new(line, column),
                        Position::new(end_line, end_column),
                    )
                }),
                range_length: None,
                text: text.to_string(),
            };
        TestScheduler::new().block_on(async {
            let server = EmojiLsp::new();
            let uri = Uri::from_str("file:///notes.md").expect("valid uri");
            let kept = || server.docs.lock().expect("emoji docs poisoned")[&uri].to_string();
            let changed = |version, content_changes| DidChangeTextDocumentParams {
                text_document: VersionedTextDocumentIdentifier::new(uri.clone(), version),
                content_changes,
            };

            let text = "h\u{e9}llo\nworld\n".to_string();
            let open = TextDocumentItem::new(uri.clone(), "markdown".to_string(), 0, text);
            server
                .did_open(DidOpenTextDocumentParams {
                    text_document: open,
                })
                .await
                .expect("did_open");

            let edits = vec![
                change(Some((1, 0, 1, 0)), "big "),
                change(Some((1, 4, 1, 9)), "there"),
                change(Some((0, 1, 0, 3)), "e"),
            ];
            server
                .did_change(changed(1, edits))
                .await
                .expect("did_change");
            assert_eq!(
                kept(),
                "hello\nbig there\n",
                "each change lands in order, at UTF-8 columns"
            );

            server
                .did_change(changed(2, vec![change(None, ":sm\n")]))
                .await
                .expect("did_change");
            assert_eq!(kept(), ":sm\n", "a change with no range replaces it all");
        });
    }
}
