//! The colors a server puts on a buffer, layered over the tree-sitter ones.
//!
//! Tree-sitter colors from the grammar alone, so it cannot tell a local from a
//! static or a trait from a struct. A server resolves the names properly and
//! reports what each one is, and those spans recolor on top of the baseline
//! rather than replacing it. The same reply feeds the symbol-kind index the
//! cursor-aware features read.
//!
//! Replies arrive as a delta against the last one where the server supports it,
//! which is why the raw stream is retained beside the resolved spans. A delta
//! that does not fit the retained stream costs a full pull rather than a
//! silently shifted highlight.
//!
//! No key reaches this. A trigger fires from the post-event fan-out, and a pump
//! collects the reply, both of them background work the user never asks for.

use crate::{
    action_handlers,
    app::Stoat,
    buffer::BufferId,
    buffer_registry::LspSymbolKindIndex,
    display_map::{syntax_theme, HighlightStyleId, SemanticTokenHighlight},
    host::{LspHost, OffsetEncoding},
    lsp::{self, util, LspSymbolKind},
};
use lsp_types::{
    Position, SemanticToken, SemanticTokenType, SemanticTokensDeltaParams, SemanticTokensEdit,
    SemanticTokensFullDeltaResult, SemanticTokensFullOptions, SemanticTokensParams,
    SemanticTokensResult, SemanticTokensServerCapabilities, TextDocumentIdentifier,
};
use std::{path::Path, sync::Arc, time::Duration};
use stoat_text::{Bias, Rope};

/// Debounce before requesting semantic tokens, so a burst of edits collapses into
/// a single request once typing settles.
const SEMANTIC_TOKENS_DEBOUNCE: Duration = Duration::from_millis(500);

/// A decoded LSP semantic token. It pairs an absolute buffer span with the
/// tree-sitter highlight scope stem its type maps to and, separately, the
/// coarser [`LspSymbolKind`] the type names.
///
/// The scope and kind are independent. A token may carry a scope but no kind (a
/// keyword), a kind but no scope (a namespace, which has no highlight
/// equivalent), or both. A token with neither is dropped during decode.
#[derive(Debug, PartialEq)]
struct DecodedToken {
    line: u32,
    start: u32,
    length: u32,
    scope: Option<&'static str>,
    kind: Option<LspSymbolKind>,
}

/// A completed semantic-tokens request's payload.
///
/// It carries the buffer, the buffer version the request was built against, the
/// server's own token stream with the result id naming it, and the resolved
/// `(byte range, scope stem, symbol kind)` spans in request-time coordinates.
/// The scope drives the highlight channel and the kind the symbol-kind index.
/// Each is optional.
///
/// The stream is carried alongside the resolved spans because the next pull
/// patches it rather than re-asking for it, and the result id is what names it
/// to the server.
pub(crate) type SemanticTokensOutcome = (
    BufferId,
    u64,
    Option<String>,
    Vec<SemanticToken>,
    Vec<(
        std::ops::Range<usize>,
        Option<&'static str>,
        Option<LspSymbolKind>,
    )>,
);

/// Request semantic tokens for the focused editor when the server advertises a
/// full-document legend and the `(buffer, version)` key changed.
///
/// A newly-focused buffer and each edit re-request behind a 500ms debounce. The
/// stale anchored tokens keep painting until the fresh response replaces them,
/// so an edit never flashes the buffer plain. Tokens layer over the tree-sitter
/// baseline and never replace it, only recoloring on top.
/// [`pump_lsp_semantic_tokens`] applies the response.
pub(crate) fn semantic_tokens_trigger(stoat: &mut Stoat) {
    let Some((_, buffer_id)) = stoat.focused_editor_ids() else {
        return;
    };
    let Some(version) = lsp::focused_buffer_version(stoat) else {
        return;
    };
    if stoat.last_semantic_tokens_key == Some((buffer_id, version)) {
        return;
    }

    let host = stoat.lsp_for(buffer_id);
    let capabilities = host.capabilities();
    let Some(legend) = semantic_tokens_legend(&capabilities) else {
        return;
    };
    let legend = legend.to_vec();
    let encoding = host.offset_encoding();

    let Some((buffer_id, version, rope, params)) = build_semantic_tokens_request(stoat) else {
        return;
    };

    let key = (buffer_id, version);
    stoat.last_semantic_tokens_key = Some(key);

    // When the buffer is unchanged since tokens were last computed, reinstall
    // the retained set instead of re-requesting behind the debounce.
    if let Some((cached_version, tokens, interner)) =
        stoat.active_workspace().buffers.lsp_tokens_for(buffer_id)
        && cached_version == version
    {
        if let Some(editor) = action_handlers::focused_editor_mut(stoat) {
            editor
                .display_map
                .set_lsp_token_highlights(buffer_id, tokens, interner);
        }
        return;
    }

    // A delta is only worth asking for against a result the server named, and
    // only if it advertises answering them at all. Everything else pulls the
    // whole set, which is also the recovery path when a delta cannot be applied.
    let previous = advertises_token_delta(&capabilities)
        .then(|| {
            stoat
                .active_workspace()
                .buffers
                .lsp_token_source_for(buffer_id)
        })
        .flatten()
        .and_then(|(result_id, data)| result_id.map(|result_id| (result_id, data)));

    let executor = stoat.executor.clone();
    let task = stoat.spawn_woken(async move {
        executor.timer(SEMANTIC_TOKENS_DEBOUNCE).await;
        let (result_id, data) = match previous {
            Some((previous_result_id, previous_data)) => {
                pull_token_delta(host.as_ref(), &params, previous_result_id, &previous_data).await?
            },
            None => pull_tokens_full(host.as_ref(), params).await?,
        };
        let items = convert_semantic_tokens(&data, &legend, &rope, encoding);
        Some((buffer_id, version, result_id, data, items))
    });
    stoat.pending_semantic_tokens.arm(task);
}

