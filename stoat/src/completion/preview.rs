//! The buffer write that shows the selected completion row before the reader
//! accepts it.
//!
//! A selection key writes the selected row's text at every cursor, over what
//! the reader typed. The next selection key puts the typed text back and
//! writes its own row. Ctrl-c puts the typed text back and closes the popup,
//! and an accept puts it back and then writes the row for good. The reader so
//! sees each row where an accept puts it, and nothing is accepted until the
//! reader moves on.
//!
//! The writes are ordinary buffer edits. They land in the insert session's
//! undo group, so undo sees only the net text, and the language server
//! receives them through the normal change sync.

use crate::{
    app::Stoat,
    buffer::TextBufferSnapshot,
    completion::{accept, request, snippet},
    editor_state::EditorId,
    pane::{FocusTarget, View},
};
use std::ops::Range;
use stoat_text::{Anchor, Bias};

/// The buffer write that shows the selected completion row.
///
/// Held on [`Stoat::completion_preview`] from the selection key that writes
/// the row until the next selection key, an accept, or an abort reverts it. It
/// answers for the buffer only while nothing else has touched it, which
/// [`is_live`] checks.
pub(crate) struct CompletionPreview {
    /// The popup generation the preview was written for.
    generation: u64,
    /// The buffer version right after the write.
    version: u64,
    /// Where the write left the newest cursor.
    landing: usize,
    /// The buffer from before the first preview of this popup.
    ///
    /// The items' replace ranges anchor into fragments that a preview write
    /// deletes, so they resolve against this snapshot, whose text each revert
    /// puts back.
    pristine: TextBufferSnapshot,
    /// One entry per cursor copy, sorted by start descending as
    /// [`accept::insert_at_every_cursor`] answers them.
    copies: Vec<PreviewCopy>,
}

/// One cursor's copy of the previewed row.
struct PreviewCopy {
    /// Where the copy starts, biased left so it stays ahead of the text a
    /// revert writes there.
    start: Anchor,
    /// The length of the previewed text.
    len: usize,
    /// The text the copy wrote over.
    original: String,
    /// The selections that land in this copy.
    ids: Vec<usize>,
}

/// Whether the preview still answers for the buffer.
///
/// The popup must be the one the preview was written for, and the focused
/// buffer must stand at the version and the cursor the write left. A paste, a
/// click, or a focus move breaks that, and the previewed text then stays in the
/// buffer as typed text.
pub(crate) fn is_live(stoat: &Stoat) -> bool {
    let Some(preview) = &stoat.completion_preview else {
        return false;
    };
    if stoat.pending_completion.is_none() || preview.generation != stoat.completion_generation {
        return false;
    }
    request::focused_buffer_probe(stoat).is_some_and(|(buffer_id, version, cursor)| {
        (buffer_id, version, cursor)
            == (preview.pristine.buffer_id, preview.version, preview.landing)
    })
}

/// Put back what the live preview wrote over, and answer the buffer from
/// before the first preview.
///
/// Drops the preview either way. A preview that is no longer live leaves its
/// text in the buffer as typed text and answers `None`.
pub(crate) fn revert(stoat: &mut Stoat) -> Option<TextBufferSnapshot> {
    let live = is_live(stoat);
    let preview = stoat.completion_preview.take()?;
    if !live {
        return None;
    }
    let editor_id = focused_editor_pane(stoat)?;
    let tails = stoat.editor_selection_tails(editor_id);

    let (copies, newest) = {
        let ws = stoat.active_workspace();
        let editor = ws.editors.get(editor_id)?;
        let handle = ws.buffers.get(editor.buffer_id)?;
        let mut buffer = handle.write().expect("poisoned");

        let spans: Vec<Range<usize>> = preview
            .copies
            .iter()
            .map(|copy| {
                let start = buffer.resolve_anchor(&copy.start);
                start..start + copy.len
            })
            .collect();
        let edits: Vec<(Range<usize>, &str)> = spans
            .iter()
            .zip(&preview.copies)
            .map(|(span, copy)| (span.clone(), copy.original.as_str()))
            .collect();
        buffer.edit_batch(&edits);

        let copies: Vec<(Range<usize>, usize, Vec<usize>)> = spans
            .into_iter()
            .zip(&preview.copies)
            .map(|(span, copy)| (span, buffer.resolve_anchor(&copy.start), copy.ids.clone()))
            .collect();
        (copies, editor.selections.newest_anchor().id)
    };

    let lens: Vec<usize> = preview
        .copies
        .iter()
        .map(|copy| copy.original.len())
        .collect();
    accept::land_past_copies(stoat, editor_id, &copies, &lens, &tails);
    request::record_dismiss(stoat);
    anchor_popup_at_newest(stoat, &copies, newest);
    Some(preview.pristine)
}

