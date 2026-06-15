use crate::{
    editor::{Editor, EditorEvent},
    globals::LanguageRegistry,
    lsp::popup::{popup_container, popup_origin_below},
    theme::ActiveTheme,
    workspace::Workspace,
};
use gpui::{
    deferred, div, point, Bounds, Context, Entity, FontWeight, HighlightStyle, IntoElement,
    ParentElement, Pixels, Point, Render, SharedString, Size, Styled, StyledText, Subscription,
    Task, WeakEntity, Window,
};
use lsp_types::{
    Documentation, ParameterLabel, SignatureHelp, SignatureHelpParams, SignatureInformation,
    TextDocumentIdentifier, TextDocumentPositionParams, Uri,
};
use std::{ops::Range, path::Path, str::FromStr, sync::Arc};
use stoat::{
    host::{LanguageServerFeature, LspServer},
    lsp::util::byte_offset_to_lsp_pos,
};
use stoat_text::{Anchor, Bias};

/// Drives `textDocument/signatureHelp` requests as the primary cursor
/// moves through a function call's argument list. Owned per-editor,
/// subscribes to the host editor's [`EditorEvent::Changed`] stream and
/// re-queries whenever the cursor's buffer offset transitions to a new
/// position. The cached signatures feed the floating widget rendered by
/// the editor layer.
///
/// Behaviour mirrors [`crate::lsp::HoverPopup`]:
/// - cursor offset equal to `anchored_at` -> no-op (the cache already corresponds to this position)
/// - cursor offset moved -> drop any in-flight request (its `Task` is replaced and the future
///   cancelled), clear the cache, and spawn a new request whose completion writes the signatures
///   back
///
/// The language server reports `None` outside a call context, so this
/// layer needs no syntactic gating; the cursor sitting between a call's
/// parentheses is what produces signatures.
///
/// Each request resolves the workspace's persistent, document-synced
/// server from the [`crate::LspManager`] cache.
pub struct SignatureHelpManager {
    editor: WeakEntity<Editor>,
    anchored_at: Option<Anchor>,
    signatures: Vec<SignatureInformation>,
    active_signature: Option<u32>,
    active_parameter: Option<u32>,
    pending_task: Option<Task<()>>,
    /// Monotonic id for the most recent signature-help RPC spawn. The
    /// spawn captures the id before launching; the response branch
    /// re-checks it so a late reply for an earlier cursor position
    /// cannot overwrite the cache's current content.
    request_seq: u64,
    _subscription: Subscription,
}

impl SignatureHelpManager {
    pub fn new(editor: Entity<Editor>, cx: &mut Context<'_, Self>) -> Self {
        let weak = editor.downgrade();
        let subscription = cx.subscribe(&editor, |this, _editor, _event: &EditorEvent, cx| {
            this.reconcile(cx);
        });
        Self {
            editor: weak,
            anchored_at: None,
            signatures: Vec::new(),
            active_signature: None,
            active_parameter: None,
            pending_task: None,
            request_seq: 0,
            _subscription: subscription,
        }
    }

    pub fn signatures(&self) -> &[SignatureInformation] {
        &self.signatures
    }

    pub fn active_signature(&self) -> Option<u32> {
        self.active_signature
    }

    pub fn active_parameter(&self) -> Option<u32> {
        self.active_parameter
    }

    pub fn anchored_at(&self) -> Option<&Anchor> {
        self.anchored_at.as_ref()
    }

    pub(crate) fn bump_request_id(&mut self) -> u64 {
        self.request_seq += 1;
        self.request_seq
    }

    pub(crate) fn request_id(&self) -> u64 {
        self.request_seq
    }

