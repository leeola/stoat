//! Acceptance handler for the completion popup.
//!
//! Replaces the highlighted item's `replace_range` with its
//! `insert_text` at every cursor in the focused buffer, puts each cursor
//! at the end of its own insertion, and clears popup state. Bound to
//! `Tab` in insert mode via the arbitration arm in
//! [`crate::app::Stoat::handle_insert_key`].
//!
//! An item's `replace_range` is anchored against the text its source
//! read, so it still names that text after the keystrokes the popup
//! outlived. LSP items widen it beyond the typed prefix when the
//! server returns a `text_edit.range`. Non-LSP items scope it to the
//! prefix range. Acceptance resolves the range against the live
//! buffer, so both shapes work uniformly.

use crate::{
    app::{Stoat, UpdateEffect},
    buffer::{BufferId, TextBuffer},
    completion::CompletionItem,
    lsp::stamp::DocumentStamp,
    pane::{FocusTarget, View},
};
use std::{ops::Range, path::Path, time::Duration};
use stoat_text::{Anchor, Bias, Rope, Selection};

/// The `additionalTextEdits` a `completionItem/resolve` returned for an
/// accepted completion, plus the buffer they apply to. Carried from the
/// accept resolve task to [`pump_completion_accept`].
pub(crate) struct AcceptedImports {
    buffer_id: BufferId,
    edits: Vec<lsp_types::TextEdit>,
}

/// Accept the highlighted item in [`Stoat::pending_completion`]. No-op
/// when the popup is not showing, the focused pane is not an editor,
/// or the popup's items list is empty.
///
/// Every cursor gets a copy of the accepted text. Snippet items
/// (`is_snippet: true`) parse the insert text via
/// [`crate::completion::snippet::parse`], and install selections at the
/// first tabstop group of every copy. The remaining groups stash on
/// [`Stoat::active_snippet`] for [`crate::completion::snippet::advance`]
/// to consume on later Tab presses. Plain items insert the text
/// verbatim, and each cursor lands at the end of its own insertion.
pub(crate) fn execute(stoat: &mut Stoat) -> UpdateEffect {
    // A narrow off the loop has not landed yet on the keystroke that armed it,
    // and the rows it installs next are the ones the reader sees when Tab
    // arrives. Landing it here is what makes them the rows accepted.
    crate::completion::request::settle_pending_narrow(stoat);

    let Some(popup) = stoat.pending_completion.take() else {
        return UpdateEffect::None;
    };
    let Some(item) = popup.selected().cloned() else {
        return UpdateEffect::None;
    };

    let editor_id = {
        let ws = stoat.active_workspace();
        let FocusTarget::SplitPane = ws.focus else {
            return UpdateEffect::None;
        };
        let View::Editor(editor_id) = ws.panes.pane(ws.panes.focus()).view else {
            return UpdateEffect::None;
        };
        editor_id
    };
    let tails = stoat.editor_selection_tails(editor_id);

    let ws = stoat.active_workspace_mut();
    let editor = match ws.editors.get_mut(editor_id) {
        Some(e) => e,
        None => return UpdateEffect::None,
    };
    let buffer_id = editor.buffer_id;
    let buffer = match ws.buffers.get(buffer_id) {
        Some(b) => b,
        None => return UpdateEffect::None,
    };

    // Resolved against the buffer as it is now, not as the source read it. An
    // edit that collapsed the text between the two ends also crosses them.
    let edit_range = {
        let guard = buffer.read().expect("poisoned");
        let start = guard.resolve_anchor(&item.replace_range.start);
        let end = guard.resolve_anchor(&item.replace_range.end);
        start.min(end)..start.max(end)
    };

    let snippet_rendered = if item.is_snippet {
        Some(crate::completion::snippet::parse(&item.insert_text).render())
    } else {
        None
    };
    let inserted_text: &str = snippet_rendered
        .as_ref()
        .map(|r| r.text.as_str())
        .unwrap_or(&item.insert_text);

    let copies = insert_at_every_cursor(
        &mut buffer.write().expect("poisoned"),
        editor.selections.all_anchors(),
        editor.selections.newest_anchor().id,
        edit_range,
        inserted_text,
    );

    let active_snippet = if let Some(rendered) = &snippet_rendered {
        let new_display = editor.display_map.snapshot();
        let new_buf = new_display.buffer_snapshot();
        let mut starts: Vec<usize> = copies.iter().map(|(_, start, _)| *start).collect();
        starts.sort_unstable();
        let (selections, active) = crate::completion::snippet::install(rendered, &starts, new_buf);
        editor.selections.replace_with(selections, new_buf);
        active
    } else {
        // Each cursor lands past its own copy, and its tail maps through every
        // copy, so a selection the reader typed into survives the accept.
        let landings: Vec<(usize, usize)> = copies
            .iter()
            .flat_map(|(_, start, ids)| {
                ids.iter().map(move |&id| (id, start + inserted_text.len()))
            })
            .collect();
        let written: Vec<(Range<usize>, usize)> = copies
            .iter()
            .rev()
            .map(|(range, _, _)| (range.clone(), inserted_text.len()))
            .collect();
        stoat.land_cursors_after_insert(editor_id, landings, &tails, &written);
        None
    };

    stoat.pending_completion_request.clear();
    crate::completion::request::record_dismiss(stoat);
    stoat.active_snippet = active_snippet;

    apply_or_resolve_additional_edits(stoat, buffer_id, &item);

    UpdateEffect::Redraw
}

