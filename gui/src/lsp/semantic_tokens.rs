use crate::{
    editor::{Editor, EditorEvent},
    globals::{LanguageRegistry, LspHostGlobal},
    theme::Theme as ThemeGlobal,
};
use gpui::{Context, Entity, Subscription, Task, WeakEntity};
use lsp_types::{
    SemanticToken, SemanticTokenType, SemanticTokensLegend, SemanticTokensParams,
    SemanticTokensResult, SemanticTokensServerCapabilities, TextDocumentIdentifier, Uri,
};
use std::{path::Path, str::FromStr, sync::Arc};
use stoat::{
    buffer::BufferId,
    display_map::{
        highlights::{HighlightStyleId, HighlightStyleInterner, SemanticTokenHighlight},
        syntax_theme::SyntaxStyles,
    },
    host::{LanguageServerFeature, LspHost},
    lsp::util::lsp_pos_to_byte_offset,
    theme::Theme,
};
use stoat_language::HighlightId;
use stoat_text::Bias;

/// Drives `textDocument/semanticTokens/full` for the active buffer
/// and pushes the decoded tokens into the editor's
/// [`crate::display_map::DisplayMap`] via
/// [`crate::display_map::DisplayMap::set_semantic_tokens`]. Owned
/// per-editor, subscribes to the editor's [`EditorEvent::Changed`]
/// stream, gates re-requests by a `(uri, buffer_id, buffer_version)`
/// signature, and invalidates the prior token set on the display map
/// the moment a new request is in flight so a stale palette doesn't
/// linger across edits.
///
/// Each request launches a fresh language server through the global
/// [`LspHostGlobal`] factory. Mirrors the per-request shape used by
/// the inlay-hint manager; both share the same per-language cache
/// follow-up.
///
/// FIXME: route through a per-language `LspServer` cache instead of
/// launching a new child process per request.
///
/// FIXME: modifier-bitset styling deferred. LSP token modifiers
/// (declaration / readonly / static / ...) are dropped today; only
/// the base type drives the highlight style.
pub struct SemanticTokensManager {
    editor: WeakEntity<Editor>,
    last_signature: Option<RequestSignature>,
    pending_task: Option<Task<()>>,
    _subscription: Subscription,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RequestSignature {
    uri: Uri,
    buffer_id: BufferId,
    buffer_version: u64,
}

impl SemanticTokensManager {
    pub fn new(editor: Entity<Editor>, cx: &mut Context<'_, Self>) -> Self {
        let weak_editor = editor.downgrade();
        let subscription = cx.subscribe(&editor, |this, _editor, _event: &EditorEvent, cx| {
            this.reconcile(cx);
        });
        Self {
            editor: weak_editor,
            last_signature: None,
            pending_task: None,
            _subscription: subscription,
        }
    }

    fn reconcile(&mut self, cx: &mut Context<'_, Self>) {
        let Some(editor) = self.editor.upgrade() else {
            return;
        };
        if !editor.read(cx).mode().is_full() {
            return;
        }
        let display_map = editor.read(cx).display_map().downgrade();
        let Some(request) = SemanticTokensRequest::build(&editor, cx) else {
            return;
        };
        if self.last_signature.as_ref() == Some(&request.signature) {
            return;
        }

        // Drop the prior tokens immediately so the next paint doesn't
        // render stale styles while the new request is in flight.
        let buffer_id = request.signature.buffer_id;
        if let Some(dm) = display_map.upgrade() {
            dm.update(cx, |dm, cx| dm.invalidate_semantic_tokens(buffer_id, cx));
        }

        self.last_signature = Some(request.signature.clone());
        let signature = request.signature.clone();
        let task = cx.spawn(async move |this, cx| {
            let outcome = request.run().await;
            let _ = this.update(cx, |this, cx| {
                if this.last_signature.as_ref() != Some(&signature) {
                    return;
                }
                this.apply(buffer_id, outcome, &display_map, cx);
            });
        });
        self.pending_task = Some(task);
    }