    /// Read the host editor's primary cursor offset and reconcile the
    /// cache against it: sit tight when the cursor has not moved,
    /// otherwise drop stale signatures and kick off a fresh request.
    fn reconcile(&mut self, cx: &mut Context<'_, Self>) {
        let Some(editor) = self.editor.upgrade() else {
            self.clear();
            return;
        };
        if !is_in_insert_mode(&editor, cx) {
            if !self.signatures.is_empty() || self.pending_task.is_some() {
                self.clear();
                cx.notify();
            }
            return;
        }
        let snapshot = editor.read(cx).multi_buffer().read(cx).snapshot();
        let head = editor.read(cx).selections().newest_anchor().head();
        let offset = snapshot.resolve_anchor(&head);
        if self
            .anchored_at
            .as_ref()
            .map(|anchor| snapshot.resolve_anchor(anchor))
            == Some(offset)
        {
            return;
        }
        self.pending_task = None;
        self.signatures.clear();
        self.active_signature = None;
        self.active_parameter = None;
        self.anchored_at = Some(snapshot.anchor_at(offset, Bias::Left));
        cx.notify();

        let Some(request) = SignatureHelpRequest::build(&editor, offset, cx) else {
            return;
        };
        let request_id = self.bump_request_id();
        let task = cx.spawn(async move |this, cx| {
            let server =
                super::cached_server(&request.workspace, request.language.clone(), cx).await;
            let outcome = request.run(server).await;
            let _ = this.update(cx, |manager, cx| {
                if manager.request_id() != request_id {
                    // A newer request superseded this one; the stale
                    // reply corresponds to a previous cursor position.
                    return;
                }
                manager.store(outcome);
                cx.notify();
            });
        });
        self.pending_task = Some(task);
    }

    fn store(&mut self, help: Option<SignatureHelp>) {
        match help {
            Some(help) => {
                self.signatures = help.signatures;
                self.active_signature = help.active_signature;
                self.active_parameter = help.active_parameter;
            },
            None => {
                self.signatures.clear();
                self.active_signature = None;
                self.active_parameter = None;
            },
        }
    }

    fn clear(&mut self) {
        self.pending_task = None;
        self.signatures.clear();
        self.active_signature = None;
        self.active_parameter = None;
        self.anchored_at = None;
    }
}

impl Render for SignatureHelpManager {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        if self.signatures.is_empty() {
            return empty().into_any_element();
        }
        let Some(editor) = self.editor.upgrade() else {
            return empty().into_any_element();
        };
        let Some(anchor) = self.anchored_at else {
            return empty().into_any_element();
        };
        let (bounds, cell, display_map, multi_buffer) = {
            let editor_ref = editor.read(cx);
            (
                editor_ref.text_region_bounds(),
                editor_ref.cell_size(),
                editor_ref.display_map().clone(),
                editor_ref.multi_buffer().clone(),
            )
        };
        let (Some(bounds), Some(cell)) = (bounds, cell) else {
            return empty().into_any_element();
        };
        let display_snapshot = display_map.update(cx, |dm, _| dm.snapshot());
        let mb_snapshot = multi_buffer.read(cx).snapshot();
        let buffer_point = mb_snapshot.point_for_anchor(&anchor);
        let display = display_snapshot.buffer_to_display(buffer_point, Bias::Left);

        let sig_idx = self.active_signature.unwrap_or(0) as usize;
        let Some(signature) = self.signatures.get(sig_idx) else {
            return empty().into_any_element();
        };
        let active = signature.active_parameter.or(self.active_parameter);
        let active_param = active.and_then(|i| signature.parameters.as_ref()?.get(i as usize));
        let highlight = active_param.and_then(|p| active_param_range(&signature.label, &p.label));
        let doc = active_param
            .and_then(|p| p.documentation.as_ref())
            .and_then(documentation_first_line);

        let theme = cx.theme();
        let line_count = if doc.is_some() { 2 } else { 1 };
        let origin = signature_popup_origin(bounds, cell, display.row, display.column, line_count);

        let runs: Vec<(Range<usize>, HighlightStyle)> = highlight
            .map(|range| {
                vec![(
                    range,
                    HighlightStyle {
                        color: Some(theme.cursor),
                        font_weight: Some(FontWeight::BOLD),
                        ..Default::default()
                    },
                )]
            })
            .unwrap_or_default();
        let label =
            StyledText::new(SharedString::from(signature.label.clone())).with_highlights(runs);