/// Write the selected row's text at every cursor, over what the reader typed,
/// in place of any preview before it.
///
/// No-op when no row is selected or the focus is not an editor pane.
///
/// Drops the completion request in flight. Its answer replaces the rows that
/// the reader chooses from, and a preview over the replaced rows then stays in
/// the buffer as typed text.
pub(crate) fn show(stoat: &mut Stoat) {
    let Some(item) = stoat
        .pending_completion
        .as_ref()
        .and_then(|popup| popup.selected())
        .cloned()
    else {
        return;
    };
    let Some(editor_id) = focused_editor_pane(stoat) else {
        return;
    };
    let pristine = match revert(stoat) {
        Some(pristine) => pristine,
        None => match request::focused_editor_snapshot(stoat) {
            Some(snapshot) => snapshot.buffer,
            None => return,
        },
    };

    let rendered = item
        .is_snippet
        .then(|| snippet::parse(&item.insert_text).render().text);
    let text = rendered.as_deref().unwrap_or(&item.insert_text);
    let edit_range = {
        let start = pristine.resolve_anchor(&item.replace_range.start);
        let end = pristine.resolve_anchor(&item.replace_range.end);
        start.min(end)..start.max(end)
    };

    let tails = stoat.editor_selection_tails(editor_id);
    let written = {
        let ws = stoat.active_workspace();
        ws.editors.get(editor_id).and_then(|editor| {
            let handle = ws.buffers.get(editor.buffer_id)?;
            let mut buffer = handle.write().expect("poisoned");
            let newest = editor.selections.newest_anchor().id;
            let copies = accept::insert_at_every_cursor(
                &mut buffer,
                editor.selections.all_anchors(),
                newest,
                edit_range,
                text,
            );
            let starts: Vec<Anchor> = copies
                .iter()
                .map(|&(_, start, _)| buffer.anchor_at(start, Bias::Left))
                .collect();
            Some((copies, starts, newest))
        })
    };
    let Some((copies, starts, newest)) = written else {
        return;
    };

    let lens = vec![text.len(); copies.len()];
    accept::land_past_copies(stoat, editor_id, &copies, &lens, &tails);
    request::record_dismiss(stoat);
    anchor_popup_at_newest(stoat, &copies, newest);
    stoat.pending_completion_request.clear();

    let Some((_, version, landing)) = request::focused_buffer_probe(stoat) else {
        return;
    };
    let copies = copies
        .into_iter()
        .zip(starts)
        .map(|((range, _, ids), start)| PreviewCopy {
            start,
            len: text.len(),
            original: pristine.visible_text.slice(range).to_string(),
            ids,
        })
        .collect();
    stoat.completion_preview = Some(CompletionPreview {
        generation: stoat.completion_generation,
        version,
        landing,
        pristine,
        copies,
    });
}

/// The editor of the focused split pane, which is the only place a completion
/// popup opens.
fn focused_editor_pane(stoat: &Stoat) -> Option<EditorId> {
    let ws = stoat.active_workspace();
    let FocusTarget::SplitPane = ws.focus else {
        return None;
    };
    match ws.panes.pane(ws.panes.focus()).view {
        View::Editor(editor_id) => Some(editor_id),
        _ => None,
    }
}

/// Anchor the popup at the start of the copy the newest cursor landed in.
///
/// The popup paints over the word the reader completes, and copies ahead of
/// the newest cursor change length with each write.
fn anchor_popup_at_newest(
    stoat: &mut Stoat,
    copies: &[(Range<usize>, usize, Vec<usize>)],
    newest: usize,
) {
    let start = copies
        .iter()
        .find(|(_, _, ids)| ids.contains(&newest))
        .map(|&(_, start, _)| start);
    if let (Some(popup), Some(start)) = (stoat.pending_completion.as_mut(), start) {
        popup.anchor_offset = start;
    }
}

#[cfg(test)]
mod tests {
    use super::is_live;
    use crate::{
        action_handlers::dispatch,
        completion::{
            anchor_range_in_focused, request::COMPLETION_DEBOUNCE, CompletionItem, CompletionPopup,
            CompletionSource,
        },
        test_fixture::open_scratch_file,
        test_harness::TestHarness,
    };
    use std::{ops::Range, path::PathBuf};
    use stoat_action::AcceptCompletion;