/// The whole token stream and the result id the server named it by.
async fn pull_tokens_full(
    host: &dyn LspHost,
    params: SemanticTokensParams,
) -> Option<(Option<String>, Vec<SemanticToken>)> {
    match host.semantic_tokens_full(params).await {
        Ok(Some(SemanticTokensResult::Tokens(tokens))) => Some((tokens.result_id, tokens.data)),
        // A partial result streams its tokens over separate progress
        // notifications, which this path does not collect.
        Ok(Some(SemanticTokensResult::Partial(_))) | Ok(None) => None,
        Err(err) => {
            tracing::warn!(target: "stoat::lsp", ?err, "semantic_tokens_full request failed");
            None
        },
    }
}

/// The token stream after the changes since `previous_result_id`, or the whole
/// one when the server answers with that instead.
///
/// Falls back to a full pull whenever the reply cannot be applied to
/// `previous_data`, so a delta that does not line up costs one extra round trip
/// rather than a silently wrong highlight.
async fn pull_token_delta(
    host: &dyn LspHost,
    params: &SemanticTokensParams,
    previous_result_id: String,
    previous_data: &[SemanticToken],
) -> Option<(Option<String>, Vec<SemanticToken>)> {
    let delta_params = SemanticTokensDeltaParams {
        work_done_progress_params: params.work_done_progress_params.clone(),
        partial_result_params: params.partial_result_params.clone(),
        text_document: params.text_document.clone(),
        previous_result_id,
    };
    let reply = match host.semantic_tokens_full_delta(delta_params).await {
        Ok(reply) => reply,
        Err(err) => {
            tracing::warn!(target: "stoat::lsp", ?err, "semantic_tokens delta request failed");
            None
        },
    };

    match reply {
        Some(SemanticTokensFullDeltaResult::Tokens(tokens)) => {
            Some((tokens.result_id, tokens.data))
        },
        Some(SemanticTokensFullDeltaResult::TokensDelta(delta)) => {
            match apply_token_delta(previous_data, &delta.edits) {
                Some(data) => Some((delta.result_id, data)),
                None => {
                    tracing::warn!(
                        target: "stoat::lsp",
                        "semantic tokens delta did not fit the retained stream, pulling in full",
                    );
                    pull_tokens_full(host, params.clone()).await
                },
            }
        },
        // A partial delta streams its edits elsewhere, and no reply at all means
        // the server declined. Neither leaves anything to patch with.
        Some(SemanticTokensFullDeltaResult::PartialTokensDelta { .. }) | None => {
            pull_tokens_full(host, params.clone()).await
        },
    }
}

fn build_semantic_tokens_request(
    stoat: &mut Stoat,
) -> Option<(BufferId, u64, Rope, SemanticTokensParams)> {
    let (buffer_id, version, rope) = {
        let editor = action_handlers::focused_editor_mut(stoat)?;
        if editor.review_view.is_some() {
            return None;
        }
        let snapshot = editor.display_map.snapshot();
        let buf_snap = snapshot.buffer_snapshot();
        (
            editor.buffer_id,
            buf_snap.version(),
            buf_snap.rope().clone(),
        )
    };

    let path = stoat
        .active_workspace()
        .buffers
        .path_for(buffer_id)
        .map(Path::to_path_buf)?;
    let uri = action_handlers::lsp::path_to_uri(&path)?;
    let params = SemanticTokensParams {
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
        text_document: TextDocumentIdentifier { uri },
    };
    Some((buffer_id, version, rope, params))
}

/// The token-type legend from the server's semantic-tokens capability, or `None`
/// when it advertises no full-document semantic tokens.
fn semantic_tokens_legend(caps: &lsp_types::ServerCapabilities) -> Option<&[SemanticTokenType]> {
    let opts = semantic_tokens_options(caps)?;
    opts.full.as_ref()?;
    Some(&opts.legend.token_types)
}

fn semantic_tokens_options(
    caps: &lsp_types::ServerCapabilities,
) -> Option<&lsp_types::SemanticTokensOptions> {
    match caps.semantic_tokens_provider.as_ref()? {
        SemanticTokensServerCapabilities::SemanticTokensOptions(o) => Some(o),
        SemanticTokensServerCapabilities::SemanticTokensRegistrationOptions(o) => {
            Some(&o.semantic_tokens_options)
        },
    }
}

/// Whether the server will answer `semanticTokens/full/delta`, which it says by
/// advertising full support in its delta-bearing form.
fn advertises_token_delta(caps: &lsp_types::ServerCapabilities) -> bool {
    matches!(
        semantic_tokens_options(caps).and_then(|o| o.full.as_ref()),
        Some(SemanticTokensFullOptions::Delta { delta: Some(true) })
    )
}

/// Apply a delta's edits to the token stream it was measured against.
///
/// `None` when an edit does not fit `previous`, which leaves the caller to pull
/// the whole set instead. A misapplied edit shifts every token after it without
/// failing, so a delta that does not line up is refused rather than guessed at.
///
/// The edits index the flat array of five-u32 tokens the protocol sends, not the
/// tokens themselves, so both bounds are in units of five. They are applied back
/// to front so each one still names a position the ones before it have not
/// moved.
fn apply_token_delta(
    previous: &[SemanticToken],
    edits: &[SemanticTokensEdit],
) -> Option<Vec<SemanticToken>> {
    let mut ordered: Vec<&SemanticTokensEdit> = edits.iter().collect();
    ordered.sort_by_key(|edit| std::cmp::Reverse(edit.start));

    let mut data = previous.to_vec();
    for edit in ordered {
        let start = usize::try_from(edit.start).ok()? / 5;
        let removed = usize::try_from(edit.delete_count).ok()? / 5;
        let end = start.checked_add(removed)?;
        if end > data.len() {
            return None;
        }
        let inserted = edit.data.clone().unwrap_or_default();
        data.splice(start..end, inserted);
    }
    Some(data)
}