/// Replace each cursor's copy of the accepted text with `text` in one batch
/// edit, and answer each copy's replaced range, where the copy starts after the
/// edit, and the ids of the selections that land in it.
///
/// `edit_range` is the range of the `newest` selection, which the request
/// measured from. The copies come sorted by start descending, as
/// [`replace_ranges`] answers them.
fn insert_at_every_cursor(
    buffer: &mut TextBuffer,
    selections: &[Selection<Anchor>],
    newest: usize,
    edit_range: Range<usize>,
    text: &str,
) -> Vec<(Range<usize>, usize, Vec<usize>)> {
    let cursors: Vec<(usize, usize)> = selections
        .iter()
        .map(|sel| {
            let tail = buffer.resolve_anchor(&sel.tail());
            let head = buffer.resolve_anchor(&sel.head());
            (sel.id, stoat_text::cursor_offset(buffer.rope(), tail, head))
        })
        .collect();
    let newest_at = cursors
        .iter()
        .find(|(id, _)| *id == newest)
        .map_or(edit_range.end, |&(_, at)| at);
    let groups = replace_ranges(buffer.rope(), &cursors, newest_at, edit_range);

    // Left bias keeps each start ahead of the text inserted there.
    let starts: Vec<Anchor> = groups
        .iter()
        .map(|(range, _)| buffer.anchor_at(range.start, Bias::Left))
        .collect();
    let edits: Vec<(Range<usize>, &str)> = groups
        .iter()
        .map(|(range, _)| (range.clone(), text))
        .collect();
    buffer.edit_batch(&edits);

    groups
        .into_iter()
        .zip(&starts)
        .map(|((range, ids), start)| (range, buffer.resolve_anchor(start), ids))
        .collect()
}

/// The range each cursor replaces when a completion is accepted at every
/// cursor, as groups of a range and the selection ids that land with it.
///
/// `cursors` holds each selection id with its insertion offset, and the cursor
/// at `newest` replaces `edit_range`. Every other cursor replaces that range
/// moved to itself when its text there is the same. A completion that reaches
/// past the typed prefix then lands the same way at each cursor. A cursor whose
/// text differs replaces its own word prefix.
///
/// The groups come sorted by start descending, with no overlap and no shared
/// start, which is what a batch edit takes. A cursor whose range collides with
/// a range already taken joins that group, so it lands with that cursor.
fn replace_ranges(
    rope: &Rope,
    cursors: &[(usize, usize)],
    newest: usize,
    edit_range: Range<usize>,
) -> Vec<(Range<usize>, Vec<usize>)> {
    let mut ranges: Vec<(Range<usize>, usize)> = cursors
        .iter()
        .map(|&(id, at)| {
            let moved = (edit_range.start + at)
                .checked_sub(newest)
                .map(|start| start..start + edit_range.len())
                .filter(|range| {
                    range.end <= rope.len()
                        && rope
                            .bytes_in_range(range.clone())
                            .eq(rope.bytes_in_range(edit_range.clone()))
                });
            let range = moved.unwrap_or_else(|| {
                crate::completion::request::compute_context(rope, at).prefix_range
            });
            (range, id)
        })
        .collect();
    ranges.sort_by(|(a, _), (b, _)| b.start.cmp(&a.start).then(b.end.cmp(&a.end)));

    let mut groups: Vec<(Range<usize>, Vec<usize>)> = Vec::with_capacity(ranges.len());
    for (range, id) in ranges {
        match groups.last_mut() {
            Some((taken, ids)) if range.end > taken.start || range.start == taken.start => {
                ids.push(id);
            },
            _ => groups.push((range, vec![id])),
        }
    }
    groups
}