    /// An active snippet's tabstop groups and its exits, as offsets.
    type SnippetOffsets = (Vec<Vec<Range<usize>>>, Vec<usize>);

    /// Each selection key writes its row over the typed text in place of the
    /// row before it, and the popup stays open on the selected row.
    #[test]
    fn a_selection_key_previews_its_row_over_the_typed_text() {
        let mut h = TestHarness::default();
        let path = popup_over(&mut h, "fo", &["foobar", "foobaz"]);

        let mut seen = Vec::new();
        for key in ["tab", "tab", "backtab", "tab", "tab"] {
            h.type_keys(key);
            seen.push((h.buffer_text_at(&path), selected(&h), h.selection_spans()));
        }

        let step = |text: &str, row| (text.to_string(), Some(Some(row)), vec![(6, 6, false)]);
        assert_eq!(
            seen,
            [
                step("foobar", 0),
                step("foobaz", 1),
                step("foobar", 0),
                step("foobaz", 1),
                step("foobar", 0),
            ],
            "tab, tab, backtab, tab, and a tab on the last row that wraps",
        );
    }

    /// Ctrl-c puts back what the reader typed, and an accept by Enter or by the
    /// next typed key keeps the previewed row.
    #[test]
    fn ctrl_c_puts_the_typed_text_back_and_an_accept_keeps_the_row() {
        let after = |keys: &str| {
            let mut h = TestHarness::default();
            let path = popup_over(&mut h, "fo", &["foobar", "foobaz"]);
            h.type_keys(keys);
            (
                h.buffer_text_at(&path),
                selected(&h),
                h.stoat.focused_mode().to_string(),
            )
        };

        assert_eq!(
            [after("tab ctrl-c"), after("tab enter"), after("tab (")],
            [
                ("fo".to_string(), None, "insert".to_string()),
                ("foobar".to_string(), None, "insert".to_string()),
                ("foobar()".to_string(), None, "insert".to_string()),
            ],
        );
    }

    /// The prefix range holds the previewed row, so the popup matches what the
    /// reader typed from its own record, and Ctrl-c puts that text back.
    #[test]
    fn a_preview_matches_and_restores_what_the_reader_typed() {
        let mut h = TestHarness::default();
        let path = popup_over(&mut h, "fb", &["foo_bar"]);

        h.type_keys("tab");
        let previewed = (
            h.buffer_text_at(&path),
            crate::render::completion::completion_popup_layout(&mut h.stoat)
                .map(|(prefix, _)| prefix),
        );
        h.type_keys("ctrl-c");

        assert_eq!(
            (previewed, h.buffer_text_at(&path)),
            (
                ("foo_bar".to_string(), Some("fb".to_string())),
                "fb".to_string()
            ),
        );
    }

    /// Each cursor's copy previews the row, and Ctrl-c gives every cursor its
    /// own text and its selection back, reversed as `i` left it.
    #[test]
    fn every_cursor_previews_the_row_and_gets_its_text_back() {
        let mut h = TestHarness::default();
        let path = open_scratch_file(&mut h, "\n\n");
        h.type_keys("C i f o");
        let items = vec![row(&h, "foobar", 3..5, false)];
        install(&mut h, items, 3..5, "fo");
        let typed = (h.buffer_text_at(&path), h.selection_spans());

        h.type_keys("tab");
        let previewed = (h.buffer_text_at(&path), h.selection_spans());
        h.type_keys("ctrl-c");

        assert_eq!(
            (previewed, (h.buffer_text_at(&path), h.selection_spans())),
            (
                (
                    "foobar\nfoobar\n".to_string(),
                    vec![(6, 7, true), (13, 14, true)]
                ),
                typed,
            ),
        );
    }

