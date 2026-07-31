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
    fuzzy,
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
/// the prefix and its byte range.
///
/// [`CompletionContext::text_before_cursor`] is the prefix plus the one
/// character preceding it, not the whole line. That character is what a
/// caller reads to spot a trigger character, since those (`(`, `,`, `:`)
/// are never prefix characters and so sit just outside it. Nothing needs
/// more. The path-shape tests all look for prefix characters, and the path
/// suffix walk covers the same character class the prefix does. The window
/// stops at the line start, so it never reports text from the line above.
///
/// Bounded rather than sliced from the line start because this runs on
/// every insert-mode keystroke, and a long line would otherwise cost a
/// copy of everything left of the cursor to answer a question about the
/// few characters beside it.
pub(crate) fn compute_context(rope: &Rope, cursor_offset: usize) -> ContextOwned {
    let cursor_offset = cursor_offset.min(rope.len());
    let row = rope.offset_to_point(cursor_offset).row;
    let line_start = rope.point_to_offset(Point::new(row, 0));

    let mut prefix_start = cursor_offset;
    let mut window_start = cursor_offset;
    for ch in rope.reversed_chars_at(cursor_offset) {
        if window_start <= line_start {
            break;
        }
        window_start -= ch.len_utf8();
        if !is_word_or_path_char(ch) {
            // Kept by the window so a trigger character is readable, but not
            // by the prefix, which is what it terminates.
            break;
        }
        prefix_start = window_start;
    }

    ContextOwned {
        cursor_offset,
        prefix: rope.slice(prefix_start..cursor_offset).to_string(),
        prefix_range: prefix_start..cursor_offset,
        text_before_cursor: rope.slice(window_start..cursor_offset).to_string(),
    }
}

fn is_word_or_path_char(ch: char) -> bool {
    ch.is_alphanumeric() || matches!(ch, '_' | '/' | '.' | '-' | '~')
}

/// Stamp [`Stoat::last_completion_signature`] with the focused editor's
/// current buffer signature, so the very-next [`trigger`] sees an
/// unchanged signature and returns early instead of re-arming a request.
///
/// Called after a completion is accepted, so the text the accept just
/// inserted does not immediately reopen the popup on the next event.
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

    // Probed before the full snapshot, which clones the rope and allocates two
    // paths that an unchanged buffer would throw away. The popup check still
    // runs first, since a motion moves the cursor out of range without touching
    // the version the dedupe reads.
    let Some((buffer_id, buffer_version, cursor_offset)) = focused_buffer_probe(stoat) else {
        return;
    };

    if let Some(popup) = &stoat.pending_completion
        && (cursor_offset < popup.prefix_range.start || cursor_offset > popup.prefix_range.end)
    {
        stoat.pending_completion = None;
    }

    let signature = (buffer_id, buffer_version);
    if stoat.last_completion_signature == Some(signature) {
        return;
    }
    stoat.last_completion_signature = Some(signature);

    let snapshot = match focused_editor_snapshot(stoat) {
        Some(s) => s,
        None => return,
    };

    let owned = compute_context(&snapshot.rope, snapshot.cursor_offset);
    // Signature help asks the same question on this same event, so it reads
    // this rather than walking the rope again.
    stoat.completion_context = Some((
        (
            snapshot.buffer_id,
            snapshot.buffer_version,
            snapshot.cursor_offset,
        ),
        owned.clone(),
    ));

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
                // Bumped on each install so the pooled list region detects a
                // re-query by comparing this counter instead of hashing labels.
                stoat.completion_generation = stoat.completion_generation.wrapping_add(1);
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
    let FocusTarget::SplitPane = ws.focus else {
        return false;
    };
    let pane_id = ws.panes.focus();
    matches!(ws.panes.pane(pane_id).view, View::Editor(_))
}

fn focused_editor_snapshot(stoat: &Stoat) -> Option<EditorSnapshot> {
    let ws = stoat.active_workspace();
    let FocusTarget::SplitPane = ws.focus else {
        return None;
    };
    let pane_id = ws.panes.focus();
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
    let (buffer_id, version, _) = focused_buffer_probe(stoat)?;
    Some((buffer_id, version))
}