/// Apply the accepted item's `additionalTextEdits` -- typically the
/// imports rust-analyzer adds when a completion needs one.
///
/// Applies them synchronously when the item already carries them.
/// Otherwise, for an LSP item whose server advertises `resolveProvider`,
/// resolves the item with a 300ms timeout and applies the edits from
/// [`pump_completion_accept`] once it lands. The main edit has already
/// been applied by [`execute`], so a resolve failure or timeout simply
/// leaves it as the only edit. Non-LSP items do nothing.
fn apply_or_resolve_additional_edits(
    stoat: &mut Stoat,
    buffer_id: BufferId,
    item: &CompletionItem,
) {
    let Some(lsp_item) = &item.lsp_item else {
        return;
    };
    if let Some(edits) = lsp_item.additional_text_edits.clone()
        && !edits.is_empty()
    {
        apply_additional_edits(
            stoat,
            buffer_id,
            edits,
            resolve_host(stoat, item, buffer_id).offset_encoding(),
        );
        return;
    }
    let lsp = resolve_host(stoat, item, buffer_id);
    let lsp_encoding = lsp.offset_encoding();
    if !resolve_advertised(&lsp) {
        return;
    }

    let raw = (**lsp_item).clone();
    let executor = stoat.executor.clone();
    let task = stoat.spawn_woken(async move {
        let resolve = std::pin::pin!(lsp.completion_resolve(raw));
        let timer = std::pin::pin!(executor.timer(Duration::from_millis(300)));
        let resolved = match futures::future::select(resolve, timer).await {
            futures::future::Either::Left((Ok(item), _)) => item,
            _ => return None,
        };
        let edits = resolved.additional_text_edits?;
        (!edits.is_empty()).then_some(AcceptedImports { buffer_id, edits })
    });
    let stamp = DocumentStamp::take(stoat, buffer_id, lsp_encoding);
    stoat.pending_completion_accept.arm(stamp, task);
}

/// The server to route `item`'s `completionItem/resolve` back to: the server
/// that produced it when known, else the buffer's primary.
fn resolve_host(
    stoat: &Stoat,
    item: &CompletionItem,
    buffer_id: BufferId,
) -> std::sync::Arc<dyn crate::host::LspHost> {
    item.server
        .as_deref()
        .and_then(|name| stoat.lsp_registry.client(name))
        .unwrap_or_else(|| crate::lsp::hosts::lsp_for(stoat, buffer_id))
}

fn resolve_advertised(host: &std::sync::Arc<dyn crate::host::LspHost>) -> bool {
    host.capabilities()
        .completion_provider
        .as_ref()
        .and_then(|opts| opts.resolve_provider)
        .unwrap_or(false)
}

fn apply_additional_edits(
    stoat: &mut Stoat,
    buffer_id: BufferId,
    edits: Vec<lsp_types::TextEdit>,
    encoding: crate::host::OffsetEncoding,
) {
    let Some(path) = stoat
        .active_workspace()
        .buffers
        .path_for(buffer_id)
        .map(Path::to_path_buf)
    else {
        return;
    };
    if let Err(err) =
        crate::lsp::edit_apply::apply_text_edits_to_buffer(stoat, &path, edits, encoding)
    {
        tracing::warn!(target: "stoat::lsp", ?err, "additionalTextEdits apply failed");
    }
}