    /// A snippet row previews its rendered text, and an accept from the preview
    /// expands it as an accept with no preview does.
    #[test]
    fn a_snippet_row_previews_its_text_and_accepts_as_a_direct_accept() {
        let accepted = |preview: bool| {
            let mut h = TestHarness::default();
            let path = open_scratch_file(&mut h, "");
            h.type_keys("i f o");
            let items = vec![row(&h, "foo(${1:x})$0", 0..2, true)];
            install(&mut h, items, 0..2, "fo");

            let previewed = match preview {
                true => {
                    h.type_keys("tab");
                    let text = h.buffer_text_at(&path);
                    h.type_keys("enter");
                    Some(text)
                },
                false => {
                    h.stoat
                        .pending_completion
                        .as_mut()
                        .expect("popup")
                        .selected_idx = Some(0);
                    dispatch(&mut h.stoat, &AcceptCompletion);
                    None
                },
            };
            (
                previewed,
                h.buffer_text_at(&path),
                h.selection_spans(),
                snippet_offsets(&h),
            )
        };

        let (previewed, text, spans, snippet) = accepted(true);
        assert_eq!(
            (previewed, (text, spans, snippet)),
            (Some("foo(x)".to_string()), {
                let (_, text, spans, snippet) = accepted(false);
                (text, spans, snippet)
            }),
        );
    }

    #[test]
    fn one_undo_takes_back_the_insert_session_around_a_preview() {
        let mut h = TestHarness::default();
        let path = popup_over(&mut h, "fo", &["foobar", "foobaz"]);

        h.type_keys("tab enter escape u");

        assert_eq!(h.buffer_text_at(&path), "");
    }

    /// The typing armed a request, and its answer replaces the rows the reader
    /// chooses from. A selection key drops it, so the preview stays live.
    #[test]
    fn a_live_preview_outlasts_the_request_the_typing_armed() {
        let mut h = TestHarness::default();
        let path = popup_over(&mut h, "fo", &["foobar", "foobaz"]);
        assert!(
            h.stoat.pending_completion_request.is_pending(),
            "the typing armed a request",
        );

        h.type_keys("tab");
        h.advance_clock(COMPLETION_DEBOUNCE);

        assert_eq!(
            (h.buffer_text_at(&path), selected(&h), is_live(&h.stoat)),
            ("foobar".to_string(), Some(Some(0)), true),
        );
    }

    /// Types `typed` into an empty buffer in insert mode under a popup of
    /// `labels`, each replacing the typed text.
    fn popup_over(h: &mut TestHarness, typed: &str, labels: &[&str]) -> PathBuf {
        let path = open_scratch_file(h, "");
        h.type_keys("i");
        h.type_text(typed);
        let items = labels
            .iter()
            .map(|label| row(h, label, 0..typed.len(), false))
            .collect();
        install(h, items, 0..typed.len(), typed);
        path
    }

    /// The row that writes `insert_text`, a snippet when `snippet` is set, over
    /// `range` of the focused buffer.
    fn row(
        h: &TestHarness,
        insert_text: &str,
        range: Range<usize>,
        snippet: bool,
    ) -> CompletionItem {
        CompletionItem {
            label: insert_text.into(),
            source: CompletionSource::Word,
            kind: None,
            detail: None,
            replace_range: anchor_range_in_focused(&h.stoat, range),
            insert_text: insert_text.into(),
            is_snippet: snippet,
            documentation: None,
            lsp_item: None,
            server: None,
        }
    }

    /// Opens `items` as the popup over `prefix_range`, where the reader typed
    /// `prefix`.
    fn install(
        h: &mut TestHarness,
        items: Vec<CompletionItem>,
        prefix_range: Range<usize>,
        prefix: &str,
    ) {
        h.stoat.pending_completion = Some(CompletionPopup {
            anchor_offset: prefix_range.start,
            prefix_range,
            prefix: prefix.into(),
            ..CompletionPopup::showing(items)
        });
    }

    /// The popup's selection, or `None` when no popup is open.
    fn selected(h: &TestHarness) -> Option<Option<usize>> {
        h.stoat
            .pending_completion
            .as_ref()
            .map(|popup| popup.selected_idx)
    }

    /// The active snippet's tabstop groups and exits, resolved to offsets.
    fn snippet_offsets(h: &TestHarness) -> Option<SnippetOffsets> {
        let snippet = h.stoat.active_snippet.as_ref()?;
        let (_, buffer_id) = h.stoat.focused_editor_ids()?;
        let handle = h.stoat.active_workspace().buffers.get(buffer_id)?;
        let buffer = handle.read().expect("poisoned");
        let groups = snippet
            .groups
            .iter()
            .map(|group| {
                group
                    .iter()
                    .map(|range| {
                        buffer.resolve_anchor(&range.start)..buffer.resolve_anchor(&range.end)
                    })
                    .collect()
            })
            .collect();
        let exits = snippet
            .exit
            .iter()
            .map(|anchor| buffer.resolve_anchor(anchor))
            .collect();
        Some((groups, exits))
    }
}
