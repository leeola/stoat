//! LSP-completion source. Calls
//! [`crate::host::LspHost::completion`] for the focused buffer's
//! cursor position, then translates each `lsp_types::CompletionItem`
//! into the unified [`crate::completion::CompletionItem`] shape so
//! the popup, the trigger pipeline, and the acceptance handler can
//! treat every source identically.

use crate::{
    completion::{CompletionContext, CompletionItem, CompletionItemKind, CompletionSource},
    host::{LspHost, OffsetEncoding},
    lsp::util,
};
use lsp_types::{
    CompletionItem as LspCompletionItem, CompletionItemKind as LspCompletionItemKind,
    CompletionParams, CompletionResponse, CompletionTextEdit,
};
use stoat_text::Rope;

/// Fetch LSP completions for the cursor described by `params`.
///
/// `rope` and `encoding` are needed to convert any `text_edit`
/// range the server returns back into byte offsets for the unified
/// item's `replace_range`. Items without a `text_edit` fall back
/// to `ctx.prefix_range` so acceptance rewrites the current prefix.
///
/// Returns an empty `Vec` when the server returns `Err` or
/// `Ok(None)`.
pub async fn fetch(
    ctx: &CompletionContext<'_>,
    lsp: &dyn LspHost,
    params: CompletionParams,
    rope: &Rope,
    encoding: OffsetEncoding,
) -> Vec<CompletionItem> {
    let items = match lsp.completion(params).await {
        Ok(Some(response)) => extract_items(response),
        _ => return Vec::new(),
    };
    items
        .into_iter()
        .map(|item| translate(item, ctx, rope, encoding))
        .collect()
}

fn extract_items(response: CompletionResponse) -> Vec<LspCompletionItem> {
    match response {
        CompletionResponse::Array(items) => items,
        CompletionResponse::List(list) => list.items,
    }
}

fn translate(
    lsp_item: LspCompletionItem,
    ctx: &CompletionContext<'_>,
    rope: &Rope,
    encoding: OffsetEncoding,
) -> CompletionItem {
    let (replace_range, edit_text) = match &lsp_item.text_edit {
        Some(CompletionTextEdit::Edit(edit)) => (
            util::lsp_range_to_byte_range(rope, edit.range, encoding),
            Some(edit.new_text.clone()),
        ),
        Some(CompletionTextEdit::InsertAndReplace(edit)) => (
            util::lsp_range_to_byte_range(rope, edit.replace, encoding),
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
        label: lsp_item.label,
        source: CompletionSource::Lsp,
        kind: lsp_item.kind.and_then(map_kind),
        detail: lsp_item.detail,
        replace_range,
        insert_text,
        is_snippet,
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
    use crate::host::{completion_params, FakeLsp};
    use lsp_types::{Position, Range, TextEdit};
    use stoat_scheduler::TestScheduler;
    use stoat_text::Rope;

    fn ctx_at(prefix_start: usize, prefix_end: usize) -> CompletionContext<'static> {
        CompletionContext {
            cursor_offset: prefix_end,
            prefix: "",
            prefix_range: prefix_start..prefix_end,
            text_before_cursor: "",
        }
    }

    fn run<F: std::future::Future<Output = T>, T>(future: F) -> T {
        TestScheduler::new().block_on(future)
    }

    #[test]
    fn empty_response_returns_no_items() {
        let lsp = FakeLsp::new();
        let rope = Rope::from("fn main() {}\n");
        let params = completion_params("/src/lib.rs", 0, 0);
        let ctx = ctx_at(0, 0);
        let items = run(fetch(&ctx, &lsp, params, &rope, OffsetEncoding::Utf16));
        assert_eq!(items, Vec::new());
    }

    #[test]
    fn programmed_labels_translate_with_default_replace_range() {
        let lsp = FakeLsp::new();
        lsp.set_completions("/src/lib.rs", 0, 5, &["foo", "bar", "baz"]);
        let rope = Rope::from("hello world\n");
        let params = completion_params("/src/lib.rs", 0, 5);
        let ctx = ctx_at(2, 7);
        let items = run(fetch(&ctx, &lsp, params, &rope, OffsetEncoding::Utf16));
        assert_eq!(items.len(), 3);
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(labels, ["foo", "bar", "baz"]);
        for item in &items {
            assert_eq!(item.source, CompletionSource::Lsp);
            assert_eq!(item.kind, None);
            assert_eq!(item.replace_range, 2..7);
            assert_eq!(item.detail, None);
            assert_eq!(item.insert_text, item.label);
        }
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
        let rope = Rope::from("print\n");
        let params = completion_params("/src/lib.rs", 0, 5);
        let ctx = ctx_at(0, 5);
        let items = run(fetch(&ctx, &lsp, params, &rope, OffsetEncoding::Utf16));
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].replace_range, 0..5);
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
        let rope = Rope::from("");
        let params = completion_params("/src/lib.rs", 0, 0);
        let ctx = ctx_at(0, 0);
        let items = run(fetch(&ctx, &lsp, params, &rope, OffsetEncoding::Utf16));
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
        let rope = Rope::from("");
        let params = completion_params("/src/lib.rs", 0, 0);
        let ctx = ctx_at(0, 0);
        let items = run(fetch(&ctx, &lsp, params, &rope, OffsetEncoding::Utf16));
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
        let rope = Rope::from("");
        let params = completion_params("/src/lib.rs", 0, 0);
        let ctx = ctx_at(0, 0);
        let items = run(fetch(&ctx, &lsp, params, &rope, OffsetEncoding::Utf16));
        assert_eq!(items[0].kind, Some(CompletionItemKind::Other));
    }

    #[test]
    fn insert_text_falls_back_to_label_when_neither_text_edit_nor_insert_text() {
        let lsp = FakeLsp::new();
        lsp.set_completions("/src/lib.rs", 0, 0, &["bare_label"]);
        let rope = Rope::from("");
        let params = completion_params("/src/lib.rs", 0, 0);
        let ctx = ctx_at(0, 0);
        let items = run(fetch(&ctx, &lsp, params, &rope, OffsetEncoding::Utf16));
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
        let rope = Rope::from("");
        let params = completion_params("/src/lib.rs", 0, 0);
        let ctx = ctx_at(0, 0);
        let items = run(fetch(&ctx, &lsp, params, &rope, OffsetEncoding::Utf16));
        assert_eq!(items[0].insert_text, "method");
        assert_eq!(items[0].label, "method (display)");
    }
}