/// The focused editor's `(buffer_id, version, cursor_offset)`.
///
/// What [`focused_editor_snapshot`] reads to decide whether the event is worth
/// answering at all, without the rope clone and two path allocations it also
/// makes. Runs on every insert-mode keystroke, most of which the dedupe it
/// feeds discards.
fn focused_buffer_probe(stoat: &Stoat) -> Option<(BufferId, u64, usize)> {
    let ws = stoat.active_workspace();
    let FocusTarget::SplitPane = ws.focus else {
        return None;
    };
    let pane_id = ws.panes.focus();
    let View::Editor(editor_id) = ws.panes.pane(pane_id).view else {
        return None;
    };
    let editor = ws.editors.get(editor_id)?;
    let sel = editor.selections.newest_anchor();
    let buffer = ws.buffers.get(editor.buffer_id)?;
    let guard = buffer.read().expect("buffer lock");
    let tail_off = guard.resolve_anchor(&sel.tail());
    let head_off = guard.resolve_anchor(&sel.head());
    Some((
        editor.buffer_id,
        guard.version(),
        stoat_text::cursor_offset(guard.rope(), tail_off, head_off),
    ))
}

/// The context the completion trigger computed for this event, when the buffer
/// and cursor have not moved since.
///
/// Both triggers run on the same event and ask the same question, so the second
/// one walking the rope again is pure repetition.
pub(crate) fn cached_context(
    stoat: &Stoat,
    buffer_id: BufferId,
    version: u64,
    cursor_offset: usize,
) -> Option<ContextOwned> {
    let (key, context) = stoat.completion_context.as_ref()?;
    (*key == (buffer_id, version, cursor_offset)).then(|| context.clone())
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
                let words = executor
                    .spawn_blocking({
                        let owned = owned.clone();
                        let rope = rope.clone();
                        move || {
                            let ctx = owned.as_borrowed();
                            crate::completion::word::fetch(&ctx, &rope)
                        }
                    })
                    .await;
                items.extend(words);
            },
        }
    }

    rank_by_prefix(&mut items, &owned.prefix);

    CompletionPopup {
        items,
        selected_idx: 0,
        anchor_offset: owned.prefix_range.start,
        prefix_range: owned.prefix_range,
    }
}

/// Order `items` by how well each answers `prefix`.
///
/// Sources are fetched one after another and concatenated, so left alone the
/// popup leads with whichever source happened to run first rather than with
/// whatever best matches what the reader typed.
///
/// Items are scored on their `filterText` where a server gave one and their
/// label otherwise, that field existing precisely for servers that want
/// matching done against something other than what they display.
///
/// Nothing is dropped. A server filters for itself and can offer an item whose
/// label the prefix does not match, so the ones that do not score keep their
/// order behind the ones that do rather than disappearing.
///
/// An empty prefix leaves the order alone, having nothing to rank by.
fn rank_by_prefix(items: &mut Vec<CompletionItem>, prefix: &str) {
    if prefix.is_empty() {
        return;
    }

    order_by_sort_text(items);

    let mut scores = vec![0u32; items.len()];
    {
        let haystacks: Vec<&str> = items.iter().map(match_against).collect();
        let ranked = fuzzy::match_and_rank(
            prefix,
            haystacks.iter().enumerate().map(|(idx, hay)| (idx, *hay)),
        );
        for m in ranked.into_iter().flatten() {
            scores[m.item] = m.score;
        }
    }

    // Stable, so items the score cannot separate keep what the sort-text pass
    // left them in, and everything else keeps the order its source produced.
    let mut order: Vec<usize> = (0..items.len()).collect();
    order.sort_by(|&a, &b| scores[b].cmp(&scores[a]));

    apply_order(items, order);
}