    fn apply(
        &mut self,
        buffer_id: BufferId,
        outcome: Option<(Arc<[SemanticTokenHighlight]>, Arc<HighlightStyleInterner>)>,
        display_map: &WeakEntity<crate::display_map::DisplayMap>,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(display_map) = display_map.upgrade() else {
            return;
        };
        let Some((tokens, interner)) = outcome else {
            return;
        };
        if tokens.is_empty() {
            display_map.update(cx, |dm, cx| dm.invalidate_semantic_tokens(buffer_id, cx));
            return;
        }
        display_map.update(cx, |dm, cx| {
            dm.set_semantic_tokens(buffer_id, tokens, interner, cx)
        });
    }
}

struct SemanticTokensRequest {
    host: Arc<dyn LspHost>,
    language: Arc<stoat_language::Language>,
    workspace_root: std::path::PathBuf,
    uri: Uri,
    rope: stoat_text::Rope,
    buffer_snapshot: stoat::multi_buffer::MultiBufferSnapshot,
    theme: Theme,
    signature: RequestSignature,
}

impl SemanticTokensRequest {
    fn build(editor: &Entity<Editor>, cx: &mut Context<'_, SemanticTokensManager>) -> Option<Self> {
        let path = editor.read(cx).file_path()?.to_path_buf();
        let host = cx.try_global::<LspHostGlobal>()?.0.clone();
        let language = cx.try_global::<LanguageRegistry>()?.0.for_path(&path)?;
        let uri = path_to_uri(&path)?;

        let buffer = editor
            .read(cx)
            .multi_buffer()
            .read(cx)
            .as_singleton()
            .cloned()?;
        let buffer_id = buffer.read(cx).read(|b| b.buffer_id());

        let buffer_snapshot = editor.read(cx).multi_buffer().read(cx).snapshot();
        let rope = buffer_snapshot.rope().clone();
        let buffer_version = buffer_snapshot.version();

        let theme = cx
            .try_global::<ThemeGlobal>()
            .map(|t| t.0.clone())
            .unwrap_or_else(Theme::empty);

        let workspace_root = path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| path.clone());

        Some(Self {
            host,
            language,
            workspace_root,
            uri: uri.clone(),
            rope,
            buffer_snapshot,
            theme,
            signature: RequestSignature {
                uri,
                buffer_id,
                buffer_version,
            },
        })
    }

    async fn run(self) -> Option<(Arc<[SemanticTokenHighlight]>, Arc<HighlightStyleInterner>)> {
        let server = match self.host.launch(&self.language, &self.workspace_root).await {
            Ok(s) => s,
            Err(err) => {
                tracing::warn!(
                    target: "stoat_gui::lsp::semantic_tokens",
                    ?err,
                    "failed to launch LSP server"
                );
                return None;
            },
        };
        let _ = server.initialize(Some(self.uri.clone())).await;
        if !server.supports_feature(LanguageServerFeature::SemanticTokens) {
            return None;
        }
        let legend = legend_from_capabilities(&server.capabilities())?;
        let encoding = server.offset_encoding();
        let params = SemanticTokensParams {
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
            text_document: TextDocumentIdentifier { uri: self.uri },
        };
        let response = match server.semantic_tokens_full(params).await {
            Ok(Some(r)) => r,
            Ok(None) => {
                return Some((
                    Arc::from(Vec::new()),
                    Arc::new(HighlightStyleInterner::default()),
                ));
            },
            Err(err) => {
                tracing::warn!(
                    target: "stoat_gui::lsp::semantic_tokens",
                    ?err,
                    "semantic_tokens_full request failed"
                );
                return None;
            },
        };
        let tokens = match response {
            SemanticTokensResult::Tokens(t) => t.data,
            SemanticTokensResult::Partial(p) => p.data,
        };

        let syntax_styles = SyntaxStyles::from_theme(&self.theme);
        let legend_to_style = resolve_legend_styles(&legend, &syntax_styles);

        let mut highlights = Vec::with_capacity(tokens.len());
        let mut line: u32 = 0;
        let mut start_col: u32 = 0;
        for SemanticToken {
            delta_line,
            delta_start,
            length,
            token_type,
            token_modifiers_bitset: _,
        } in tokens
        {
            if delta_line != 0 {
                line = line.saturating_add(delta_line);
                start_col = delta_start;
            } else {
                start_col = start_col.saturating_add(delta_start);
            }
            let Some(Some(style_id)) = legend_to_style.get(token_type as usize).copied() else {
                continue;
            };
            let start_offset = lsp_pos_to_byte_offset(
                &self.rope,
                lsp_types::Position::new(line, start_col),
                encoding,
            );
            let end_offset = lsp_pos_to_byte_offset(
                &self.rope,
                lsp_types::Position::new(line, start_col.saturating_add(length)),
                encoding,
            );
            if end_offset <= start_offset {
                continue;
            }
            let start_anchor = self.buffer_snapshot.anchor_at(start_offset, Bias::Right);
            let end_anchor = self.buffer_snapshot.anchor_at(end_offset, Bias::Left);
            highlights.push(SemanticTokenHighlight {
                range: start_anchor..end_anchor,
                style: style_id,
            });
        }

        Some((Arc::from(highlights), syntax_styles.interner.clone()))
    }
}

