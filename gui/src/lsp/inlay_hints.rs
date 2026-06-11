use crate::{
    display_map::DisplayMap,
    editor::{Editor, EditorEvent},
    globals::{LanguageRegistry, LspHostGlobal},
};
use gpui::{Context, Entity, Subscription, Task, WeakEntity};
use lsp_types::{InlayHintLabel, InlayHintParams, TextDocumentIdentifier, Uri};
use std::{path::Path, str::FromStr, sync::Arc};
use stoat::{
    display_map::{InlayId, InlayKind},
    host::{LanguageServerFeature, LspHost},
    lsp::util::{byte_range_to_lsp_range, lsp_pos_to_byte_offset},
    DisplayPoint,
};
use stoat_text::Bias;

/// Default visible-row window applied when the editor has not yet
/// reported text-region bounds or cell metrics (no paint has run).
/// Keeps headless callers and freshly-installed editors getting a
/// useful inlay window before the first frame lands.
const DEFAULT_VIEWPORT_ROWS: u32 = 80;

/// Drives `textDocument/inlayHint` requests for the visible buffer
/// range and pushes the results into the editor's [`DisplayMap`] via
/// [`DisplayMap::set_inlay_hints`]. Owned per-editor, subscribes to
/// the editor's [`EditorEvent::Changed`] stream (which already
/// coalesces buffer edits and scroll changes), gates re-requests by
/// a `(uri, buffer_version, start_row, end_row)` signature, and
/// tracks the live [`InlayId`] set so each subsequent splice removes
/// the prior inlays before inserting new ones.
///
/// Each request launches a fresh language server through the global
/// [`LspHostGlobal`] factory. This is wasteful in production -- the
/// per-language LSP server should be cached -- but matches the
/// shape used by [`crate::lsp::HoverPopup`] and
/// [`crate::lsp::CompletionPopup`].
///
/// FIXME: route through a per-language `LspServer` cache instead of
/// launching a new child process per request.
pub struct InlayHintsManager {
    editor: WeakEntity<Editor>,
    current_ids: Vec<InlayId>,
    last_signature: Option<RequestSignature>,
    pending_task: Option<Task<()>>,
    _subscription: Subscription,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RequestSignature {
    uri: Uri,
    buffer_version: u64,
    start_row: u32,
    end_row: u32,
}

impl InlayHintsManager {
    pub fn new(editor: Entity<Editor>, cx: &mut Context<'_, Self>) -> Self {
        let weak_editor = editor.downgrade();
        let subscription = cx.subscribe(&editor, |this, _editor, _event: &EditorEvent, cx| {
            this.reconcile(cx);
        });
        Self {
            editor: weak_editor,
            current_ids: Vec::new(),
            last_signature: None,
            pending_task: None,
            _subscription: subscription,
        }
    }

    pub fn current_inlay_count(&self) -> usize {
        self.current_ids.len()
    }

    fn reconcile(&mut self, cx: &mut Context<'_, Self>) {
        let Some(editor) = self.editor.upgrade() else {
            return;
        };
        if !editor.read(cx).mode().is_full() {
            return;
        }
        let display_map = editor.read(cx).display_map().downgrade();
        let Some(request) = InlayHintsRequest::build(&editor, cx) else {
            return;
        };
        if self.last_signature.as_ref() == Some(&request.signature) {
            return;
        }
        self.last_signature = Some(request.signature.clone());
        let signature = request.signature.clone();
        let task = cx.spawn(async move |this, cx| {
            let outcome = request.run().await;
            let _ = this.update(cx, |this, cx| {
                if this.last_signature.as_ref() != Some(&signature) {
                    return;
                }
                this.apply(outcome, &display_map, cx);
            });
        });
        self.pending_task = Some(task);
    }

