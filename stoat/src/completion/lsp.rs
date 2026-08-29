//! LSP-completion source. Calls
//! [`crate::host::LspHost::completion`] for the focused buffer's
//! cursor position, then translates each `lsp_types::CompletionItem`
//! into the unified [`crate::completion::CompletionItem`] shape so
//! the popup, the trigger pipeline, and the acceptance handler can
//! treat every source identically.

use crate::{
    buffer::TextBufferSnapshot,
    completion::{
        anchor_range, CompletionContext, CompletionItem, CompletionItemKind, CompletionSource,
    },
    host::{LspHost, OffsetEncoding},
    lsp::util,
};
use lsp_types::{
    CompletionItem as LspCompletionItem, CompletionItemKind as LspCompletionItemKind,
    CompletionParams, CompletionResponse, CompletionTextEdit, Documentation,
};
use std::sync::Arc;

/// What one server offered, and whether that is all of it.
#[derive(Default)]
pub struct Answer {
    pub items: Vec<CompletionItem>,
    /// `false` where the server marked its list incomplete, meaning it stopped
    /// early and wants asking again as the prefix grows rather than having the
    /// client narrow what it gave.
    pub complete: bool,
}

/// Fetch LSP completions for the cursor described by `params`.
///
/// Returns the server raw items and whether it called the list complete.
/// Translating them is [`translate_all`], kept apart so it runs somewhere
/// other than where this awaits.
///
/// A server that errors or answers `None` yields no items, and is reported
/// incomplete so nothing is narrowed from an answer that never came.
pub async fn fetch(lsp: &dyn LspHost, params: CompletionParams) -> (Vec<LspCompletionItem>, bool) {
    match lsp.completion(params).await {
        Ok(Some(response)) => extract_items(response),
        _ => (Vec::new(), false),
    }
}

/// Turn a server's raw items into the popup's own, in the coordinates of the
/// text the request was built against.
///
/// Every item costs several string clones, two anchors, and two rope descents,
/// so a large answer is a stretch of work worth keeping off the thread that
/// paints. Nothing here awaits or touches the app, which is what lets a caller
/// run it wherever it wants.
pub fn translate_all(
    items: Vec<LspCompletionItem>,
    ctx: &CompletionContext<'_>,
    buffer: &TextBufferSnapshot,
    server: &Arc<str>,
    encoding: OffsetEncoding,
) -> Vec<CompletionItem> {
    items
        .into_iter()
        .map(|item| translate(item, server, ctx, buffer, encoding))
        .collect()
}

/// Split a response into its items and whether the server called the list
/// complete.
///
/// A bare array carries no such flag, and the protocol reads that as complete.
fn extract_items(response: CompletionResponse) -> (Vec<LspCompletionItem>, bool) {
    match response {
        CompletionResponse::Array(items) => (items, true),
        CompletionResponse::List(list) => (list.items, !list.is_incomplete),
    }
}

fn translate(
    lsp_item: LspCompletionItem,
    server: &Arc<str>,
    ctx: &CompletionContext<'_>,
    buffer: &TextBufferSnapshot,
    encoding: OffsetEncoding,
) -> CompletionItem {
    let (byte_range, edit_text) = match &lsp_item.text_edit {
        Some(CompletionTextEdit::Edit(edit)) => (
            util::lsp_range_to_byte_range(&buffer.visible_text, edit.range, encoding),
            Some(edit.new_text.clone()),
        ),
        Some(CompletionTextEdit::InsertAndReplace(edit)) => (
            util::lsp_range_to_byte_range(&buffer.visible_text, edit.replace, encoding),
            Some(edit.new_text.clone()),
        ),
        None => (ctx.prefix_range.clone(), None),
    };

    let insert_text = edit_text
        .or_else(|| lsp_item.insert_text.clone())
        .unwrap_or_else(|| lsp_item.label.clone());

    let is_snippet = matches!(
        lsp_item.insert_text_format,
        Some(lsp_types::InsertTextFormat::SNIPPET)
    );

    CompletionItem {
        label: lsp_item.label.clone(),
        source: CompletionSource::Lsp,
        kind: lsp_item.kind.and_then(map_kind),
        detail: lsp_item.detail.clone(),
        documentation: documentation_string(lsp_item.documentation.as_ref()),
        replace_range: anchor_range(buffer, byte_range),
        insert_text,
        is_snippet,
        lsp_item: Some(Box::new(lsp_item)),
        server: Some(Arc::clone(server)),
    }
}

/// Flatten an LSP [`Documentation`] value into plain text. Markdown
/// content is kept verbatim. The popup footer renders the first line.
pub(crate) fn documentation_string(doc: Option<&Documentation>) -> Option<String> {
    match doc? {
        Documentation::String(text) => Some(text.clone()),
        Documentation::MarkupContent(markup) => Some(markup.value.clone()),
    }
}