/// The text an item is matched against, which is its `filterText` when the
/// server set one and its label otherwise.
fn match_against(item: &CompletionItem) -> &str {
    item.lsp_item
        .as_ref()
        .and_then(|lsp| lsp.filter_text.as_deref())
        .unwrap_or(&item.label)
}

/// Reorder the items carrying a `sortText` among themselves, leaving every
/// other item where it is.
///
/// Only LSP items have one, so ordering the whole list by it would have to
/// decide how an item with a sort text ranks against an item without, and any
/// answer to that shuffles items across sources for no reason. Confining the
/// reorder to the items that have one asks no such question.
fn order_by_sort_text(items: &mut Vec<CompletionItem>) {
    let slots: Vec<usize> = (0..items.len())
        .filter(|&idx| sort_text(&items[idx]).is_some())
        .collect();
    if slots.len() < 2 {
        return;
    }

    let mut sorted = slots.clone();
    sorted.sort_by(|&a, &b| {
        sort_text(&items[a])
            .cmp(&sort_text(&items[b]))
            .then(a.cmp(&b))
    });

    // Every position keeps what it holds except the slots, which take the
    // sorted sequence between them.
    let mut order: Vec<usize> = (0..items.len()).collect();
    for (slot, from) in slots.into_iter().zip(sorted) {
        order[slot] = from;
    }

    apply_order(items, order);
}

fn sort_text(item: &CompletionItem) -> Option<&str> {
    item.lsp_item
        .as_ref()
        .and_then(|lsp| lsp.sort_text.as_deref())
}