    fn apply(
        &mut self,
        outcome: Option<Vec<(stoat_text::Anchor, String, InlayKind)>>,
        display_map: &WeakEntity<DisplayMap>,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(display_map) = display_map.upgrade() else {
            return;
        };
        let remove = std::mem::take(&mut self.current_ids);
        let insert = outcome.unwrap_or_default();
        if remove.is_empty() && insert.is_empty() {
            return;
        }
        let new_ids = display_map.update(cx, |dm, cx| dm.set_inlay_hints(remove, insert, cx));
        self.current_ids = new_ids;
    }
}

struct InlayHintsRequest {
    host: Arc<dyn LspHost>,
    language: Arc<stoat_language::Language>,
    workspace_root: std::path::PathBuf,
    uri: Uri,
    rope: stoat_text::Rope,
    buffer_snapshot: stoat::multi_buffer::MultiBufferSnapshot,
    range_bytes: std::ops::Range<usize>,
    signature: RequestSignature,
}

impl InlayHintsRequest {
    fn build(editor: &Entity<Editor>, cx: &mut Context<'_, InlayHintsManager>) -> Option<Self> {
        let path = editor.read(cx).file_path()?.to_path_buf();
        let host = cx.try_global::<LspHostGlobal>()?.0.clone();
        let language = cx.try_global::<LanguageRegistry>()?.0.for_path(&path)?;
        let uri = path_to_uri(&path)?;
        let display_map = editor.read(cx).display_map().clone();
        let display_snapshot = display_map.update(cx, |dm, _| dm.snapshot());
        let buffer_snapshot = editor.read(cx).multi_buffer().read(cx).snapshot();
        let rope = buffer_snapshot.rope().clone();
        let buffer_version = buffer_snapshot.version();
        let max_buffer_row = rope.max_point().row;

        let (start_row, end_row) = visible_buffer_rows(editor, cx, &display_snapshot)?;
        let end_row = end_row.min(max_buffer_row + 1);
        if end_row <= start_row {
            return None;
        }

        let start_offset = rope.point_to_offset(stoat_text::Point::new(start_row, 0));
        let end_offset = if end_row > max_buffer_row {
            rope.len()
        } else {
            rope.point_to_offset(stoat_text::Point::new(end_row, 0))
        };
        let range_bytes = start_offset..end_offset;

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
            range_bytes,
            signature: RequestSignature {
                uri,
                buffer_version,
                start_row,
                end_row,
            },
        })
    }

    async fn run(self) -> Option<Vec<(stoat_text::Anchor, String, InlayKind)>> {
        let server = match self.host.launch(&self.language, &self.workspace_root).await {
            Ok(s) => s,
            Err(err) => {
                tracing::warn!(target: "stoat_gui::lsp::inlay_hints", ?err, "failed to launch LSP server");
                return None;
            },
        };
        let _ = server.initialize(Some(self.uri.clone())).await;
        if !server.supports_feature(LanguageServerFeature::InlayHints) {
            return Some(Vec::new());
        }
        let encoding = server.offset_encoding();
        let range = byte_range_to_lsp_range(&self.rope, self.range_bytes.clone(), encoding);
        let params = InlayHintParams {
            work_done_progress_params: Default::default(),
            text_document: TextDocumentIdentifier { uri: self.uri },
            range,
        };
        let hints = match server.range_inlay_hint(params).await {
            Ok(Some(hints)) => hints,
            Ok(None) => return Some(Vec::new()),
            Err(err) => {
                tracing::warn!(target: "stoat_gui::lsp::inlay_hints", ?err, "range_inlay_hint request failed");
                return None;
            },
        };

        let mut converted = Vec::with_capacity(hints.len());
        for hint in hints {
            let offset = lsp_pos_to_byte_offset(&self.rope, hint.position, encoding);
            let anchor = self.buffer_snapshot.anchor_at(offset, Bias::Right);
            let text = flatten_label(hint.label);
            converted.push((anchor, text, InlayKind::Hint));
        }
        Some(converted)
    }
}

fn visible_buffer_rows(
    editor: &Entity<Editor>,
    cx: &Context<'_, InlayHintsManager>,
    display_snapshot: &stoat::DisplaySnapshot,
) -> Option<(u32, u32)> {
    let editor_ref = editor.read(cx);
    let scroll_display_row = editor_ref.scroll_row();
    let viewport_rows = match (editor_ref.text_region_bounds(), editor_ref.cell_size()) {
        (Some(bounds), Some(cell)) => {
            let line_height = f32::from(cell.height);
            let viewport_height = f32::from(bounds.size.height);
            if line_height > 0.0 && viewport_height > 0.0 {
                (viewport_height / line_height).ceil() as u32
            } else {
                DEFAULT_VIEWPORT_ROWS
            }
        },
        _ => DEFAULT_VIEWPORT_ROWS,
    };
    let max_display_row = display_snapshot.max_point().row;
    let start_display = scroll_display_row.min(max_display_row);
    let end_display = scroll_display_row
        .saturating_add(viewport_rows)
        .min(max_display_row.saturating_add(1));

    let start_buffer = display_snapshot
        .display_to_buffer(DisplayPoint::new(start_display, 0), Bias::Left)?
        .row;
    let end_buffer = if end_display == 0 {
        0
    } else {
        let probe = end_display.saturating_sub(1);
        let buffer = display_snapshot
            .display_to_buffer(DisplayPoint::new(probe, 0), Bias::Left)?
            .row;
        buffer.saturating_add(1)
    };
    Some((start_buffer, end_buffer))
}

