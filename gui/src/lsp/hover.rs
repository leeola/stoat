use crate::{
    editor::{Editor, EditorEvent},
    globals::{LanguageRegistry, LspHostGlobal},
    theme::ActiveTheme,
};
use gpui::{
    deferred, div, point, Bounds, Context, Entity, IntoElement, ParentElement, Pixels, Point,
    Render, SharedString, Size, Styled, Subscription, Task, WeakEntity, Window,
};
use lsp_types::{
    HoverContents, HoverParams, MarkedString, TextDocumentIdentifier, TextDocumentPositionParams,
    Uri,
};
use std::{path::Path, str::FromStr, sync::Arc};
use stoat::{
    host::{LanguageServerFeature, LspHost},
    lsp::util::byte_offset_to_lsp_pos,
    DisplayPoint,
};
use stoat_text::Bias;

/// Floating panel that shows the language server's `textDocument/hover`
/// response under the cursor. Owned per-editor; observes the host
/// editor's [`EditorEvent::Changed`] stream and re-queries whenever
/// the editor's stored hover grid position transitions to a new cell.
///
/// Behaviour:
/// - hover position `None` -> popup is empty (no paint, no request)
/// - hover position `Some(p)` with `p == anchored_at` -> no-op (the currently displayed lines
///   already correspond to `p`)
/// - hover position `Some(p)` with `p != anchored_at` -> drop any in-flight request (its `Task` is
///   replaced and the future is cancelled), clear `lines`, and spawn a new hover request whose
///   completion writes the new content into `lines`.
///
/// Each request launches a fresh language server through the global
/// [`LspHostGlobal`] factory. This is wasteful in production -- the
/// per-language LSP server should be cached -- but is the most
/// honest shape available until the higher-level LSP cache lands.
///
/// FIXME: route hover through a per-language `LspServer` cache
/// instead of launching a new child process per request.
pub struct HoverPopup {
    editor: WeakEntity<Editor>,
    anchored_at: Option<(u32, u32)>,
    lines: Vec<SharedString>,
    pending_task: Option<Task<()>>,
    /// Monotonic id for the most recent hover RPC spawn. The
    /// spawn captures the id before launching; the response
    /// branch re-checks it so a late reply for an earlier cursor
    /// position cannot paint over the popup's current content.
    request_seq: u64,
    _subscription: Subscription,
}

impl HoverPopup {
    pub fn new(editor: Entity<Editor>, cx: &mut Context<'_, Self>) -> Self {
        let weak = editor.downgrade();
        let subscription = cx.subscribe(&editor, |this, _editor, _event: &EditorEvent, cx| {
            this.reconcile(cx);
        });
        Self {
            editor: weak,
            anchored_at: None,
            lines: Vec::new(),
            pending_task: None,
            request_seq: 0,
            _subscription: subscription,
        }
    }

    pub fn lines(&self) -> &[SharedString] {
        &self.lines
    }

    pub fn anchored_at(&self) -> Option<(u32, u32)> {
        self.anchored_at
    }

    pub(crate) fn bump_request_id(&mut self) -> u64 {
        self.request_seq += 1;
        self.request_seq
    }

    pub(crate) fn request_id(&self) -> u64 {
        self.request_seq
    }

    /// Read the host editor's `hover_position` and reconcile popup
    /// state against it: drop stale content, kick off a new LSP
    /// request, or sit tight when the position has not moved.
    fn reconcile(&mut self, cx: &mut Context<'_, Self>) {
        let Some(editor) = self.editor.upgrade() else {
            self.clear();
            return;
        };
        let position = editor.read(cx).hover_position();
        if position == self.anchored_at {
            return;
        }
        self.pending_task = None;
        self.lines.clear();
        self.anchored_at = position;
        cx.notify();

        let Some((row, col)) = position else {
            return;
        };
        let Some(request) = HoverRequest::build(&editor, row, col, cx) else {
            return;
        };
        let request_id = self.bump_request_id();
        let task = cx.spawn(async move |this, cx| {
            let outcome = request.run().await;
            let _ = this.update(cx, |popup, cx| {
                if popup.request_id() != request_id {
                    // A newer hover request superseded this one; the
                    // stale reply would paint the popup with the
                    // previous cursor's content.
                    return;
                }
                if let Some(lines) = outcome {
                    popup.lines = lines.into_iter().map(SharedString::from).collect();
                } else {
                    popup.lines.clear();
                }
                cx.notify();
            });
        });
        self.pending_task = Some(task);
    }