        let mut content = div().flex().flex_col().child(label);
        if let Some(doc) = doc {
            content = content.child(
                div()
                    .text_color(theme.muted_text)
                    .child(SharedString::from(doc)),
            );
        }
        deferred(popup_container(origin, cx).child(content))
            .with_priority(2)
            .into_any_element()
    }
}

fn empty() -> impl IntoElement {
    div()
}

fn is_in_insert_mode(editor: &Entity<Editor>, cx: &Context<'_, SignatureHelpManager>) -> bool {
    let Some(workspace) = editor.read(cx).workspace().cloned() else {
        return false;
    };
    let Some(workspace) = workspace.upgrade() else {
        return false;
    };
    let sm = workspace.read(cx).input_state_machine().clone();
    sm.read(cx).mode() == "insert"
}

/// Byte range of the active parameter `param` within the signature
/// `label`, or `None` when it cannot be located. `Simple` matches the
/// first substring occurrence; `LabelOffsets` are LSP UTF-16 code-unit
/// offsets converted to byte offsets.
fn active_param_range(label: &str, param: &ParameterLabel) -> Option<Range<usize>> {
    match param {
        ParameterLabel::Simple(text) => {
            let start = label.find(text.as_str())?;
            Some(start..start + text.len())
        },
        ParameterLabel::LabelOffsets([start, end]) => {
            let start = utf16_offset_to_byte(label, *start)?;
            let end = utf16_offset_to_byte(label, *end)?;
            (start <= end).then_some(start..end)
        },
    }
}

/// Byte offset in `s` at UTF-16 code-unit offset `target`, or `None` when
/// `target` exceeds the string's UTF-16 length.
fn utf16_offset_to_byte(s: &str, target: u32) -> Option<usize> {
    let target = target as usize;
    let mut units = 0;
    for (byte, ch) in s.char_indices() {
        if units == target {
            return Some(byte);
        }
        units += ch.len_utf16();
    }
    (units == target).then_some(s.len())
}

/// First non-empty line of an LSP [`Documentation`] value, trimmed.
fn documentation_first_line(doc: &Documentation) -> Option<String> {
    let text = match doc {
        Documentation::String(s) => s.as_str(),
        Documentation::MarkupContent(markup) => markup.value.as_str(),
    };
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_string)
}

/// Pixel origin for the signature box: one row below the cursor, or
/// `line_count` rows above it when the box would overflow the bottom of
/// `bounds`.
fn signature_popup_origin(
    bounds: Bounds<Pixels>,
    cell: Size<Pixels>,
    row: u32,
    col: u32,
    line_count: u32,
) -> Point<Pixels> {
    let below = popup_origin_below(bounds, cell, row, col);
    let box_height = cell.height * line_count as f32;
    if below.y + box_height <= bounds.origin.y + bounds.size.height {
        return below;
    }
    let x = bounds.origin.x + cell.width * col as f32;
    let y = bounds.origin.y + cell.height * row as f32 - box_height;
    point(x, y)
}

struct SignatureHelpRequest {
    workspace: Option<WeakEntity<Workspace>>,
    language: Arc<stoat_language::Language>,
    uri: Uri,
    offset: usize,
    rope: stoat_text::Rope,
}

impl SignatureHelpRequest {
    fn build(
        editor: &Entity<Editor>,
        offset: usize,
        cx: &mut Context<'_, SignatureHelpManager>,
    ) -> Option<Self> {
        let path = editor.read(cx).file_path()?.to_path_buf();
        let workspace = editor.read(cx).workspace().cloned();
        let language = cx.try_global::<LanguageRegistry>()?.0.for_path(&path)?;
        let uri = path_to_uri(&path)?;
        let rope = editor
            .read(cx)
            .multi_buffer()
            .read(cx)
            .snapshot()
            .rope()
            .clone();
        Some(Self {
            workspace,
            language,
            uri,
            offset,
            rope,
        })
    }