fn map_kind(kind: LspCompletionItemKind) -> Option<CompletionItemKind> {
    Some(match kind {
        LspCompletionItemKind::METHOD => CompletionItemKind::Method,
        LspCompletionItemKind::FUNCTION | LspCompletionItemKind::CONSTRUCTOR => {
            CompletionItemKind::Function
        },
        LspCompletionItemKind::FIELD | LspCompletionItemKind::PROPERTY => CompletionItemKind::Field,
        LspCompletionItemKind::VARIABLE | LspCompletionItemKind::VALUE => {
            CompletionItemKind::Variable
        },
        LspCompletionItemKind::CLASS | LspCompletionItemKind::INTERFACE => {
            CompletionItemKind::Class
        },
        LspCompletionItemKind::MODULE => CompletionItemKind::Module,
        LspCompletionItemKind::ENUM | LspCompletionItemKind::ENUM_MEMBER => {
            CompletionItemKind::Enum
        },
        LspCompletionItemKind::STRUCT => CompletionItemKind::Struct,
        LspCompletionItemKind::CONSTANT => CompletionItemKind::Constant,
        LspCompletionItemKind::KEYWORD => CompletionItemKind::Keyword,
        LspCompletionItemKind::FILE => CompletionItemKind::File,
        LspCompletionItemKind::FOLDER => CompletionItemKind::Folder,
        LspCompletionItemKind::SNIPPET => CompletionItemKind::Snippet,
        _ => CompletionItemKind::Other,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        buffer::{BufferId, TextBuffer},
        completion::replace_offsets,
        host::{completion_params, FakeLsp},
    };
    use lsp_types::{Position, Range, TextEdit};
    use stoat_scheduler::TestScheduler;

    fn ctx_at(prefix_start: usize, prefix_end: usize) -> CompletionContext<'static> {
        CompletionContext {
            cursor_offset: prefix_end,
            prefix: "",
            prefix_range: prefix_start..prefix_end,
            text_before_cursor: "",
        }
    }

    fn snapshot(text: &str) -> TextBufferSnapshot {
        TextBuffer::with_text(BufferId::new(1), text).snapshot
    }

    fn run<F: Future<Output = T>, T>(future: F) -> T {
        TestScheduler::new().block_on(future)
    }

    /// Fetch and translate in one call, which is what the app does across a
    /// blocking job. These tests are about what a server answer becomes, not
    /// where the translation runs.
    fn fetch_items(
        ctx: &CompletionContext<'_>,
        server: &str,
        lsp: &dyn LspHost,
        params: CompletionParams,
        buffer: &TextBufferSnapshot,
        encoding: OffsetEncoding,
    ) -> Answer {
        let (raw, complete) = run(fetch(lsp, params));
        Answer {
            items: translate_all(raw, ctx, buffer, &Arc::from(server), encoding),
            complete,
        }
    }

    #[test]
    fn empty_response_returns_no_items() {
        let lsp = FakeLsp::new();
        let buffer = snapshot("fn main() {}\n");
        let params = completion_params("/src/lib.rs", 0, 0);
        let ctx = ctx_at(0, 0);
        let items = fetch_items(
            &ctx,
            "test-server",
            &lsp,
            params,
            &buffer,
            OffsetEncoding::Utf16,
        )
        .items;
        assert_eq!(items, Vec::new());
    }

    #[test]
    fn programmed_labels_translate_with_default_replace_range() {
        let lsp = FakeLsp::new();
        lsp.set_completions("/src/lib.rs", 0, 5, &["foo", "bar", "baz"]);
        let buffer = snapshot("hello world\n");
        let params = completion_params("/src/lib.rs", 0, 5);
        let ctx = ctx_at(2, 7);
        let items = fetch_items(
            &ctx,
            "test-server",
            &lsp,
            params,
            &buffer,
            OffsetEncoding::Utf16,
        )
        .items;
        assert_eq!(items.len(), 3);
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(labels, ["foo", "bar", "baz"]);
        for item in &items {
            assert_eq!(item.source, CompletionSource::Lsp);
            assert_eq!(item.kind, None);
            assert_eq!(replace_offsets(&buffer, item), 2..7);
            assert_eq!(item.detail, None);
            assert_eq!(item.insert_text, item.label);
        }
    }

    /// A server's whole answer names the same server, so the name is shared
    /// rather than copied per item. A large answer runs to thousands of them.
    #[test]
    fn every_item_from_one_server_shares_its_name() {
        let lsp = FakeLsp::new();
        lsp.set_completions("/src/lib.rs", 0, 5, &["foo", "bar", "baz"]);
        let buffer = snapshot("hello world\n");
        let items = fetch_items(
            &ctx_at(2, 7),
            "test-server",
            &lsp,
            completion_params("/src/lib.rs", 0, 5),
            &buffer,
            OffsetEncoding::Utf16,
        )
        .items;

        let names: Vec<*const u8> = items
            .iter()
            .map(|item| item.server.as_ref().expect("an lsp item names its server"))
            .map(|name| name.as_ptr())
            .collect();
        assert_eq!(names.len(), 3, "the fixture has items to share between");
        assert!(
            names.windows(2).all(|pair| pair[0] == pair[1]),
            "one allocation carries the name for the whole answer",
        );
    }

    #[test]
    fn text_edit_overrides_replace_range_and_insert_text() {
        let lsp = FakeLsp::new();
        lsp.set_completion_items(
            "/src/lib.rs",
            0,
            5,
            vec![LspCompletionItem {
                label: "println!".into(),
                text_edit: Some(CompletionTextEdit::Edit(TextEdit {
                    range: Range::new(Position::new(0, 0), Position::new(0, 5)),
                    new_text: "println!(\"\")".into(),
                })),
                ..LspCompletionItem::default()
            }],
        );
        let buffer = snapshot("print\n");
        let params = completion_params("/src/lib.rs", 0, 5);
        let ctx = ctx_at(0, 5);
        let items = fetch_items(
            &ctx,
            "test-server",
            &lsp,
            params,
            &buffer,
            OffsetEncoding::Utf16,
        )
        .items;
        assert_eq!(items.len(), 1);
        assert_eq!(replace_offsets(&buffer, &items[0]), 0..5);
        assert_eq!(items[0].insert_text, "println!(\"\")");
    }

    #[test]
    fn detail_propagates_through() {
        let lsp = FakeLsp::new();
        lsp.set_completion_items(
            "/src/lib.rs",
            0,
            0,
            vec![LspCompletionItem {
                label: "open".into(),
                detail: Some("fn open(path: &Path) -> io::Result<File>".into()),
                ..LspCompletionItem::default()
            }],
        );
        let buffer = snapshot("");
        let params = completion_params("/src/lib.rs", 0, 0);
        let ctx = ctx_at(0, 0);
        let items = fetch_items(
            &ctx,
            "test-server",
            &lsp,
            params,
            &buffer,
            OffsetEncoding::Utf16,
        )
        .items;
        assert_eq!(
            items[0].detail.as_deref(),
            Some("fn open(path: &Path) -> io::Result<File>"),
        );
    }

    #[test]
    fn known_kind_maps_through() {
        let lsp = FakeLsp::new();
        lsp.set_completion_items(
            "/src/lib.rs",
            0,
            0,
            vec![LspCompletionItem {
                label: "push".into(),
                kind: Some(LspCompletionItemKind::METHOD),
                ..LspCompletionItem::default()
            }],
        );
        let buffer = snapshot("");
        let params = completion_params("/src/lib.rs", 0, 0);
        let ctx = ctx_at(0, 0);
        let items = fetch_items(
            &ctx,
            "test-server",
            &lsp,
            params,
            &buffer,
            OffsetEncoding::Utf16,
        )
        .items;
        assert_eq!(items[0].kind, Some(CompletionItemKind::Method));
    }

    #[test]
    fn unknown_kind_falls_back_to_other() {
        let lsp = FakeLsp::new();
        lsp.set_completion_items(
            "/src/lib.rs",
            0,
            0,
            vec![LspCompletionItem {
                label: "x".into(),
                kind: Some(LspCompletionItemKind::COLOR),
                ..LspCompletionItem::default()
            }],
        );
        let buffer = snapshot("");
        let params = completion_params("/src/lib.rs", 0, 0);
        let ctx = ctx_at(0, 0);
        let items = fetch_items(
            &ctx,
            "test-server",
            &lsp,
            params,
            &buffer,
            OffsetEncoding::Utf16,
        )
        .items;
        assert_eq!(items[0].kind, Some(CompletionItemKind::Other));
    }

    #[test]
    fn insert_text_falls_back_to_label_when_neither_text_edit_nor_insert_text() {
        let lsp = FakeLsp::new();
        lsp.set_completions("/src/lib.rs", 0, 0, &["bare_label"]);
        let buffer = snapshot("");
        let params = completion_params("/src/lib.rs", 0, 0);
        let ctx = ctx_at(0, 0);
        let items = fetch_items(
            &ctx,
            "test-server",
            &lsp,
            params,
            &buffer,
            OffsetEncoding::Utf16,
        )
        .items;
        assert_eq!(items[0].insert_text, "bare_label");
    }

    #[test]
    fn insert_text_field_overrides_label_when_no_text_edit() {
        let lsp = FakeLsp::new();
        lsp.set_completion_items(
            "/src/lib.rs",
            0,
            0,
            vec![LspCompletionItem {
                label: "method (display)".into(),
                insert_text: Some("method".into()),
                ..LspCompletionItem::default()
            }],
        );
        let buffer = snapshot("");
        let params = completion_params("/src/lib.rs", 0, 0);
        let ctx = ctx_at(0, 0);
        let items = fetch_items(
            &ctx,
            "test-server",
            &lsp,
            params,
            &buffer,
            OffsetEncoding::Utf16,
        )
        .items;
        assert_eq!(items[0].insert_text, "method");
        assert_eq!(items[0].label, "method (display)");
    }
}