    fn clear(&mut self) {
        self.pending_task = None;
        self.lines.clear();
        self.anchored_at = None;
    }
}

impl Render for HoverPopup {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let Some(editor) = self.editor.upgrade() else {
            return empty().into_any_element();
        };
        if self.lines.is_empty() {
            return empty().into_any_element();
        }
        let Some((row, col)) = self.anchored_at else {
            return empty().into_any_element();
        };
        let editor_ref = editor.read(cx);
        let Some(bounds) = editor_ref.text_region_bounds() else {
            return empty().into_any_element();
        };
        let Some(cell) = editor_ref.cell_size() else {
            return empty().into_any_element();
        };
        let origin = popup_origin(bounds, cell, row, col);
        let lines = self.lines.clone();
        let theme = cx.theme();
        deferred(
            div()
                .absolute()
                .left(origin.x)
                .top(origin.y)
                .px_2()
                .py_1()
                .bg(theme.popup_background)
                .text_color(theme.popup_text)
                .border_1()
                .border_color(theme.popup_border)
                .child(
                    div().flex().flex_col().children(
                        lines
                            .into_iter()
                            .map(|line| div().child(line).into_any_element()),
                    ),
                ),
        )
        .with_priority(2)
        .into_any_element()
    }
}

fn empty() -> impl IntoElement {
    div()
}

fn popup_origin(bounds: Bounds<Pixels>, cell: Size<Pixels>, row: u32, col: u32) -> Point<Pixels> {
    let x = bounds.origin.x + cell.width * col as f32;
    let y = bounds.origin.y + cell.height * (row + 1) as f32;
    point(x, y)
}

struct HoverRequest {
    host: Arc<dyn LspHost>,
    language: Arc<stoat_language::Language>,
    workspace_root: std::path::PathBuf,
    uri: Uri,
    offset: usize,
    rope: stoat_text::Rope,
}

impl HoverRequest {
    fn build(
        editor: &Entity<Editor>,
        row: u32,
        col: u32,
        cx: &mut Context<'_, HoverPopup>,
    ) -> Option<Self> {
        let host = cx.global::<LspHostGlobal>().0.clone();
        let path = editor.read(cx).file_path()?.to_path_buf();
        let language = cx.global::<LanguageRegistry>().0.for_path(&path)?;
        let uri = path_to_uri(&path)?;
        let display_map = editor.read(cx).display_map().clone();
        let display_snapshot = display_map.update(cx, |dm, _| dm.snapshot());
        let mb_snapshot = editor.read(cx).multi_buffer().read(cx).snapshot();
        let clipped = display_snapshot.clip_point(DisplayPoint::new(row, col), Bias::Left);
        let buffer_point = display_snapshot.display_to_buffer(clipped)?;
        let rope = mb_snapshot.rope().clone();
        let offset = rope.point_to_offset(buffer_point);
        let workspace_root = path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| path.clone());
        Some(Self {
            host,
            language,
            workspace_root,
            uri,
            offset,
            rope,
        })
    }

    async fn run(self) -> Option<Vec<String>> {
        let server = match self.host.launch(&self.language, &self.workspace_root).await {
            Ok(s) => s,
            Err(err) => {
                tracing::warn!(target: "stoat_gui::lsp::hover", ?err, "failed to launch LSP server");
                return None;
            },
        };
        // `initialize` is best-effort; test fakes accept hover requests
        // without an explicit handshake.
        let _ = server.initialize(Some(self.uri.clone())).await;
        if !server.supports_feature(LanguageServerFeature::Hover) {
            return None;
        }
        let encoding = server.offset_encoding();
        let position = byte_offset_to_lsp_pos(&self.rope, self.offset, encoding);
        let params = HoverParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: self.uri },
                position,
            },
            work_done_progress_params: Default::default(),
        };
        match server.hover(params).await {
            Ok(Some(hover)) => Some(flatten_hover_contents(hover.contents)),
            Ok(None) => None,
            Err(err) => {
                tracing::warn!(target: "stoat_gui::lsp::hover", ?err, "hover request failed");
                None
            },
        }
    }
}