    async fn run(self, server: Option<Arc<dyn LspServer>>) -> Option<SignatureHelp> {
        let server = server?;
        if !server.supports_feature(LanguageServerFeature::SignatureHelp) {
            return None;
        }
        let encoding = server.offset_encoding();
        let position = byte_offset_to_lsp_pos(&self.rope, self.offset, encoding);
        let params = SignatureHelpParams {
            context: None,
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: self.uri },
                position,
            },
            work_done_progress_params: Default::default(),
        };
        match server.signature_help(params).await {
            Ok(help) => help,
            Err(err) => {
                tracing::warn!(target: "stoat_gui::lsp::signature_help", ?err, "signature help request failed");
                None
            },
        }
    }
}

fn path_to_uri(path: &Path) -> Option<Uri> {
    let path_str = path.to_str()?;
    Uri::from_str(&format!("file://{path_str}")).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        buffer::Buffer,
        diff_map::DiffMap,
        display_map::DisplayMap,
        editor::EditorMode,
        globals::{ExecutorGlobal, LspHostGlobal},
        multi_buffer::MultiBuffer,
        workspace::Workspace,
    };
    use gpui::{px, size, AppContext, TestAppContext};
    use lsp_types::{
        ParameterInformation, ParameterLabel, ServerCapabilities, SignatureHelpOptions,
    };
    use std::path::PathBuf;
    use stoat::{
        buffer::BufferId,
        host::{
            fake::{FakeLsp, FakeLspHost},
            LspHost,
        },
    };
    use stoat_scheduler::{Executor, TestScheduler};

    #[test]
    fn active_param_range_simple_finds_substring() {
        let label = "fn add(x: i32, y: i32) -> i32";
        assert_eq!(
            active_param_range(label, &ParameterLabel::Simple("y: i32".to_string())),
            Some(15..21)
        );
    }

    #[test]
    fn active_param_range_label_offsets_map_utf16_to_bytes() {
        let label = "fn add(x: i32, y: i32)";
        assert_eq!(
            active_param_range(label, &ParameterLabel::LabelOffsets([7, 13])),
            Some(7..13)
        );
    }

    #[test]
    fn active_param_range_label_offsets_handle_multibyte() {
        let label = "café(α: T)";
        let range =
            active_param_range(label, &ParameterLabel::LabelOffsets([5, 6])).expect("range");
        assert_eq!(&label[range], "α");
    }

    #[test]
    fn signature_popup_origin_below_when_room() {
        let bounds = Bounds {
            origin: point(px(0.), px(0.)),
            size: size(px(200.), px(200.)),
        };
        let origin = signature_popup_origin(bounds, size(px(8.), px(16.)), 2, 3, 1);
        assert_eq!(origin, point(px(24.), px(48.)));
    }

    #[test]
    fn signature_popup_origin_above_when_no_room() {
        let bounds = Bounds {
            origin: point(px(0.), px(0.)),
            size: size(px(200.), px(200.)),
        };
        let origin = signature_popup_origin(bounds, size(px(8.), px(16.)), 12, 0, 2);
        assert_eq!(origin, point(px(0.), px(160.)));
    }

    fn install_globals(cx: &mut TestAppContext) -> Arc<FakeLsp> {
        let lsp = Arc::new(FakeLsp::new());
        let lsp_host = Arc::new(FakeLspHost::new(lsp.clone())) as Arc<dyn LspHost>;
        let executor = Executor::new(Arc::new(TestScheduler::new()));
        cx.update(|cx| {
            cx.set_global(LspHostGlobal(lsp_host));
            cx.set_global(LanguageRegistry::standard());
            cx.set_global(ExecutorGlobal(executor));
        });
        lsp
    }

    fn build_workspace_editor(
        cx: &mut TestAppContext,
        path: &Path,
        text: &str,
    ) -> (Entity<Workspace>, Entity<Editor>) {
        let path = path.to_path_buf();
        cx.update(|cx| {
            let workspace = cx.new(|cx| Workspace::new("main", PathBuf::from("/tmp/repo"), cx));
            let workspace_handle = workspace.downgrade();
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
            let editor = cx.new(|cx| {
                let mut ed = Editor::new(multi, display, diff, EditorMode::full(), cx);
                ed.set_workspace(Some(workspace_handle));
                ed.set_file_path(Some(path), cx);
                ed
            });
            (workspace, editor)
        })
    }

    fn set_mode(cx: &mut TestAppContext, ws: &Entity<Workspace>, mode: &str) {
        let sm = ws.read_with(cx, |w, _| w.input_state_machine().clone());
        sm.update(cx, |sm, _| {
            sm.set_mode_for_test(stoat::keymap::StateValue::String(mode.into()))
        });
    }

    fn sample_help() -> SignatureHelp {
        SignatureHelp {
            signatures: vec![SignatureInformation {
                label: "fn add(x: i32, y: i32) -> i32".to_string(),
                documentation: None,
                parameters: Some(vec![
                    ParameterInformation {
                        label: ParameterLabel::Simple("x: i32".to_string()),
                        documentation: None,
                    },
                    ParameterInformation {
                        label: ParameterLabel::Simple("y: i32".to_string()),
                        documentation: None,
                    },
                ]),
                active_parameter: Some(0),
            }],
            active_signature: Some(0),
            active_parameter: Some(0),
        }
    }

    fn signature_help_capabilities() -> ServerCapabilities {
        ServerCapabilities {
            signature_help_provider: Some(SignatureHelpOptions::default()),
            ..Default::default()
        }
    }

    #[test]
    fn caches_signatures_when_cursor_sits_in_call() {
        let mut cx = TestAppContext::single();
        let lsp = install_globals(&mut cx);
        let path = PathBuf::from("/tmp/main.rs");
        lsp.set_capabilities(signature_help_capabilities());
        lsp.set_signature_help(path.to_str().unwrap(), 0, 4, sample_help());

        let (workspace, editor) = build_workspace_editor(&mut cx, &path, "add()\n");
        set_mode(&mut cx, &workspace, "insert");
        let manager = cx.update(|cx| {
            let editor_clone = editor.clone();
            cx.new(|cx| SignatureHelpManager::new(editor_clone, cx))
        });
        editor.update(&mut cx, |ed, cx| ed.set_cursor_at_grid(0, 4, cx));
        cx.run_until_parked();

        let (signatures, active_parameter, anchored) = manager.read_with(&cx, |m, _| {
            (
                m.signatures().to_vec(),
                m.active_parameter(),
                m.anchored_at().is_some(),
            )
        });
        assert_eq!(signatures, sample_help().signatures);
        assert_eq!(active_parameter, Some(0));
        assert!(anchored, "manager anchors at the cursor it requested for");
    }

    #[test]
    fn bump_request_id_increments_and_records_latest() {
        let mut cx = TestAppContext::single();
        let _lsp = install_globals(&mut cx);
        let path = PathBuf::from("/tmp/main.rs");
        let (_workspace, editor) = build_workspace_editor(&mut cx, &path, "add()\n");
        let manager = cx.update(|cx| cx.new(|cx| SignatureHelpManager::new(editor.clone(), cx)));

        let first = manager.update(&mut cx, |m, _| m.bump_request_id());
        let second = manager.update(&mut cx, |m, _| m.bump_request_id());

        assert_eq!(first, 1);
        assert_eq!(second, 2);
        assert_eq!(
            manager.read_with(&cx, |m, _| m.request_id()),
            2,
            "request_id must track the most recent bump",
        );
    }

    #[test]
    fn cache_stays_empty_at_unprogrammed_position() {
        let mut cx = TestAppContext::single();
        let lsp = install_globals(&mut cx);
        let path = PathBuf::from("/tmp/main.rs");
        lsp.set_capabilities(signature_help_capabilities());
        lsp.set_signature_help(path.to_str().unwrap(), 0, 4, sample_help());

        let (workspace, editor) = build_workspace_editor(&mut cx, &path, "add()\n");
        set_mode(&mut cx, &workspace, "insert");
        let manager = cx.update(|cx| {
            let editor_clone = editor.clone();
            cx.new(|cx| SignatureHelpManager::new(editor_clone, cx))
        });
        editor.update(&mut cx, |ed, cx| ed.set_cursor_at_grid(0, 0, cx));
        cx.run_until_parked();

        let signatures = manager.read_with(&cx, |m, _| m.signatures().to_vec());
        assert!(
            signatures.is_empty(),
            "cursor outside the programmed call position yields no signatures"
        );
    }
}