/// Decode a server's token stream into `(byte range, scope stem)` spans using
/// the request-time rope.
fn convert_semantic_tokens(
    data: &[SemanticToken],
    legend: &[SemanticTokenType],
    rope: &Rope,
    encoding: OffsetEncoding,
) -> Vec<(
    std::ops::Range<usize>,
    Option<&'static str>,
    Option<LspSymbolKind>,
)> {
    let decoded = decode_semantic_tokens(data, legend);
    let positions: Vec<Position> = decoded
        .iter()
        .flat_map(|t| {
            [
                Position::new(t.line, t.start),
                Position::new(t.line, t.start + t.length),
            ]
        })
        .collect();
    let offsets = util::lsp_positions_to_byte_offsets_batch(rope, &positions, encoding);
    decoded
        .iter()
        .enumerate()
        .map(|(i, t)| (offsets[2 * i]..offsets[2 * i + 1], t.scope, t.kind))
        .collect()
}

/// Map an LSP `SemanticTokenType` name onto a stoat tree-sitter scope stem. Types
/// with no stoat equivalent return `None` and are skipped.
fn lsp_token_scope(token_type: &str) -> Option<&'static str> {
    Some(match token_type {
        "function" | "method" => "function",
        "macro" => "function.special",
        "type" | "class" | "enum" | "interface" | "struct" | "typeParameter" => "type",
        "variable" => "variable",
        "parameter" => "variable.parameter",
        "property" | "enumMember" => "property",
        "keyword" | "modifier" => "keyword",
        "comment" => "comment",
        "string" => "string",
        "number" => "number",
        "operator" => "operator",
        _ => return None,
    })
}

/// Map an LSP `SemanticTokenType` name onto the coarse [`LspSymbolKind`] it
/// names, so the distinction highlight decoding collapses (trait vs struct vs
/// enum, all "type") survives for cursor-aware features. Types that name no
/// symbol -- keywords, punctuation, literals -- return `None`.
fn lsp_symbol_kind(token_type: &str) -> Option<LspSymbolKind> {
    Some(match token_type {
        "interface" => LspSymbolKind::Trait,
        "type" | "class" | "struct" | "enum" | "union" | "typeAlias" | "builtinType"
        | "typeParameter" | "selfTypeKeyword" => LspSymbolKind::Type,
        "function" | "method" => LspSymbolKind::Function,
        "variable" | "parameter" | "property" | "enumMember" | "constParameter" | "selfKeyword" => {
            LspSymbolKind::Value
        },
        "namespace"
        | "macro"
        | "decorator"
        | "event"
        | "derive"
        | "attribute"
        | "label"
        | "lifetime"
        | "unresolvedReference" => LspSymbolKind::Symbol,
        _ => return None,
    })
}

/// Decode the LSP relative token stream into absolute-positioned spans.
///
/// Each token's line and start accumulate from the previous per the LSP encoding.
/// `delta_start` is relative within a line and absolute after a line break. Tokens
/// whose type index falls outside the legend, or whose type maps to neither a
/// highlight scope nor a symbol kind, are skipped.
fn decode_semantic_tokens(
    data: &[SemanticToken],
    legend: &[SemanticTokenType],
) -> Vec<DecodedToken> {
    let mut out = Vec::new();
    let mut line = 0u32;
    let mut col = 0u32;
    for token in data {
        line += token.delta_line;
        if token.delta_line == 0 {
            col += token.delta_start;
        } else {
            col = token.delta_start;
        }
        let Some(ty) = legend.get(token.token_type as usize) else {
            continue;
        };
        let scope = lsp_token_scope(ty.as_str());
        let kind = lsp_symbol_kind(ty.as_str());
        if scope.is_none() && kind.is_none() {
            continue;
        }
        out.push(DecodedToken {
            line,
            start: col,
            length: token.length,
            scope,
            kind,
        });
    }
    out
}

/// Poll any in-flight semantic-tokens request and paint the results onto the
/// focused editor's LSP highlight channel. Returns true when state changed.
pub(crate) fn pump_lsp_semantic_tokens(stoat: &mut Stoat) -> bool {
    let Some(outcome) = stoat.pending_semantic_tokens.poll() else {
        return false;
    };
    if let Some((buffer_id, version, result_id, data, items)) = outcome {
        // Retained before the anchoring below, which drops the reply when the
        // buffer has moved on. The stream is still what the server holds
        // whatever this buffer now reads as, so the next delta can name it.
        stoat
            .active_workspace_mut()
            .buffers
            .store_lsp_token_source(buffer_id, result_id, data);
        apply_semantic_tokens(stoat, buffer_id, version, items);
    }
    true
}

