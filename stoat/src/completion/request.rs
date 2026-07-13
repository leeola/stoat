//! Per-keystroke completion trigger pipeline.
//!
//! [`trigger`] runs after every event in [`crate::app::Stoat::update`].
//! In insert mode with a focused editor pane it computes the cursor
//! context, decides which sources apply, and spawns a debounced
//! task on the stored [`stoat_scheduler::Executor`]. Replacing the
//! prior in-flight task drops it -- async-task cancels the dropped
//! future before its debounce timer or downstream LSP request can
//! land, which is what keeps stale responses from painting over
//! fresh ones.
//!
//! [`pump`] polls the in-flight task each render tick and writes
//! resolved [`CompletionPopup`] results onto
//! [`crate::app::Stoat::pending_completion`]. The hover pipeline
//! at `stoat/src/action_handlers/lsp.rs::pump_lsp_hover` is the
//! local reference shape; the broader design follows
//! `references/helix/helix-term/src/handlers/completion/request.rs`.
//!
//! Suppression: pressing `Esc` while a popup is open clears the
//! popup, cancels the in-flight task, and stamps
//! [`crate::app::Stoat::last_completion_signature`] to the current
//! buffer version so the very-next [`trigger`] returns early instead
//! of immediately re-arming the request from the unchanged buffer.
//! Any actual edit bumps the buffer version, the signature mismatch
//! re-fires the trigger, and the popup comes back on the next
//! response.

use crate::{
    app::Stoat,
    buffer::BufferId,
    completion::{
        applicable_sources, CompletionContext, CompletionItem, CompletionPopup, CompletionSource,
    },
    host::{FsHost, LanguageServerFeature, LspHost, OffsetEncoding},
    keymap_state,
    lsp::util,
    pane::{FocusTarget, View},
};
use lsp_types::{
    CompletionContext as LspCompletionContext, CompletionParams, CompletionTriggerKind,
    PartialResultParams, TextDocumentIdentifier, TextDocumentPositionParams,
    WorkDoneProgressParams,
};
use std::{
    future::Future,
    ops::Range,
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};
use stoat_text::{Point, Rope};

/// Quiet window from the most recent keystroke before a completion
/// request is dispatched. Each new keystroke replaces the in-flight
/// task and restarts the timer.
pub(crate) const COMPLETION_DEBOUNCE: Duration = Duration::from_millis(150);

/// Owned snapshot of [`CompletionContext`] for a spawned task. The
/// public context borrows from the rope and prefix; this struct
/// holds owned strings so it can outlive the trigger frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ContextOwned {
    pub cursor_offset: usize,
    pub prefix: String,
    pub prefix_range: Range<usize>,
    pub text_before_cursor: String,
}

impl ContextOwned {
    fn as_borrowed(&self) -> CompletionContext<'_> {
        CompletionContext {
            cursor_offset: self.cursor_offset,
            prefix: &self.prefix,
            prefix_range: self.prefix_range.clone(),
            text_before_cursor: &self.text_before_cursor,
        }
    }
}

/// Compute the cursor's completion context from a rope plus byte
/// offset. Walks back from the cursor through identifier-or-path
/// characters (alphanumeric, `_`, `/`, `.`, `-`, `~`) to determine
/// the prefix and its byte range, and slices the line content from
/// the line start to the cursor for [`CompletionContext::text_before_cursor`].
///
/// Multi-byte safe: walk-back uses [`str::char_indices`] so prefix
/// boundaries always land on UTF-8 codepoint edges.
pub(crate) fn compute_context(rope: &Rope, cursor_offset: usize) -> ContextOwned {
    let cursor_offset = cursor_offset.min(rope.len());
    let row = rope.offset_to_point(cursor_offset).row;
    let line_start = rope.point_to_offset(Point::new(row, 0));
    let text_before_cursor = rope.slice(line_start..cursor_offset).to_string();

    let mut start = text_before_cursor.len();
    for (idx, ch) in text_before_cursor.char_indices().rev() {
        if is_word_or_path_char(ch) {
            start = idx;
        } else {
            break;
        }
    }
    let prefix = text_before_cursor[start..].to_string();
    let prefix_byte_len = prefix.len();
    let prefix_range = (cursor_offset - prefix_byte_len)..cursor_offset;

    ContextOwned {
        cursor_offset,
        prefix,
        prefix_range,
        text_before_cursor,
    }
}