fn flatten_label(label: InlayHintLabel) -> String {
    match label {
        InlayHintLabel::String(s) => s,
        InlayHintLabel::LabelParts(parts) => {
            let mut out = String::new();
            for part in parts {
                out.push_str(&part.value);
            }
            out
        },
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
        buffer::Buffer, diff_map::DiffMap, display_map::DisplayMap, editor::EditorMode,
        globals::ExecutorGlobal, multi_buffer::MultiBuffer,
    };
    use gpui::{AppContext, TestAppContext};
    use lsp_types::{InlayHintKind, Position, ServerCapabilities};
    use std::path::PathBuf;
    use stoat::{
        buffer::BufferId,
        host::fake::{FakeLsp, FakeLspHost},
    };
    use stoat_scheduler::{Executor, TestScheduler};

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

    fn capability_supporting_inlay_hints() -> ServerCapabilities {
        ServerCapabilities {
            inlay_hint_provider: Some(lsp_types::OneOf::Left(true)),
            ..Default::default()
        }
    }

    fn make_hint(
        line: u32,
        character: u32,
        label: &str,
        kind: InlayHintKind,
    ) -> lsp_types::InlayHint {
        lsp_types::InlayHint {
            position: Position::new(line, character),
            label: InlayHintLabel::String(label.to_string()),
            kind: Some(kind),
            text_edits: None,
            tooltip: None,
            padding_left: None,
            padding_right: None,
            data: None,
        }
    }

    #[test]
    fn programmed_hint_is_applied_to_display_map() {
        let mut cx = TestAppContext::single();
        let lsp = install_globals(&mut cx);
        lsp.set_capabilities(capability_supporting_inlay_hints());
        let path = PathBuf::from("/tmp/main.rs");
        lsp.set_range_inlay_hints(
            path.to_str().unwrap(),
            vec![make_hint(0, 5, ": str", InlayHintKind::TYPE)],
        );

        let editor = build_editor(&mut cx, &path, "hello world");
        let display_map = editor.read_with(&cx, |ed, _| ed.display_map().clone());

        let manager = cx.update(|cx| {
            let editor_clone = editor.clone();
            cx.new(|cx| InlayHintsManager::new(editor_clone, cx))
        });

        editor.update(&mut cx, |ed, cx| ed.set_scroll_row(0, cx));
        editor.update(&mut cx, |ed, cx| {
            let buffer = ed.multi_buffer().read(cx).as_singleton().cloned().unwrap();
            buffer.update(cx, |b, cx| b.edit(0..0, "", cx));
        });
        cx.run_until_parked();

        let inlay_text = display_map.update(&mut cx, |dm, _| {
            dm.snapshot().inlay_snapshot().inlay_text().to_string()
        });
        assert_eq!(inlay_text, "hello: str world");
        assert_eq!(manager.read_with(&cx, |m, _| m.current_inlay_count()), 1);
    }

    #[test]
    fn clearing_programmed_hints_removes_inlays() {
        let mut cx = TestAppContext::single();
        let lsp = install_globals(&mut cx);
        lsp.set_capabilities(capability_supporting_inlay_hints());
        let path = PathBuf::from("/tmp/clear.rs");
        lsp.set_range_inlay_hints(
            path.to_str().unwrap(),
            vec![make_hint(0, 5, ": str", InlayHintKind::TYPE)],
        );

        let editor = build_editor(&mut cx, &path, "hello world");
        let display_map = editor.read_with(&cx, |ed, _| ed.display_map().clone());
        let manager = cx.update(|cx| {
            let editor_clone = editor.clone();
            cx.new(|cx| InlayHintsManager::new(editor_clone, cx))
        });
        editor.update(&mut cx, |ed, cx| {
            let buffer = ed.multi_buffer().read(cx).as_singleton().cloned().unwrap();
            buffer.update(cx, |b, cx| b.edit(0..0, "", cx));
        });
        cx.run_until_parked();
        assert_eq!(
            display_map.update(&mut cx, |dm, _| dm
                .snapshot()
                .inlay_snapshot()
                .inlay_text()
                .to_string()),
            "hello: str world"
        );

        lsp.set_range_inlay_hints(path.to_str().unwrap(), Vec::new());
        editor.update(&mut cx, |ed, cx| {
            let buffer = ed.multi_buffer().read(cx).as_singleton().cloned().unwrap();
            buffer.update(cx, |b, cx| b.edit(11..11, "!", cx));
        });
        cx.run_until_parked();

        let inlay_text = display_map.update(&mut cx, |dm, _| {
            dm.snapshot().inlay_snapshot().inlay_text().to_string()
        });
        assert_eq!(inlay_text, "hello world!");
        assert_eq!(manager.read_with(&cx, |m, _| m.current_inlay_count()), 0);
    }

    #[test]
    fn skips_request_when_server_does_not_advertise_inlay_hints() {
        let mut cx = TestAppContext::single();
        let lsp = install_globals(&mut cx);
        // No inlay_hint_provider in capabilities.
        lsp.set_capabilities(ServerCapabilities::default());
        let path = PathBuf::from("/tmp/no_caps.rs");
        lsp.set_range_inlay_hints(
            path.to_str().unwrap(),
            vec![make_hint(0, 5, ": str", InlayHintKind::TYPE)],
        );

        let editor = build_editor(&mut cx, &path, "hello world");
        let display_map = editor.read_with(&cx, |ed, _| ed.display_map().clone());
        let _manager = cx.update(|cx| {
            let editor_clone = editor.clone();
            cx.new(|cx| InlayHintsManager::new(editor_clone, cx))
        });
        editor.update(&mut cx, |ed, cx| {
            let buffer = ed.multi_buffer().read(cx).as_singleton().cloned().unwrap();
            buffer.update(cx, |b, cx| b.edit(0..0, "", cx));
        });
        cx.run_until_parked();

        let inlay_text = display_map.update(&mut cx, |dm, _| {
            dm.snapshot().inlay_snapshot().inlay_text().to_string()
        });
        assert_eq!(inlay_text, "hello world");
    }
}