/// Poll the in-flight accept-resolve task. On completion, apply the
/// resolved `additionalTextEdits` to the captured buffer. The placed
/// cursor rides edit-tracking anchors, so imports inserted above it keep
/// it correct. Returns `true` when the buffer changed.
pub(crate) fn pump_completion_accept(stoat: &mut Stoat) -> bool {
    let Some((requested_at, imports)) = stoat.pending_completion_accept.poll() else {
        return false;
    };
    // Imports name lines in the text the accept left behind. Typing since then
    // moves them, and an import landing mid-line is worse than none.
    let Some(imports) = imports.filter(|_| requested_at.is_current(stoat)) else {
        return false;
    };

    apply_additional_edits(
        stoat,
        imports.buffer_id,
        imports.edits,
        requested_at.encoding(),
    );
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        action_handlers::dispatch,
        completion::{anchor_range_in_focused, CompletionItem, CompletionPopup, CompletionSource},
        test_fixture::open_scratch_at,
        test_harness::TestHarness,
    };
    use std::{ops::Range, path::PathBuf};
    use stoat_action::AcceptCompletion;
    use stoat_text::{Anchor, SelectionGoal};

    fn open_scratch(h: &mut TestHarness, contents: &str) -> PathBuf {
        open_scratch_at(h, "/ws/buf.rs", contents)
    }

    fn cursor_offset(h: &mut TestHarness) -> usize {
        let ws = h.stoat.active_workspace_mut();
        let FocusTarget::SplitPane = ws.focus else {
            panic!("not a split pane");
        };
        let pane_id = ws.panes.focus();
        let View::Editor(editor_id) = ws.panes.pane(pane_id).view else {
            panic!("not an editor pane");
        };
        let editor = ws.editors.get_mut(editor_id).expect("editor");
        let snapshot = editor.display_map.snapshot();
        let buf_snapshot = snapshot.buffer_snapshot();
        let head = editor.selections.newest_anchor().head();
        buf_snapshot.resolve_anchor(&head)
    }

    /// The range a source mints for `range`, for tests that install a popup
    /// rather than driving the request pipeline for one.
    fn anchors(h: &TestHarness, range: Range<usize>) -> Range<Anchor> {
        anchor_range_in_focused(&h.stoat, range)
    }

    fn install_popup(h: &mut TestHarness, items: Vec<CompletionItem>, prefix_range: Range<usize>) {
        h.stoat.pending_completion = Some(CompletionPopup {
            anchor_offset: prefix_range.start,
            prefix_range,
            ..CompletionPopup::showing(items)
        });
    }

    #[test]
    fn accept_replaces_prefix_with_insert_text() {
        let mut h = TestHarness::default();
        let path = open_scratch(&mut h, "");
        h.type_keys("i");
        h.type_text("foo");
        let items = vec![CompletionItem {
            label: "foobar".into(),
            source: CompletionSource::Word,
            kind: None,
            detail: None,
            replace_range: anchors(&h, 0..3),
            insert_text: "foobar".into(),
            is_snippet: false,
            documentation: None,
            lsp_item: None,
            server: None,
        }];
        install_popup(&mut h, items, 0..3);

        dispatch(&mut h.stoat, &AcceptCompletion);

        assert_eq!(h.buffer_text_at(&path), "foobar");
        assert_eq!(cursor_offset(&mut h), 6);
        assert!(h.stoat.pending_completion.is_none());
        assert!(!h.stoat.pending_completion_request.is_pending());
    }

    #[test]
    fn accept_keeps_a_selection_the_reader_typed_into() {
        let mut h = TestHarness::default();
        let path = open_scratch(&mut h, "bar baz\n");
        h.type_keys("w i");
        h.type_text("fo");
        let items = vec![CompletionItem {
            label: "foo".into(),
            source: CompletionSource::Word,
            kind: None,
            detail: None,
            replace_range: anchors(&h, 0..2),
            insert_text: "foo".into(),
            is_snippet: false,
            documentation: None,
            lsp_item: None,
            server: None,
        }];
        install_popup(&mut h, items, 0..2);

        dispatch(&mut h.stoat, &AcceptCompletion);

        assert_eq!(
            (h.buffer_text_at(&path), h.selection_spans()),
            ("foobar baz\n".to_string(), vec![(3, 7, true)]),
            "the caret lands past the completion and the tail past the text it moved",
        );
    }

    #[test]
    fn accept_honors_widened_replace_range_from_lsp_text_edit() {
        let mut h = TestHarness::default();
        let path = open_scratch(&mut h, "");
        h.type_keys("i");
        h.type_text("print");
        let items = vec![CompletionItem {
            label: "println!".into(),
            source: CompletionSource::Lsp,
            kind: None,
            detail: None,
            replace_range: anchors(&h, 0..5),
            insert_text: "println!(\"\")".into(),
            is_snippet: false,
            documentation: None,
            lsp_item: None,
            server: None,
        }];
        install_popup(&mut h, items, 0..5);

        dispatch(&mut h.stoat, &AcceptCompletion);

        assert_eq!(h.buffer_text_at(&path), "println!(\"\")");
        assert_eq!(cursor_offset(&mut h), "println!(\"\")".len());
    }

    #[test]
    fn accept_uses_selected_idx_not_first_item() {
        let mut h = TestHarness::default();
        let path = open_scratch(&mut h, "");
        h.type_keys("i");
        h.type_text("fo");
        let replace_range = anchors(&h, 0..2);
        h.stoat.pending_completion = Some(CompletionPopup {
            selected_idx: 1,
            prefix_range: 0..2,
            ..CompletionPopup::showing(vec![
                CompletionItem {
                    label: "foo".into(),
                    source: CompletionSource::Word,
                    kind: None,
                    detail: None,
                    replace_range: replace_range.clone(),
                    insert_text: "foo".into(),
                    is_snippet: false,
                    documentation: None,
                    lsp_item: None,
                    server: None,
                },
                CompletionItem {
                    label: "foobar".into(),
                    source: CompletionSource::Word,
                    kind: None,
                    detail: None,
                    replace_range,
                    insert_text: "foobar".into(),
                    is_snippet: false,
                    documentation: None,
                    lsp_item: None,
                    server: None,
                },
            ])
        });

        dispatch(&mut h.stoat, &AcceptCompletion);

        assert_eq!(h.buffer_text_at(&path), "foobar");
    }

    #[test]
    fn accept_with_no_popup_is_noop() {
        let mut h = TestHarness::default();
        let path = open_scratch(&mut h, "abc");
        h.type_keys("a");
        assert!(h.stoat.pending_completion.is_none());

        let effect = dispatch(&mut h.stoat, &AcceptCompletion);

        assert_eq!(effect, UpdateEffect::None);
        assert_eq!(h.buffer_text_at(&path), "abc");
    }

    #[test]
    fn accept_snippet_expands_placeholder_and_arms_cursor() {
        let mut h = TestHarness::default();
        let path = open_scratch(&mut h, "");
        h.type_keys("i");
        h.type_text("pri");
        let items = vec![CompletionItem {
            label: "println!".into(),
            source: CompletionSource::Lsp,
            kind: None,
            detail: None,
            replace_range: anchors(&h, 0..3),
            insert_text: "println!(${1:msg})$0".into(),
            is_snippet: true,
            documentation: None,
            lsp_item: None,
            server: None,
        }];
        install_popup(&mut h, items, 0..3);

        dispatch(&mut h.stoat, &AcceptCompletion);

        assert_eq!(h.buffer_text_at(&path), "println!(msg)");
        let ws = h.stoat.active_workspace_mut();
        let FocusTarget::SplitPane = ws.focus else {
            panic!("not split");
        };
        let pane_id = ws.panes.focus();
        let View::Editor(eid) = ws.panes.pane(pane_id).view else {
            panic!("not editor");
        };
        let editor = ws.editors.get_mut(eid).expect("editor");
        let snap = editor.display_map.snapshot();
        let buf_snap = snap.buffer_snapshot();
        let sel = editor.selections.newest_anchor();
        let start = buf_snap.resolve_anchor(&sel.start);
        let end = buf_snap.resolve_anchor(&sel.end);
        assert_eq!((start, end), (9, 12), "selection on `msg` placeholder");
        assert!(
            h.stoat.active_snippet.is_some(),
            "active snippet should remain so Tab can advance to $0",
        );
    }

    #[test]
    fn accept_snippet_with_only_exit_does_not_arm_active() {
        let mut h = TestHarness::default();
        let path = open_scratch(&mut h, "");
        h.type_keys("i");
        h.type_text("pri");
        let items = vec![CompletionItem {
            label: "println!".into(),
            source: CompletionSource::Lsp,
            kind: None,
            detail: None,
            replace_range: anchors(&h, 0..3),
            insert_text: "println!()$0".into(),
            is_snippet: true,
            documentation: None,
            lsp_item: None,
            server: None,
        }];
        install_popup(&mut h, items, 0..3);

        dispatch(&mut h.stoat, &AcceptCompletion);

        assert_eq!(h.buffer_text_at(&path), "println!()");
        assert!(h.stoat.active_snippet.is_none());
    }

    #[test]
    fn accept_snippet_with_linked_tabstops_places_multi_cursor() {
        let mut h = TestHarness::default();
        let path = open_scratch(&mut h, "");
        h.type_keys("i");
        h.type_text("foo");
        let items = vec![CompletionItem {
            label: "linked".into(),
            source: CompletionSource::Lsp,
            kind: None,
            detail: None,
            replace_range: anchors(&h, 0..3),
            insert_text: "${1:x} = ${1}".into(),
            is_snippet: true,
            documentation: None,
            lsp_item: None,
            server: None,
        }];
        install_popup(&mut h, items, 0..3);

        dispatch(&mut h.stoat, &AcceptCompletion);

        assert_eq!(h.buffer_text_at(&path), "x = ");
        let ws = h.stoat.active_workspace_mut();
        let FocusTarget::SplitPane = ws.focus else {
            panic!("not split");
        };
        let pane_id = ws.panes.focus();
        let View::Editor(eid) = ws.panes.pane(pane_id).view else {
            panic!("not editor");
        };
        let editor = ws.editors.get(eid).expect("editor");
        assert_eq!(
            editor.selections.all_anchors().len(),
            2,
            "two cursors at the linked tabstop sites",
        );
    }

    #[test]
    fn non_snippet_item_keeps_existing_behavior() {
        let mut h = TestHarness::default();
        let path = open_scratch(&mut h, "");
        h.type_keys("i");
        h.type_text("foo");
        let items = vec![CompletionItem {
            label: "foobar".into(),
            source: CompletionSource::Word,
            kind: None,
            detail: None,
            replace_range: anchors(&h, 0..3),
            insert_text: "foobar".into(),
            is_snippet: false,
            documentation: None,
            lsp_item: None,
            server: None,
        }];
        install_popup(&mut h, items, 0..3);

        dispatch(&mut h.stoat, &AcceptCompletion);

        assert_eq!(h.buffer_text_at(&path), "foobar");
        assert!(h.stoat.active_snippet.is_none());
    }

    fn enable_resolve(h: &TestHarness) {
        use lsp_types::{CompletionOptions, ServerCapabilities};
        h.fake_lsp().set_capabilities(ServerCapabilities {
            completion_provider: Some(CompletionOptions {
                resolve_provider: Some(true),
                ..CompletionOptions::default()
            }),
            ..ServerCapabilities::default()
        });
    }

    fn lsp_row(h: &TestHarness, label: &str, range: Range<usize>) -> CompletionItem {
        CompletionItem {
            label: label.into(),
            source: CompletionSource::Lsp,
            kind: None,
            detail: None,
            replace_range: anchors(h, range),
            insert_text: label.into(),
            is_snippet: false,
            documentation: None,
            lsp_item: Some(Box::new(lsp_types::CompletionItem {
                label: label.into(),
                ..Default::default()
            })),
            server: None,
        }
    }

    fn resolved_with_import(label: &str, text: &str) -> lsp_types::CompletionItem {
        lsp_types::CompletionItem {
            label: label.into(),
            additional_text_edits: Some(vec![lsp_types::TextEdit {
                range: lsp_types::Range::new(
                    lsp_types::Position::new(0, 0),
                    lsp_types::Position::new(0, 0),
                ),
                new_text: text.into(),
            }]),
            ..Default::default()
        }
    }

    #[test]
    fn accept_resolves_and_applies_additional_text_edits() {
        let mut h = TestHarness::default();
        let path = open_scratch(&mut h, "");
        enable_resolve(&h);
        h.fake_lsp()
            .set_completion_resolve("barbaz", resolved_with_import("barbaz", "use foo;\n"));
        h.type_keys("i");
        h.type_text("bar");
        let items = vec![lsp_row(&h, "barbaz", 0..3)];
        install_popup(&mut h, items, 0..3);

        dispatch(&mut h.stoat, &AcceptCompletion);
        assert_eq!(
            h.buffer_text_at(&path),
            "barbaz",
            "the main edit lands at once"
        );
        h.settle();

        assert_eq!(
            h.buffer_text_at(&path),
            "use foo;\nbarbaz",
            "the resolved import is applied above the completion",
        );
    }

    #[test]
    fn a_resolved_import_is_dropped_when_the_buffer_moved_under_it() {
        let mut h = TestHarness::default();
        let path = open_scratch(&mut h, "");
        enable_resolve(&h);
        h.fake_lsp()
            .set_completion_resolve("barbaz", resolved_with_import("barbaz", "use foo;\n"));
        h.type_keys("i");
        h.type_text("bar");
        let items = vec![lsp_row(&h, "barbaz", 0..3)];
        install_popup(&mut h, items, 0..3);

        dispatch(&mut h.stoat, &AcceptCompletion);

        // The import names a line in the text the accept left behind. Edited
        // directly because a keypress would also pump the reply, closing the
        // window before the edit lands.
        let buffer_id = h.stoat.focused_editor_ids().expect("editor").1;
        h.stoat
            .active_workspace()
            .buffers
            .get(buffer_id)
            .expect("buffer")
            .write()
            .expect("poisoned")
            .edit(0..0, "x");
        h.settle();

        assert_eq!(
            h.buffer_text_at(&path),
            "xbarbaz",
            "an import landed against text it was not resolved for"
        );
    }

    #[test]
    fn accept_resolve_timeout_leaves_the_main_edit_alone() {
        let mut h = TestHarness::default();
        let path = open_scratch(&mut h, "");
        enable_resolve(&h);
        h.fake_lsp()
            .set_request_delay("completionItem/resolve", Duration::from_millis(400));
        h.fake_lsp()
            .set_completion_resolve("barbaz", resolved_with_import("barbaz", "use foo;\n"));
        h.type_keys("i");
        h.type_text("bar");
        let items = vec![lsp_row(&h, "barbaz", 0..3)];
        install_popup(&mut h, items, 0..3);

        dispatch(&mut h.stoat, &AcceptCompletion);
        // The resolve is delayed past the 300ms timeout, which fires first.
        h.advance_clock(Duration::from_millis(300));

        assert_eq!(h.buffer_text_at(&path), "barbaz");
    }

    #[test]
    fn word_accept_arms_no_resolve() {
        let mut h = TestHarness::default();
        let path = open_scratch(&mut h, "");
        enable_resolve(&h);
        h.type_keys("i");
        h.type_text("foo");
        let items = vec![CompletionItem {
            label: "foobar".into(),
            source: CompletionSource::Word,
            kind: None,
            detail: None,
            replace_range: anchors(&h, 0..3),
            insert_text: "foobar".into(),
            is_snippet: false,
            documentation: None,
            lsp_item: None,
            server: None,
        }];
        install_popup(&mut h, items, 0..3);

        dispatch(&mut h.stoat, &AcceptCompletion);

        assert_eq!(h.buffer_text_at(&path), "foobar");
        assert!(
            !h.stoat.pending_completion_accept.is_pending(),
            "a word accept issues no resolve",
        );
    }

    /// The popup outlives the keystroke that shifted every offset past the
    /// cursor, and the debounce leaves it reachable while a fresh request is
    /// still in flight. A range frozen at fetch time then names the middle of
    /// the multibyte character the deletion pulled under it.
    #[test]
    fn accept_after_backspacing_beside_a_multibyte_character_replaces_live_text() {
        let mut h = TestHarness::default();
        let path = open_scratch(&mut h, "éxx");
        h.type_keys("i");
        h.type_text("foo");
        let items = vec![CompletionItem {
            label: "foobar".into(),
            source: CompletionSource::Word,
            kind: None,
            detail: None,
            replace_range: anchors(&h, 0..3),
            insert_text: "foobar".into(),
            is_snippet: false,
            documentation: None,
            lsp_item: None,
            server: None,
        }];
        install_popup(&mut h, items, 0..3);

        h.type_keys("backspace");
        dispatch(&mut h.stoat, &AcceptCompletion);

        assert_eq!(h.buffer_text_at(&path), "foobaréxx");
    }

    /// The refine path carries an item across a keystroke rather than fetching
    /// again, so the item has to answer for the prefix as it stands now.
    #[test]
    fn a_character_typed_after_the_popup_opened_joins_what_accept_replaces() {
        let mut h = TestHarness::default();
        let path = open_scratch(&mut h, "");
        h.type_keys("i");
        h.type_text("pri");
        h.stoat.pending_completion = Some(CompletionPopup {
            prefix_range: 0..3,
            prefix: "pri".into(),
            ..CompletionPopup::showing(vec![CompletionItem {
                label: "println!".into(),
                source: CompletionSource::Lsp,
                kind: None,
                detail: None,
                replace_range: anchors(&h, 0..3),
                insert_text: "println!()".into(),
                is_snippet: false,
                documentation: None,
                lsp_item: None,
                server: None,
            }])
        });

        h.type_text("n");
        dispatch(&mut h.stoat, &AcceptCompletion);

        assert_eq!(h.buffer_text_at(&path), "println!()");
    }

    /// The same rule through the real pipeline, where the word source mints the
    /// range off the snapshot its scan read.
    #[test]
    fn a_word_item_replaces_the_prefix_typed_after_it_was_fetched() {
        let mut h = TestHarness::default();
        let contents = "foobar\n";
        let path = open_scratch(&mut h, contents);
        // Typed past the word, so the scan finds it as a token of its own
        // rather than as part of the prefix.
        crate::action_handlers::movement::jump_to_offset(&mut h.stoat, contents.len());
        h.type_keys("i");
        h.type_text("fo");
        h.advance_clock(crate::completion::request::COMPLETION_DEBOUNCE);
        let popup = h.stoat.pending_completion.as_ref().expect("popup armed");
        assert_eq!(
            popup
                .items
                .iter()
                .map(|item| (item.label.as_str(), item.source))
                .collect::<Vec<_>>(),
            [("foobar", CompletionSource::Word)],
        );

        h.type_text("o");
        dispatch(&mut h.stoat, &AcceptCompletion);

        assert_eq!(h.buffer_text_at(&path), "foobar\nfoobar");
    }

    /// The other cursor typed the same prefix, so it takes the newest cursor's
    /// range moved to itself, and each cursor ends after its own insertion.
    #[test]
    fn accept_completes_the_prefix_at_every_cursor() {
        let mut h = TestHarness::default();
        let path = open_scratch(&mut h, "foobar\n\n\n");
        crate::action_handlers::movement::jump_to_offset(&mut h.stoat, 7);
        h.type_keys("C");
        h.type_keys("i");
        h.type_text("foo");
        h.advance_clock(crate::completion::request::COMPLETION_DEBOUNCE);

        dispatch(&mut h.stoat, &AcceptCompletion);

        assert_eq!(
            (h.buffer_text_at(&path), cursor_offsets(&mut h)),
            ("foobar\nfoobar\nfoobar\n".to_string(), vec![13, 20]),
        );
    }

    /// A cursor whose text differs from the newest cursor's replaces its own
    /// word prefix, not the newest cursor's range moved to it.
    #[test]
    fn accept_completes_a_cursor_with_other_text_at_its_own_prefix() {
        let mut h = TestHarness::default();
        let path = open_scratch(&mut h, "ab fo\nfoo\n");
        set_cursors(&mut h, &[5, 9]);
        let items = vec![lsp_row(&h, "foobar", 6..9)];
        install_popup(&mut h, items, 6..9);

        dispatch(&mut h.stoat, &AcceptCompletion);

        assert_eq!(
            (h.buffer_text_at(&path), cursor_offsets(&mut h)),
            ("ab foobar\nfoobar\n".to_string(), vec![9, 16]),
        );
    }

    /// A server's text edit sometimes reaches past the typed prefix. That range
    /// replaces the same span around every cursor whose text there matches.
    #[test]
    fn accept_replaces_a_widened_range_at_every_cursor() {
        let mut h = TestHarness::default();
        let path = open_scratch(&mut h, "foo_x\nfoo_x\n");
        set_cursors(&mut h, &[3, 9]);
        let items = vec![lsp_row(&h, "foobar", 6..11)];
        install_popup(&mut h, items, 6..9);

        dispatch(&mut h.stoat, &AcceptCompletion);

        assert_eq!(
            (h.buffer_text_at(&path), cursor_offsets(&mut h)),
            ("foobar\nfoobar\n".to_string(), vec![6, 13]),
        );
    }

    /// A snippet expands at every cursor, each copy with its own tabstops, and
    /// Tab puts a cursor at the exit of every copy.
    #[test]
    fn accept_expands_a_snippet_at_every_cursor() {
        let mut h = TestHarness::default();
        let path = open_scratch(&mut h, "fo\nfo\n");
        h.type_keys("i");
        set_cursors(&mut h, &[2, 5]);
        let items = vec![CompletionItem {
            label: "foo".into(),
            source: CompletionSource::Lsp,
            kind: None,
            detail: None,
            replace_range: anchors(&h, 3..5),
            insert_text: "foo($1)$0".into(),
            is_snippet: true,
            documentation: None,
            lsp_item: None,
            server: None,
        }];
        install_popup(&mut h, items, 3..5);

        dispatch(&mut h.stoat, &AcceptCompletion);
        let expanded = (h.buffer_text_at(&path), selection_spans(&mut h));
        h.type_keys("tab");

        assert_eq!(
            (expanded, selection_spans(&mut h)),
            (
                ("foo()\nfoo()\n".to_string(), vec![(4, 4), (10, 10)]),
                vec![(5, 5), (11, 11)],
            ),
        );
    }

    /// The start and end of every selection, in document order.
    fn selection_spans(h: &mut TestHarness) -> Vec<(usize, usize)> {
        let editor = crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
        let snapshot = editor.display_map.snapshot();
        let buf = snapshot.buffer_snapshot();
        let mut spans: Vec<(usize, usize)> = editor
            .selections
            .all_anchors()
            .iter()
            .map(|sel| (buf.resolve_anchor(&sel.start), buf.resolve_anchor(&sel.end)))
            .collect();
        spans.sort_unstable();
        spans
    }

    /// Put a collapsed cursor at each of `offsets`, the last one newest.
    fn set_cursors(h: &mut TestHarness, offsets: &[usize]) {
        let (&first, rest) = offsets.split_first().expect("at least one cursor");
        let editor = crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
        let snapshot = editor.display_map.snapshot();
        let buf = snapshot.buffer_snapshot();
        let cursor = |id, at| Selection {
            id,
            start: buf.anchor_at(at, Bias::Right),
            end: buf.anchor_at(at, Bias::Right),
            reversed: false,
            goal: SelectionGoal::None,
        };
        editor
            .selections
            .transform(buf, |sel| cursor(sel.id, first));
        let others = rest.iter().map(|&at| cursor(0, at)).collect();
        editor.selections.extend_with_fresh_ids(others, buf);
    }

    /// The insertion offset of every cursor, in document order.
    fn cursor_offsets(h: &mut TestHarness) -> Vec<usize> {
        let editor = crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
        let snapshot = editor.display_map.snapshot();
        let buf = snapshot.buffer_snapshot();
        let mut offsets: Vec<usize> = editor
            .selections
            .all_anchors()
            .iter()
            .map(|sel| {
                let tail = buf.resolve_anchor(&sel.tail());
                let head = buf.resolve_anchor(&sel.head());
                stoat_text::cursor_offset(buf.rope(), tail, head)
            })
            .collect();
        offsets.sort_unstable();
        offsets
    }
}