fn is_word_or_path_char(ch: char) -> bool {
    ch.is_alphanumeric() || matches!(ch, '_' | '/' | '.' | '-' | '~')
}

/// Stamp [`Stoat::last_completion_signature`] with the focused
/// editor's current buffer signature. Used by the Esc-dismiss path
/// in `handle_insert_key` so the very-next [`trigger`] sees an
/// unchanged signature and returns early instead of re-arming the
/// request that was just dismissed.
pub(crate) fn record_dismiss(stoat: &mut Stoat) {
    let Some((buffer_id, version)) = focused_buffer_signature(stoat) else {
        return;
    };
    stoat.last_completion_signature = Some((buffer_id, version));
}

/// Per-event entry point. In insert mode with a focused
/// [`View::Editor`] pane and no modal open, computes the cursor
/// context and arms a debounced completion request. Outside that gate,
/// clears any in-flight request, the popup, and the suppression
/// signature so re-entering insert mode starts from a clean slate.
///
/// A modal input (finder, palette, isearch, ...) owns the keystream
/// while open, so completion never triggers beneath one: opening a
/// modal flows through the clearing branch and dismisses any live
/// popup.
pub(crate) fn trigger(stoat: &mut Stoat) {
    if !insert_mode_in_editor_pane(stoat) {
        stoat.pending_completion_request = None;
        stoat.pending_completion = None;
        stoat.last_completion_signature = None;
        stoat.active_snippet = None;
        return;
    }

    let snapshot = match focused_editor_snapshot(stoat) {
        Some(s) => s,
        None => return,
    };

    if let Some(popup) = &stoat.pending_completion
        && (snapshot.cursor_offset < popup.prefix_range.start
            || snapshot.cursor_offset > popup.prefix_range.end)
    {
        stoat.pending_completion = None;
    }

    let signature = (snapshot.buffer_id, snapshot.buffer_version);
    if stoat.last_completion_signature == Some(signature) {
        return;
    }
    stoat.last_completion_signature = Some(signature);

    let owned = compute_context(&snapshot.rope, snapshot.cursor_offset);

    let trigger_char = owned.text_before_cursor.chars().last();
    let is_trigger_char = match (
        trigger_char,
        server_trigger_characters(
            &stoat.lsp_for_feature(snapshot.buffer_id, LanguageServerFeature::Completion),
        ),
    ) {
        (Some(ch), Some(triggers)) => triggers.contains(&ch.to_string()),
        _ => false,
    };

    let mut sources = applicable_sources(&owned.as_borrowed());
    if is_trigger_char && !sources.contains(&CompletionSource::Lsp) {
        let at = sources
            .iter()
            .position(|s| matches!(s, CompletionSource::Word))
            .unwrap_or(sources.len());
        sources.insert(at, CompletionSource::Lsp);
    }
    if sources.is_empty() {
        stoat.pending_completion_request = None;
        stoat.pending_completion = None;
        return;
    }

    let completion_hosts =
        stoat.feature_hosts(snapshot.buffer_id, LanguageServerFeature::Completion);
    let fs_host = stoat.fs_host.clone();
    let executor = stoat.executor.clone();
    let home_dir = stoat.env_host.var("HOME").map(PathBuf::from);
    let encoding = completion_hosts
        .first()
        .map(|(_, host)| host.offset_encoding())
        .unwrap_or(OffsetEncoding::Utf16);

    let base_dir = base_dir_for(snapshot.source_path.as_deref(), &snapshot.git_root);

    let completion_context = if is_trigger_char {
        LspCompletionContext {
            trigger_kind: CompletionTriggerKind::TRIGGER_CHARACTER,
            trigger_character: trigger_char.map(|ch| ch.to_string()),
        }
    } else {
        LspCompletionContext {
            trigger_kind: CompletionTriggerKind::INVOKED,
            trigger_character: None,
        }
    };

    let lsp_params = if sources.contains(&CompletionSource::Lsp) && !completion_hosts.is_empty() {
        build_lsp_params(
            snapshot.source_path.as_deref(),
            &snapshot.rope,
            snapshot.cursor_offset,
            encoding,
            Some(completion_context),
        )
    } else {
        None
    };

    let task = stoat.spawn_woken(run_request(
        executor,
        owned,
        sources,
        completion_hosts,
        fs_host,
        snapshot.rope,
        encoding,
        base_dir,
        home_dir,
        lsp_params,
        is_trigger_char,
    ));
    stoat.pending_completion_request = Some(task);
}