fn path_to_uri(path: &Path) -> Option<Uri> {
    let path_str = path.to_str()?;
    Uri::from_str(&format!("file://{path_str}")).ok()
}

fn flatten_hover_contents(contents: HoverContents) -> Vec<String> {
    fn marked_to_string(m: MarkedString) -> String {
        match m {
            MarkedString::String(s) => s,
            MarkedString::LanguageString(ls) => ls.value,
        }
    }
    let raw = match contents {
        HoverContents::Scalar(m) => marked_to_string(m),
        HoverContents::Array(items) => items
            .into_iter()
            .map(marked_to_string)
            .collect::<Vec<_>>()
            .join("\n"),
        HoverContents::Markup(markup) => markup.value,
    };
    raw.lines().map(str::to_string).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        buffer::Buffer, diff_map::DiffMap, display_map::DisplayMap, editor::EditorMode,
        globals::ExecutorGlobal, multi_buffer::MultiBuffer,
    };
    use gpui::{AppContext, TestAppContext};
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

    #[test]
    fn popup_populates_lines_when_hover_position_matches_program() {
        let mut cx = TestAppContext::single();
        let lsp = install_globals(&mut cx);
        let path = PathBuf::from("/tmp/main.rs");
        lsp.set_capabilities(lsp_types::ServerCapabilities {
            hover_provider: Some(lsp_types::HoverProviderCapability::Simple(true)),
            ..Default::default()
        });
        lsp.set_hover(path.to_str().unwrap(), 0, 0, "**hello**\nworld");

        let editor = build_editor(&mut cx, &path, "let x = 1;\n");
        let popup = cx.update(|cx| {
            let editor_clone = editor.clone();
            cx.new(|cx| HoverPopup::new(editor_clone, cx))
        });
        editor.update(&mut cx, |ed, cx| ed.set_hover_position(Some((0, 0)), cx));
        cx.run_until_parked();

        let (lines, anchored) = popup.read_with(&cx, |p, _| (p.lines().to_vec(), p.anchored_at()));
        assert_eq!(
            lines,
            vec![SharedString::from("**hello**"), SharedString::from("world")]
        );
        assert_eq!(anchored, Some((0, 0)));
    }

    #[test]
    fn bump_request_id_increments_and_records_latest() {
        let mut cx = TestAppContext::single();
        let _lsp = install_globals(&mut cx);
        let path = PathBuf::from("/tmp/main.rs");
        let editor = build_editor(&mut cx, &path, "x\n");
        let popup = cx.update(|cx| cx.new(|cx| HoverPopup::new(editor.clone(), cx)));

        let first = popup.update(&mut cx, |p, _| p.bump_request_id());
        let second = popup.update(&mut cx, |p, _| p.bump_request_id());

        assert_eq!(first, 1);
        assert_eq!(second, 2);
        assert_eq!(
            popup.read_with(&cx, |p, _| p.request_id()),
            2,
            "request_id must track the most recent bump",
        );
    }

    #[test]
    fn popup_clears_when_hover_position_becomes_none() {
        let mut cx = TestAppContext::single();
        let lsp = install_globals(&mut cx);
        let path = PathBuf::from("/tmp/main.rs");
        lsp.set_capabilities(lsp_types::ServerCapabilities {
            hover_provider: Some(lsp_types::HoverProviderCapability::Simple(true)),
            ..Default::default()
        });
        lsp.set_hover(path.to_str().unwrap(), 0, 0, "hi");

        let editor = build_editor(&mut cx, &path, "x\n");
        let popup = cx.update(|cx| {
            let editor_clone = editor.clone();
            cx.new(|cx| HoverPopup::new(editor_clone, cx))
        });
        editor.update(&mut cx, |ed, cx| ed.set_hover_position(Some((0, 0)), cx));
        cx.run_until_parked();
        let primed = popup.read_with(&cx, |p, _| p.lines().to_vec());
        assert!(
            !primed.is_empty(),
            "popup should populate before clear test"
        );

        editor.update(&mut cx, |ed, cx| ed.set_hover_position(None, cx));
        cx.run_until_parked();
        let (lines, anchored) = popup.read_with(&cx, |p, _| (p.lines().to_vec(), p.anchored_at()));
        assert!(
            lines.is_empty(),
            "popup should clear on hover_position -> None"
        );
        assert!(anchored.is_none());
    }
}