/// Anchor a semantic-token reply onto `buffer_id` and paint it, provided the
/// buffer still reads as it did when the request went out.
///
/// `version` is the buffer version the server measured `items` against. The
/// offsets in them name that text, so anchoring them to a buffer that has moved
/// since would pin each token to whatever now sits at its offset. A reply that
/// misses is dropped rather than adjusted. The trigger is keyed on the buffer
/// version, so the edit that invalidated this reply has already asked for
/// another.
fn apply_semantic_tokens(
    stoat: &mut Stoat,
    buffer_id: BufferId,
    version: u64,
    items: Vec<(
        std::ops::Range<usize>,
        Option<&'static str>,
        Option<LspSymbolKind>,
    )>,
) {
    let live = stoat
        .active_workspace()
        .buffers
        .get(buffer_id)
        .map(|shared| shared.read().expect("buffer poisoned").snapshot.version);
    if live != Some(version) {
        return;
    }

    // The highlight channel takes the scope-bearing spans, the symbol-kind index
    // the kind-bearing ones. A token may feed one, both, or (dropped in decode)
    // neither.
    //
    // Ids come from the shared theme table rather than a per-response interner,
    // so a stored token means the same scope after a theme switch and can be
    // recolored by swapping the table instead of re-requesting it.
    //
    // Split into the shapes `anchors_at_batch` and the zip below want in one
    // walk. Holding the spans first would clone a range per token only to copy
    // its endpoints back out, and both payloads here are `Copy`. Each vector is
    // reserved against the token count, which bounds it above and is the only
    // allocation it takes.
    let mut token_starts: Vec<usize> = Vec::with_capacity(items.len());
    let mut token_ends: Vec<usize> = Vec::with_capacity(items.len());
    let mut token_styles: Vec<HighlightStyleId> = Vec::with_capacity(items.len());
    let mut kind_starts: Vec<usize> = Vec::with_capacity(items.len());
    let mut kind_ends: Vec<usize> = Vec::with_capacity(items.len());
    let mut kinds_seen: Vec<LspSymbolKind> = Vec::with_capacity(items.len());

    for (range, scope, kind) in &items {
        if let Some(style) = (*scope)
            .and_then(syntax_theme::highlight_id_for_key)
            .and_then(|id| stoat.syntax_styles.id_for_highlight(id))
        {
            token_starts.push(range.start);
            token_ends.push(range.end);
            token_styles.push(style);
        }

        if let Some(kind) = *kind {
            kind_starts.push(range.start);
            kind_ends.push(range.end);
            kinds_seen.push(kind);
        }
    }

    let interner = stoat.syntax_styles.interner.clone();

    let ws = stoat.active_workspace_mut();
    let Some(shared) = ws.buffers.get(buffer_id) else {
        return;
    };

    // Anchor against the buffer's own snapshot from the registry, not the
    // focused editor's, so the response lands and is retained even when focus
    // has since moved to another buffer.
    let (tokens, kinds): (Arc<[SemanticTokenHighlight]>, LspSymbolKindIndex) = {
        let buf_snap = shared.read().expect("buffer poisoned").snapshot.clone();

        let tokens: Arc<[SemanticTokenHighlight]> = token_styles
            .into_iter()
            .zip(buf_snap.anchors_at_batch(&token_starts, Bias::Right))
            .zip(buf_snap.anchors_at_batch(&token_ends, Bias::Left))
            .map(|((style, start), end)| SemanticTokenHighlight {
                range: start..end,
                style,
            })
            .collect();

        let kinds: LspSymbolKindIndex = kinds_seen
            .into_iter()
            .zip(buf_snap.anchors_at_batch(&kind_starts, Bias::Right))
            .zip(buf_snap.anchors_at_batch(&kind_ends, Bias::Left))
            .map(|((kind, start), end)| (start..end, kind))
            .collect();

        (tokens, kinds)
    };

    ws.buffers
        .store_lsp_tokens(buffer_id, version, tokens.clone(), interner.clone());
    ws.buffers.store_lsp_symbol_kinds(buffer_id, kinds);
    for editor in ws.editors.values_mut() {
        if editor.buffer_id == buffer_id {
            editor.display_map.set_lsp_token_highlights(
                buffer_id,
                tokens.clone(),
                interner.clone(),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        test_fixture::{open_buffer, open_stcfg_with_server, seed},
        test_harness::TestHarness,
    };
    use stoat_action::OpenFile;

    #[test]
    fn decode_semantic_tokens_accumulates_deltas() {
        use lsp_types::{SemanticToken, SemanticTokenType};
        let legend = vec![
            SemanticTokenType::new("keyword"),
            SemanticTokenType::new("function"),
            SemanticTokenType::new("boolean"),
            SemanticTokenType::new("namespace"),
        ];
        let tok = |delta_line, delta_start, length, token_type| SemanticToken {
            delta_line,
            delta_start,
            length,
            token_type,
            token_modifiers_bitset: 0,
        };
        let data = vec![
            tok(0, 0, 3, 0),
            tok(0, 4, 2, 1),
            tok(1, 2, 5, 0),
            tok(0, 6, 1, 2),
            tok(0, 8, 4, 3),
        ];
        let decoded = decode_semantic_tokens(&data, &legend);
        let want = |line, start, length, scope, kind| DecodedToken {
            line,
            start,
            length,
            scope,
            kind,
        };
        // The boolean token maps to neither a scope nor a kind and is dropped.
        // The namespace token has no highlight scope but keeps its Symbol kind.
        assert_eq!(
            decoded,
            vec![
                want(0, 0, 3, Some("keyword"), None),
                want(0, 4, 2, Some("function"), Some(LspSymbolKind::Function)),
                want(1, 2, 5, Some("keyword"), None),
                want(1, 16, 4, None, Some(LspSymbolKind::Symbol)),
            ]
        );
    }

    #[test]
    fn lsp_token_scope_maps_standard_types() {
        assert_eq!(lsp_token_scope("function"), Some("function"));
        assert_eq!(lsp_token_scope("method"), Some("function"));
        assert_eq!(lsp_token_scope("parameter"), Some("variable.parameter"));
        assert_eq!(lsp_token_scope("struct"), Some("type"));
        assert_eq!(lsp_token_scope("regexp"), None);
    }

    #[test]
    fn lsp_symbol_kind_classifies_token_types() {
        use lsp_symbol_kind;
        assert_eq!(lsp_symbol_kind("interface"), Some(LspSymbolKind::Trait));
        assert_eq!(lsp_symbol_kind("struct"), Some(LspSymbolKind::Type));
        assert_eq!(lsp_symbol_kind("enum"), Some(LspSymbolKind::Type));
        assert_eq!(lsp_symbol_kind("method"), Some(LspSymbolKind::Function));
        assert_eq!(lsp_symbol_kind("parameter"), Some(LspSymbolKind::Value));
        assert_eq!(lsp_symbol_kind("namespace"), Some(LspSymbolKind::Symbol));
        assert_eq!(lsp_symbol_kind("keyword"), None);
        assert_eq!(lsp_symbol_kind("string"), None);
    }

    fn enable_semantic_tokens(h: &TestHarness) {
        use lsp_types::{
            SemanticTokenType, SemanticTokensFullOptions, SemanticTokensLegend,
            SemanticTokensOptions, SemanticTokensServerCapabilities, ServerCapabilities,
        };
        h.fake_lsp().set_capabilities(ServerCapabilities {
            semantic_tokens_provider: Some(
                SemanticTokensServerCapabilities::SemanticTokensOptions(SemanticTokensOptions {
                    legend: SemanticTokensLegend {
                        token_types: vec![SemanticTokenType::new("function")],
                        token_modifiers: vec![],
                    },
                    full: Some(SemanticTokensFullOptions::Bool(true)),
                    range: None,
                    work_done_progress_options: Default::default(),
                }),
            ),
            ..Default::default()
        });
    }

    /// [`enable_semantic_tokens`] with the server also advertising that it
    /// answers `full/delta`.
    fn enable_semantic_token_deltas(h: &TestHarness) {
        use lsp_types::{
            SemanticTokenType, SemanticTokensFullOptions, SemanticTokensLegend,
            SemanticTokensOptions, SemanticTokensServerCapabilities, ServerCapabilities,
        };
        h.fake_lsp().set_capabilities(ServerCapabilities {
            semantic_tokens_provider: Some(
                SemanticTokensServerCapabilities::SemanticTokensOptions(SemanticTokensOptions {
                    legend: SemanticTokensLegend {
                        token_types: vec![SemanticTokenType::new("function")],
                        token_modifiers: vec![],
                    },
                    full: Some(SemanticTokensFullOptions::Delta { delta: Some(true) }),
                    range: None,
                    work_done_progress_options: Default::default(),
                }),
            ),
            ..Default::default()
        });
    }

    /// A one-character token identified by `n`, which it carries as its column
    /// step so a splice's output says which of the originals survived where.
    /// Every token stays on the first line, so a converted stream lands inside a
    /// single-line fixture buffer.
    fn tok(n: u32) -> SemanticToken {
        SemanticToken {
            delta_line: 0,
            delta_start: n,
            length: 1,
            token_type: 0,
            token_modifiers_bitset: 0,
        }
    }

    fn edit(start: u32, delete_count: u32, data: Option<Vec<u32>>) -> SemanticTokensEdit {
        SemanticTokensEdit {
            start,
            delete_count,
            data: data.map(|ns| ns.into_iter().map(tok).collect()),
        }
    }

    /// The protocol numbers an edit over the flat array of five-u32 tokens it
    /// sends, not over the tokens, so every bound is five times the token index.
    /// Reading them as token indices would shift every token after the edit
    /// without failing anywhere.
    #[test]
    fn a_token_delta_replaces_the_span_its_edit_names() {
        let previous = vec![tok(1), tok(2), tok(3), tok(4)];

        assert_eq!(
            apply_token_delta(&previous, &[edit(5, 10, Some(vec![9]))]),
            Some(vec![tok(1), tok(9), tok(4)]),
            "tokens one and two go, replaced by the one the edit carries",
        );
    }

    /// A delete carries no data at all rather than an empty list, and an insert
    /// deletes nothing. Both are ordinary shapes a server sends.
    #[test]
    fn a_token_delta_handles_a_bare_delete_and_a_bare_insert() {
        let previous = vec![tok(1), tok(2), tok(3)];

        assert_eq!(
            apply_token_delta(&previous, &[edit(5, 5, None)]),
            Some(vec![tok(1), tok(3)]),
            "a delete with no data drops its span",
        );
        assert_eq!(
            apply_token_delta(&previous, &[edit(10, 0, Some(vec![8, 9]))]),
            Some(vec![tok(1), tok(2), tok(8), tok(9), tok(3)]),
            "an insert-only edit adds without removing",
        );
    }

    /// Edits name positions in the stream as it was before any of them applied,
    /// so applying them front to back would move the ones still to come.
    #[test]
    fn a_token_delta_applies_several_edits_back_to_front() {
        let previous = vec![tok(1), tok(2), tok(3), tok(4)];

        assert_eq!(
            apply_token_delta(
                &previous,
                &[edit(0, 5, Some(vec![7])), edit(15, 5, Some(vec![8]))],
            ),
            Some(vec![tok(7), tok(2), tok(3), tok(8)]),
        );
    }

    /// A delta measured against a stream this one is not costs a full pull
    /// rather than a splice past the end, which would silently drop tokens.
    #[test]
    fn a_token_delta_reaching_past_the_stream_is_refused() {
        assert_eq!(apply_token_delta(&[tok(1)], &[edit(5, 10, None)]), None,);
    }

    /// The first pull has nothing to diff against, so it asks for the whole
    /// set. Once the server has named a result, the next pull asks only for what
    /// changed, and the spans it installs are the ones the equivalent full reply
    /// would have.
    #[test]
    fn a_second_token_pull_asks_for_a_delta_and_installs_the_same_spans() {
        use lsp_types::{SemanticTokens, SemanticTokensDelta, SemanticTokensResult};
        let mut h = TestHarness::with_size(24, 4);
        enable_semantic_token_deltas(&h);
        let root = seed(&mut h, &[("main.rs", "let x = y\n")]);
        let path = root.join("main.rs");
        let uri = path.to_str().unwrap().to_string();
        open_buffer(&mut h, path.clone());

        h.fake_lsp().set_semantic_tokens_full(
            &uri,
            SemanticTokensResult::Tokens(SemanticTokens {
                result_id: Some("r1".into()),
                data: vec![tok(1), tok(2)],
            }),
        );
        // Arms the trigger now that a reply is programmed for it to receive.
        h.type_keys("escape");
        h.advance_clock(Duration::from_millis(550));
        assert_eq!(lsp_token_count(&mut h), 2, "the first pull is a full one");
        assert!(
            h.fake_lsp().observed_semantic_token_deltas().is_empty(),
            "with nothing to name, it could not have asked for a delta",
        );

        // The delta drops the second token and adds two in its place.
        h.fake_lsp().set_semantic_tokens_full_delta(
            &uri,
            SemanticTokensFullDeltaResult::TokensDelta(SemanticTokensDelta {
                result_id: Some("r2".into()),
                edits: vec![edit(5, 5, Some(vec![3, 4]))],
            }),
        );
        h.type_keys("i");
        h.type_text("z");
        h.advance_clock(Duration::from_millis(550));

        let asked = h.fake_lsp().observed_semantic_token_deltas();
        assert_eq!(
            asked
                .iter()
                .map(|p| p.previous_result_id.clone())
                .collect::<Vec<_>>(),
            ["r1"],
            "the second pull named the result the first one came back with",
        );
        assert_eq!(
            lsp_token_count(&mut h),
            3,
            "and the spliced stream is what got installed",
        );
    }

    fn lsp_token_count(h: &mut TestHarness) -> usize {
        let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
        let snapshot = editor.display_map.snapshot();
        snapshot
            .lsp_token_highlights()
            .values()
            .map(|channel| channel.len())
            .sum()
    }

    #[test]
    fn snapshot_semantic_tokens_recolor_over_tree_sitter() {
        use lsp_types::{SemanticToken, SemanticTokens, SemanticTokensResult};
        let mut h = TestHarness::with_size(24, 4);
        enable_semantic_tokens(&h);
        let root = seed(&mut h, &[("main.rs", "let x = y\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp().set_semantic_tokens_full(
            path.to_str().unwrap(),
            SemanticTokensResult::Tokens(SemanticTokens {
                result_id: None,
                data: vec![SemanticToken {
                    delta_line: 0,
                    delta_start: 8,
                    length: 1,
                    token_type: 0,
                    token_modifiers_bitset: 0,
                }],
            }),
        );
        h.type_keys("escape");
        h.advance_clock(Duration::from_millis(550));
        assert_eq!(lsp_token_count(&mut h), 1);
        h.assert_snapshot("semantic_tokens_recolor");
    }

    /// A token can carry a scope, a kind, or both, so the styled and the kinded
    /// subsets of one reply are different sequences. Anchoring walks each of
    /// them as its own run of offsets, and a run built against the wrong subset
    /// pairs every payload past the first divergence with someone else's span.
    #[test]
    fn a_reply_anchors_its_styles_and_kinds_onto_their_own_spans() {
        let mut h = TestHarness::default();
        let root = seed(&mut h, &[("main.rs", "alpha beta gamma delta\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path);

        let buffer_id = action_handlers::focused_editor_mut(&mut h.stoat)
            .expect("editor")
            .buffer_id;
        let version = h
            .stoat
            .active_workspace()
            .buffers
            .get(buffer_id)
            .expect("buffer")
            .read()
            .expect("buffer poisoned")
            .snapshot
            .version;

        // The first token styles without a kind and the second kinds without a
        // style, so the two sequences diverge before the third, which is in
        // both.
        apply_semantic_tokens(
            &mut h.stoat,
            buffer_id,
            version,
            vec![
                (0..5, Some("function"), None),
                (6..10, None, Some(LspSymbolKind::Type)),
                (11..16, Some("function"), Some(LspSymbolKind::Function)),
            ],
        );

        let ws = h.stoat.active_workspace();
        let snapshot = ws
            .buffers
            .get(buffer_id)
            .expect("buffer")
            .read()
            .expect("buffer poisoned")
            .snapshot
            .clone();
        let (_, tokens, _) = ws.buffers.lsp_tokens_for(buffer_id).expect("tokens stored");

        let spans: Vec<(usize, usize)> = tokens
            .iter()
            .map(|token| {
                (
                    snapshot.resolve_anchor(&token.range.start),
                    snapshot.resolve_anchor(&token.range.end),
                )
            })
            .collect();
        assert_eq!(
            spans,
            [(0, 5), (11, 16)],
            "the styled tokens keep the spans they arrived on, skipping the kind-only one"
        );

        assert_eq!(
            ws.buffers.lsp_symbol_kind_at(buffer_id, 7),
            Some(Some(LspSymbolKind::Type)),
            "the kind-only token indexes on its own span"
        );
        assert_eq!(
            ws.buffers.lsp_symbol_kind_at(buffer_id, 12),
            Some(Some(LspSymbolKind::Function)),
            "and the token carrying both lands its kind where its style went"
        );
    }

    /// The offsets in a reply name the text the server measured them against.
    /// An edit landing while the request is in flight moves that text out from
    /// under them, and painting them anyway pins each token to whatever now
    /// sits at its offset.
    #[test]
    fn a_semantic_token_reply_is_dropped_when_the_buffer_moved_under_it() {
        use lsp_types::{SemanticToken, SemanticTokens, SemanticTokensResult};
        let mut h = TestHarness::with_size(24, 4);
        enable_semantic_tokens(&h);
        let root = seed(&mut h, &[("main.rs", "let x = y\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp().set_semantic_tokens_full(
            path.to_str().unwrap(),
            SemanticTokensResult::Tokens(SemanticTokens {
                result_id: None,
                data: vec![SemanticToken {
                    delta_line: 0,
                    delta_start: 8,
                    length: 1,
                    token_type: 0,
                    token_modifiers_bitset: 0,
                }],
            }),
        );

        // Arm the request, then move the buffer while it sits in the debounce.
        // Editing the registry directly leaves the in-flight task alone, which
        // is the window a background pump's edit lands in.
        h.type_keys("escape");
        let buffer_id = action_handlers::focused_editor_mut(&mut h.stoat)
            .expect("editor")
            .buffer_id;
        h.stoat
            .active_workspace()
            .buffers
            .get(buffer_id)
            .expect("buffer")
            .write()
            .expect("poisoned")
            .edit(0..0, "inserted\n");

        h.advance_clock(Duration::from_millis(550));

        assert_eq!(
            lsp_token_count(&mut h),
            0,
            "the reply measured the text before the insert",
        );

        // The trigger is keyed on the buffer version, so the edit that dropped
        // the reply is itself what asks for the next one.
        semantic_tokens_trigger(&mut h.stoat);
        assert!(
            h.stoat.pending_semantic_tokens.is_pending(),
            "and a fresh request is already out for the moved text",
        );
    }

    fn one_full_token(path: &Path) -> impl Fn(&TestHarness) + '_ {
        use lsp_types::{SemanticToken, SemanticTokens, SemanticTokensResult};
        move |h: &TestHarness| {
            h.fake_lsp().set_semantic_tokens_full(
                path.to_str().unwrap(),
                SemanticTokensResult::Tokens(SemanticTokens {
                    result_id: None,
                    data: vec![SemanticToken {
                        delta_line: 0,
                        delta_start: 8,
                        length: 1,
                        token_type: 0,
                        token_modifiers_bitset: 0,
                    }],
                }),
            );
        }
    }

    #[test]
    fn switching_back_keeps_lsp_tokens_on_first_frame() {
        let mut h = TestHarness::with_size(24, 4);
        enable_semantic_tokens(&h);
        let root = seed(&mut h, &[("a.rs", "let x = y\n"), ("b.rs", "let z = w\n")]);
        let path_a = root.join("a.rs");

        open_buffer(&mut h, path_a.clone());
        one_full_token(&path_a)(&h);
        h.type_keys("escape");
        h.advance_clock(Duration::from_millis(550));
        assert_eq!(lsp_token_count(&mut h), 1, "A receives LSP tokens");

        open_buffer(&mut h, root.join("b.rs"));

        // Switch back to A with no debounce cycle. The fresh editor keeps the
        // LSP highlighting only if seeded from the registry's cached tokens.
        action_handlers::dispatch(&mut h.stoat, &OpenFile { path: path_a });
        assert!(
            lsp_token_count(&mut h) > 0,
            "re-shown buffer keeps LSP tokens on the first frame"
        );
    }

    #[test]
    fn cached_lsp_tokens_skip_the_re_request() {
        let mut h = TestHarness::with_size(24, 4);
        enable_semantic_tokens(&h);
        let root = seed(&mut h, &[("a.rs", "let x = y\n")]);
        let path_a = root.join("a.rs");

        open_buffer(&mut h, path_a.clone());
        one_full_token(&path_a)(&h);
        h.type_keys("escape");
        h.advance_clock(Duration::from_millis(550));
        assert_eq!(lsp_token_count(&mut h), 1);

        // Re-triggering for the same unchanged version reinstalls the cached
        // tokens without spawning a second request.
        h.stoat.last_semantic_tokens_key = None;
        semantic_tokens_trigger(&mut h.stoat);
        assert!(
            !h.stoat.pending_semantic_tokens.is_pending(),
            "a version-current cache hit spawns no request"
        );
        assert_eq!(lsp_token_count(&mut h), 1, "cached tokens are reinstalled");
    }

    /// A cached reinstall after a theme switch paints the current theme.
    ///
    /// The cache is keyed by buffer version alone, so a switch with no edit
    /// reinstalls the same tokens forever. Before the interners were swapped
    /// that meant reinstalling the colors of whichever theme was active when
    /// the response landed.
    #[test]
    fn a_cached_reinstall_after_a_theme_switch_uses_the_new_theme() {
        use crate::display_map::syntax_theme;
        use stoat_action::SetTheme;

        let mut h = TestHarness::with_size(24, 4);
        enable_semantic_tokens(&h);
        let root = seed(&mut h, &[("a.rs", "let x = y\n")]);
        let path = root.join("a.rs");

        open_buffer(&mut h, path.clone());
        one_full_token(&path)(&h);
        h.type_keys("escape");
        h.advance_clock(Duration::from_millis(550));
        assert_eq!(lsp_token_count(&mut h), 1);

        let function = syntax_theme::highlight_id_for_key("function").expect("function is a key");
        let color_now = |h: &mut TestHarness| {
            let style_id = h
                .stoat
                .syntax_styles
                .id_for_highlight(function)
                .expect("function resolves");
            let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
            let snapshot = editor.display_map.snapshot();
            snapshot
                .lsp_token_highlights()
                .values()
                .next()
                .map(|channel| channel.interner[style_id].foreground)
                .expect("a token channel is installed")
        };
        let before = color_now(&mut h);

        action_handlers::dispatch(
            &mut h.stoat,
            &SetTheme {
                name: "gruvbox-light".to_string(),
            },
        );

        // Force the version-hit path. The buffer is unedited, so the trigger
        // reinstalls the cached tokens rather than asking the server again.
        h.stoat.last_semantic_tokens_key = None;
        semantic_tokens_trigger(&mut h.stoat);
        assert!(
            !h.stoat.pending_semantic_tokens.is_pending(),
            "a version-current cache hit spawns no request"
        );
        assert_eq!(lsp_token_count(&mut h), 1, "cached tokens are reinstalled");
        assert_ne!(
            color_now(&mut h),
            before,
            "the reinstalled tokens carry the new theme's colors"
        );
    }

    /// Every scope stem the LSP mapping can emit is a key the shared theme
    /// table knows.
    ///
    /// The two lists live in different files, so one drifting from the other
    /// would silently drop those tokens from highlighting rather than fail to
    /// build.
    #[test]
    fn every_lsp_token_scope_resolves_to_a_theme_key() {
        use crate::display_map::syntax_theme;

        let token_types = [
            "function",
            "method",
            "macro",
            "type",
            "class",
            "enum",
            "interface",
            "struct",
            "typeParameter",
            "variable",
            "parameter",
            "property",
            "enumMember",
            "keyword",
            "modifier",
            "comment",
            "string",
            "number",
            "operator",
        ];
        for token_type in token_types {
            let scope = lsp_token_scope(token_type)
                .unwrap_or_else(|| panic!("{token_type} must map to a scope"));
            assert!(
                syntax_theme::highlight_id_for_key(scope).is_some(),
                "{token_type} maps to {scope}, which is not a theme key"
            );
        }
        assert_eq!(lsp_token_scope("noSuchTokenType"), None);
    }

    /// An applied token carries the shared theme table's id for its scope, not
    /// an id minted per response. That is what lets a theme switch recolor
    /// retained tokens by swapping the table.
    #[test]
    fn lsp_tokens_carry_the_shared_theme_table_id() {
        use crate::display_map::syntax_theme;
        use lsp_types::{
            SemanticToken, SemanticTokenType, SemanticTokens, SemanticTokensFullOptions,
            SemanticTokensLegend, SemanticTokensOptions, SemanticTokensResult,
            SemanticTokensServerCapabilities, ServerCapabilities,
        };

        let mut h = TestHarness::with_size(24, 4);
        h.fake_lsp().set_capabilities(ServerCapabilities {
            semantic_tokens_provider: Some(
                SemanticTokensServerCapabilities::SemanticTokensOptions(SemanticTokensOptions {
                    legend: SemanticTokensLegend {
                        token_types: vec![SemanticTokenType::new("keyword")],
                        token_modifiers: vec![],
                    },
                    full: Some(SemanticTokensFullOptions::Bool(true)),
                    range: None,
                    work_done_progress_options: Default::default(),
                }),
            ),
            ..Default::default()
        });

        let root = seed(&mut h, &[("a.rs", "let x = y\n")]);
        let path = root.join("a.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp().set_semantic_tokens_full(
            path.to_str().unwrap(),
            SemanticTokensResult::Tokens(SemanticTokens {
                result_id: None,
                data: vec![SemanticToken {
                    delta_line: 0,
                    delta_start: 0,
                    length: 3,
                    token_type: 0,
                    token_modifiers_bitset: 0,
                }],
            }),
        );
        h.type_keys("escape");
        h.advance_clock(Duration::from_millis(550));

        let expected = h
            .stoat
            .syntax_styles
            .id_for_highlight(
                syntax_theme::highlight_id_for_key("keyword").expect("keyword is a theme key"),
            )
            .expect("keyword resolves through the shared table");

        let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
        let snapshot = editor.display_map.snapshot();
        let ids: Vec<_> = snapshot
            .lsp_token_highlights()
            .values()
            .flat_map(|channel| channel.iter().map(|token| token.style))
            .collect();
        assert_eq!(ids, vec![expected]);
    }

    #[test]
    fn an_edit_keeps_lsp_tokens_until_the_fresh_set_arrives() {
        use lsp_types::{SemanticToken, SemanticTokens, SemanticTokensResult};

        let mut h = TestHarness::with_size(24, 4);
        enable_semantic_tokens(&h);
        let root = seed(&mut h, &[("a.rs", "let x = y\n")]);
        let path_a = root.join("a.rs");
        let token = |delta_start: u32| SemanticToken {
            delta_line: 0,
            delta_start,
            length: 1,
            token_type: 0,
            token_modifiers_bitset: 0,
        };

        open_buffer(&mut h, path_a.clone());
        h.fake_lsp().set_semantic_tokens_full(
            path_a.to_str().unwrap(),
            SemanticTokensResult::Tokens(SemanticTokens {
                result_id: None,
                data: vec![token(8)],
            }),
        );
        h.type_keys("escape");
        h.advance_clock(Duration::from_millis(550));
        assert_eq!(lsp_token_count(&mut h), 1, "the first token set lands");

        // Editing re-requests behind the debounce. The stale anchored token
        // keeps painting in the meantime instead of clearing to plain.
        h.fake_lsp().set_semantic_tokens_full(
            path_a.to_str().unwrap(),
            SemanticTokensResult::Tokens(SemanticTokens {
                result_id: None,
                data: vec![token(0), token(2)],
            }),
        );
        h.type_keys("i");
        h.type_text("z");
        h.type_keys("escape");
        assert!(
            lsp_token_count(&mut h) > 0,
            "the stale token rides the edit while the new request is in flight",
        );

        h.advance_clock(Duration::from_millis(550));
        assert_eq!(
            lsp_token_count(&mut h),
            2,
            "the fresh set replaces the stale one",
        );
    }

    #[test]
    fn stcfg_buffer_receives_semantic_tokens_from_the_in_process_server() {
        let mut h = TestHarness::with_size(80, 24);
        h.allow_host_swap();
        open_stcfg_with_server(&mut h);

        h.type_text("on init { format_on_save = true; }");
        h.type_keys("escape");

        // did_change (50ms) syncs the buffer to the server before the semantic
        // tokens request (500ms debounce) reads it.
        h.advance_clock(action_handlers::lsp::LSP_DID_CHANGE_DEBOUNCE);
        h.advance_clock(Duration::from_millis(550));

        assert!(
            lsp_token_count(&mut h) > 0,
            "the in-process stcfg server highlights the config buffer",
        );
    }
}