/// Poll the in-flight completion task. On `Ready` writes the
/// returned [`CompletionPopup`] onto [`Stoat::pending_completion`]
/// (or clears it when the result has no items). Returns `true` when
/// the popup state changed, mirroring the convention used by the
/// other LSP pumps so the render loop can drive both for free.
pub(crate) fn pump(stoat: &mut Stoat) -> bool {
    let Some(mut task) = stoat.pending_completion_request.take() else {
        return false;
    };
    let waker = futures::task::noop_waker();
    let mut cx = Context::from_waker(&waker);
    match Pin::new(&mut task).poll(&mut cx) {
        Poll::Ready(popup) => {
            if popup.items.is_empty() {
                stoat.pending_completion = None;
            } else {
                stoat.pending_completion = Some(popup);
            }
            crate::action_handlers::completion::arm_completion_resolve(stoat);
            true
        },
        Poll::Pending => {
            stoat.pending_completion_request = Some(task);
            false
        },
    }
}

struct EditorSnapshot {
    rope: Rope,
    cursor_offset: usize,
    buffer_id: BufferId,
    buffer_version: u64,
    source_path: Option<PathBuf>,
    git_root: PathBuf,
}

fn insert_mode_in_editor_pane(stoat: &Stoat) -> bool {
    if stoat.focused_mode() != "insert" {
        return false;
    }
    // A modal input (finder, palette, isearch, rename, ...) is an off-pane
    // InputView that leaves `ws.focus` on the editor, so the mode and focus
    // checks below still pass while one is open. It owns the keystream, so
    // completion must not arm from the editor's cursor beneath it.
    if keymap_state::modal_predicate(stoat).is_some() {
        return false;
    }
    let ws = stoat.active_workspace();
    let FocusTarget::SplitPane(pane_id) = ws.focus else {
        return false;
    };
    matches!(ws.panes.pane(pane_id).view, View::Editor(_))
}

fn focused_editor_snapshot(stoat: &Stoat) -> Option<EditorSnapshot> {
    let ws = stoat.active_workspace();
    let FocusTarget::SplitPane(pane_id) = ws.focus else {
        return None;
    };
    let View::Editor(editor_id) = ws.panes.pane(pane_id).view else {
        return None;
    };
    let editor = ws.editors.get(editor_id)?;
    let sel = editor.selections.newest_anchor();
    let buffer_id = editor.buffer_id;
    let buffer = ws.buffers.get(buffer_id)?;
    let guard = buffer.read().expect("buffer lock");
    let tail_off = guard.resolve_anchor(&sel.tail());
    let head_off = guard.resolve_anchor(&sel.head());
    let cursor_offset = stoat_text::cursor_offset(guard.rope(), tail_off, head_off);
    let rope = guard.rope().clone();
    let buffer_version = guard.version();
    drop(guard);
    let source_path = ws.buffers.path_for(buffer_id).map(Path::to_path_buf);
    Some(EditorSnapshot {
        rope,
        cursor_offset,
        buffer_id,
        buffer_version,
        source_path,
        git_root: ws.git_root.clone(),
    })
}