fn legend_from_capabilities(caps: &lsp_types::ServerCapabilities) -> Option<SemanticTokensLegend> {
    match caps.semantic_tokens_provider.as_ref()? {
        SemanticTokensServerCapabilities::SemanticTokensOptions(opts) => Some(opts.legend.clone()),
        SemanticTokensServerCapabilities::SemanticTokensRegistrationOptions(opts) => {
            Some(opts.semantic_tokens_options.legend.clone())
        },
    }
}

fn resolve_legend_styles(
    legend: &SemanticTokensLegend,
    styles: &SyntaxStyles,
) -> Vec<Option<HighlightStyleId>> {
    legend
        .token_types
        .iter()
        .map(|t| {
            let theme_key = lsp_token_type_to_theme_key(t)?;
            let id = highlight_id_for_theme_key(theme_key, styles)?;
            styles.id_for_highlight(id)
        })
        .collect()
}

fn highlight_id_for_theme_key(key: &str, styles: &SyntaxStyles) -> Option<HighlightId> {
    let idx = styles.theme_keys().iter().position(|k| *k == key)?;
    Some(HighlightId(idx as u32))
}

fn lsp_token_type_to_theme_key(t: &SemanticTokenType) -> Option<&'static str> {
    Some(match t.as_str() {
        "namespace" | "type" | "class" | "enum" | "struct" | "typeParameter" => "type",
        "interface" => "type.interface",
        "parameter" => "variable.parameter",
        "variable" | "event" => "variable",
        "property" => "property",
        "enumMember" => "constant",
        "function" => "function",
        "method" => "function.method",
        "macro" => "function.special",
        "keyword" | "modifier" => "keyword",
        "comment" => "comment",
        "string" | "regexp" => "string",
        "number" => "number",
        "operator" => "operator",
        "decorator" => "attribute",
        _ => return None,
    })
}