/// Permute `items` so that position `n` holds what `order`'s `n`th index names.
fn apply_order(items: &mut Vec<CompletionItem>, order: Vec<usize>) {
    let mut taken: Vec<Option<CompletionItem>> = items.drain(..).map(Some).collect();
    items.extend(
        order
            .into_iter()
            .map(|from| taken[from].take().expect("each index is named once")),
    );
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
        assert_eq!(
            ctx.text_before_cursor, " foo",
            "the window is the prefix plus the character that ended it",
        );
    }

    /// The character before an empty prefix is the one a caller reads to spot a
    /// trigger character, and trigger characters are exactly the ones a prefix
    /// stops at, so the window has to reach one past it.
    #[test]
    fn a_trigger_character_is_inside_the_window_past_an_empty_prefix() {
        let r = rope("foo(");
        let ctx = compute_context(&r, 4);
        assert_eq!(ctx.prefix, "");
        assert_eq!(ctx.text_before_cursor, "(");
        assert_eq!(ctx.text_before_cursor.chars().last(), Some('('));
    }

    /// The walk stops at the prefix, so the cost of a keystroke does not grow
    /// with how far along the line the cursor sits.
    #[test]
    fn a_long_line_does_not_widen_the_window() {
        let mut line = "x".repeat(10_000);
        line.push_str(" foo");
        let r = rope(&line);
        let ctx = compute_context(&r, r.len());
        assert_eq!(ctx.prefix, "foo");
        assert_eq!(ctx.text_before_cursor, " foo");
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
        assert_eq!(ctx.text_before_cursor, " foo");
        let prefix_byte_len = "foo".len();
        assert_eq!(ctx.prefix_range, (cursor - prefix_byte_len)..cursor);

        // At column zero the window has nowhere on this line to reach, and it
        // must not reach into the line above.
        let start_of_second = "first line\n".len();
        let at_line_start = compute_context(&r, start_of_second);
        assert_eq!(at_line_start.prefix, "");
        assert_eq!(at_line_start.text_before_cursor, "");
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

    /// A motion moves the cursor without touching the buffer version, so the
    /// signature dedupe would sit the event out. The popup still has to notice
    /// the cursor leaving the range it was filtered for, which is why the range
    /// check runs ahead of that dedupe.
    #[test]
    fn a_motion_out_of_range_dismisses_the_popup() {
        let mut h = TestHarness::default();
        enable_completion(&h);
        open_scratch(&mut h, "");
        h.fake_lsp()
            .set_completions("/ws/buf.rs", 0, 7, &["foobar", "foobaz"]);

        // Typed past column zero, so the prefix has somewhere to leave from.
        h.type_keys("i");
        h.type_text("bar foo");
        h.advance_clock(COMPLETION_DEBOUNCE);
        let popup = h.stoat.pending_completion.clone().expect("popup armed");
        assert!(popup.prefix_range.start > 0, "the prefix has room to leave");

        let version = |h: &mut TestHarness| {
            let (_, buffer_id) = h.stoat.focused_editor_ids().expect("focused editor");
            let ws = h.stoat.active_workspace();
            ws.buffers
                .get(buffer_id)
                .expect("buffer")
                .read()
                .expect("poisoned")
                .version()
        };
        let before = version(&mut h);

        // Arrowing back off the prefix moves the cursor and edits nothing.
        h.type_keys("left left left left");
        assert_eq!(version(&mut h), before, "a motion changes no text");
        assert!(
            h.stoat.pending_completion.is_none(),
            "the cursor left the range the popup was filtered for",
        );
    }

    #[test]
    fn a_re_query_bumps_the_generation_while_a_stable_popup_does_not() {
        let mut h = TestHarness::default();
        enable_completion(&h);
        open_scratch(&mut h, "");
        h.fake_lsp()
            .set_completions("/ws/buf.rs", 0, 3, &["foobar", "foobaz"]);

        assert_eq!(h.stoat.completion_generation, 0, "no completion armed yet");

        h.type_keys("i");
        h.type_text("foo");
        h.advance_clock(COMPLETION_DEBOUNCE);
        assert!(h.stoat.pending_completion.is_some(), "popup armed");
        assert_eq!(
            h.stoat.completion_generation, 1,
            "installing a popup bumps the generation"
        );

        // Settling with no new query leaves the popup in place, so the pool's
        // content version stays put across emits.
        h.settle();
        assert_eq!(
            h.stoat.completion_generation, 1,
            "a stable popup does not bump the generation"
        );

        // Extending the prefix re-queries and installs fresh items.
        h.fake_lsp()
            .set_completions("/ws/buf.rs", 0, 4, &["foobar"]);
        h.type_text("b");
        h.advance_clock(COMPLETION_DEBOUNCE);
        assert!(h.stoat.pending_completion.is_some(), "re-query re-armed");
        assert_eq!(
            h.stoat.completion_generation, 2,
            "a re-query bumps the generation so the pool refills"
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
    fn esc_dismisses_popup_and_leaves_insert_in_one_press() {
        let mut h = TestHarness::default();
        enable_completion(&h);
        open_scratch(&mut h, "");
        h.fake_lsp().set_completions("/ws/buf.rs", 0, 3, &["foo"]);

        h.type_keys("i");
        h.type_text("foo");
        h.advance_clock(COMPLETION_DEBOUNCE);
        assert!(h.stoat.pending_completion.is_some());

        h.type_keys("escape");
        assert!(
            h.stoat.pending_completion.is_none(),
            "one escape closes the popup"
        );
        assert!(h.stoat.pending_completion_request.is_none());
        assert_eq!(
            h.stoat.focused_mode(),
            "normal",
            "the same escape also leaves insert mode",
        );

        h.type_keys("i");
        h.type_text("x");
        assert!(
            h.stoat.pending_completion_request.is_some(),
            "typing after re-entering insert arms a fresh request",
        );
    }

    /// A buffer-word item, which carries no server fields.
    fn word(label: &str) -> CompletionItem {
        CompletionItem {
            label: label.to_string(),
            source: CompletionSource::Word,
            kind: None,
            detail: None,
            documentation: None,
            replace_range: 0..0,
            insert_text: label.to_string(),
            is_snippet: false,
            lsp_item: None,
            server: None,
        }
    }

    /// A server item, optionally carrying the two fields a server uses to steer
    /// matching and ordering.
    fn served(label: &str, filter_text: Option<&str>, sort_text: Option<&str>) -> CompletionItem {
        CompletionItem {
            source: CompletionSource::Lsp,
            lsp_item: Some(Box::new(lsp_types::CompletionItem {
                label: label.to_string(),
                filter_text: filter_text.map(str::to_string),
                sort_text: sort_text.map(str::to_string),
                ..Default::default()
            })),
            server: Some("test".to_string()),
            ..word(label)
        }
    }

    #[test]
    fn ranking_leads_with_what_answers_the_prefix_not_its_source() {
        // Interleaved as the sources produce them, with the run of characters
        // the reader typed scattered through the first and contiguous in the
        // second. The scorer separates those. It does not separate a match from
        // a longer haystack around the same match.
        let mut items = vec![
            word("a_pretty_pear"),
            served("apply_theme", None, None),
            word("zebra_handler"),
        ];

        rank_by_prefix(&mut items, "app");

        assert_eq!(
            labels(&items),
            ["apply_theme", "a_pretty_pear", "zebra_handler"],
            "the popup leads with the best answer whichever source produced it"
        );
    }

    #[test]
    fn an_item_the_prefix_cannot_match_stays_in_the_list() {
        let mut items = vec![served("unrelated", None, None), word("apply")];

        rank_by_prefix(&mut items, "app");

        assert_eq!(
            labels(&items),
            ["apply", "unrelated"],
            "a server filters for itself, so what it offered is kept rather than dropped"
        );
    }

    #[test]
    fn an_empty_prefix_leaves_the_sources_order_alone() {
        let mut items = vec![
            word("zebra"),
            served("beta", None, Some("0001")),
            served("alpha", None, Some("0000")),
        ];

        rank_by_prefix(&mut items, "");

        assert_eq!(
            labels(&items),
            ["zebra", "beta", "alpha"],
            "nothing to rank by, so the order the sources gave stands"
        );
    }

    #[test]
    fn equal_scores_fall_back_to_the_servers_sort_text() {
        // Identical labels score identically, so only the sort text separates
        // them, and the word item has none to be judged by.
        let mut items = vec![
            served("item", Some("app"), Some("0002")),
            word("app"),
            served("item", Some("app"), Some("0001")),
        ];

        rank_by_prefix(&mut items, "app");

        let ordering: Vec<Option<&str>> = items.iter().map(sort_text).collect();
        assert_eq!(
            ordering,
            [Some("0001"), None, Some("0002")],
            "the two server items take the server's order between them, and the \
             word item they straddle keeps the position its source gave it"
        );
    }

    #[test]
    fn matching_uses_the_filter_text_a_server_supplies() {
        // The label alone would put the word item first, its characters being
        // findable in order where the server's label's are not.
        let mut items = vec![
            word("a_p_p_lication"),
            served("unhelpful label", Some("app"), None),
        ];

        rank_by_prefix(&mut items, "app");

        assert_eq!(
            labels(&items),
            ["unhelpful label", "a_p_p_lication"],
            "a server asking to be matched on something else is matched on it"
        );
    }

    #[test]
    fn the_installed_popup_is_already_ranked() {
        let mut h = TestHarness::default();
        enable_completion(&h);
        open_scratch(&mut h, "");

        // Offered worst-first, so arrival order and ranked order disagree.
        h.fake_lsp()
            .set_completions("/ws/buf.rs", 0, 3, &["f_o_o_bar", "foobar"]);

        h.type_keys("i");
        h.type_text("foo");
        h.advance_clock(COMPLETION_DEBOUNCE);

        let popup = h.stoat.pending_completion.clone().expect("popup armed");
        let got = labels(&popup.items);
        let contiguous = got
            .iter()
            .position(|l| l == "foobar")
            .expect("the contiguous match is offered");
        let scattered = got
            .iter()
            .position(|l| l == "f_o_o_bar")
            .expect("the scattered match is offered");

        assert!(
            contiguous < scattered,
            "the popup is ranked where it is installed rather than left in arrival order: {got:?}"
        );
    }
}