fn focused_buffer_signature(stoat: &Stoat) -> Option<(BufferId, u64)> {
    let snapshot = focused_editor_snapshot(stoat)?;
    Some((snapshot.buffer_id, snapshot.buffer_version))
}

fn base_dir_for(source_path: Option<&Path>, git_root: &Path) -> PathBuf {
    source_path
        .and_then(|p| p.parent())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| git_root.to_path_buf())
}

/// The server's completion trigger characters, if it advertises any.
/// Each is a single character (e.g. `.`, `:`). Typing one fires
/// completion immediately with
/// [`CompletionTriggerKind::TRIGGER_CHARACTER`] instead of waiting out
/// the prefix debounce.
fn server_trigger_characters(lsp_host: &Arc<dyn LspHost>) -> Option<Vec<String>> {
    lsp_host
        .capabilities()
        .completion_provider
        .as_ref()?
        .trigger_characters
        .clone()
}

fn build_lsp_params(
    source_path: Option<&Path>,
    rope: &Rope,
    cursor_offset: usize,
    encoding: OffsetEncoding,
    context: Option<LspCompletionContext>,
) -> Option<CompletionParams> {
    let path = source_path?;
    let uri = crate::action_handlers::lsp::path_to_uri(path)?;
    let position = util::byte_offset_to_lsp_pos(rope, cursor_offset, encoding);
    Some(CompletionParams {
        text_document_position: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri },
            position,
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: PartialResultParams::default(),
        context,
    })
}