fn path_to_uri(path: &Path) -> Option<Uri> {
    let path_str = path.to_str()?;
    Uri::from_str(&format!("file://{path_str}")).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        buffer::Buffer, diff_map::DiffMap, display_map::DisplayMap, editor::EditorMode,
        globals::ExecutorGlobal, multi_buffer::MultiBuffer,
    };
    use gpui::{AppContext, TestAppContext};
    use lsp_types::{
        SemanticTokens, SemanticTokensFullOptions, SemanticTokensOptions, ServerCapabilities,
    };
    use std::path::PathBuf;
    use stoat::host::fake::{FakeLsp, FakeLspHost};
    use stoat_scheduler::{Executor, TestScheduler};

    fn install_globals(cx: &mut TestAppContext) -> Arc<FakeLsp> {
        let lsp = Arc::new(FakeLsp::new());
        let lsp_host = Arc::new(FakeLspHost::new(lsp.clone())) as Arc<dyn LspHost>;
        let executor = Executor::new(Arc::new(TestScheduler::new()));
        cx.update(|cx| {
            cx.set_global(LspHostGlobal(lsp_host));
            cx.set_global(LanguageRegistry::standard());
            cx.set_global(ExecutorGlobal(executor));
            cx.set_global(ThemeGlobal::load_from_source(
                "theme t { syntax.function.fg = blue; syntax.keyword.fg = red; }",
                "t",
            ));
        });
        lsp
    }

    fn build_editor(cx: &mut TestAppContext, path: &Path, text: &str) -> Entity<Editor> {
        let path = path.to_path_buf();
        cx.update(|cx| {
            let buffer = cx.new(|_| Buffer::with_text(BufferId::new(0), text));
            let executor = cx.global::<ExecutorGlobal>().0.clone();
            let multi = cx.new({
                let buffer = buffer.clone();
                |cx| MultiBuffer::singleton(buffer, cx)
            });
            let display = cx.new({
                let buffer = buffer.clone();
                |cx| DisplayMap::new(buffer, executor.clone(), cx)
            });
            let diff = cx.new({
                let buffer = buffer.clone();
                |cx| DiffMap::new(buffer, cx)
            });
            cx.new(|cx| {
                let mut ed = Editor::new(multi, display, diff, EditorMode::full(), cx);
                ed.set_file_path(Some(path), cx);
                ed
            })
        })
    }

    fn legend_with(types: &[SemanticTokenType]) -> SemanticTokensLegend {
        SemanticTokensLegend {
            token_types: types.to_vec(),
            token_modifiers: Vec::new(),
        }
    }

    fn caps_with_legend(legend: SemanticTokensLegend) -> ServerCapabilities {
        ServerCapabilities {
            semantic_tokens_provider: Some(
                SemanticTokensServerCapabilities::SemanticTokensOptions(SemanticTokensOptions {
                    work_done_progress_options: Default::default(),
                    legend,
                    range: Some(false),
                    full: Some(SemanticTokensFullOptions::Bool(true)),
                }),
            ),
            ..Default::default()
        }
    }

    fn token(delta_line: u32, delta_start: u32, length: u32, type_idx: u32) -> SemanticToken {
        SemanticToken {
            delta_line,
            delta_start,
            length,
            token_type: type_idx,
            token_modifiers_bitset: 0,
        }
    }

    fn semantic_tokens_count_for_buffer(
        display_map: &Entity<DisplayMap>,
        cx: &mut TestAppContext,
        buffer_id: BufferId,
    ) -> usize {
        display_map.update(cx, |dm, _| {
            let snap = dm.snapshot();
            snap.semantic_token_highlights()
                .get(&buffer_id)
                .map(|(tokens, _)| tokens.len())
                .unwrap_or(0)
        })
    }

    #[test]
    fn programmed_tokens_are_applied_to_display_map() {
        let mut cx = TestAppContext::single();
        let lsp = install_globals(&mut cx);
        let path = PathBuf::from("/tmp/main.rs");
        let legend = legend_with(&[SemanticTokenType::FUNCTION]);
        lsp.set_capabilities(caps_with_legend(legend));
        lsp.set_semantic_tokens_full(
            path.to_str().unwrap(),
            SemanticTokensResult::Tokens(SemanticTokens {
                result_id: None,
                data: vec![token(0, 0, 3, 0)],
            }),
        );

        let editor = build_editor(&mut cx, &path, "foo bar");
        let display_map = editor.read_with(&cx, |ed, _| ed.display_map().clone());
        let _manager = cx.update(|cx| {
            let editor_clone = editor.clone();
            cx.new(|cx| SemanticTokensManager::new(editor_clone, cx))
        });

        editor.update(&mut cx, |ed, cx| {
            let buffer = ed.multi_buffer().read(cx).as_singleton().cloned().unwrap();
            buffer.update(cx, |b, cx| b.edit(0..0, "", cx));
        });
        cx.run_until_parked();

        let count = semantic_tokens_count_for_buffer(&display_map, &mut cx, BufferId::new(0));
        assert_eq!(count, 1);
    }

    #[test]
    fn unknown_token_types_are_dropped() {
        let mut cx = TestAppContext::single();
        let lsp = install_globals(&mut cx);
        let path = PathBuf::from("/tmp/unknown.rs");
        // "unknownThing" doesn't map to any THEME_KEYS entry.
        let legend = legend_with(&[SemanticTokenType::new("unknownThing")]);
        lsp.set_capabilities(caps_with_legend(legend));
        lsp.set_semantic_tokens_full(
            path.to_str().unwrap(),
            SemanticTokensResult::Tokens(SemanticTokens {
                result_id: None,
                data: vec![token(0, 0, 3, 0)],
            }),
        );

        let editor = build_editor(&mut cx, &path, "foo bar");
        let display_map = editor.read_with(&cx, |ed, _| ed.display_map().clone());
        let _manager = cx.update(|cx| {
            let editor_clone = editor.clone();
            cx.new(|cx| SemanticTokensManager::new(editor_clone, cx))
        });

        editor.update(&mut cx, |ed, cx| {
            let buffer = ed.multi_buffer().read(cx).as_singleton().cloned().unwrap();
            buffer.update(cx, |b, cx| b.edit(0..0, "", cx));
        });
        cx.run_until_parked();

        let count = semantic_tokens_count_for_buffer(&display_map, &mut cx, BufferId::new(0));
        assert_eq!(count, 0);
    }

    #[test]
    fn skips_request_when_server_lacks_semantic_tokens_capability() {
        let mut cx = TestAppContext::single();
        let lsp = install_globals(&mut cx);
        // Capabilities default has no semantic_tokens_provider.
        lsp.set_capabilities(ServerCapabilities::default());
        let path = PathBuf::from("/tmp/no_caps.rs");
        lsp.set_semantic_tokens_full(
            path.to_str().unwrap(),
            SemanticTokensResult::Tokens(SemanticTokens {
                result_id: None,
                data: vec![token(0, 0, 3, 0)],
            }),
        );

        let editor = build_editor(&mut cx, &path, "foo bar");
        let display_map = editor.read_with(&cx, |ed, _| ed.display_map().clone());
        let _manager = cx.update(|cx| {
            let editor_clone = editor.clone();
            cx.new(|cx| SemanticTokensManager::new(editor_clone, cx))
        });

        editor.update(&mut cx, |ed, cx| {
            let buffer = ed.multi_buffer().read(cx).as_singleton().cloned().unwrap();
            buffer.update(cx, |b, cx| b.edit(0..0, "", cx));
        });
        cx.run_until_parked();

        let count = semantic_tokens_count_for_buffer(&display_map, &mut cx, BufferId::new(0));
        assert_eq!(count, 0);
    }

    #[test]
    fn delta_decoding_accumulates_positions_across_lines() {
        let mut cx = TestAppContext::single();
        let lsp = install_globals(&mut cx);
        let path = PathBuf::from("/tmp/multi.rs");
        let legend = legend_with(&[SemanticTokenType::FUNCTION, SemanticTokenType::KEYWORD]);
        lsp.set_capabilities(caps_with_legend(legend));
        lsp.set_semantic_tokens_full(
            path.to_str().unwrap(),
            SemanticTokensResult::Tokens(SemanticTokens {
                result_id: None,
                data: vec![
                    token(0, 0, 3, 0), // line 0, col 0..3, function
                    token(0, 4, 3, 1), // same line, col 4..7, keyword
                    token(1, 0, 2, 0), // line 1, col 0..2, function
                ],
            }),
        );

        let editor = build_editor(&mut cx, &path, "foo bar\nab cd");
        let display_map = editor.read_with(&cx, |ed, _| ed.display_map().clone());
        let _manager = cx.update(|cx| {
            let editor_clone = editor.clone();
            cx.new(|cx| SemanticTokensManager::new(editor_clone, cx))
        });

        editor.update(&mut cx, |ed, cx| {
            let buffer = ed.multi_buffer().read(cx).as_singleton().cloned().unwrap();
            buffer.update(cx, |b, cx| b.edit(0..0, "", cx));
        });
        cx.run_until_parked();

        let count = semantic_tokens_count_for_buffer(&display_map, &mut cx, BufferId::new(0));
        assert_eq!(count, 3);
    }
}