#[allow(clippy::too_many_arguments)]
async fn run_request(
    executor: stoat_scheduler::Executor,
    owned: ContextOwned,
    sources: Vec<CompletionSource>,
    completion_hosts: Vec<(String, Arc<dyn LspHost>)>,
    fs_host: Arc<dyn FsHost>,
    rope: Rope,
    encoding: OffsetEncoding,
    base_dir: PathBuf,
    home_dir: Option<PathBuf>,
    lsp_params: Option<CompletionParams>,
    immediate: bool,
) -> CompletionPopup {
    if !immediate {
        executor.timer(COMPLETION_DEBOUNCE).await;
    }

    let ctx = owned.as_borrowed();
    let mut items: Vec<CompletionItem> = Vec::new();
    for source in &sources {
        match source {
            CompletionSource::Path => {
                items.extend(crate::completion::path::fetch(
                    &ctx,
                    fs_host.as_ref(),
                    &base_dir,
                    home_dir.as_deref(),
                ));
            },
            CompletionSource::Lsp => {
                if let Some(params) = &lsp_params {
                    for (name, host) in &completion_hosts {
                        items.extend(
                            crate::completion::lsp::fetch(
                                &ctx,
                                name,
                                host.as_ref(),
                                params.clone(),
                                &rope,
                                encoding,
                            )
                            .await,
                        );
                    }
                }
            },
            CompletionSource::Word => {
                items.extend(crate::completion::word::fetch(&ctx, &rope));
            },
        }
    }

    CompletionPopup {
        items,
        selected_idx: 0,
        anchor_offset: owned.prefix_range.start,
        prefix_range: owned.prefix_range,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rope(s: &str) -> Rope {
        Rope::from(s)
    }

    #[test]
    fn empty_buffer_yields_empty_prefix() {
        let r = rope("");
        let ctx = compute_context(&r, 0);
        assert_eq!(ctx.prefix, "");
        assert_eq!(ctx.prefix_range, 0..0);
        assert_eq!(ctx.text_before_cursor, "");
    }

    #[test]
    fn identifier_prefix_at_end_of_line() {
        let r = rope("let foo");
        let ctx = compute_context(&r, 7);
        assert_eq!(ctx.prefix, "foo");
        assert_eq!(ctx.prefix_range, 4..7);
        assert_eq!(ctx.text_before_cursor, "let foo");
    }

    #[test]
    fn path_shaped_prefix_walks_back_through_slashes_and_dots() {
        let r = rope("let p = ./src/lib");
        let ctx = compute_context(&r, r.len());
        assert_eq!(ctx.prefix, "./src/lib");
        assert_eq!(ctx.prefix_range, 8..17);
    }

    #[test]
    fn dot_slash_prefix_is_path_shaped() {
        let r = rope("./");
        let ctx = compute_context(&r, 2);
        assert_eq!(ctx.prefix, "./");
        assert_eq!(ctx.prefix_range, 0..2);
    }

    #[test]
    fn cursor_at_buffer_start_yields_empty_prefix() {
        let r = rope("foo");
        let ctx = compute_context(&r, 0);
        assert_eq!(ctx.prefix, "");
        assert_eq!(ctx.prefix_range, 0..0);
    }

    #[test]
    fn cursor_after_whitespace_yields_empty_prefix() {
        let r = rope("foo ");
        let ctx = compute_context(&r, 4);
        assert_eq!(ctx.prefix, "");
        assert_eq!(ctx.prefix_range, 4..4);
    }

    #[test]
    fn second_line_uses_line_relative_text_before_cursor() {
        let r = rope("first line\nsecond foo");
        let cursor = r.len();
        let ctx = compute_context(&r, cursor);
        assert_eq!(ctx.prefix, "foo");
        assert_eq!(ctx.text_before_cursor, "second foo");
        let prefix_byte_len = "foo".len();
        assert_eq!(ctx.prefix_range, (cursor - prefix_byte_len)..cursor);
    }

    #[test]
    fn multibyte_chars_keep_prefix_on_codepoint_boundaries() {
        let r = rope("résumé");
        let ctx = compute_context(&r, r.len());
        assert_eq!(ctx.prefix, "résumé");
        assert_eq!(ctx.prefix_range, 0..r.len());
    }

    #[test]
    fn cursor_past_end_clamps_to_buffer_length() {
        let r = rope("foo");
        let ctx = compute_context(&r, 99);
        assert_eq!(ctx.cursor_offset, 3);
        assert_eq!(ctx.prefix, "foo");
    }

    #[test]
    fn applicable_sources_picks_path_for_path_shaped_prefix() {
        let r = rope("./");
        let ctx_owned = compute_context(&r, 2);
        let sources = applicable_sources(&ctx_owned.as_borrowed());
        assert_eq!(sources, vec![CompletionSource::Path]);
    }

    #[test]
    fn applicable_sources_picks_lsp_word_for_identifier_prefix() {
        let r = rope("foo");
        let ctx_owned = compute_context(&r, 3);
        let sources = applicable_sources(&ctx_owned.as_borrowed());
        assert_eq!(sources, vec![CompletionSource::Lsp, CompletionSource::Word]);
    }
}

#[cfg(test)]
mod harness_tests {
    use super::*;
    use crate::{
        action_handlers::dispatch, completion::CompletionSource, test_harness::TestHarness,
    };
    use lsp_types::{CompletionOptions, ServerCapabilities};
    use std::path::PathBuf;
    use stoat_action::OpenFile;

    fn enable_completion(h: &TestHarness) {
        h.fake_lsp().set_capabilities(ServerCapabilities {
            completion_provider: Some(CompletionOptions::default()),
            ..ServerCapabilities::default()
        });
    }

    fn enable_completion_with_triggers(h: &TestHarness, triggers: &[&str]) {
        h.fake_lsp().set_capabilities(ServerCapabilities {
            completion_provider: Some(CompletionOptions {
                trigger_characters: Some(triggers.iter().map(|t| t.to_string()).collect()),
                ..CompletionOptions::default()
            }),
            ..ServerCapabilities::default()
        });
    }

    fn open_scratch(h: &mut TestHarness, contents: &str) -> PathBuf {
        let path = PathBuf::from("/ws/buf.rs");
        h.fake_fs()
            .insert_files(std::iter::once((path.clone(), contents.as_bytes())));
        h.stoat.active_workspace_mut().git_root = PathBuf::from("/ws");
        dispatch(&mut h.stoat, &OpenFile { path: path.clone() });
        h.settle();
        path
    }

    fn labels(items: &[CompletionItem]) -> Vec<String> {
        items.iter().map(|i| i.label.clone()).collect()
    }

    #[test]
    fn trigger_character_fires_immediately_with_context() {
        let mut h = TestHarness::default();
        enable_completion_with_triggers(&h, &["."]);
        open_scratch(&mut h, "");

        h.type_keys("i");
        h.type_text(".");
        // A trigger character skips the prefix debounce, so no clock advance.
        h.settle();

        let observed = h.fake_lsp().observed_completions();
        assert_eq!(
            observed.len(),
            1,
            "trigger char issues an immediate request"
        );
        assert_eq!(
            observed[0].context,
            Some(LspCompletionContext {
                trigger_kind: CompletionTriggerKind::TRIGGER_CHARACTER,
                trigger_character: Some(".".to_string()),
            }),
        );
    }

    #[test]
    fn plain_letter_keeps_the_debounce_and_sends_invoked() {
        let mut h = TestHarness::default();
        enable_completion_with_triggers(&h, &["."]);
        open_scratch(&mut h, "");

        h.type_keys("i");
        h.type_text("f");
        h.settle();
        assert!(
            h.fake_lsp().observed_completions().is_empty(),
            "a plain letter waits out the debounce",
        );

        h.advance_clock(COMPLETION_DEBOUNCE);
        let observed = h.fake_lsp().observed_completions();
        assert_eq!(
            observed.len(),
            1,
            "the request fires after the quiet window"
        );
        assert_eq!(
            observed[0].context,
            Some(LspCompletionContext {
                trigger_kind: CompletionTriggerKind::INVOKED,
                trigger_character: None,
            }),
        );
    }

    #[test]
    fn a_modal_open_over_a_mid_word_cursor_does_not_trigger_completion() {
        let mut h = TestHarness::default();
        enable_completion(&h);
        open_scratch(&mut h, "Greeter");

        // Sit the editor cursor inside `Greeter`, then open the finder over it.
        // The finder is an insert-mode input that leaves focus on the editor, so
        // without the modal gate `trigger` would arm a completion for the word.
        h.type_keys("ll");
        h.type_keys("space p");
        h.advance_clock(COMPLETION_DEBOUNCE);
        h.settle();

        assert!(
            h.fake_lsp().observed_completions().is_empty(),
            "a modal open must not arm a completion from the editor cursor"
        );
        assert!(
            h.stoat.pending_completion.is_none(),
            "no completion popup shows beneath a modal"
        );
    }

    #[test]
    fn identifier_in_insert_mode_opens_popup_after_debounce() {
        let mut h = TestHarness::default();
        enable_completion(&h);
        open_scratch(&mut h, "");
        h.fake_lsp()
            .set_completions("/ws/buf.rs", 0, 3, &["foobar", "foobaz"]);

        h.type_keys("i");
        h.type_text("foo");
        assert!(
            h.stoat.pending_completion.is_none(),
            "popup arrives only after debounce"
        );
        h.advance_clock(COMPLETION_DEBOUNCE);

        let popup = h.stoat.pending_completion.clone().expect("popup armed");
        let got = labels(&popup.items);
        assert!(
            got.iter().any(|l| l == "foobar"),
            "expected foobar in {got:?}",
        );
        assert!(
            got.iter().any(|l| l == "foobaz"),
            "expected foobaz in {got:?}",
        );
    }

    #[test]
    fn path_prefix_in_insert_opens_path_popup() {
        let mut h = TestHarness::default();
        enable_completion(&h);
        h.fake_fs().insert_file("/ws/lib.rs", b"");
        h.fake_fs().insert_file("/ws/main.rs", b"");
        let path = PathBuf::from("/ws/buf.rs");
        h.fake_fs()
            .insert_files(std::iter::once((path.clone(), b"".as_slice())));
        h.stoat.active_workspace_mut().git_root = PathBuf::from("/ws");
        dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();

        h.type_keys("i");
        h.type_text("./");
        h.advance_clock(COMPLETION_DEBOUNCE);

        let popup = h
            .stoat
            .pending_completion
            .clone()
            .expect("path popup armed");
        let mut got: Vec<String> = labels(&popup.items)
            .into_iter()
            .filter(|l| l == "lib.rs" || l == "main.rs" || l == "buf.rs")
            .collect();
        got.sort();
        assert_eq!(got, vec!["buf.rs", "lib.rs", "main.rs"]);
        for item in &popup.items {
            if matches!(item.label.as_str(), "lib.rs" | "main.rs" | "buf.rs") {
                assert_eq!(item.source, CompletionSource::Path);
            }
        }
    }

    #[test]
    fn whitespace_context_leaves_popup_empty() {
        let mut h = TestHarness::default();
        enable_completion(&h);
        h.fake_lsp().set_completions("/ws/buf.rs", 0, 0, &["foo"]);
        open_scratch(&mut h, "");

        h.type_keys("i");
        h.type_text("   ");
        h.advance_clock(COMPLETION_DEBOUNCE);

        assert!(
            h.stoat.pending_completion.is_none(),
            "whitespace prefix should not arm the popup",
        );
    }

    #[test]
    fn leaving_insert_mode_clears_state() {
        let mut h = TestHarness::default();
        enable_completion(&h);
        open_scratch(&mut h, "");
        h.fake_lsp().set_completions("/ws/buf.rs", 0, 3, &["foo"]);

        h.type_keys("i");
        h.type_text("foo");
        h.advance_clock(COMPLETION_DEBOUNCE);
        assert!(h.stoat.pending_completion.is_some());

        // First Esc dismisses the popup but stays in insert; second
        // Esc actually exits insert mode. Trigger fires after each
        // event, sees mode != insert on the second pass, and clears
        // every completion-related field.
        h.type_keys("escape escape");
        assert_eq!(h.stoat.focused_mode(), "normal");
        assert!(h.stoat.pending_completion.is_none());
        assert!(h.stoat.pending_completion_request.is_none());
        assert!(h.stoat.last_completion_signature.is_none());
    }

    #[test]
    fn rapid_typing_cancels_in_flight_request() {
        let mut h = TestHarness::default();
        enable_completion(&h);
        open_scratch(&mut h, "");
        h.fake_lsp()
            .set_request_delay("textDocument/completion", Duration::from_millis(500));
        h.fake_lsp().set_completions("/ws/buf.rs", 0, 3, &["foo"]);

        h.type_keys("i");
        h.type_text("f");
        h.advance_clock(COMPLETION_DEBOUNCE);
        h.type_text("o");
        h.advance_clock(COMPLETION_DEBOUNCE);
        h.type_text("o");
        h.advance_clock(COMPLETION_DEBOUNCE);
        h.advance_clock(Duration::from_millis(500));

        let cancelled = h.fake_lsp().cancelled_requests();
        let completion_cancellations = cancelled
            .iter()
            .filter(|m| m == &"textDocument/completion")
            .count();
        assert!(
            completion_cancellations >= 2,
            "expected at least 2 cancelled completion requests, got {cancelled:?}",
        );
    }

    #[test]
    fn esc_dismiss_does_not_reopen_popup_on_next_event() {
        let mut h = TestHarness::default();
        enable_completion(&h);
        open_scratch(&mut h, "");
        h.fake_lsp().set_completions("/ws/buf.rs", 0, 3, &["foo"]);

        h.type_keys("i");
        h.type_text("foo");
        h.advance_clock(COMPLETION_DEBOUNCE);
        assert!(h.stoat.pending_completion.is_some());

        h.type_keys("escape");
        assert!(h.stoat.pending_completion.is_none());
        assert!(h.stoat.pending_completion_request.is_none());

        h.type_keys("left right");
        assert!(
            h.stoat.pending_completion_request.is_none(),
            "cursor-only motion should not re-arm the request after dismiss",
        );

        h.fake_lsp().set_completions("/ws/buf.rs", 0, 4, &["foox"]);
        h.type_text("x");
        assert!(
            h.stoat.pending_completion_request.is_some(),
            "buffer change must arm a fresh debounced request",
        );
    }
}
